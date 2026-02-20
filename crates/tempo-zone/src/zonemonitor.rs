//! Zone L2 block monitor with integrated batch submission.
//!
//! Watches the **Zone L2** chain for new blocks, collecting withdrawal events and
//! reading on-chain state to produce [`BatchData`]. Aggregates multiple zone blocks
//! into a single L1 batch submission to minimize L1 transactions.
//!
//! ## Multi-block batching
//!
//! Instead of submitting one L1 transaction per zone block, the monitor scans all
//! available zone blocks and submits a single `submitBatch` call covering the entire
//! range. This dramatically reduces L1 transaction count during catch-up and keeps
//! the monitor in sync with the zone tip.
//!
//! Withdrawals from all blocks in the range are combined into a single hash chain
//! and stored under one portal queue slot.
//!
//! ## EIP-2935 and ancestry mode
//!
//! The portal verifies `tempoBlockNumber` via EIP-2935, which stores the last 8192
//! block hashes. When `tempoBlockNumber` is within this window the batch submitter
//! uses **direct mode** (reading the hash straight from EIP-2935). If the zone
//! falls behind (e.g. sequencer downtime >2 hours), the submitter automatically
//! switches to **ancestry mode**: it supplies a recent L1 block number that IS
//! within the EIP-2935 window, and the proof must include a block header chain
//! linking that anchor back to `tempoBlockNumber`.

use std::{sync::Arc, time::Duration};

