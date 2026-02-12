//! Zone L2 block monitor with integrated batch submission.
//!
//! Watches the **Zone L2** chain for new blocks, collecting withdrawal events and
//! reading on-chain state to produce [`BatchData`]. Submits each batch **synchronously**
//! to the ZonePortal on Tempo L1 before advancing local state, ensuring that
//! `prev_block_hash` and `prev_processed_deposit_hash` never diverge from the portal.
//!
//! ## Data produced
//!
//! - **[`BatchData`]** — built from zone block data and submitted directly to the
//!   ZonePortal on L1. Local state only advances on successful submission.
//! - **Withdrawals** — extracted from `WithdrawalRequested` events and stored in the
//!   [`SharedWithdrawalStore`] so the withdrawal processor can later call
//!   `processWithdrawal` on L1.

use std::sync::Arc;
use std::time::Duration;

use alloy_primitives::{Address, B256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_rpc_types_eth::BlockNumberOrTag;
use alloy_signer_local::PrivateKeySigner;
use eyre::Result;
use tempo_alloy::TempoNetwork;
use tokio::sync::Notify;
use tracing::{debug, error, info, instrument, warn};

use crate::{
    abi::{self, TempoState, ZoneInbox, ZoneOutbox},
    batch::{BatchData, BatchSubmitter, BatchSubmitterConfig},
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
    /// Tempo L1 RPC URL (HTTP).
    pub l1_rpc_url: String,
    /// Private key signer for L1 batch submission transactions.
    pub signer: PrivateKeySigner,
}

/// Monitors the Zone L2 chain for new blocks, submits batches synchronously to
/// the ZonePortal on L1, and populates the [`SharedWithdrawalStore`].
///
/// In the current POC every zone block corresponds to exactly one batch.
/// Local state (`prev_block_hash`, `prev_processed_deposit_hash`) only advances
/// after a successful L1 submission. On repeated failures the monitor resyncs
/// from the portal's on-chain `blockHash()`.
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
    /// for posting batches to the ZonePortal on L1.
    pub async fn new(
        config: ZoneMonitorConfig,
        withdrawal_store: SharedWithdrawalStore,
        withdrawal_notify: Arc<Notify>,
    ) -> Self {
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_http(config.zone_rpc_url.parse().expect("valid Zone RPC URL"))
            .erased();

        let outbox = ZoneOutbox::new(config.outbox_address, provider.clone());
        let inbox = ZoneInbox::new(config.inbox_address, provider.clone());
        let tempo_state = TempoState::new(config.tempo_state_address, provider.clone());

        let batch_config = BatchSubmitterConfig {
            portal_address: config.portal_address,
            l1_rpc_url: config.l1_rpc_url.clone(),
        };
        let batch_submitter = BatchSubmitter::new(batch_config, config.signer.clone()).await;

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
                        for block_number in from..=latest {
                            if let Err(e) = self.process_block(block_number).await {
                                error!(block_number, error = %e, "Failed to process zone block");
                                break;
                            }
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

    /// Submit a batch to L1 with retry logic.
    ///
    /// Retries up to [`MAX_RETRIES`] times with exponential backoff (starting at
    /// [`INITIAL_RETRY_DELAY`]). On success, advances local state and notifies the
    /// withdrawal processor. On exhaustion of retries, resyncs `prev_block_hash`
    /// from the portal's on-chain `blockHash()` and skips this block.
    async fn submit_batch_with_retry(
        &mut self,
        batch_data: &BatchData,
        block_number: u64,
    ) -> Result<()> {
        let mut delay = INITIAL_RETRY_DELAY;

        for attempt in 1..=MAX_RETRIES {
            match self.batch_submitter.submit_batch(batch_data).await {
                Ok(tx_hash) => {
                    info!(
                        block_number,
                        tempo_block_number = batch_data.tempo_block_number,
                        %tx_hash,
                        withdrawal_queue_hash = %batch_data.withdrawal_queue_hash,
                        "Batch successfully submitted to L1"
                    );

                    // Only advance local state on success.
                    self.prev_block_hash = batch_data.next_block_hash;
                    self.prev_processed_deposit_hash =
                        batch_data.next_processed_deposit_hash;
                    self.last_processed_block = block_number;

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
                            block_number,
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
        self.resync_from_portal(block_number).await;

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

    /// Process a single zone block.
    ///
    /// In the POC every zone block is one batch. This method gathers everything
    /// needed to call `ZonePortal.submitBatch()` on L1:
    ///
    /// 1. Collects `WithdrawalRequested` events from the ZoneOutbox and stores the
    ///    full withdrawal structs in the [`SharedWithdrawalStore`]. The L1 portal only
    ///    stores hashes, so the sequencer must retain the original data to later call
    ///    `processWithdrawal()`.
    /// 2. Reads `BatchFinalized` to get the `withdrawalQueueHash` (the hash chain over
    ///    this batch's withdrawals) and the `withdrawalBatchIndex` used to key the store.
    /// 3. Reads `TempoState.tempoBlockNumber()` (the latest L1 block number the zone has
    ///    synced to) and `ZoneInbox.processedDepositQueueHash()` (how far the zone has
    ///    consumed the L1 deposit queue). These form the `BlockTransition` and
    ///    `DepositQueueTransition` structs the portal verifies on L1.
    /// 4. Constructs [`BatchData`] and submits it synchronously to L1. Local state
    ///    only advances on successful submission.
    #[instrument(skip(self), fields(block_number))]
    async fn process_block(&mut self, block_number: u64) -> Result<()> {
        debug!(block_number, "Processing zone block");

        // --- 1. Collect withdrawal events ---
        let withdrawal_events = self
            .outbox
            .WithdrawalRequested_filter()
            .from_block(block_number)
            .to_block(block_number)
            .query()
            .await?;

        // --- 2. Read BatchFinalized event ---
        let batch_finalized_events = self
            .outbox
            .BatchFinalized_filter()
            .from_block(block_number)
            .to_block(block_number)
            .query()
            .await?;

        let (withdrawal_queue_hash, withdrawal_batch_index) =
            if let Some((event, _log)) = batch_finalized_events.first() {
                (event.withdrawalQueueHash, Some(event.withdrawalBatchIndex))
            } else {
                (B256::ZERO, None)
            };

        // Store withdrawals under the portal's queue tail position (not the L2
        // outbox's internal withdrawalBatchIndex). The portal queue only advances
        // its tail when a non-zero withdrawalQueueHash is enqueued, so
        // `portal_withdrawal_queue_tail` is the slot index the processor will use.
        if withdrawal_queue_hash != B256::ZERO && !withdrawal_events.is_empty() {
            let portal_slot = self.portal_withdrawal_queue_tail;
            let mut store = self.withdrawal_store.lock();
            for (event, _log) in &withdrawal_events {
                let withdrawal = abi::Withdrawal {
                    sender: event.sender,
                    to: event.to,
                    amount: event.amount,
                    fee: event.fee,
                    memo: event.memo,
                    gasLimit: event.gasLimit,
                    fallbackRecipient: event.fallbackRecipient,
                    callbackData: event.data.clone(),
                };
                store.add_withdrawal(portal_slot, withdrawal);
            }
            info!(
                block_number,
                portal_slot,
                l2_batch_index = ?withdrawal_batch_index,
                count = withdrawal_events.len(),
                "Stored withdrawals for portal queue slot"
            );
        }

        // --- 3. Read zone state ---
        let tempo_block_number = self.tempo_state.tempoBlockNumber().call().await?;

        let next_processed_deposit_hash = self.inbox.processedDepositQueueHash().call().await?;

        let block = self
            .provider
            .get_block_by_number(BlockNumberOrTag::Number(block_number))
            .await?
            .ok_or_else(|| eyre::eyre!("zone block {block_number} not found"))?;

        let block_hash = block.header.hash;

        // --- 4. Build and submit BatchData synchronously ---
        // Use the tracked prev_block_hash rather than the L2 parent_hash.
        // The portal's blockHash is initialized to B256::ZERO at genesis, so the
        // first batch must use B256::ZERO as prev_block_hash. After each successful
        // submission the portal updates blockHash to next_block_hash, which we mirror.
        let batch_data = BatchData {
            tempo_block_number,
            prev_block_hash: self.prev_block_hash,
            next_block_hash: block_hash,
            prev_processed_deposit_hash: self.prev_processed_deposit_hash,
            next_processed_deposit_hash,
            withdrawal_queue_hash,
        };

        // Submit synchronously — state only advances on success.
        self.submit_batch_with_retry(&batch_data, block_number).await?;

        Ok(())
    }
}

/// Spawn the zone monitor as a background task.
///
/// The monitor polls the Zone L2 for new blocks, extracts withdrawal events into the
/// [`SharedWithdrawalStore`], builds [`BatchData`], and submits each batch synchronously
/// to the ZonePortal on Tempo L1. Local state only advances on successful submission.
pub fn spawn_zone_monitor(
    config: ZoneMonitorConfig,
    withdrawal_store: SharedWithdrawalStore,
    withdrawal_notify: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut monitor = ZoneMonitor::new(config, withdrawal_store, withdrawal_notify).await;
        loop {
            if let Err(e) = monitor.run().await {
                error!(error = %e, "Zone monitor failed, restarting in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    })
}
