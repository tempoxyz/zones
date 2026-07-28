//! Zone L2 block monitor with integrated batch submission.
//!
//! Watches the **Zone L2** chain for new blocks, collecting withdrawal events and
//! reading on-chain state to produce [`BatchData`]. Emits one L1 batch
//! submission for each L2 `BatchFinalized` boundary.
//!
//! ## Batch boundaries
//!
//! The payload builder records batch boundaries by appending
//! `ZoneOutbox.finalizeWithdrawalBatch` to final blocks only. The monitor treats
//! those `BatchFinalized` events as authoritative and emits exactly one
//! `submitBatch` per boundary, in order.
//!
//! ## EIP-2935 and ancestry mode
//!
//! The portal verifies `tempoBlockNumber` via EIP-2935, which stores the last 8192
//! block hashes. When `tempoBlockNumber` is within this window the batch submitter
//! uses **direct mode** (reading the hash straight from EIP-2935). If the zone
//! falls behind (e.g. sequencer downtime >2 hours), the submitter automatically
//! switches to **ancestry mode** for that batch: it supplies a recent L1 block
//! number that IS within the EIP-2935 window, and the proof must include a
//! block header chain linking that anchor back to `tempoBlockNumber`.

use std::{sync::Arc, time::Duration};

use alloy_primitives::{Address, B256};
use alloy_provider::DynProvider;
use alloy_signer_local::PrivateKeySigner;
use eyre::{Result, WrapErr};
use futures::StreamExt;
use tempo_alloy::TempoNetwork;
use tokio::sync::Notify;
use tracing::{error, info, instrument, warn};

use alloy_sol_types::{ContractError, SolInterface as _};

use crate::{
    AttestationStore, ZoneSequencerProvider,
    abi::{self, NO_QUEUE_INDEX, ZonePortal},
    settlement::{
        BatchAnchorConfig, BatchData, BatchSubmitter, FinalizedBatchLog, ZoneBlockSnapshot,
        fetch_finalized_batch, fetch_finalized_batch_boundaries, read_zone_block_snapshot,
    },
    withdrawals::SharedWithdrawalStore,
};

/// Maximum number of times to retry a failed batch submission before resyncing.
const MAX_RETRIES: u32 = 3;

/// Initial delay between retries (doubles on each attempt).
const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(2);

/// Fallback reconciliation interval for lagged notifications and retryable provider failures.
const FALLBACK_RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

/// Configuration for the [`ZoneMonitor`].
#[derive(Debug, Clone)]
pub struct ZoneMonitorConfig {
    /// ZoneOutbox contract address on Zone L2.
    pub outbox_address: Address,
    /// ZoneInbox contract address on Zone L2.
    pub inbox_address: Address,
    /// ZonePortal contract address on Tempo L1.
    pub portal_address: Address,
    /// EIP-2935 history and safety-margin limits used by the batch submitter.
    pub batch_anchor_config: BatchAnchorConfig,
    /// Shared P2P attestations, required after a settlement signer set is activated.
    pub attestation_store: Option<AttestationStore>,
}

/// Monitors the Zone L2 chain for new finalized batch boundaries and submits
/// them to the ZonePortal on L1.
///
/// Local state only advances after a successful L1 submission. On repeated
/// failures the monitor resyncs from the portal's on-chain `blockHash()`.
pub struct ZoneMonitor<P: ZoneSequencerProvider> {
    config: ZoneMonitorConfig,
    /// Metrics for zone observation and L1 batch submission.
    metrics: crate::metrics::ZoneMonitorMetrics,
    /// Native provider for canonical Tempo blocks, transactions, and receipts.
    provider: P,
    /// Shared store for withdrawal data, written here and consumed by the
    /// [`WithdrawalProcessor`](crate::withdrawals::WithdrawalProcessor) on **Tempo L1**.
    withdrawal_store: SharedWithdrawalStore,
    /// Batch submitter for posting batches to the ZonePortal on **Tempo L1**.
    batch_submitter: BatchSubmitter,
    /// Notifier for the withdrawal processor — signalled after each successful
    /// batch submission so it can process newly enqueued withdrawal slots.
    withdrawal_notify: Arc<Notify>,
    /// Notifier from the withdrawal processor when the current portal head slot
    /// is missing from the in-memory store and a full portal resync is needed.
    repair_notify: Arc<Notify>,
    /// Last **Zone L2** block number that was successfully submitted to L1.
    last_submitted_zone_block: u64,
    /// Deposit queue hash from the previous block, used to construct the
    /// [`DepositQueueTransition`](crate::abi::DepositQueueTransition) for each batch.
    prev_processed_deposit_hash: B256,
    /// Deposit counter from the previous batch, used to construct the
    /// [`DepositQueueTransition`](crate::abi::DepositQueueTransition) for each batch.
    prev_processed_deposit_number: u64,
    /// Previous zone block hash, used as `prev_block_hash` in [`BatchData`].
    /// Initialized from the portal's on-chain `blockHash()` at startup.
    prev_zone_block_hash: B256,
    /// Most recent canonical zone block observed from the node.
    latest_observed_zone_block: u64,
}