use alloy_primitives::{Address, B256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use eyre::Result;
use tempo_alloy::TempoNetwork;
use tokio::sync::Notify;
use tracing::{error, info, instrument, warn};

use crate::{
    abi::{self, TempoState, ZoneInbox, ZoneOutbox, ZonePortal},
    batch::{BatchData, BatchSubmitter},
    withdrawals::SharedWithdrawalStore,
};

/// Maximum number of times to retry a failed batch submission before resyncing.
const MAX_RETRIES: u32 = 3;

/// Initial delay between retries (doubles on each attempt).
const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Configuration for the [`ZoneMonitor`].
#[derive(Debug, Clone)]
pub struct ZoneMonitorConfig {
    /// ZoneOutbox contract address on Zone L2.
    pub outbox_address: Address,
    /// ZoneInbox contract address on Zone L2.
    pub inbox_address: Address,
    /// TempoState predeploy address on Zone L2 (usually [`abi::TEMPO_STATE_ADDRESS`]).
    pub tempo_state_address: Address,
    /// Zone L2 RPC URL (HTTP).
    pub zone_rpc_url: String,
    /// How often to poll for new zone blocks.
    pub poll_interval: Duration,
    /// ZonePortal contract address on Tempo L1.
    pub portal_address: Address,
}

/// Monitors the Zone L2 chain for new blocks, aggregates them into batches, and
/// submits to the ZonePortal on L1.
///
/// Multiple zone blocks are combined into a single `submitBatch` call whenever
/// possible, reducing L1 transaction count. Local state only advances after a
/// successful L1 submission. On repeated failures the monitor resyncs from the
/// portal's on-chain `blockHash()`.
pub struct ZoneMonitor {
    config: ZoneMonitorConfig,
    /// Read-only HTTP provider pointed at the **Zone L2** RPC node.
    provider: DynProvider<TempoNetwork>,
    /// ZoneOutbox contract on **Zone L2** — source of `WithdrawalRequested` and
    /// `BatchFinalized` events.
    outbox: ZoneOutbox::ZoneOutboxInstance<DynProvider<TempoNetwork>, TempoNetwork>,
    /// ZoneInbox contract on **Zone L2** — queried for the processed deposit queue hash.
    inbox: ZoneInbox::ZoneInboxInstance<DynProvider<TempoNetwork>, TempoNetwork>,
    /// TempoState predeploy on **Zone L2** — provides the latest Tempo L1 block number
    /// as seen by the zone.
    tempo_state: TempoState::TempoStateInstance<DynProvider<TempoNetwork>, TempoNetwork>,
    /// Shared store for withdrawal data, written here and consumed by the
    /// [`WithdrawalProcessor`](crate::withdrawals::WithdrawalProcessor) on **Tempo L1**.
    withdrawal_store: SharedWithdrawalStore,
    /// Batch submitter for posting batches to the ZonePortal on **Tempo L1**.
    batch_submitter: BatchSubmitter,
    /// Notifier for the withdrawal processor — signalled after each successful
    /// batch submission so it can process newly enqueued withdrawal slots.
    withdrawal_notify: Arc<Notify>,
    /// Last **Zone L2** block number that was fully processed.
    last_processed_block: u64,
    /// Deposit queue hash from the previous block, used to construct the
    /// [`DepositQueueTransition`](crate::abi::DepositQueueTransition) for each batch.
    prev_processed_deposit_hash: B256,
    /// Previous zone block hash, used as `prev_block_hash` in [`BatchData`].
    /// Initialized from the portal's on-chain `blockHash()` at startup.
    prev_block_hash: B256,
    /// Tracks the portal's withdrawal queue tail position.
    /// The withdrawal store keys must match the portal's queue slot indices
    /// (not the L2 outbox's internal `withdrawalBatchIndex`). This counter is
    /// initialized from the portal’s on-chain `withdrawalQueueTail()` at startup,
    /// and incremented each time a batch with a non-zero
    /// `withdrawal_queue_hash` is successfully submitted to L1.
    portal_withdrawal_queue_tail: u64,
}

impl ZoneMonitor {
    /// Create a new zone monitor with integrated batch submission.
    ///
    /// Builds a read-only HTTP provider (no wallet) pointed at the Zone L2 RPC,
    /// instantiates the on-chain contract handles, and creates a [`BatchSubmitter`]
    /// backed by the shared `l1_provider` for posting batches to the ZonePortal on L1.
    pub async fn new(
        config: ZoneMonitorConfig,
        l1_provider: DynProvider<TempoNetwork>,
        withdrawal_store: SharedWithdrawalStore,
        withdrawal_notify: Arc<Notify>,
    ) -> Self {
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&config.zone_rpc_url)
            .await
            .expect("failed to connect to Zone RPC")
            .erased();

        let outbox = ZoneOutbox::new(config.outbox_address, provider.clone());
        let inbox = ZoneInbox::new(config.inbox_address, provider.clone());
        let tempo_state = TempoState::new(config.tempo_state_address, provider.clone());

        let portal_for_init = ZonePortal::new(config.portal_address, l1_provider.clone());
        let genesis_tempo_block_number: u64 = portal_for_init
            .genesisTempoBlockNumber()
            .call()
            .await
            .expect("failed to read genesisTempoBlockNumber");

        let batch_submitter = BatchSubmitter::new(
            config.portal_address,
            l1_provider,
            genesis_tempo_block_number,
        );

        let (prev_block_hash, portal_withdrawal_queue_tail) = tokio::try_join!(
            batch_submitter.read_portal_block_hash(),
            batch_submitter.read_portal_withdrawal_queue_tail(),
        )
        .expect("failed to read portal state at startup");

        let prev_processed_deposit_hash = inbox
            .processedDepositQueueHash()
            .call()
            .await
            .unwrap_or(B256::ZERO);

        info!(
            %prev_block_hash,
            %prev_processed_deposit_hash,
            portal_withdrawal_queue_tail,
            "Initialized from portal state"
        );

        Self {
            config,
            provider,
            outbox,
            inbox,
            tempo_state,
            withdrawal_store,
            batch_submitter,
            withdrawal_notify,
            last_processed_block: 0,
            prev_processed_deposit_hash,
            prev_block_hash,
            portal_withdrawal_queue_tail,
        }
    }

    /// Run the monitor loop. This method never returns under normal operation.
    #[instrument(skip_all, fields(
        outbox = %self.config.outbox_address,
        inbox = %self.config.inbox_address,
    ))]
    pub async fn run(&mut self) -> Result<()> {
        info!(
            zone_rpc = %self.config.zone_rpc_url,
            "Zone monitor started"
        );

        loop {
            match self.provider.get_block_number().await {
                Ok(latest) => {
                    if latest > self.last_processed_block {
                        let from = self.last_processed_block + 1;
                        if let Err(e) = self.process_block_range(from, latest).await {
                            error!(from, to = latest, error = %e, "Failed to process zone block range");
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, "Failed to query zone L2 block number");
                }
            }

            tokio::time::sleep(self.config.poll_interval).await;
        }
    }

    /// Process a range of zone blocks as a single batch.
    ///
    /// Scans all blocks in `[from, to]`, collects withdrawal events, reads end-of-range
    /// state, and submits one `submitBatch` call covering the entire range.
    ///
    /// ## Withdrawal handling
    ///
    /// The `withdrawalQueueHash` submitted to the portal must match the hash chain
    /// produced by `finalizeWithdrawalBatch` on L2. We collect all `WithdrawalRequested`
    /// events across the range and build a combined hash chain. The L2 outbox finalizes
    /// withdrawals per-block, but across a multi-block range we combine all withdrawals
    /// into a single portal queue slot.
    ///
    /// The `BatchFinalized` event's `withdrawalQueueHash` is used as the authoritative
    /// hash for single-block ranges (common case). For multi-block ranges with
    /// withdrawals, we recompute the combined hash from the collected withdrawal structs.
    #[instrument(skip(self), fields(from, to))]
    async fn process_block_range(&mut self, from: u64, to: u64) -> Result<()> {
        let block_count = to - from + 1;
        info!(from, to, block_count, "Processing zone block range");

        // --- 1. Fetch withdrawal events, finalized hashes, and block state concurrently ---
        let (all_withdrawals, finalized_hashes, end_state) = tokio::try_join!(
            self.fetch_withdrawals(from, to),
            self.fetch_finalized_hashes(from, to),
            self.fetch_block_snapshot(to),
        )?;

        // --- 2. Determine the withdrawal queue hash ---
        let withdrawal_queue_hash = if finalized_hashes.len() == 1 {
            // Single finalized batch — use the L2-authoritative hash directly.
            finalized_hashes[0]
        } else if finalized_hashes.len() > 1 || !all_withdrawals.is_empty() {
            // Multiple finalized batches or withdrawals present — recompute
            // a combined hash from all withdrawal structs.
            abi::Withdrawal::queue_hash(&all_withdrawals)
        } else {
            B256::ZERO
        };

        // --- 3. Validate withdrawal data consistency ---
        if !withdrawal_queue_hash.is_zero() && all_withdrawals.is_empty() {
            return Err(eyre::eyre!(
                "withdrawal_queue_hash is non-zero but no withdrawal events found — \
                 RPC may have returned incomplete data"
            ));
        }

        if !all_withdrawals.is_empty() {
            info!(
                from,
                to,
                count = all_withdrawals.len(),
                finalized_batches = finalized_hashes.len(),
                withdrawal_queue_hash = %withdrawal_queue_hash,
                "📤 Collected withdrawal requests from zone"
            );
        }

        // --- 4. Build and submit BatchData ---
        let batch_data = BatchData {
            tempo_block_number: end_state.tempo_block_number,
            prev_block_hash: self.prev_block_hash,
            next_block_hash: end_state.block_hash,
            prev_processed_deposit_hash: self.prev_processed_deposit_hash,
            next_processed_deposit_hash: end_state.processed_deposit_hash,
            withdrawal_queue_hash,
        };

        // Submit — state and withdrawal store only advance on success.
        self.submit_batch_with_retry(&batch_data, to, all_withdrawals)
            .await?;

        Ok(())
    }

    /// Fetch all `WithdrawalRequested` events in the given block range and convert
    /// them to [`abi::Withdrawal`] structs (order preserved from events).
    async fn fetch_withdrawals(&self, from: u64, to: u64) -> Result<Vec<abi::Withdrawal>> {
        let events = self
            .outbox
            .WithdrawalRequested_filter()
            .from_block(from)
            .to_block(to)
            .query()
            .await?;

        let mut events_with_order: Vec<_> = events
            .into_iter()
            .map(|(event, log)| {
                let sort_key = (
                    log.block_number.unwrap_or(0),
                    log.transaction_index.unwrap_or(0),
                    log.log_index.unwrap_or(0),
                );
                (sort_key, event)
            })
            .collect();

        events_with_order.sort_by_key(|(key, _)| *key);

        Ok(events_with_order
            .into_iter()
            .map(|(_, event)| abi::Withdrawal {
                token: event.token,
                sender: event.sender,
                to: event.to,
                amount: event.amount,
                fee: event.fee,
                memo: event.memo,
                gasLimit: event.gasLimit,
                fallbackRecipient: event.fallbackRecipient,
                callbackData: event.data,
            })
            .collect())
    }

    /// Fetch all non-zero `withdrawalQueueHash` values from `BatchFinalized` events
    /// in the given block range.
    async fn fetch_finalized_hashes(&self, from: u64, to: u64) -> Result<Vec<B256>> {
        let events = self
            .outbox
            .BatchFinalized_filter()
            .from_block(from)
            .to_block(to)
            .query()
            .await?;

        Ok(events
            .iter()
            .map(|(event, _)| event.withdrawalQueueHash)
            .filter(|h| !h.is_zero())
            .collect())
    }

    /// Read the zone state at block `to`: tempo block number, processed deposit
    /// queue hash, and block hash.
    async fn fetch_block_snapshot(&self, to: u64) -> Result<ZoneBlockSnapshot> {
        let tempo_call = self.tempo_state.tempoBlockNumber().block(to.into());
        let deposit_call = self.inbox.processedDepositQueueHash().block(to.into());
        let block_fut = async {
            self.provider
                .get_block_by_number(to.into())
                .await
                .map_err(Into::into)
        };
        let (tempo_block_number, processed_deposit_hash, block) =
            tokio::try_join!(tempo_call.call(), deposit_call.call(), block_fut,)?;

        let block_hash = block
            .ok_or_else(|| eyre::eyre!("zone block {to} not found"))?
            .header
            .hash;

        Ok(ZoneBlockSnapshot {
            tempo_block_number,
            processed_deposit_hash,
            block_hash,
        })
    }

    /// Submit a `submitBatch` transaction to the ZonePortal on L1 with exponential
    /// backoff retry.
    ///
    /// On success:
    /// - Advances `prev_block_hash`, `prev_processed_deposit_hash`, and
    ///   `last_processed_block` to reflect the submitted range.
    /// - Increments `portal_withdrawal_queue_tail` if the batch included withdrawals.
    /// - Notifies the [`WithdrawalProcessor`](crate::withdrawals::WithdrawalProcessor)
    ///   so it can finalize newly enqueued withdrawal slots.
    ///
    /// On failure (after [`MAX_RETRIES`] attempts with [`INITIAL_RETRY_DELAY`]
    /// doubling each time): resyncs `prev_block_hash` and
    /// `portal_withdrawal_queue_tail` from the portal and skips this block range
    /// so the monitor can continue.
    async fn submit_batch_with_retry(
        &mut self,
        batch_data: &BatchData,
        last_block_number: u64,
        withdrawals: Vec<abi::Withdrawal>,
    ) -> Result<()> {
        // Preflight: verify prev_block_hash matches portal state.
        match self.batch_submitter.read_portal_block_hash().await {
            Ok(portal_hash) if portal_hash != batch_data.prev_block_hash => {
                warn!(
                    local_prev = %batch_data.prev_block_hash,
                    portal_hash = %portal_hash,
                    "prev_block_hash mismatch with portal, resyncing"
                );
                self.resync_from_portal().await;
                return Ok(());
            }
            Err(e) => {
                warn!(error = %e, "Failed preflight portal hash check, continuing with submission");
            }
            _ => {}
        }

        let mut delay = INITIAL_RETRY_DELAY;

        for attempt in 1..=MAX_RETRIES {
            match self.batch_submitter.submit_batch(batch_data).await {
                Ok(tx_hash) => {
                    let blocks_in_batch = last_block_number - self.last_processed_block;
                    info!(
                        last_block_number,
                        blocks_in_batch,
                        tempo_block_number = batch_data.tempo_block_number,
                        %tx_hash,
                        withdrawal_queue_hash = %batch_data.withdrawal_queue_hash,
                        "Batch successfully submitted to L1"
                    );

                    // Only advance local state on success.
                    self.prev_block_hash = batch_data.next_block_hash;
                    self.prev_processed_deposit_hash = batch_data.next_processed_deposit_hash;
                    self.last_processed_block = last_block_number;

                    // Store withdrawals and advance portal queue tail if this batch had withdrawals.
                    if !batch_data.withdrawal_queue_hash.is_zero() {
                        if !withdrawals.is_empty() {
                            let portal_slot = self.portal_withdrawal_queue_tail;
                            let mut store = self.withdrawal_store.lock();
                            for w in &withdrawals {
                                store.add_withdrawal(portal_slot, w.clone());
                            }
                            info!(
                                portal_slot,
                                count = withdrawals.len(),
                                "Stored withdrawals for portal queue slot"
                            );
                        }
                        self.portal_withdrawal_queue_tail += 1;
                    }

                    // Signal the withdrawal processor.
                    self.withdrawal_notify.notify_one();

                    return Ok(());
                }
                Err(e) => {
                    if attempt < MAX_RETRIES {
                        warn!(
                            attempt,
                            max_retries = MAX_RETRIES,
                            delay_secs = delay.as_secs(),
                            error = %e,
                            "Batch submission failed, retrying"
                        );
                        tokio::time::sleep(delay).await;
                        delay *= 2;
                    } else {
                        error!(
                            error = %e,
                            last_block_number,
                            tempo_block_number = batch_data.tempo_block_number,
                            prev_block_hash = %batch_data.prev_block_hash,
                            next_block_hash = %batch_data.next_block_hash,
                            "Batch submission failed after {MAX_RETRIES} retries"
                        );
                    }
                }
            }
        }

        // All retries exhausted — resync from portal.
        self.resync_from_portal().await;

        Ok(())
    }

    /// Resync `prev_block_hash`, `prev_processed_deposit_hash`, and
    /// `portal_withdrawal_queue_tail` from on-chain state.
    ///
    /// Called after exhausting retries or when a preflight hash mismatch is
    /// detected, so subsequent batches start from the portal's actual state
    /// rather than stale local values. Does NOT advance `last_processed_block`
    /// — the same block range will be retried on the next poll cycle.
    async fn resync_from_portal(&mut self) {
        let old_hash = self.prev_block_hash;
        let old_tail = self.portal_withdrawal_queue_tail;
        match tokio::try_join!(
            self.batch_submitter.read_portal_block_hash(),
            self.batch_submitter.read_portal_withdrawal_queue_tail(),
        ) {
            Ok((portal_hash, portal_tail)) => {
                // Also resync prev_processed_deposit_hash from ZoneInbox.
                let deposit_hash = self
                    .inbox
                    .processedDepositQueueHash()
                    .call()
                    .await
                    .unwrap_or(self.prev_processed_deposit_hash);

                warn!(
                    old_prev_block_hash = %old_hash,
                    new_block_hash = %portal_hash,
                    old_portal_tail = old_tail,
                    new_portal_tail = portal_tail,
                    %deposit_hash,
                    "Resynced from portal and zone state"
                );
                self.prev_block_hash = portal_hash;
                self.portal_withdrawal_queue_tail = portal_tail;
                self.prev_processed_deposit_hash = deposit_hash;
            }
            Err(e) => {
                error!(
                    error = %e,
                    "Failed to read portal state during resync"
                );
            }
        }
    }
}

