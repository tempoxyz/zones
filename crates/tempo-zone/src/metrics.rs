//! Internal metrics definitions for zone observability.

use reth_metrics::{
    Metrics,
    metrics::{Counter, Gauge, Histogram},
};

/// Metrics emitted by the withdrawal processor.
#[derive(Metrics, Clone)]
#[metrics(scope = "tempo_zone_withdrawal_processor")]
pub(crate) struct WithdrawalProcessorMetrics {
    /// Current portal withdrawal queue head slot.
    pub(crate) portal_queue_head: Gauge,

    /// Current portal withdrawal queue tail slot.
    pub(crate) portal_queue_tail: Gauge,

    /// Number of pending portal withdrawal queue slots.
    pub(crate) portal_queue_pending_slots: Gauge,

    /// Number of withdrawal batches currently stored in memory.
    pub(crate) store_batch_count: Gauge,

    /// Number of `processWithdrawal` attempts started.
    pub(crate) withdrawals_processed_total: Counter,

    /// Number of withdrawals confirmed on L1.
    pub(crate) withdrawals_confirmed_total: Counter,

    /// Number of withdrawals that failed to send, confirm, or reverted after inclusion.
    pub(crate) withdrawals_failed_total: Counter,

    /// Number of `processWithdrawal` transactions that were included on L1 but reverted.
    pub(crate) withdrawals_reverted_total: Counter,

    /// Time spent processing a withdrawal queue slot.
    pub(crate) slot_processing_duration_seconds: Histogram,
}

/// Metrics emitted by the zone monitor and batch submitter.
#[derive(Metrics, Clone)]
#[metrics(scope = "tempo_zone_monitor")]
pub(crate) struct ZoneMonitorMetrics {
    /// Most recent zone block observed on L2.
    pub latest_zone_block_observed: Gauge,

    /// Most recent zone block successfully submitted to L1.
    pub latest_zone_block_submitted_to_l1: Gauge,

    /// Gap between the latest observed zone block and the latest submitted zone block.
    pub zone_to_l1_submission_lag_blocks: Gauge,

    /// Number of zone blocks included in a batch submission.
    pub batch_size_blocks: Histogram,

    /// Number of withdrawals included in a batch submission.
    pub withdrawals_per_batch: Histogram,

    /// End-to-end latency of a batch submission attempt in seconds.
    pub batch_submit_latency_seconds: Histogram,

    /// Successful batch submissions.
    pub batch_submit_success_total: Counter,

    /// Failed batch submissions after exhausting retries.
    pub batch_submit_failure_total: Counter,

    /// Retry attempts for batch submissions.
    pub batch_submit_retry_total: Counter,

    /// Number of times local monitor state was resynced from the portal.
    pub resync_from_portal_total: Counter,

    /// Failed attempts to rebuild the in-memory withdrawal store from chain state.
    pub withdrawal_store_restore_failure_total: Counter,
}
