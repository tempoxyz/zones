//! Withdrawal collection store and L1 withdrawal processor for the zone sequencer.
//!
//! This module provides two main components:
//!
//! - [`WithdrawalStore`] — an in-memory store that holds [`abi::Withdrawal`] structs grouped by
//!   batch index. The L1 portal queue only stores hashes, so the sequencer must retain the actual
//!   withdrawal data to provide it when calling `processWithdrawal`.
//!
//! - [`WithdrawalProcessor`] — a background task that polls the ZonePortal withdrawal queue on
//!   **Tempo L1** and processes withdrawals by calling `processWithdrawal(withdrawal, remainingQueue)`.
//!
//! ## Data flow
//!
//! 1. Withdrawal requests originate on the **Zone L2** (`ZoneOutbox.requestWithdrawal`).
//! 2. The sequencer observes `WithdrawalRequested` events and stores the withdrawal data in the
//!    [`WithdrawalStore`].
//! 3. At batch finalization, the sequencer calls `finalizeWithdrawalBatch` on L2, which builds a
//!    hash chain. The proof then enqueues this hash chain into the portal's withdrawal queue on L1.
//! 4. The [`WithdrawalProcessor`] polls the portal queue on L1 and processes each withdrawal by
//!    providing the original data and the remaining queue hash.
//!
//! ## Batch-to-slot mapping
//!
//! The portal's withdrawal queue slots correspond to batch indices. The store's `batch_index`
//! should match the portal slot index. The caller (batch submitter) is responsible for tracking
//! which `batch_index` maps to which portal slot.

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256};
use alloy_provider::DynProvider;
use parking_lot::Mutex;
use tempo_alloy::TempoNetwork;
use tokio::sync::Notify;
use tracing::{debug, error, info, instrument, warn};

use crate::{
    abi::{self, ZonePortal},
    metrics::WithdrawalProcessorMetrics,
};

/// Shared handle to the withdrawal store.
#[derive(Clone)]
pub struct SharedWithdrawalStore(Arc<Mutex<WithdrawalStore>>);

impl SharedWithdrawalStore {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(WithdrawalStore::new())))
    }

    pub fn lock(&self) -> parking_lot::MutexGuard<'_, WithdrawalStore> {
        self.0.lock()
    }
}

impl Default for SharedWithdrawalStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Configuration for the withdrawal processor.
#[derive(Debug, Clone)]
pub struct WithdrawalProcessorConfig {
    /// ZonePortal contract address on Tempo L1.
    pub portal_address: Address,
    /// Tempo L1 RPC URL (HTTP).
    pub l1_rpc_url: String,
    /// Fallback timeout for checking the withdrawal queue if no notification arrives.
    pub fallback_poll_interval: Duration,
}

/// In-memory store for withdrawal data grouped by batch index.
///
/// The L1 portal queue only stores hash chains. The sequencer must keep the actual
/// [`abi::Withdrawal`] structs so it can provide them when calling `processWithdrawal`.
///
/// Withdrawals are grouped by batch index, where each batch is a `Vec<Withdrawal>` in FIFO order
/// (oldest first). The batch index corresponds to the portal's withdrawal queue slot index.
pub struct WithdrawalStore {
    batches: BTreeMap<u64, Vec<abi::Withdrawal>>,
    metrics: WithdrawalProcessorMetrics,
}

impl WithdrawalStore {
    pub fn new() -> Self {
        Self::with_metrics(WithdrawalProcessorMetrics::default())
    }

    fn with_metrics(metrics: WithdrawalProcessorMetrics) -> Self {
        let store = Self {
            batches: BTreeMap::new(),
            metrics,
        };
        store.record_batch_count();
        store
    }

    /// Add a withdrawal to the given batch.
    ///
    /// Withdrawals within a batch are stored in FIFO order (oldest first).
    pub fn add_withdrawal(&mut self, batch_index: u64, withdrawal: abi::Withdrawal) {
        self.batches
            .entry(batch_index)
            .or_default()
            .push(withdrawal);
        self.record_batch_count();
    }

