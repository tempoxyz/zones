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
use tokio_util::sync;
use tracing::{debug, error, info, instrument, warn};

use alloy_sol_types::{ContractError, SolInterface as _};

use crate::{
    AttestationStore, ZoneSequencerProvider,
    abi::{self, NO_QUEUE_INDEX, ZonePortal},
    prover::ShadowProver,
    resolve_portal_zone_anchor,
    settlement::{
        BatchAnchorConfig, BatchData, BatchSubmitError, BatchSubmitter, FinalizedBatchLog,
        WithdrawalPage, ZoneBlockSnapshot, fetch_finalized_batch, fetch_finalized_batch_boundaries,
        read_zone_block_snapshot,
    },
    withdrawals::SharedWithdrawalStore,
};

/// Maximum number of times to retry a failed batch submission before resyncing.
const MAX_RETRIES: u32 = 3;

/// Initial delay between retries (doubles on each attempt).
const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(200);

/// Backoff before rebuilding the monitor after a start or run failure.
const RESTART_BACKOFF: Duration = Duration::from_secs(5);

/// Configuration for the [`ZoneMonitor`].
#[derive(Debug, Clone)]
pub struct ZoneMonitorConfig {
    /// ZoneOutbox contract address on Zone L2.
    pub outbox_address: Address,
    /// ZoneInbox contract address on Zone L2.
    pub inbox_address: Address,
    /// Fallback interval for reconciling the canonical head when no notification arrives.
    pub poll_interval: Duration,
    /// ZonePortal contract address on Tempo L1.
    pub portal_address: Address,
    /// EIP-2935 history and safety-margin limits used by the batch submitter.
    pub batch_anchor_config: BatchAnchorConfig,
    /// Shared P2P attestations, required after a settlement signer set is activated.
    pub attestation_store: Option<AttestationStore>,
}

/// Withdrawal state shared between the zone monitor and withdrawal processor.
#[derive(Clone)]
pub struct ZoneMonitorSharedState {
    withdrawal_store: SharedWithdrawalStore,
    withdrawal_notify: Arc<Notify>,
    repair_notify: Arc<Notify>,
}

impl ZoneMonitorSharedState {
    /// Create the shared withdrawal state used by the zone monitor.
    pub fn new(
        withdrawal_store: SharedWithdrawalStore,
        withdrawal_notify: Arc<Notify>,
        repair_notify: Arc<Notify>,
    ) -> Self {
        Self {
            withdrawal_store,
            withdrawal_notify,
            repair_notify,
        }
    }
}

/// Monitors the Zone L2 chain for new finalized batch boundaries and submits
/// them to the ZonePortal on L1.
///
/// Local state only advances after a successful L1 submission. On repeated
/// failures the monitor resyncs from the portal's on-chain `blockHash()`.
///
/// Canonical consistency is strict: a non-zero portal anchor must resolve to a canonical local
/// block, and a reorg terminates the current monitor instance so it can be rebuilt from the portal
/// anchor. This deliberately fails closed instead of silently replaying from genesis.
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
    /// is missing or stale and its bounded recovery page must be refilled.
    repair_notify: Arc<Notify>,
    /// Last **Zone L2** block number that was successfully submitted to L1.
    last_submitted_zone_block: u64,
    /// Deposit queue hash from the previous block, used to construct the
    /// [`DepositQueueTransition`](crate::abi::DepositQueueTransition) for each batch.
    prev_processed_deposit_hash: B256,
    /// Deposit counter from the previous batch, used to construct the
    /// [`DepositQueueTransition`](crate::abi::DepositQueueTransition) for each batch.
    prev_processed_deposit_number: u64,
    /// Enabled-token prefix confirmed by the previous batch.
    prev_processed_token_count: u64,
    /// Previous zone block hash, used as `prev_block_hash` in [`BatchData`].
    /// Initialized from the portal's on-chain `blockHash()` at startup.
    prev_zone_block_hash: B256,
    /// Most recent canonical zone block observed from the node.
    latest_observed_zone_block: u64,
    /// Detached, observational SPF worker.
    shadow_prover: Option<ShadowProver>,
}

