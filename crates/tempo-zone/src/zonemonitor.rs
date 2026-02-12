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
//! ## EIP-2935 constraint
//!
//! The portal verifies `tempoBlockNumber` via EIP-2935, which stores the last 8192
//! block hashes. The batch's `tempoBlockNumber` must be within this window of the
//! current L1 block. In practice this is not a concern because the zone produces
//! blocks at L1 speed — the monitor would need to fall ~2.3 hours behind to hit
//! this limit.

use std::{sync::Arc, time::Duration};

use alloy_primitives::{Address, B256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_rpc_types_eth::BlockNumberOrTag;
use eyre::Result;
use tempo_alloy::TempoNetwork;
use tokio::sync::Notify;
use tracing::{error, info, instrument, warn};

use crate::{
    abi::{self, TempoState, ZoneInbox, ZoneOutbox},
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
    /// Initialized to `B256::ZERO` to match the portal's genesis `blockHash`.
    prev_block_hash: B256,
    /// Tracks the portal's withdrawal queue tail position.
    /// The withdrawal store keys must match the portal's queue slot indices
    /// (not the L2 outbox's internal `withdrawalBatchIndex`). This counter
    /// starts at 0 and increments each time a batch with a non-zero
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
            .connect_http(config.zone_rpc_url.parse().expect("valid Zone RPC URL"))
            .erased();

        let outbox = ZoneOutbox::new(config.outbox_address, provider.clone());
        let inbox = ZoneInbox::new(config.inbox_address, provider.clone());
        let tempo_state = TempoState::new(config.tempo_state_address, provider.clone());

        let batch_submitter = BatchSubmitter::new(config.portal_address, l1_provider);

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
            prev_processed_deposit_hash: B256::ZERO,
            prev_block_hash: B256::ZERO,
            portal_withdrawal_queue_tail: 0,
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

        // --- 1. Collect all withdrawal events across the range ---
        let withdrawal_events = self
            .outbox
            .WithdrawalRequested_filter()
            .from_block(from)
            .to_block(to)
            .query()
            .await?;

        // Convert events to withdrawal structs (order preserved from events)
        let all_withdrawals: Vec<abi::Withdrawal> = withdrawal_events
            .iter()
            .map(|(event, _log)| abi::Withdrawal {
                sender: event.sender,
                to: event.to,
                amount: event.amount,
                fee: event.fee,
                memo: event.memo,
                gasLimit: event.gasLimit,
                fallbackRecipient: event.fallbackRecipient,
                callbackData: event.data.clone(),
            })
            .collect();

        // --- 2. Determine the withdrawal queue hash ---
        //
        // For correctness, read the `BatchFinalized` events to get the L2-computed
        // hash. When the range covers exactly one finalized batch, use its hash
        // directly. For multiple finalized batches (or to be safe), recompute from
        // the collected withdrawal structs.
        let batch_finalized_events = self
            .outbox
            .BatchFinalized_filter()
            .from_block(from)
            .to_block(to)
            .query()
            .await?;

        // Filter to non-zero hashes (zero = no withdrawals in that block)
        let finalized_hashes: Vec<B256> = batch_finalized_events
            .iter()
            .map(|(event, _)| event.withdrawalQueueHash)
            .filter(|h| *h != B256::ZERO)
            .collect();

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

        if !all_withdrawals.is_empty() {
            info!(
                from,
                to,
                count = all_withdrawals.len(),
                finalized_batches = finalized_hashes.len(),
                withdrawal_queue_hash = %withdrawal_queue_hash,
                "Collected withdrawals across block range"
            );
        }

        // --- 3. Store withdrawals under the next portal queue slot ---
        if withdrawal_queue_hash != B256::ZERO {
            let portal_slot = self.portal_withdrawal_queue_tail;
            let mut store = self.withdrawal_store.lock();
            for w in &all_withdrawals {
                store.add_withdrawal(portal_slot, w.clone());
            }
            info!(
                portal_slot,
                count = all_withdrawals.len(),
                "Stored withdrawals for portal queue slot"
            );
        }

        // --- 4. Read end-of-range zone state ---
        let tempo_block_number: u64 = self
            .tempo_state
            .tempoBlockNumber()
            .block(BlockNumberOrTag::Number(to).into())
            .call()
            .await?;

        let next_processed_deposit_hash: B256 = self
            .inbox
            .processedDepositQueueHash()
            .block(BlockNumberOrTag::Number(to).into())
            .call()
            .await?;

        let block = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Number(to))
            .await?
            .ok_or_else(|| eyre::eyre!("zone block {to} not found"))?;

        let block_hash = block.header.hash;

        // --- 5. Build and submit BatchData ---
        let batch_data = BatchData {
            tempo_block_number,
            prev_block_hash: self.prev_block_hash,
            next_block_hash: block_hash,
            prev_processed_deposit_hash: self.prev_processed_deposit_hash,
            next_processed_deposit_hash,
            withdrawal_queue_hash,
        };

        // Submit — state only advances on success.
        self.submit_batch_with_retry(&batch_data, to).await?;

        Ok(())
    }

    /// Submit a batch to L1 with retry logic.
    ///
    /// Retries up to [`MAX_RETRIES`] times with exponential backoff (starting at
    /// [`INITIAL_RETRY_DELAY`]). On success, advances local state and notifies the
    /// withdrawal processor. On exhaustion of retries, resyncs `prev_block_hash`
    /// from the portal's on-chain `blockHash()` and skips this block range.
    async fn submit_batch_with_retry(
        &mut self,
        batch_data: &BatchData,
        last_block_number: u64,
    ) -> Result<()> {
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

                    // Advance portal queue tail if this batch had withdrawals.
                    if batch_data.withdrawal_queue_hash != B256::ZERO {
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
        self.resync_from_portal(last_block_number).await;

        Ok(())
    }

    /// Resync `prev_block_hash` from the portal's on-chain `blockHash()`.
    ///
    /// Called after exhausting retries so subsequent batches start from the
    /// portal's actual state rather than the stale local value.
    async fn resync_from_portal(&mut self, block_number: u64) {
        let old_hash = self.prev_block_hash;
        match self.batch_submitter.read_portal_block_hash().await {
            Ok(portal_hash) => {
                warn!(
                    old_prev_block_hash = %old_hash,
                    new_block_hash = %portal_hash,
                    block_number,
                    "Resynced prev_block_hash from portal after max retries"
                );
                self.prev_block_hash = portal_hash;
                // Skip this block — the monitor will pick up from the next one.
                self.last_processed_block = block_number;
            }
            Err(e) => {
                error!(
                    error = %e,
                    "Failed to read blockHash from portal during resync"
                );
                // Don't advance last_processed_block so we retry this block.
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