    /// Set all withdrawals for a batch at once, replacing any existing data.
    pub fn add_batch(&mut self, batch_index: u64, withdrawals: Vec<abi::Withdrawal>) {
        self.batches.insert(batch_index, withdrawals);
        self.record_batch_count();
    }

    /// Get all withdrawals for a batch.
    pub fn get_batch(&self, batch_index: u64) -> Option<&Vec<abi::Withdrawal>> {
        self.batches.get(&batch_index)
    }

    /// Remove a batch after all its withdrawals are processed.
    pub fn remove_batch(&mut self, batch_index: u64) {
        self.batches.remove(&batch_index);
        self.record_batch_count();
    }

    pub fn has_batch(&self, batch_index: u64) -> bool {
        self.batches.contains_key(&batch_index)
    }

    pub fn batch_count(&self) -> usize {
        self.batches.len()
    }

    fn record_batch_count(&self) {
        self.metrics
            .store_batch_count
            .set(self.batch_count() as f64);
    }
}

impl Default for WithdrawalStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute the remaining queue hash after removing the first `processed_count` withdrawals.
///
/// This value is passed as `remainingQueue` to `processWithdrawal` on the portal contract.
///
/// - If `processed_count >= withdrawals.len()`, returns `B256::ZERO` (no remaining items).
/// - Otherwise, computes the hash chain over `withdrawals[processed_count..]` via
///   [`abi::Withdrawal::queue_hash`].
pub fn compute_remaining_queue(withdrawals: &[abi::Withdrawal], processed_count: usize) -> B256 {
    if processed_count >= withdrawals.len() {
        return B256::ZERO;
    }

    let remaining = &withdrawals[processed_count..];

    abi::Withdrawal::queue_hash(remaining)
}

#[derive(Debug, Clone, Copy)]
struct TrackedHeadSlot {
    slot: u64,
    first_seen_at: Instant,
}

#[derive(Debug, Default)]
struct HeadSlotTracker {
    tracked: Option<TrackedHeadSlot>,
}

impl HeadSlotTracker {
    fn observe(&mut self, head: u64, tail: u64, now: Instant) -> f64 {
        if head == tail {
            self.clear();
            return 0.0;
        }

        match self.tracked {
            Some(tracked) if tracked.slot == head => {
                now.duration_since(tracked.first_seen_at).as_secs_f64()
            }
            _ => {
                self.tracked = Some(TrackedHeadSlot {
                    slot: head,
                    first_seen_at: now,
                });
                0.0
            }
        }
    }

    fn clear(&mut self) {
        self.tracked = None;
    }
}

fn record_queue_metrics(
    metrics: &WithdrawalProcessorMetrics,
    tracker: &mut HeadSlotTracker,
    head: u64,
    tail: u64,
    store_batch_count: usize,
    now: Instant,
) {
    metrics.portal_queue_head.set(head as f64);
    metrics.portal_queue_tail.set(tail as f64);
    metrics
        .portal_queue_pending_slots
        .set((tail.saturating_sub(head)) as f64);
    metrics.store_batch_count.set(store_batch_count as f64);
    metrics
        .head_slot_stuck_age_seconds
        .set(tracker.observe(head, tail, now));
}

#[derive(Clone)]
struct SlotProcessingRecorder {
    metrics: WithdrawalProcessorMetrics,
    started_at: Instant,
}

impl SlotProcessingRecorder {
    fn new(metrics: WithdrawalProcessorMetrics, started_at: Instant) -> Self {
        Self {
            metrics,
            started_at,
        }
    }

    fn record_attempt(&self) {
        self.metrics.withdrawals_processed_total.increment(1);
    }

    fn record_confirmed(&self) {
        self.metrics.withdrawals_confirmed_total.increment(1);
    }

    fn record_failed(&self) {
        self.metrics.withdrawals_failed_total.increment(1);
    }