impl<P: ZoneSequencerProvider> ZoneMonitor<P> {
    /// Create a new zone monitor with integrated batch submission.
    ///
    /// Uses the node's native provider for Zone data and creates a [`BatchSubmitter`] backed by
    /// the shared `l1_provider` for posting batches to the ZonePortal on L1.
    pub async fn new(
        config: ZoneMonitorConfig,
        provider: P,
        l1_provider: DynProvider<TempoNetwork>,
        signer: PrivateKeySigner,
        withdrawal_store: SharedWithdrawalStore,
        withdrawal_notify: Arc<Notify>,
        repair_notify: Arc<Notify>,
    ) -> Result<Self> {
        Self::new_with_provider(
            config,
            provider,
            l1_provider,
            Some(signer),
            withdrawal_store,
            withdrawal_notify,
            repair_notify,
        )
        .await
    }

    async fn new_with_provider(
        config: ZoneMonitorConfig,
        provider: P,
        l1_provider: DynProvider<TempoNetwork>,
        signer: Option<PrivateKeySigner>,
        withdrawal_store: SharedWithdrawalStore,
        withdrawal_notify: Arc<Notify>,
        repair_notify: Arc<Notify>,
    ) -> Result<Self> {
        let metrics = crate::metrics::ZoneMonitorMetrics::default();
        let mut batch_submitter = BatchSubmitter::with_optional_signer_and_anchor_config(
            config.portal_address,
            l1_provider,
            signer,
            config.batch_anchor_config,
        );
        batch_submitter.set_attestation_store(config.attestation_store.clone());

        let prev_zone_block_hash = batch_submitter
            .read_portal_block_hash()
            .await
            .wrap_err("failed to read portal block hash during zone monitor startup")?;

        let last_submitted_zone_block =
            Self::resolve_zone_block_number(&provider, prev_zone_block_hash)?;
        let previous_snapshot = Self::snapshot_at_or_genesis(
            &provider,
            config.inbox_address,
            last_submitted_zone_block,
        )?;
        let prev_processed_deposit_hash = previous_snapshot.processed_deposit_hash;
        let prev_processed_deposit_number = previous_snapshot.processed_deposit_number;

        info!(
            last_submitted_zone_block,
            %prev_zone_block_hash,
            %prev_processed_deposit_hash,
            prev_processed_deposit_number,
            "Initialized from portal state"
        );

        metrics
            .latest_zone_block_observed
            .set(last_submitted_zone_block as f64);
        metrics
            .latest_zone_block_submitted_to_l1
            .set(last_submitted_zone_block as f64);
        metrics.zone_to_l1_submission_lag_blocks.set(0.0);

        let monitor = Self {
            config,
            metrics,
            provider,
            withdrawal_store,
            batch_submitter,
            withdrawal_notify,
            repair_notify,
            last_submitted_zone_block,
            prev_processed_deposit_hash,
            prev_processed_deposit_number,
            prev_zone_block_hash,
            latest_observed_zone_block: last_submitted_zone_block,
        };

        // Restore pending withdrawal data from zone L2 events so the
        // withdrawal processor can pick up where it left off.
        monitor
            .restore_pending_withdrawals_from_chain()
            .await
            .wrap_err("failed to restore pending withdrawals during zone monitor startup")?;

        Ok(monitor)
    }