/// Spawn the zone monitor as a background task.
///
/// The monitor polls the Zone L2 for new blocks, aggregates them into batches,
/// and submits each batch to the ZonePortal on Tempo L1. Local state only
/// advances on successful submission.
///
/// The `l1_provider` must already include the sequencer wallet for signing L1 transactions.
pub fn spawn_zone_monitor(
    config: ZoneMonitorConfig,
    l1_provider: DynProvider<TempoNetwork>,
    withdrawal_store: SharedWithdrawalStore,
    withdrawal_notify: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut monitor =
            ZoneMonitor::new(config, l1_provider, withdrawal_store, withdrawal_notify).await;
        loop {
            if let Err(e) = monitor.run().await {
                error!(error = %e, "Zone monitor failed, restarting in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    })
}

/// Zone L2 state read at the last block of a processed range, used to populate
/// [`BatchData`] for the `submitBatch` call on L1.
struct ZoneBlockSnapshot {
    /// Latest Tempo L1 block number as seen by the zone (from the `TempoState`
    /// predeploy). Submitted to the portal for EIP-2935 verification.
    tempo_block_number: u64,
    /// Cumulative hash of all deposits processed by the zone up to this block
    /// (from `ZoneInbox.processedDepositQueueHash`).
    processed_deposit_hash: B256,
    /// Zone L2 block hash at the end of the range.
    block_hash: B256,
}