    fn finish(&self, ended_at: Instant) {
        self.record_duration(ended_at.duration_since(self.started_at));
    }

    fn record_duration(&self, duration: Duration) {
        self.metrics
            .slot_processing_duration_seconds
            .record(duration.as_secs_f64());
    }
}

// ---------------------------------------------------------------------------
//  Withdrawal processor
// ---------------------------------------------------------------------------

/// Background task that processes withdrawals from the ZonePortal queue on Tempo L1.
///
/// The processor waits for a [`Notify`] signal from the batch submitter (indicating a batch
/// has landed on L1) and then processes the head slot of the portal's withdrawal queue.
/// A fallback timeout ensures the processor still checks periodically if a notification
/// is missed.
///
/// ## POC limitations
///
/// - Transactions are submitted sequentially and the processor waits for each confirmation.
///   On failure, processing stops and remaining withdrawals are retried on the next cycle.
/// - The portal automatically pays the processing fee to the sequencer; this processor does not
///   handle fee accounting.
pub struct WithdrawalProcessor {
    config: WithdrawalProcessorConfig,
    portal: ZonePortal::ZonePortalInstance<DynProvider<TempoNetwork>, TempoNetwork>,
    store: SharedWithdrawalStore,
    notify: Arc<Notify>,
    metrics: WithdrawalProcessorMetrics,
    head_tracker: Mutex<HeadSlotTracker>,
}

impl WithdrawalProcessor {
    /// Create a new withdrawal processor from a shared L1 provider.
    ///
    /// The provider must already include the sequencer wallet for signing.
    pub fn new(
        config: WithdrawalProcessorConfig,
        provider: DynProvider<TempoNetwork>,
        store: SharedWithdrawalStore,
        notify: Arc<Notify>,
    ) -> Self {
        let portal = ZonePortal::new(config.portal_address, provider);

        Self {
            config,
            portal,
            store,
            notify,
            metrics: WithdrawalProcessorMetrics::default(),
            head_tracker: Mutex::new(HeadSlotTracker::default()),
        }
    }

    /// Run the processor loop. This method never returns under normal operation.
    ///
    /// Waits for a notification from the batch submitter (or a fallback timeout) before
    /// checking the L1 withdrawal queue.
    #[instrument(skip_all, fields(portal = %self.config.portal_address))]
    pub async fn run(&self) -> eyre::Result<()> {
        info!(l1_rpc = %self.config.l1_rpc_url, "Withdrawal processor started");

        loop {
            tokio::select! {
                _ = self.notify.notified() => {
                    debug!("Woken by batch submission notification");
                }
                _ = tokio::time::sleep(self.config.fallback_poll_interval) => {
                    debug!("Fallback poll interval elapsed");
                }
            }

            if let Err(e) = self.process_queue().await {
                error!(error = %e, "Withdrawal processing cycle failed");
            }
        }
    }