    /// Run the monitor loop. This method never returns under normal operation.
    ///
    /// Reconciles the canonical head at startup and after every canonical-state notification.
    #[instrument(skip_all, fields(
        outbox = %self.config.outbox_address,
        inbox = %self.config.inbox_address,
    ))]
    pub async fn run(&mut self) -> Result<()> {
        info!("Native zone monitor started");

        // Subscribe before reading the head so a block imported during startup cannot be missed.
        let mut canonical = self.provider.canonical_state_stream();
        let mut fallback = tokio::time::interval(FALLBACK_RECONCILE_INTERVAL);

        loop {
            self.process_available_blocks().await;

            tokio::select! {
                _ = fallback.tick() => {}
                notification = canonical.next() => {
                    let Some(notification) = notification else {
                        return Err(eyre::eyre!("canonical zone state notification stream closed"));
                    };
                    if notification.reverted().is_some() {
                        return Err(eyre::eyre!(
                            "canonical zone chain reorged while the sequencer was active"
                        ));
                    }
                }
                _ = self.repair_notify.notified() => {
                    self.repair_missing_withdrawal_slot().await;
                }
            }
        }
    }

    async fn process_available_blocks(&mut self) {
        let latest_zone_block = match self.provider.best_block_number() {
            Ok(number) => number,
            Err(error) => {
                error!(%error, "Failed to read canonical zone head");
                return;
            }
        };
        let scan_from = self.latest_observed_zone_block.saturating_add(1);
        if latest_zone_block < scan_from {
            return;
        }

        match self.process_block_range(scan_from, latest_zone_block).await {
            Ok(_) => self.record_observed_zone_block(latest_zone_block),
            Err(error) => {
                error!(
                    from = scan_from,
                    to = latest_zone_block,
                    %error,
                    "Failed to process canonical zone block range"
                );
            }
        }
    }

    /// Rebuild the in-memory withdrawal store from authoritative chain state.
    ///
    /// The L1 portal only stores queue hashes, so the monitor reconstructs the
    /// pending withdrawal payloads from L1 + zone-L2 events and replaces the
    /// local store with that result. Used during startup and after a portal
    /// resync when local withdrawal data may be stale or missing.
    async fn restore_pending_withdrawals_from_chain(&self) -> Result<()> {
        let pending = match self
            .batch_submitter
            .fetch_pending_withdrawals(&self.provider, self.config.outbox_address)
            .await
        {
            Ok(pending) => pending,
            Err(err) => {
                self.metrics
                    .withdrawal_store_restore_failure_total
                    .increment(1);
                return Err(err);
            }
        };
        let restored_withdrawals = pending.values().map(Vec::len).sum::<usize>();
        let reconciled_first_slot = pending.keys().next().copied();
        let reconciled_last_slot = pending.keys().next_back().copied();

        let mut store = self.withdrawal_store.lock();
        let (previous_slots, previous_first_slot, previous_last_slot) = store.summary();
        store.replace_batches(pending);
        let reconciled_slots = store.batch_count();
        drop(store);

        if reconciled_slots > 0 {
            info!(
                previous_slots,
                previous_first_slot,
                previous_last_slot,
                reconciled_slots,
                reconciled_first_slot,
                reconciled_last_slot,
                restored_withdrawals,
                "Restored pending withdrawals from chain"
            );
            self.withdrawal_notify.notify_one();
        } else if previous_slots > 0 {
            info!(
                previous_slots,
                previous_first_slot,
                previous_last_slot,
                "Cleared stale withdrawal batches after restoring pending withdrawals from chain"
            );
        }

        Ok(())
    }

    /// Repair monitor state after the withdrawal processor reports a missing head slot.
    ///
    /// This intentionally goes through a full portal resync rather than only
    /// rebuilding the withdrawal store. An ambiguous `submitBatch` outcome can
    /// leave both the portal anchor and the in-memory withdrawal data stale, so
    /// the monitor first reloads the portal-confirmed anchor and then rebuilds
    /// pending withdrawals from chain state.
    async fn repair_missing_withdrawal_slot(&mut self) {
        warn!("Withdrawal processor reported a missing portal head slot");
        self.resync_from_portal().await;
    }

    /// Process finalized batch boundaries in `[from, to]`.
    ///
    /// The builder records each batch boundary with one `BatchFinalized` event.
    /// The monitor must walk those boundaries one at a time so the L2 outbox
    /// index and L1 portal index advance in lockstep.
    #[instrument(skip(self), fields(from, to))]
    async fn process_block_range(&mut self, from: u64, to: u64) -> Result<bool> {
        let block_count = to - from + 1;
        info!(from, to, block_count, "Processing zone block range");

        let boundaries =
            fetch_finalized_batch_boundaries(&self.provider, self.config.outbox_address, from, to)
                .await?;
        if boundaries.is_empty() {
            info!(from, to, "No finalized batch boundaries ready to submit");
            return Ok(false);
        }

        info!(
            boundary_count = boundaries.len(),
            from,
            to,
            first_boundary = boundaries[0].block_number,
            last_boundary = boundaries[boundaries.len() - 1].block_number,
            "Submitting finalized zone batches"
        );

        for (idx, boundary) in boundaries.into_iter().enumerate() {
            let boundary_block = boundary.block_number;
            if boundary_block <= self.last_submitted_zone_block {
                continue;
            }
            let range_start = self.last_submitted_zone_block + 1;
            info!(
                batch = idx + 1,
                zone_from = range_start,
                zone_to = boundary_block,
                "Submitting finalized zone batch"
            );
            let before_submit = self.last_submitted_zone_block;
            self.process_finalized_batch(range_start, boundary).await?;
            if self.last_submitted_zone_block <= before_submit {
                warn!(
                    before_submit,
                    boundary = boundary_block,
                    current_last_submitted = self.last_submitted_zone_block,
                    "Batch submission did not advance local state; stopping boundary walk"
                );
                break;
            }
        }

        Ok(true)
    }

    /// Process one boundary-aligned finalized batch.
    async fn process_finalized_batch(
        &mut self,
        from: u64,
        boundary: FinalizedBatchLog,
    ) -> Result<()> {
        let to = boundary.block_number;
        let finalized_batch =
            fetch_finalized_batch(&self.provider, self.config.outbox_address, from, &boundary)
                .await?;
        let end_state = self.fetch_block_snapshot(to).await?;

        if !finalized_batch.withdrawals.is_empty() {
            info!(
                from,
                to,
                count = finalized_batch.withdrawals.len(),
                withdrawal_queue_hash = %finalized_batch.finalized_hash,
                withdrawal_batch_index = finalized_batch.finalized_index,
                "Collected finalized withdrawals from zone"
            );
        }

        let batch_data = BatchData {
            zone_height: to,
            tempo_block_number: end_state.tempo_block_number,
            prev_block_hash: self.prev_zone_block_hash,
            next_block_hash: end_state.block_hash,
            prev_processed_deposit_hash: self.prev_processed_deposit_hash,
            next_processed_deposit_hash: end_state.processed_deposit_hash,
            prev_deposit_number: self.prev_processed_deposit_number,
            next_deposit_number: end_state.processed_deposit_number,
            withdrawal_queue_hash: finalized_batch.finalized_hash,
            withdrawal_batch_index: finalized_batch.finalized_index,
        };

        self.submit_batch_with_retry(&batch_data, to, finalized_batch.withdrawals)
            .await
    }

    /// Read the zone state at block `to`: tempo block number, processed deposit
    /// queue hash, and block hash.
    async fn fetch_block_snapshot(&self, to: u64) -> Result<ZoneBlockSnapshot> {
        read_zone_block_snapshot(&self.provider, self.config.inbox_address, to)
    }

    /// Submit a `submitBatch` transaction to the ZonePortal on L1 with exponential
    /// backoff retry.
    ///
    /// On success:
    /// - Advances `prev_zone_block_hash`, `prev_processed_deposit_hash`, and
    ///   `last_submitted_zone_block` to reflect the submitted range.
    /// - Stores withdrawals under the receipt's assigned portal queue index when
    ///   the batch included withdrawals.
    /// - Signals the [`WithdrawalProcessor`](crate::withdrawals::WithdrawalProcessor)
    ///   so it can finalize newly enqueued withdrawal slots.
    ///
    /// On failure (after [`MAX_RETRIES`] attempts with [`INITIAL_RETRY_DELAY`]
    /// doubling each time): resyncs the local submission anchor from the
    /// portal-confirmed zone block so the next poll starts from accepted
    /// on-chain state.
    async fn submit_batch_with_retry(
        &mut self,
        batch_data: &BatchData,
        last_zone_block: u64,
        withdrawals: Vec<abi::Withdrawal>,
    ) -> Result<()> {
        let mut delay = INITIAL_RETRY_DELAY;

        for attempt in 1..=MAX_RETRIES {
            // Reconcile before every attempt. A prior submitBatch may have landed even when its
            // receipt timed out, in which case waiting for the old certificate can block forever.
            let portal_hash = match self.batch_submitter.read_portal_block_hash().await {
                Ok(portal_hash) => portal_hash,
                Err(e) => {
                    if attempt < MAX_RETRIES {
                        self.metrics.batch_submit_retry_total.increment(1);
                        warn!(
                            attempt,
                            max_retries = MAX_RETRIES,
                            delay_secs = delay.as_secs(),
                            error = %e,
                            "Failed reading portal state before batch submission, retrying"
                        );
                        tokio::time::sleep(delay).await;
                        delay *= 2;
                        continue;
                    }
                    self.metrics.batch_submit_failure_total.increment(1);
                    error!(
                        error = %e,
                        last_zone_block,
                        "Failed reading portal state before batch submission after {MAX_RETRIES} retries"
                    );
                    break;
                }
            };
            if portal_hash != batch_data.prev_block_hash {
                warn!(
                    local_prev = %batch_data.prev_block_hash,
                    portal_hash = %portal_hash,
                    "prev_block_hash mismatch with portal, resyncing"
                );
                self.resync_from_portal().await;
                return Ok(());
            }

            let submit_started = std::time::Instant::now();
            match self.batch_submitter.submit_batch(batch_data).await {
                Ok(event) => {
                    let portal_index = if event.withdrawalQueueIndex == NO_QUEUE_INDEX {
                        None
                    } else {
                        Some(event.withdrawalQueueIndex.try_into().map_err(|_| {
                            eyre::eyre!("withdrawal queue index overflow in BatchSubmitted")
                        })?)
                    };

                    self.metrics
                        .batch_submit_latency_seconds
                        .record(submit_started.elapsed().as_secs_f64());
                    let blocks_in_batch = last_zone_block - self.last_submitted_zone_block;
                    info!(
                        last_zone_block,
                        blocks_in_batch,
                        tempo_block_number = batch_data.tempo_block_number,
                        withdrawal_batch_index = event.withdrawalBatchIndex,
                        withdrawal_queue_index = %event.withdrawalQueueIndex,
                        withdrawal_queue_hash = %batch_data.withdrawal_queue_hash,
                        "Batch successfully submitted to L1"
                    );
                    self.metrics.batch_submit_success_total.increment(1);
                    self.metrics
                        .batch_size_blocks
                        .record(blocks_in_batch as f64);
                    self.metrics
                        .withdrawals_per_batch
                        .record(withdrawals.len() as f64);

                    // Only advance local state on success.
                    self.prev_zone_block_hash = batch_data.next_block_hash;
                    self.prev_processed_deposit_hash = batch_data.next_processed_deposit_hash;
                    self.prev_processed_deposit_number = batch_data.next_deposit_number;
                    self.last_submitted_zone_block = last_zone_block;
                    self.metrics
                        .latest_zone_block_submitted_to_l1
                        .set(last_zone_block as f64);
                    self.update_submission_lag();

                    // Store withdrawals under the logical portal queue index assigned on-chain.
                    if let Some(portal_index) = portal_index {
                        if !withdrawals.is_empty() {
                            let count = withdrawals.len();
                            let mut store = self.withdrawal_store.lock();
                            store.add_batch(portal_index, withdrawals);
                            info!(
                                portal_index,
                                count, "Stored withdrawals for portal queue index"
                            );
                        }
                    } else {
                        if !batch_data.withdrawal_queue_hash.is_zero() || !withdrawals.is_empty() {
                            warn!(
                                withdrawal_queue_hash = %batch_data.withdrawal_queue_hash,
                                withdrawal_count = withdrawals.len(),
                                "submitBatch emitted NO_QUEUE_INDEX for a batch that locally had withdrawals"
                            );
                        }
                    }

                    self.withdrawal_notify.notify_one();

                    return Ok(());
                }
                Err(e) => {
                    self.metrics
                        .batch_submit_latency_seconds
                        .record(submit_started.elapsed().as_secs_f64());
                    if attempt < MAX_RETRIES {
                        self.metrics.batch_submit_retry_total.increment(1);
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
                        self.metrics.batch_submit_failure_total.increment(1);
                        let revert_reason = decode_portal_revert(&e);
                        error!(
                            error = %e,
                            revert_reason,
                            last_zone_block,
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

        Err(eyre::eyre!(
            "batch submission failed after {MAX_RETRIES} retries for zone block {last_zone_block}"
        ))
    }

    /// Resync the local submission anchor from portal-confirmed on-chain state.
    ///
    /// Called after exhausting retries or when a preflight hash mismatch is
    /// detected, so subsequent batches start from the portal's actual accepted
    /// zone block rather than stale local values.
    async fn resync_from_portal(&mut self) {
        self.metrics.resync_from_portal_total.increment(1);
        let old_hash = self.prev_zone_block_hash;
        let old_last_submitted = self.last_submitted_zone_block;
        let (
            store_batches_before_resync,
            store_first_slot_before_resync,
            store_last_slot_before_resync,
        ) = {
            let store = self.withdrawal_store.lock();
            store.summary()
        };
        match self.batch_submitter.read_portal_block_hash().await {
            Ok(portal_hash) => {
                let last_submitted_zone_block =
                    match Self::resolve_zone_block_number(&self.provider, portal_hash) {
                        Ok(number) => number,
                        Err(error) => {
                            error!(%error, "Failed to resolve portal-confirmed zone block");
                            return;
                        }
                    };
                let previous_snapshot = match Self::snapshot_at_or_genesis(
                    &self.provider,
                    self.config.inbox_address,
                    last_submitted_zone_block,
                ) {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        error!(
                            %error,
                            last_submitted_zone_block,
                            "Failed to read portal-confirmed zone commitments"
                        );
                        return;
                    }
                };
                let deposit_hash = previous_snapshot.processed_deposit_hash;
                let deposit_number = previous_snapshot.processed_deposit_number;

                warn!(
                    old_prev_block_hash = %old_hash,
                    new_block_hash = %portal_hash,
                    old_last_submitted_zone_block = old_last_submitted,
                    new_last_submitted_zone_block = last_submitted_zone_block,
                    store_batches_before_resync,
                    store_first_slot_before_resync,
                    store_last_slot_before_resync,
                    %deposit_hash,
                    deposit_number,
                    "Resynced from portal and zone state"
                );
                self.prev_zone_block_hash = portal_hash;
                self.last_submitted_zone_block = last_submitted_zone_block;
                self.latest_observed_zone_block = last_submitted_zone_block;
                self.prev_processed_deposit_hash = deposit_hash;
                self.prev_processed_deposit_number = deposit_number;
                self.metrics
                    .latest_zone_block_submitted_to_l1
                    .set(last_submitted_zone_block as f64);
                self.update_submission_lag();
                if let Some(store) = &self.config.attestation_store {
                    store.remove_submitted(last_submitted_zone_block);
                }
                if let Err(e) = self.restore_pending_withdrawals_from_chain().await {
                    let (stale_store_batches, stale_store_first_slot, stale_store_last_slot) = {
                        let mut store = self.withdrawal_store.lock();
                        let summary = store.summary();
                        store.replace_batches(Default::default());
                        summary
                    };
                    error!(
                        error = %e,
                        stale_store_batches,
                        stale_store_first_slot,
                        stale_store_last_slot,
                        "Failed to restore pending withdrawals during portal resync; cleared local withdrawal store"
                    );
                }
            }
            Err(e) => {
                error!(
                    error = %e,
                    "Failed to read portal state during resync"
                );
            }
        }
    }

    fn resolve_zone_block_number(provider: &P, zone_block_hash: B256) -> Result<u64> {
        if zone_block_hash.is_zero() {
            return Ok(0);
        }

        match provider.block_number(zone_block_hash) {
            Ok(Some(number)) => Ok(number),
            Ok(None) => Err(eyre::eyre!(
                "portal block hash {zone_block_hash} is not canonical in the Zone node"
            )),
            Err(error) => Err(error.into()),
        }
    }

    fn snapshot_at_or_genesis(
        provider: &P,
        inbox_address: Address,
        zone_block_number: u64,
    ) -> Result<ZoneBlockSnapshot> {
        if zone_block_number == 0 {
            return Ok(ZoneBlockSnapshot {
                tempo_block_number: 0,
                processed_deposit_hash: B256::ZERO,
                processed_deposit_number: 0,
                block_hash: B256::ZERO,
            });
        }
        read_zone_block_snapshot(provider, inbox_address, zone_block_number)
    }

    fn record_observed_zone_block(&mut self, latest_zone_block: u64) {
        self.latest_observed_zone_block = latest_zone_block;
        self.metrics
            .latest_zone_block_observed
            .set(latest_zone_block as f64);
        self.update_submission_lag();
    }

    fn update_submission_lag(&self) {
        self.metrics.zone_to_l1_submission_lag_blocks.set(
            self.latest_observed_zone_block
                .saturating_sub(self.last_submitted_zone_block) as f64,
        );
    }
}

/// Spawn the zone monitor as a background task.
///
/// The monitor consumes canonical Zone state notifications and submits finalized batch
/// boundaries to the ZonePortal on Tempo L1. Local state only advances on successful submission.
///
/// The `l1_provider` must already include the sequencer wallet for signing L1 transactions.
pub fn spawn_zone_monitor<P: ZoneSequencerProvider>(
    config: ZoneMonitorConfig,
    zone_provider: P,
    l1_provider: DynProvider<TempoNetwork>,
    signer: PrivateKeySigner,
    withdrawal_store: SharedWithdrawalStore,
    withdrawal_notify: Arc<Notify>,
    repair_notify: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut monitor = loop {
            match ZoneMonitor::new(
                config.clone(),
                zone_provider.clone(),
                l1_provider.clone(),
                signer.clone(),
                withdrawal_store.clone(),
                withdrawal_notify.clone(),
                repair_notify.clone(),
            )
            .await
            {
                Ok(monitor) => break monitor,
                Err(e) => {
                    error!(error = %e, "Zone monitor failed to start, retrying in 5s");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        };

        loop {
            if let Err(e) = monitor.run().await {
                error!(error = %e, "Zone monitor failed, restarting in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    })
}

/// Try to decode a ZonePortal revert reason from an eyre error chain.
///
/// Extracts hex-encoded revert data from the error's display string and decodes
/// it using alloy's `ContractError`, which handles standard `Revert(string)`,
/// `Panic(uint256)`, and ZonePortal custom errors (`NotSequencer`, etc.).
fn decode_portal_revert(err: &eyre::Report) -> Option<String> {
    let msg = format!("{err}");
    let start = msg.find("data: \"0x")? + "data: \"".len();
    let end = msg[start..].find('"')? + start;
    let bytes = alloy_primitives::hex::decode(&msg[start..end]).ok()?;
    let error = ContractError::<ZonePortal::ZonePortalErrors>::abi_decode(&bytes).ok()?;
    Some(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Signed, TxLegacy};
    use alloy_primitives::{Bytes, Log, Signature, U256};
    use alloy_provider::Provider as _;
    use alloy_sol_types::{SolEvent, SolValue};
    use alloy_transport::mock::Asserter;
    use reth_provider::test_utils::MockEthProvider;
    use tempo_primitives::{
        Block, TempoHeader, TempoPrimitives, TempoReceipt, TempoTxEnvelope, TempoTxType,
    };

    fn mock_provider(asserter: Asserter) -> DynProvider<TempoNetwork> {
        alloy_provider::ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter)
            .erased()
    }

    type TestZoneProvider = MockEthProvider<TempoPrimitives>;

    fn mock_zone_provider(
        hash: B256,
        number: u64,
        processed_deposit_hash: B256,
    ) -> TestZoneProvider {
        let provider = TestZoneProvider::new();
        let event = abi::IZoneInbox::TempoAdvanced {
            tempoBlockHash: B256::repeat_byte(0x55),
            tempoBlockNumber: 123,
            depositsProcessed: U256::ZERO,
            newProcessedDepositQueueHash: processed_deposit_hash,
            lastProcessedDepositNumber: 0,
        };
        let tx = TempoTxEnvelope::Legacy(Signed::new_unhashed(
            TxLegacy::default(),
            Signature::test_signature(),
        ));
        let mut header = TempoHeader::default();
        header.inner.number = number;
        provider.add_block(
            hash,
            Block {
                header,
                body: alloy_consensus::BlockBody {
                    transactions: vec![tx],
                    ..Default::default()
                },
            },
        );
        provider.add_receipts(
            number,
            vec![TempoReceipt {
                tx_type: TempoTxType::Legacy,
                success: true,
                cumulative_gas_used: 0,
                logs: vec![Log {
                    address: Address::repeat_byte(0x33),
                    data: event.encode_log_data(),
                }],
            }],
        );
        provider
    }

    fn abi_encode_b256(value: B256) -> Bytes {
        Bytes::copy_from_slice(value.as_slice())
    }

    fn abi_encode_u64(value: u64) -> Bytes {
        Bytes::copy_from_slice(&U256::from(value).to_be_bytes::<32>())
    }

    fn abi_encode_multicall(values: Vec<Bytes>) -> Bytes {
        (U256::ZERO, values).abi_encode_params().into()
    }

    fn test_monitor(
        l1: Asserter,
        zone_provider: TestZoneProvider,
    ) -> ZoneMonitor<TestZoneProvider> {
        let portal_address = Address::repeat_byte(0x11);
        let config = ZoneMonitorConfig {
            outbox_address: Address::repeat_byte(0x22),
            inbox_address: Address::repeat_byte(0x33),
            portal_address,
            batch_anchor_config: BatchAnchorConfig::default(),
            attestation_store: None,
        };
        let l1_provider = mock_provider(l1);

        ZoneMonitor {
            config,
            metrics: crate::metrics::ZoneMonitorMetrics::default(),
            provider: zone_provider,
            withdrawal_store: SharedWithdrawalStore::new(),
            batch_submitter: BatchSubmitter::new(portal_address, l1_provider),
            withdrawal_notify: Arc::new(Notify::new()),
            repair_notify: Arc::new(Notify::new()),
            last_submitted_zone_block: 10,
            prev_processed_deposit_hash: B256::repeat_byte(0xaa),
            prev_processed_deposit_number: 0,
            prev_zone_block_hash: B256::repeat_byte(0xbb),
            latest_observed_zone_block: 50,
        }
    }

    #[tokio::test]
    async fn new_returns_error_when_startup_l1_read_fails() {
        let l1 = Asserter::new();
        let portal_address = Address::repeat_byte(0x11);
        let config = ZoneMonitorConfig {
            outbox_address: Address::repeat_byte(0x22),
            inbox_address: Address::repeat_byte(0x33),
            portal_address,
            batch_anchor_config: BatchAnchorConfig::default(),
            attestation_store: None,
        };

        l1.push_failure_msg("boom");

        let err = match ZoneMonitor::new_with_provider(
            config,
            TestZoneProvider::new(),
            mock_provider(l1.clone()),
            None,
            SharedWithdrawalStore::new(),
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
        )
        .await
        {
            Ok(_) => panic!("zone monitor startup should fail"),
            Err(err) => err,
        };

        assert!(
            err.to_string()
                .contains("failed to read portal block hash during zone monitor startup")
        );
        assert!(l1.read_q().is_empty());
    }

    #[tokio::test]
    async fn resync_uses_portal_confirmed_zone_block_for_processed_deposit_hash() {
        let l1 = Asserter::new();
        let portal_hash = B256::from(U256::from(7).to_be_bytes::<32>());
        let confirmed_zone_block = 42;
        let confirmed_deposit_hash = B256::repeat_byte(0x33);
        let zone = mock_zone_provider(portal_hash, confirmed_zone_block, confirmed_deposit_hash);

        l1.push_success(&abi_encode_b256(portal_hash));
        l1.push_success(&abi_encode_multicall(vec![
            abi_encode_u64(7),
            abi_encode_u64(7),
        ]));

        let mut monitor = test_monitor(l1.clone(), zone);

        monitor.resync_from_portal().await;

        assert_eq!(monitor.prev_zone_block_hash, portal_hash);
        assert_eq!(monitor.last_submitted_zone_block, confirmed_zone_block);
        assert_eq!(monitor.prev_processed_deposit_hash, confirmed_deposit_hash);
    }

    #[tokio::test]
    async fn repair_missing_withdrawal_slot_resyncs_portal_and_rebuilds_withdrawal_store() {
        let l1 = Asserter::new();
        let portal_hash = B256::from(U256::from(7).to_be_bytes::<32>());
        let confirmed_zone_block = 42;
        let confirmed_deposit_hash = B256::repeat_byte(0x33);
        let zone = mock_zone_provider(portal_hash, confirmed_zone_block, confirmed_deposit_hash);

        l1.push_success(&abi_encode_b256(portal_hash));
        l1.push_success(&abi_encode_multicall(vec![
            abi_encode_u64(7),
            abi_encode_u64(7),
        ]));

        let mut monitor = test_monitor(l1.clone(), zone);
        monitor.withdrawal_store.lock().add_withdrawal(
            3,
            abi::Withdrawal {
                token: Address::repeat_byte(0x10),
                senderTag: B256::repeat_byte(0x11),
                to: Address::repeat_byte(0x12),
                amount: 100,
                memo: B256::ZERO,
                gasLimit: 0,
                fallbackNonce: 1,
                callbackData: Default::default(),
                encryptedSender: Default::default(),
            },
        );

        monitor.repair_missing_withdrawal_slot().await;

        let store = monitor.withdrawal_store.lock();
        assert_eq!(store.batch_count(), 0);
        assert_eq!(monitor.prev_zone_block_hash, portal_hash);
        assert_eq!(monitor.last_submitted_zone_block, confirmed_zone_block);
        assert_eq!(monitor.prev_processed_deposit_hash, confirmed_deposit_hash);
    }

    #[tokio::test]
    async fn resync_clears_stale_withdrawal_store_when_restore_fails() {
        let l1 = Asserter::new();
        let portal_hash = B256::from(U256::from(7).to_be_bytes::<32>());
        let confirmed_zone_block = 42;
        let confirmed_deposit_hash = B256::repeat_byte(0x33);
        let zone = mock_zone_provider(portal_hash, confirmed_zone_block, confirmed_deposit_hash);

        l1.push_success(&abi_encode_b256(portal_hash));
        l1.push_failure_msg("head read failed");
        l1.push_failure_msg("tail read failed");

        let mut monitor = test_monitor(l1.clone(), zone);
        monitor.withdrawal_store.lock().add_withdrawal(
            3,
            abi::Withdrawal {
                token: Address::repeat_byte(0x10),
                senderTag: B256::repeat_byte(0x11),
                to: Address::repeat_byte(0x12),
                amount: 100,
                memo: B256::ZERO,
                gasLimit: 0,
                fallbackNonce: 1,
                callbackData: Default::default(),
                encryptedSender: Default::default(),
            },
        );

        monitor.resync_from_portal().await;

        let store = monitor.withdrawal_store.lock();
        assert_eq!(store.batch_count(), 0);
        assert_eq!(monitor.prev_zone_block_hash, portal_hash);
        assert_eq!(monitor.last_submitted_zone_block, confirmed_zone_block);
        assert_eq!(monitor.prev_processed_deposit_hash, confirmed_deposit_hash);
    }

    #[tokio::test]
    async fn preflight_hash_mismatch_resyncs_to_portal_confirmed_anchor() {
        let l1 = Asserter::new();
        let portal_hash = B256::from(U256::from(7).to_be_bytes::<32>());
        let confirmed_zone_block = 42;
        let confirmed_deposit_hash = B256::repeat_byte(0x33);
        let zone = mock_zone_provider(portal_hash, confirmed_zone_block, confirmed_deposit_hash);

        l1.push_success(&abi_encode_b256(portal_hash));
        l1.push_success(&abi_encode_b256(portal_hash));
        l1.push_success(&abi_encode_multicall(vec![
            abi_encode_u64(7),
            abi_encode_u64(7),
        ]));

        let mut monitor = test_monitor(l1.clone(), zone);
        let batch_data = BatchData {
            zone_height: 20,
            tempo_block_number: 123,
            prev_block_hash: B256::repeat_byte(0x99),
            next_block_hash: B256::repeat_byte(0x55),
            prev_processed_deposit_hash: B256::repeat_byte(0x77),
            next_processed_deposit_hash: B256::repeat_byte(0x66),
            prev_deposit_number: 0,
            next_deposit_number: 0,
            withdrawal_queue_hash: B256::ZERO,
            withdrawal_batch_index: 8,
        };

        monitor
            .submit_batch_with_retry(&batch_data, 20, Vec::new())
            .await
            .unwrap();

        assert_eq!(monitor.prev_zone_block_hash, portal_hash);
        assert_eq!(monitor.last_submitted_zone_block, confirmed_zone_block);
        assert_eq!(monitor.prev_processed_deposit_hash, confirmed_deposit_hash);
        assert_ne!(monitor.prev_zone_block_hash, batch_data.next_block_hash);
        assert_ne!(
            monitor.prev_processed_deposit_hash,
            batch_data.next_processed_deposit_hash
        );
        assert!(l1.read_q().is_empty());
    }
}
