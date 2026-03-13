use reth_metrics::{
    Metrics,
    metrics::{Counter, Gauge, Histogram},
};

#[derive(Metrics, Clone)]
#[metrics(scope = "zone_withdrawal_processor")]
pub(crate) struct WithdrawalProcessorMetrics {
    /// Current portal withdrawal queue head slot.
    pub(crate) portal_queue_head: Gauge,
    /// Current portal withdrawal queue tail slot.
    pub(crate) portal_queue_tail: Gauge,
    /// Number of pending portal withdrawal queue slots.
    pub(crate) portal_queue_pending_slots: Gauge,
    /// Number of withdrawal batches currently stored in memory.
    pub(crate) store_batch_count: Gauge,
    /// How long the current head slot has remained pending.
    pub(crate) head_slot_stuck_age_seconds: Gauge,
    /// Number of `processWithdrawal` attempts started.
    pub(crate) withdrawals_processed_total: Counter,
    /// Number of withdrawals confirmed on L1.
    pub(crate) withdrawals_confirmed_total: Counter,
    /// Number of withdrawals that failed to send or confirm.
    pub(crate) withdrawals_failed_total: Counter,
    /// Time spent processing a withdrawal queue slot.
    pub(crate) slot_processing_duration_seconds: Histogram,
}