    /// Process the current head slot of the portal's withdrawal queue on Tempo L1.
    #[instrument(skip_all)]
    async fn process_queue(&self) -> eyre::Result<()> {
        let head: alloy_primitives::U256 = self.portal.withdrawalQueueHead().call().await?;
        let tail: alloy_primitives::U256 = self.portal.withdrawalQueueTail().call().await?;

        let head_val: u64 = head.try_into().map_err(|_| eyre::eyre!("head overflow"))?;
        let tail_val: u64 = tail.try_into().map_err(|_| eyre::eyre!("tail overflow"))?;
        let store_batch_count = self.store.lock().batch_count();
        {
            let mut tracker = self.head_tracker.lock();
            record_queue_metrics(
                &self.metrics,
                &mut tracker,
                head_val,
                tail_val,
                store_batch_count,
                Instant::now(),
            );
        }

        if head_val == tail_val {
            debug!("Withdrawal queue empty, nothing to process");
            return Ok(());
        }

        let pending_slots = tail_val - head_val;
        info!(
            head = head_val,
            tail = tail_val,
            pending_slots,
            "Withdrawal queue has pending slots"
        );

        let withdrawals = {
            let store = self.store.lock();
            store.get_batch(head_val).cloned()
        };

        let withdrawals = match withdrawals {
            Some(w) if !w.is_empty() => w,
            _ => {
                warn!(
                    slot = head_val,
                    store_batches = self.store.lock().batch_count(),
                    "No withdrawal data in store for current head slot, waiting for data"
                );
                return Ok(());
            }
        };

        info!(
            slot = head_val,
            count = withdrawals.len(),
            "Processing withdrawal batch"
        );
        let slot_metrics = SlotProcessingRecorder::new(self.metrics.clone(), Instant::now());

        for (i, withdrawal) in withdrawals.iter().enumerate() {
            slot_metrics.record_attempt();
            let remaining_queue = compute_remaining_queue(&withdrawals, i + 1);
            let is_last = i + 1 == withdrawals.len();

            info!(
                slot = head_val,
                index = i,
                total = withdrawals.len(),
                token = %withdrawal.token,
                to = %withdrawal.to,
                amount = %withdrawal.amount,
                fee = %withdrawal.fee,
                has_callback = withdrawal.gasLimit > 0,
                is_last,
                "📤 Submitting withdrawal to L1"
            );

            let call = self
                .portal
                .processWithdrawal(withdrawal.clone(), remaining_queue);

            // When the withdrawal has a callback (`gasLimit > 0`), we must
            // override `eth_estimateGas` because the estimate only covers the
            // revert / bounce-back path, which is much cheaper than the happy
            // path where the callback actually executes.
            //
            // The tx gas limit is composed of three parts:
            //
            //   txGas = gasLimit + CALLBACK_OVERHEAD + eip150_cushion
            //
            // 1. `gasLimit`          — gas the user requested for their callback.
            // 2. `CALLBACK_OVERHEAD` — fixed cost for the portal + messenger
            //    logic that runs *around* the callback: queue dequeue & hash
            //    verification, TIP-20 transferFrom (~500k), messenger relay
            //    setup, fee payment, event emission, and the bounce-back path
            //    if the callback reverts.
            // 3. EIP-150 cushion     — the 63/64 forwarding rule means the
            //    caller must hold back 1/64 of remaining gas. To guarantee
            //    the inner CALL receives at least `gasLimit`, the outer frame
            //    needs an extra `ceil(gasLimit / 63)`.
            let call = if withdrawal.gasLimit > 0 {
                const CALLBACK_OVERHEAD: u64 = 2_000_000;
                let eip150_cushion = withdrawal.gasLimit.div_ceil(63);
                call.gas(withdrawal.gasLimit + CALLBACK_OVERHEAD + eip150_cushion)
            } else {
                call
            };

            let tx_result = call.send().await;

            match tx_result {
                Ok(pending) => {
                    let tx_hash = *pending.tx_hash();
                    match pending
                        .with_required_confirmations(1)
                        .with_timeout(Some(std::time::Duration::from_secs(30)))
                        .watch()
                        .await
                    {
                        Ok(_) => {
                            slot_metrics.record_confirmed();
                            info!(
                                slot = head_val,
                                index = i,
                                %tx_hash,
                                token = %withdrawal.token,
                                to = %withdrawal.to,
                                amount = %withdrawal.amount,
                                "✅ Withdrawal confirmed on L1"
                            );
                        }
                        Err(e) => {
                            slot_metrics.record_failed();
                            slot_metrics.finish(Instant::now());
                            error!(
                                slot = head_val,
                                index = i,
                                %tx_hash,
                                to = %withdrawal.to,
                                amount = %withdrawal.amount,
                                error = %e,
                                "processWithdrawal tx not confirmed, stopping batch processing"
                            );
                            return Ok(());
                        }
                    }
                }
                Err(e) => {
                    slot_metrics.record_failed();
                    slot_metrics.finish(Instant::now());
                    error!(
                        slot = head_val,
                        index = i,
                        to = %withdrawal.to,
                        amount = %withdrawal.amount,
                        error = %e,
                        "processWithdrawal tx failed to send, stopping batch processing"
                    );
                    return Ok(());
                }
            }
        }
        slot_metrics.finish(Instant::now());

        // All withdrawals in this slot confirmed — safe to remove.
        self.store.lock().remove_batch(head_val);
        self.metrics.head_slot_stuck_age_seconds.set(0.0);
        self.head_tracker.lock().clear();

        info!(
            slot = head_val,
            count = withdrawals.len(),
            "Batch fully processed and removed from store"
        );
        Ok(())
    }
}