struct PortalResyncSnapshot {
    portal_anchor: crate::settlement::PortalZoneAnchor,
    previous_snapshot: ZoneBlockSnapshot,
    pending_withdrawals: WithdrawalPage,
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
            None,
        )
        .await
    }

    #[expect(clippy::too_many_arguments)]
    async fn new_with_provider(
        config: ZoneMonitorConfig,
        provider: P,
        l1_provider: DynProvider<TempoNetwork>,
        signer: Option<PrivateKeySigner>,
        withdrawal_store: SharedWithdrawalStore,
        withdrawal_notify: Arc<Notify>,
        repair_notify: Arc<Notify>,
        shadow_prover: Option<ShadowProver>,
    ) -> Result<Self> {
        let metrics = crate::metrics::ZoneMonitorMetrics::default();
        let mut batch_submitter = BatchSubmitter::with_optional_signer_and_anchor_config(
            config.portal_address,
            l1_provider,
            signer,
            config.batch_anchor_config,
        );
        batch_submitter.set_attestation_store(config.attestation_store.clone());

        let portal_anchor = resolve_portal_zone_anchor(
            &provider,
            config.portal_address,
            batch_submitter.l1_provider(),
        )
        .await
        .wrap_err("failed to resolve portal-confirmed zone block during zone monitor startup")?;
        let prev_zone_block_hash = portal_anchor.block_hash;
        let last_submitted_zone_block = portal_anchor.block_number;
        let previous_snapshot = Self::snapshot_at_or_genesis(
            &provider,
            config.inbox_address,
            last_submitted_zone_block,
        )?;
        let prev_processed_deposit_hash = previous_snapshot.processed_deposit_hash;
        let prev_processed_deposit_number = previous_snapshot.processed_deposit_number;
        let prev_processed_token_count = previous_snapshot.processed_token_count;

        info!(
            last_submitted_zone_block,
            %prev_zone_block_hash,
            %prev_processed_deposit_hash,
            prev_processed_deposit_number,
            prev_processed_token_count,
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
            prev_processed_token_count,
            prev_zone_block_hash,
            latest_observed_zone_block: last_submitted_zone_block,
            shadow_prover,
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
    /// Returns `Ok(())` only when `shutdown` fires; the token is observed at the poll
    /// boundary so an in-flight batch submission resolves before teardown.
    pub async fn run(&mut self, shutdown: &sync::CancellationToken) -> Result<()> {
        info!("Native zone monitor started");

        // Subscribe before reading the head so a block imported during startup cannot be missed.
        let mut canonical = self.provider.canonical_state_stream();
        let mut fallback = tokio::time::interval(self.config.poll_interval);

        loop {
            self.process_available_blocks(shutdown).await;

            tokio::select! {
                biased;
                () = shutdown.cancelled() => {
                    info!("Zone monitor observed shutdown");
                    return Ok(());
                }
                _ = self.repair_notify.notified() => {
                    self.refill_withdrawal_cache().await;
                }
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
            }
        }
    }

    async fn process_available_blocks(&mut self, shutdown: &sync::CancellationToken) {
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

        match self
            .process_block_range(scan_from, latest_zone_block, shutdown)
            .await
        {
            Ok(_) => self.record_observed_zone_block(latest_zone_block),
            Err(BatchSubmitError::Cancelled) => {}
            Err(BatchSubmitError::PortalAdvanced) => {
                unreachable!("portal advancement is reconciled by submit_batch_with_retry")
            }
            Err(BatchSubmitError::Other(error)) => {
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
    /// The L1 portal only stores queue hashes, so the monitor reconstructs one head page from
    /// L1 + zone-L2 events. Existing valid tail payloads are retained within the cache bound.
    async fn restore_pending_withdrawals_from_chain(&self) -> Result<()> {
        let pending = self.fetch_pending_withdrawals_from_chain().await?;
        self.replace_pending_withdrawals(pending);
        Ok(())
    }

    async fn fetch_pending_withdrawals_from_chain(&self) -> Result<WithdrawalPage> {
        match self
            .batch_submitter
            .fetch_pending_withdrawals(&self.provider, self.config.outbox_address)
            .await
        {
            Ok(pending) => Ok(pending),
            Err(err) => {
                self.metrics
                    .withdrawal_store_restore_failure_total
                    .increment(1);
                Err(err)
            }
        }
    }

    fn replace_pending_withdrawals(&self, pending: WithdrawalPage) {
        let restored_withdrawals = pending.batches.values().map(Vec::len).sum::<usize>();
        let page_head = pending.head;
        let tail = pending.tail;

        let mut store = self.withdrawal_store.lock();
        let previous_slots = store.batch_count();
        let evicted_tail_slots = store.replace_page(pending);
        let reconciled_slots = store.batch_count();
        drop(store);

        if reconciled_slots > 0 {
            info!(
                page_head,
                tail,
                previous_slots,
                reconciled_slots,
                restored_withdrawals,
                evicted_tail_slots,
                "Restored pending withdrawals from chain"
            );
            self.withdrawal_notify.notify_one();
        } else if previous_slots > 0 {
            info!(
                page_head,
                tail,
                previous_slots,
                "Cleared stale withdrawal cache after observing an empty portal queue"
            );
        }
    }

    /// Refill the bounded withdrawal cache after the processor reports a missing head slot.
    async fn refill_withdrawal_cache(&self) {
        self.metrics.withdrawal_store_refill_total.increment(1);
        if let Err(error) = self.restore_pending_withdrawals_from_chain().await {
            error!(%error, "Failed to refill the portal withdrawal head page");
        }
    }

    /// Process finalized batch boundaries in `[from, to]`.
    ///
    /// The builder records each batch boundary with one `BatchFinalized` event.
    /// The monitor must walk those boundaries one at a time so the L2 outbox
    /// index and L1 portal index advance in lockstep.
    #[instrument(skip(self), fields(from, to))]
    async fn process_block_range(
        &mut self,
        from: u64,
        to: u64,
        shutdown: &sync::CancellationToken,
    ) -> std::result::Result<bool, BatchSubmitError> {
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
            self.process_finalized_batch(range_start, boundary, shutdown)
                .await?;
            if self.last_submitted_zone_block < boundary_block {
                return Err(eyre::eyre!(
                    "zone batch boundary {boundary_block} remains unsubmitted after reconciliation \
                     (previous anchor {before_submit}, current anchor {})",
                    self.last_submitted_zone_block
                )
                .into());
            }
        }

        Ok(true)
    }

    /// Process one boundary-aligned finalized batch.
    async fn process_finalized_batch(
        &mut self,
        from: u64,
        boundary: FinalizedBatchLog,
        shutdown: &sync::CancellationToken,
    ) -> std::result::Result<(), BatchSubmitError> {
        let to = boundary.block_number;
        let finalized_batch =
            fetch_finalized_batch(&self.provider, self.config.outbox_address, &boundary).await?;
        let end_state = read_zone_block_snapshot(&self.provider, self.config.inbox_address, to)?;

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
            prev_processed_token_count: self.prev_processed_token_count,
            next_processed_token_count: end_state.processed_token_count,
            withdrawal_queue_hash: finalized_batch.finalized_hash,
            withdrawal_batch_index: finalized_batch.finalized_index,
        };

        if let Some(prover) = &self.shadow_prover {
            prover.try_enqueue(from, to, batch_data.clone());
        }
        self.submit_batch_with_retry(&batch_data, to, finalized_batch.withdrawals, shutdown)
            .await
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
        shutdown: &sync::CancellationToken,
    ) -> std::result::Result<(), BatchSubmitError> {
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
                let portal_anchor = self.resync_from_portal().await?;
                if portal_anchor < last_zone_block {
                    return Err(eyre::eyre!(
                        "portal resynced to zone block {portal_anchor}, before pending batch \
                         boundary {last_zone_block}"
                    )
                    .into());
                }
                return Ok(());
            }

            let submit_started = std::time::Instant::now();
            match self
                .batch_submitter
                .submit_batch(batch_data, shutdown)
                .await
            {
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
                    self.prev_processed_token_count = batch_data.next_processed_token_count;
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
                            if !store.add_batch(portal_index, withdrawals) {
                                debug!(
                                    portal_index,
                                    count,
                                    "Withdrawal cache full; dropped reconstructible far-tail payload"
                                );
                            }
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
                Err(BatchSubmitError::Cancelled) => return Err(BatchSubmitError::Cancelled),
                Err(BatchSubmitError::PortalAdvanced) => {
                    self.metrics
                        .batch_submit_latency_seconds
                        .record(submit_started.elapsed().as_secs_f64());
                    warn!(
                        local_prev = %batch_data.prev_block_hash,
                        last_zone_block,
                        "Portal advanced while waiting for settlement quorum; resyncing"
                    );
                    let portal_anchor = self.resync_from_portal().await?;
                    if portal_anchor < last_zone_block {
                        return Err(eyre::eyre!(
                            "portal resynced to zone block {portal_anchor}, before pending batch \
                             boundary {last_zone_block}"
                        )
                        .into());
                    }
                    return Ok(());
                }
                Err(BatchSubmitError::Other(e)) => {
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
        self.resync_from_portal()
            .await
            .wrap_err("failed to resync after exhausting batch submission retries")?;

        Err(eyre::eyre!(
            "batch submission failed after {MAX_RETRIES} retries for zone block {last_zone_block}"
        )
        .into())
    }

    /// Resync the local submission anchor from portal-confirmed on-chain state.
    ///
    /// Called after exhausting retries or when a preflight hash mismatch is
    /// detected, so subsequent batches start from the portal's actual accepted
    /// zone block rather than stale local values.
    ///
    /// Returns the portal-confirmed canonical Zone block number. Callers must verify that this
    /// anchor covers any batch boundary they were attempting to submit.
    async fn resync_from_portal(&mut self) -> Result<u64> {
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
        let snapshot = self.build_portal_resync_snapshot().await?;
        let portal_anchor = snapshot.portal_anchor;
        let portal_hash = portal_anchor.block_hash;
        let last_submitted_zone_block = portal_anchor.block_number;
        let deposit_hash = snapshot.previous_snapshot.processed_deposit_hash;
        let deposit_number = snapshot.previous_snapshot.processed_deposit_number;

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
        // Rewind boundary discovery to the portal-confirmed anchor. The caller may only advance
        // this cursor again after every discovered boundary is confirmed submitted.
        self.latest_observed_zone_block = last_submitted_zone_block;
        self.prev_processed_deposit_hash = deposit_hash;
        self.prev_processed_deposit_number = deposit_number;
        self.prev_processed_token_count = snapshot.previous_snapshot.processed_token_count;
        self.replace_pending_withdrawals(snapshot.pending_withdrawals);
        self.metrics
            .latest_zone_block_submitted_to_l1
            .set(last_submitted_zone_block as f64);
        self.update_submission_lag();
        if let Some(store) = &self.config.attestation_store {
            store.remove_submitted(last_submitted_zone_block);
        }

        Ok(last_submitted_zone_block)
    }

    async fn build_portal_resync_snapshot(&self) -> Result<PortalResyncSnapshot> {
        let portal_anchor = resolve_portal_zone_anchor(
            &self.provider,
            self.config.portal_address,
            self.batch_submitter.l1_provider(),
        )
        .await
        .wrap_err("failed to resolve portal-confirmed zone block during resync")?;
        let previous_snapshot = Self::snapshot_at_or_genesis(
            &self.provider,
            self.config.inbox_address,
            portal_anchor.block_number,
        )
        .wrap_err("failed to read portal-confirmed zone commitments")?;
        let pending_withdrawals = self.fetch_pending_withdrawals_from_chain().await?;

        let confirmed_portal_anchor = resolve_portal_zone_anchor(
            &self.provider,
            self.config.portal_address,
            self.batch_submitter.l1_provider(),
        )
        .await
        .wrap_err("failed to confirm portal-confirmed zone block during resync")?;
        if confirmed_portal_anchor != portal_anchor {
            eyre::bail!(
                "portal anchor changed while building resync snapshot: initial={portal_anchor:?}, \
                 confirmed={confirmed_portal_anchor:?}"
            );
        }

        Ok(PortalResyncSnapshot {
            portal_anchor,
            previous_snapshot,
            pending_withdrawals,
        })
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
                processed_token_count: 0,
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
pub(crate) fn spawn_zone_monitor<P: ZoneSequencerProvider>(
    config: ZoneMonitorConfig,
    zone_provider: P,
    l1_provider: DynProvider<TempoNetwork>,
    signer: PrivateKeySigner,
    shared_state: ZoneMonitorSharedState,
    shadow_prover: Option<ShadowProver>,
    shutdown: sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let ZoneMonitorSharedState {
        withdrawal_store,
        withdrawal_notify,
        repair_notify,
    } = shared_state;
    tokio::spawn(async move {
        loop {
            if shutdown.is_cancelled() {
                info!("Zone monitor stopped before start");
                return;
            }
            let mut monitor = match ZoneMonitor::new_with_provider(
                config.clone(),
                zone_provider.clone(),
                l1_provider.clone(),
                Some(signer.clone()),
                withdrawal_store.clone(),
                withdrawal_notify.clone(),
                repair_notify.clone(),
                shadow_prover.clone(),
            )
            .await
            {
                Ok(monitor) => monitor,
                Err(e) => {
                    error!(error = %e, "Zone monitor failed to start, retrying in 5s");
                    if shutdown
                        .run_until_cancelled(tokio::time::sleep(RESTART_BACKOFF))
                        .await
                        .is_none()
                    {
                        info!("Zone monitor stopped before start");
                        return;
                    }
                    continue;
                }
            };

            match monitor.run(&shutdown).await {
                Ok(()) => {
                    info!("Zone monitor stopped");
                    return;
                }
                Err(e) => {
                    error!(
                        error = %e,
                        "Zone monitor failed; rebuilding from the portal anchor in 5s"
                    );
                    if shutdown
                        .run_until_cancelled(tokio::time::sleep(RESTART_BACKOFF))
                        .await
                        .is_none()
                    {
                        info!("Zone monitor stopped");
                        return;
                    }
                }
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
        let event = abi::TempoAdvanced {
            tempoBlockHash: B256::repeat_byte(0x55),
            tempoBlockNumber: 123,
            depositsProcessed: U256::ZERO,
            newProcessedDepositQueueHash: processed_deposit_hash,
            lastProcessedDepositNumber: 0,
            lastProcessedEnabledTokenCount: 0,
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
            poll_interval: Duration::from_secs(1),
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
            prev_processed_token_count: 0,
            prev_zone_block_hash: B256::repeat_byte(0xbb),
            latest_observed_zone_block: 50,
            shadow_prover: None,
        }
    }

    #[tokio::test]
    async fn leader_demotion_stops_batch_submission_waiting_for_settlement_quorum() {
        let l1 = Asserter::new();
        let mut monitor = test_monitor(l1.clone(), TestZoneProvider::new());
        monitor
            .batch_submitter
            .set_attestation_store(Some(AttestationStore::default()));

        let batch_data = BatchData {
            zone_height: 71,
            tempo_block_number: 123,
            prev_block_hash: B256::repeat_byte(0xbb),
            next_block_hash: B256::repeat_byte(0xcc),
            prev_processed_deposit_hash: B256::repeat_byte(0xaa),
            next_processed_deposit_hash: B256::repeat_byte(0xdd),
            prev_deposit_number: 0,
            next_deposit_number: 0,
            prev_processed_token_count: 0,
            next_processed_token_count: 0,
            withdrawal_queue_hash: B256::ZERO,
            withdrawal_batch_index: 1,
        };

        // Preflight portal hash, followed by submission metadata with a 2-of-N threshold.
        l1.push_success(&abi_encode_b256(batch_data.prev_block_hash));
        l1.push_success(&abi_encode_multicall(vec![
            abi_encode_u64(0),
            abi_encode_u64(1),
            abi_encode_u64(2),
            abi_encode_u64(1),
            Address::ZERO.abi_encode().into(),
            abi_encode_u64(7),
            abi_encode_u64(42431),
        ]));

        let shutdown = sync::CancellationToken::new();
        let submission_shutdown = shutdown.clone();
        let submission = tokio::spawn(async move {
            monitor
                .submit_batch_with_retry(&batch_data, 71, Vec::new(), &submission_shutdown)
                .await
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            while !l1.read_q().is_empty() {
                assert!(
                    !submission.is_finished(),
                    "batch submission stopped before waiting for settlement quorum"
                );
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("batch submission did not reach the settlement quorum wait");
        assert!(!submission.is_finished());

        shutdown.cancel();
        let error = tokio::time::timeout(Duration::from_secs(1), submission)
            .await
            .expect("batch submission did not stop after leader demotion")
            .unwrap()
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("settlement quorum wait cancelled")
        );
        assert!(l1.read_q().is_empty());
    }

    #[tokio::test]
    async fn new_returns_error_when_startup_l1_read_fails() {
        let l1 = Asserter::new();
        let portal_address = Address::repeat_byte(0x11);
        let config = ZoneMonitorConfig {
            outbox_address: Address::repeat_byte(0x22),
            inbox_address: Address::repeat_byte(0x33),
            poll_interval: Duration::from_secs(1),
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
            None,
        )
        .await
        {
            Ok(_) => panic!("zone monitor startup should fail"),
            Err(err) => err,
        };

        assert!(
            err.to_string().contains(
                "failed to resolve portal-confirmed zone block during zone monitor startup"
            )
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
        l1.push_success(&abi_encode_b256(portal_hash));

        let mut monitor = test_monitor(l1.clone(), zone);
        monitor.prev_processed_token_count = 99;

        let anchor = monitor.resync_from_portal().await.unwrap();

        assert_eq!(anchor, confirmed_zone_block);
        assert_eq!(monitor.prev_zone_block_hash, portal_hash);
        assert_eq!(monitor.last_submitted_zone_block, confirmed_zone_block);
        assert_eq!(monitor.prev_processed_deposit_hash, confirmed_deposit_hash);
        assert_eq!(monitor.prev_processed_token_count, 0);
    }

    #[tokio::test]
    async fn refill_withdrawal_cache_does_not_resync_the_portal_anchor() {
        let l1 = Asserter::new();
        let portal_hash = B256::from(U256::from(7).to_be_bytes::<32>());
        let zone = mock_zone_provider(portal_hash, 42, B256::repeat_byte(0x33));

        l1.push_success(&abi_encode_multicall(vec![
            abi_encode_u64(7),
            abi_encode_u64(7),
        ]));

        let monitor = test_monitor(l1.clone(), zone);
        let old_hash = monitor.prev_zone_block_hash;
        let old_last_submitted = monitor.last_submitted_zone_block;
        let old_deposit_hash = monitor.prev_processed_deposit_hash;
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

        monitor.refill_withdrawal_cache().await;

        let store = monitor.withdrawal_store.lock();
        assert_eq!(store.batch_count(), 0);
        assert_eq!(monitor.prev_zone_block_hash, old_hash);
        assert_eq!(monitor.last_submitted_zone_block, old_last_submitted);
        assert_eq!(monitor.prev_processed_deposit_hash, old_deposit_hash);
        assert!(l1.read_q().is_empty());
    }

    #[tokio::test]
    async fn resync_preserves_last_known_good_state_when_restore_fails() {
        let l1 = Asserter::new();
        let portal_hash = B256::from(U256::from(7).to_be_bytes::<32>());
        let confirmed_zone_block = 42;
        let confirmed_deposit_hash = B256::repeat_byte(0x33);
        let zone = mock_zone_provider(portal_hash, confirmed_zone_block, confirmed_deposit_hash);

        l1.push_success(&abi_encode_b256(portal_hash));
        l1.push_failure_msg("head read failed");
        l1.push_failure_msg("tail read failed");

        let mut monitor = test_monitor(l1.clone(), zone);
        let old_hash = monitor.prev_zone_block_hash;
        let old_last_submitted = monitor.last_submitted_zone_block;
        let old_latest_observed = monitor.latest_observed_zone_block;
        let old_deposit_hash = monitor.prev_processed_deposit_hash;
        let old_deposit_number = monitor.prev_processed_deposit_number;
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

        let error = monitor.resync_from_portal().await.unwrap_err();

        assert!(error.to_string().contains("head read failed"));
        let store = monitor.withdrawal_store.lock();
        assert_eq!(store.batch_count(), 1);
        assert_eq!(monitor.prev_zone_block_hash, old_hash);
        assert_eq!(monitor.last_submitted_zone_block, old_last_submitted);
        assert_eq!(monitor.latest_observed_zone_block, old_latest_observed);
        assert_eq!(monitor.prev_processed_deposit_hash, old_deposit_hash);
        assert_eq!(monitor.prev_processed_deposit_number, old_deposit_number);
    }

    #[tokio::test]
    async fn resync_preserves_last_known_good_state_when_portal_anchor_changes() {
        let l1 = Asserter::new();
        let portal_hash = B256::from(U256::from(7).to_be_bytes::<32>());
        let changed_portal_hash = B256::from(U256::from(8).to_be_bytes::<32>());
        let zone = mock_zone_provider(portal_hash, 42, B256::repeat_byte(0x33));
        let mut changed_header = TempoHeader::default();
        changed_header.inner.number = 43;
        zone.add_block(
            changed_portal_hash,
            Block {
                header: changed_header,
                body: Default::default(),
            },
        );

        l1.push_success(&abi_encode_b256(portal_hash));
        l1.push_success(&abi_encode_multicall(vec![
            abi_encode_u64(7),
            abi_encode_u64(7),
        ]));
        l1.push_success(&abi_encode_b256(changed_portal_hash));

        let mut monitor = test_monitor(l1.clone(), zone);
        let old_hash = monitor.prev_zone_block_hash;
        let old_last_submitted = monitor.last_submitted_zone_block;
        let old_latest_observed = monitor.latest_observed_zone_block;
        let old_deposit_hash = monitor.prev_processed_deposit_hash;
        let old_deposit_number = monitor.prev_processed_deposit_number;
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

        let error = monitor.resync_from_portal().await.unwrap_err();

        assert!(
            error
                .to_string()
                .contains("portal anchor changed while building resync snapshot")
        );
        assert_eq!(monitor.withdrawal_store.lock().batch_count(), 1);
        assert_eq!(monitor.prev_zone_block_hash, old_hash);
        assert_eq!(monitor.last_submitted_zone_block, old_last_submitted);
        assert_eq!(monitor.latest_observed_zone_block, old_latest_observed);
        assert_eq!(monitor.prev_processed_deposit_hash, old_deposit_hash);
        assert_eq!(monitor.prev_processed_deposit_number, old_deposit_number);
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
        l1.push_success(&abi_encode_b256(portal_hash));

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
            prev_processed_token_count: 0,
            next_processed_token_count: 0,
            withdrawal_queue_hash: B256::ZERO,
            withdrawal_batch_index: 8,
        };

        monitor
            .submit_batch_with_retry(&batch_data, 20, Vec::new(), &sync::CancellationToken::new())
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

    #[tokio::test]
    async fn preflight_hash_mismatch_rejects_anchor_before_pending_boundary() {
        let l1 = Asserter::new();
        let portal_hash = B256::from(U256::from(7).to_be_bytes::<32>());
        let confirmed_zone_block = 15;
        let pending_boundary = 20;
        let confirmed_deposit_hash = B256::repeat_byte(0x33);
        let zone = mock_zone_provider(portal_hash, confirmed_zone_block, confirmed_deposit_hash);

        l1.push_success(&abi_encode_b256(portal_hash));
        l1.push_success(&abi_encode_b256(portal_hash));
        l1.push_success(&abi_encode_multicall(vec![
            abi_encode_u64(7),
            abi_encode_u64(7),
        ]));
        l1.push_success(&abi_encode_b256(portal_hash));

        let mut monitor = test_monitor(l1.clone(), zone);
        let batch_data = BatchData {
            zone_height: pending_boundary,
            tempo_block_number: 123,
            prev_block_hash: B256::repeat_byte(0x99),
            next_block_hash: B256::repeat_byte(0x55),
            prev_processed_deposit_hash: B256::repeat_byte(0x77),
            next_processed_deposit_hash: B256::repeat_byte(0x66),
            prev_deposit_number: 0,
            next_deposit_number: 0,
            prev_processed_token_count: 0,
            next_processed_token_count: 0,
            withdrawal_queue_hash: B256::ZERO,
            withdrawal_batch_index: 8,
        };

        let error = monitor
            .submit_batch_with_retry(
                &batch_data,
                pending_boundary,
                Vec::new(),
                &sync::CancellationToken::new(),
            )
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("before pending batch boundary 20")
        );
        assert_eq!(monitor.last_submitted_zone_block, confirmed_zone_block);
        assert_eq!(monitor.latest_observed_zone_block, confirmed_zone_block);
        assert!(l1.read_q().is_empty());
    }

    #[tokio::test]
    async fn preflight_hash_mismatch_propagates_failed_resync() {
        let l1 = Asserter::new();
        let portal_hash = B256::repeat_byte(0x77);
        let zone = TestZoneProvider::new();

        l1.push_success(&abi_encode_b256(portal_hash));
        l1.push_failure_msg("resync unavailable");

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
            prev_processed_token_count: 0,
            next_processed_token_count: 0,
            withdrawal_queue_hash: B256::ZERO,
            withdrawal_batch_index: 8,
        };

        let error = monitor
            .submit_batch_with_retry(&batch_data, 20, Vec::new(), &sync::CancellationToken::new())
            .await
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("failed to resolve portal-confirmed zone block during resync")
        );
        assert_eq!(monitor.last_submitted_zone_block, 10);
        assert_eq!(monitor.latest_observed_zone_block, 50);
        assert!(l1.read_q().is_empty());
    }
}