/// Spawn the withdrawal processor as a background task.
///
/// The processor waits for notifications from the batch submitter (via `notify`) and then
/// processes withdrawals from the ZonePortal queue on Tempo L1.
///
/// The `provider` must already include the sequencer wallet for signing L1 transactions.
pub fn spawn_withdrawal_processor(
    config: WithdrawalProcessorConfig,
    provider: DynProvider<TempoNetwork>,
    store: SharedWithdrawalStore,
    notify: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let processor = WithdrawalProcessor::new(config, provider, store, notify);
        loop {
            if let Err(e) = processor.run().await {
                error!(error = %e, "Withdrawal processor failed, restarting in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::EMPTY_SENTINEL;
    use alloy_primitives::{address, keccak256};
    use alloy_sol_types::SolValue;
    use metrics_util::{
        CompositeKey, MetricKind,
        debugging::{DebugValue, DebuggingRecorder},
    };
    use std::{
        collections::HashMap,
        sync::{Mutex as StdMutex, OnceLock},
    };

    type SnapshotMap = HashMap<
        CompositeKey,
        (
            Option<metrics::Unit>,
            Option<metrics::SharedString>,
            DebugValue,
        ),
    >;

    fn metric_lock() -> &'static StdMutex<()> {
        static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| StdMutex::new(()))
    }

    fn with_metrics_snapshot<T>(action: impl FnOnce() -> T) -> (T, SnapshotMap) {
        let _guard = metric_lock().lock().unwrap();
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let result = metrics::with_local_recorder(&recorder, action);
        let snapshot = snapshotter.snapshot().into_hashmap();
        (result, snapshot)
    }

    fn metric_value<'a>(
        snapshot: &'a SnapshotMap,
        kind: MetricKind,
        name: &str,
        labels: &[(&str, &str)],
    ) -> &'a DebugValue {
        snapshot
            .iter()
            .find(|(key, _)| {
                key.kind() == kind
                    && key.key().name() == name
                    && labels_match(key.key().labels(), labels)
            })
            .map(|(_, (_, _, value))| value)
            .unwrap_or_else(|| panic!("metric {name} with labels {labels:?} not found"))
    }

    fn labels_match<'a>(
        labels: impl Iterator<Item = &'a metrics::Label>,
        expected: &[(&str, &str)],
    ) -> bool {
        let mut actual: Vec<_> = labels.map(|label| (label.key(), label.value())).collect();
        let mut expected = expected.to_vec();
        actual.sort_unstable();
        expected.sort_unstable();
        actual == expected
    }

    fn counter(snapshot: &SnapshotMap, name: &str, labels: &[(&str, &str)]) -> u64 {
        match metric_value(snapshot, MetricKind::Counter, name, labels) {
            DebugValue::Counter(value) => *value,
            other => panic!("expected counter for {name}, got {other:?}"),
        }
    }

    fn gauge(snapshot: &SnapshotMap, name: &str, labels: &[(&str, &str)]) -> f64 {
        match metric_value(snapshot, MetricKind::Gauge, name, labels) {
            DebugValue::Gauge(value) => value.into_inner(),
            other => panic!("expected gauge for {name}, got {other:?}"),
        }
    }

    fn histogram(snapshot: &SnapshotMap, name: &str, labels: &[(&str, &str)]) -> Vec<f64> {
        match metric_value(snapshot, MetricKind::Histogram, name, labels) {
            DebugValue::Histogram(values) => {
                values.iter().map(|value| value.into_inner()).collect()
            }
            other => panic!("expected histogram for {name}, got {other:?}"),
        }
    }

    fn test_metrics(label: &'static str) -> WithdrawalProcessorMetrics {
        WithdrawalProcessorMetrics::new_with_labels(&[("test", label)])
    }

    fn test_withdrawal(to: Address, amount: u128) -> abi::Withdrawal {
        abi::Withdrawal {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to,
            amount,
            fee: 0,
            memo: B256::ZERO,
            gasLimit: 0,
            fallbackRecipient: to,
            callbackData: Default::default(),
        }
    }

    #[test]
    fn empty_queue_hash_is_zero() {
        assert_eq!(abi::Withdrawal::queue_hash(&[]), B256::ZERO);
    }

    #[test]
    fn single_withdrawal_queue_hash() {
        let w = test_withdrawal(address!("0x0000000000000000000000000000000000000042"), 1000);
        let hash = abi::Withdrawal::queue_hash(std::slice::from_ref(&w));

        let expected = keccak256((w, EMPTY_SENTINEL).abi_encode());
        assert_eq!(hash, expected);
    }

    #[test]
    fn two_withdrawal_queue_hash() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000042"), 100);
        let w1 = test_withdrawal(address!("0x0000000000000000000000000000000000000043"), 200);

        let hash = abi::Withdrawal::queue_hash(&[w0.clone(), w1.clone()]);

        let inner = keccak256((w1, EMPTY_SENTINEL).abi_encode());
        let expected = keccak256((w0, inner).abi_encode());
        assert_eq!(hash, expected);
    }

    #[test]
    fn remaining_queue_single_item_is_hash() {
        let w = test_withdrawal(address!("0x0000000000000000000000000000000000000042"), 1000);
        let expected = abi::Withdrawal::queue_hash(std::slice::from_ref(&w));
        assert_eq!(compute_remaining_queue(&[w], 0), expected);
    }

    #[test]
    fn remaining_queue_all_consumed() {
        let w = test_withdrawal(address!("0x0000000000000000000000000000000000000042"), 1000);
        assert_eq!(
            compute_remaining_queue(std::slice::from_ref(&w), 1),
            B256::ZERO
        );
        assert_eq!(compute_remaining_queue(&[w], 5), B256::ZERO);
    }

    #[test]
    fn remaining_queue_partial() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000042"), 100);
        let w1 = test_withdrawal(address!("0x0000000000000000000000000000000000000043"), 200);
        let w2 = test_withdrawal(address!("0x0000000000000000000000000000000000000044"), 300);

        let remaining = compute_remaining_queue(&[w0, w1.clone(), w2.clone()], 1);
        let expected = abi::Withdrawal::queue_hash(&[w1, w2]);
        assert_eq!(remaining, expected);
    }

    #[test]
    fn store_operations() {
        let mut store = WithdrawalStore::new();
        let w = test_withdrawal(address!("0x0000000000000000000000000000000000000042"), 100);

        assert_eq!(store.batch_count(), 0);
        assert!(!store.has_batch(0));

        store.add_withdrawal(0, w.clone());
        assert!(store.has_batch(0));
        assert_eq!(store.batch_count(), 1);
        assert_eq!(store.get_batch(0).unwrap().len(), 1);

        store.add_withdrawal(0, w);
        assert_eq!(store.get_batch(0).unwrap().len(), 2);

        store.remove_batch(0);
        assert!(!store.has_batch(0));
        assert_eq!(store.batch_count(), 0);
    }

    #[test]
    fn store_slot_index_must_match_portal_tail() {
        // Demonstrates that withdrawals must be stored under the portal's actual
        // queue tail index. If the monitor starts with tail=0 but the portal is
        // at tail=5, withdrawals end up in slot 0 while the withdrawal processor
        // looks for them in slot 5.
        let mut store = WithdrawalStore::new();
        let w = test_withdrawal(address!("0x0000000000000000000000000000000000000042"), 100);

        // Simulate storing under the wrong slot (tail=0 when portal is at 5).
        store.add_withdrawal(0, w.clone());
        assert!(store.has_batch(0));
        assert!(
            !store.has_batch(5),
            "withdrawal processor would look at slot 5 and find nothing"
        );

        // Correct: store under the portal's actual tail.
        let portal_tail = 5u64;
        store.add_withdrawal(portal_tail, w);
        assert!(store.has_batch(portal_tail));
    }

    #[test]
    fn store_add_batch() {
        let mut store = WithdrawalStore::new();
        let addr = address!("0x0000000000000000000000000000000000000042");
        let batch: Vec<_> = (0..3).map(|i| test_withdrawal(addr, i * 100)).collect();

        store.add_batch(0, batch);
        assert!(store.has_batch(0));
        assert_eq!(store.get_batch(0).unwrap().len(), 3);

        // Calling add_batch again replaces existing data (idempotent).
        let more: Vec<_> = (0..2).map(|i| test_withdrawal(addr, i * 200)).collect();
        store.add_batch(0, more);
        assert_eq!(store.get_batch(0).unwrap().len(), 2);

        store.add_batch(1, vec![test_withdrawal(addr, 999)]);
        assert_eq!(store.batch_count(), 2);
    }

    #[test]
    fn queue_metrics_reset_when_empty() {
        let labels = [("test", "queue_metrics_reset_when_empty")];
        let (_, snapshot) = with_metrics_snapshot(|| {
            let metrics = test_metrics("queue_metrics_reset_when_empty");
            let mut tracker = HeadSlotTracker::default();
            record_queue_metrics(&metrics, &mut tracker, 7, 7, 0, Instant::now());
        });

        assert_eq!(
            gauge(
                &snapshot,
                "zone_withdrawal_processor.portal_queue_head",
                &labels
            ),
            7.0
        );
        assert_eq!(
            gauge(
                &snapshot,
                "zone_withdrawal_processor.portal_queue_tail",
                &labels
            ),
            7.0
        );
        assert_eq!(
            gauge(
                &snapshot,
                "zone_withdrawal_processor.portal_queue_pending_slots",
                &labels,
            ),
            0.0
        );
        assert_eq!(
            gauge(
                &snapshot,
                "zone_withdrawal_processor.head_slot_stuck_age_seconds",
                &labels,
            ),
            0.0
        );
    }

    #[test]
    fn queue_metrics_track_stuck_head_age() {
        let labels = [("test", "queue_metrics_track_stuck_head_age")];
        let now = Instant::now();
        let (_, snapshot) = with_metrics_snapshot(|| {
            let metrics = test_metrics("queue_metrics_track_stuck_head_age");
            let mut tracker = HeadSlotTracker::default();
            record_queue_metrics(&metrics, &mut tracker, 11, 14, 2, now);
            record_queue_metrics(
                &metrics,
                &mut tracker,
                11,
                14,
                2,
                now + Duration::from_secs(8),
            );
        });

        assert_eq!(
            gauge(
                &snapshot,
                "zone_withdrawal_processor.portal_queue_pending_slots",
                &labels,
            ),
            3.0
        );
        assert_eq!(
            gauge(
                &snapshot,
                "zone_withdrawal_processor.store_batch_count",
                &labels,
            ),
            2.0
        );
        assert_eq!(
            gauge(
                &snapshot,
                "zone_withdrawal_processor.head_slot_stuck_age_seconds",
                &labels,
            ),
            8.0
        );
    }

    #[test]
    fn queue_metrics_reset_when_head_advances() {
        let labels = [("test", "queue_metrics_reset_when_head_advances")];
        let now = Instant::now();
        let (_, snapshot) = with_metrics_snapshot(|| {
            let metrics = test_metrics("queue_metrics_reset_when_head_advances");
            let mut tracker = HeadSlotTracker::default();
            record_queue_metrics(&metrics, &mut tracker, 3, 5, 1, now);
            record_queue_metrics(
                &metrics,
                &mut tracker,
                4,
                5,
                1,
                now + Duration::from_secs(12),
            );
        });

        assert_eq!(
            gauge(
                &snapshot,
                "zone_withdrawal_processor.head_slot_stuck_age_seconds",
                &labels,
            ),
            0.0
        );
        assert_eq!(
            gauge(
                &snapshot,
                "zone_withdrawal_processor.portal_queue_head",
                &labels
            ),
            4.0
        );
    }

    #[test]
    fn slot_processing_metrics_record_success() {
        let labels = [("test", "slot_processing_metrics_record_success")];
        let start = Instant::now();
        let (_, snapshot) = with_metrics_snapshot(|| {
            let recorder = SlotProcessingRecorder::new(
                test_metrics("slot_processing_metrics_record_success"),
                start,
            );
            recorder.record_attempt();
            recorder.record_confirmed();
            recorder.record_attempt();
            recorder.record_confirmed();
            recorder.finish(start + Duration::from_secs(3));
        });

        assert_eq!(
            counter(
                &snapshot,
                "zone_withdrawal_processor.withdrawals_processed_total",
                &labels,
            ),
            2
        );
        assert_eq!(
            counter(
                &snapshot,
                "zone_withdrawal_processor.withdrawals_confirmed_total",
                &labels,
            ),
            2
        );
        assert_eq!(
            counter(
                &snapshot,
                "zone_withdrawal_processor.withdrawals_failed_total",
                &labels,
            ),
            0
        );
        assert_eq!(
            histogram(
                &snapshot,
                "zone_withdrawal_processor.slot_processing_duration_seconds",
                &labels,
            ),
            vec![3.0]
        );
    }

    #[test]
    fn slot_processing_metrics_record_failure() {
        let labels = [("test", "slot_processing_metrics_record_failure")];
        let start = Instant::now();
        let (_, snapshot) = with_metrics_snapshot(|| {
            let recorder = SlotProcessingRecorder::new(
                test_metrics("slot_processing_metrics_record_failure"),
                start,
            );
            recorder.record_attempt();
            recorder.record_confirmed();
            recorder.record_attempt();
            recorder.record_failed();
            recorder.finish(start + Duration::from_secs(2));
        });

        assert_eq!(
            counter(
                &snapshot,
                "zone_withdrawal_processor.withdrawals_processed_total",
                &labels,
            ),
            2
        );
        assert_eq!(
            counter(
                &snapshot,
                "zone_withdrawal_processor.withdrawals_confirmed_total",
                &labels,
            ),
            1
        );
        assert_eq!(
            counter(
                &snapshot,
                "zone_withdrawal_processor.withdrawals_failed_total",
                &labels,
            ),
            1
        );
        assert_eq!(
            histogram(
                &snapshot,
                "zone_withdrawal_processor.slot_processing_duration_seconds",
                &labels,
            ),
            vec![2.0]
        );
    }

    #[test]
    fn store_batch_count_metric_tracks_mutations() {
        let labels = [("test", "store_batch_count_metric_tracks_mutations")];
        let (_, snapshot) = with_metrics_snapshot(|| {
            let mut store = WithdrawalStore::with_metrics(test_metrics(
                "store_batch_count_metric_tracks_mutations",
            ));
            let addr = address!("0x0000000000000000000000000000000000000042");
            store.add_batch(0, vec![test_withdrawal(addr, 1)]);
            store.add_batch(1, vec![test_withdrawal(addr, 2)]);
            store.remove_batch(0);
        });

        assert_eq!(
            gauge(
                &snapshot,
                "zone_withdrawal_processor.store_batch_count",
                &labels,
            ),
            1.0
        );
    }
}
