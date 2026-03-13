//! Internal metrics definitions for zone observability.

use reth_metrics::{
    Metrics,
    metrics::{Counter, Gauge, Histogram},
};

/// Metrics emitted by the L1 subscriber / deposit ingestion pipeline.
#[derive(Metrics, Clone)]
#[metrics(scope = "zone_l1_subscriber")]
pub(crate) struct L1SubscriberMetrics {
    /// Whether a backfill is currently running (1) or idle (0).
    pub backfill_in_progress: Gauge,

    /// The first L1 block number of the most recent backfill run.
    pub backfill_start_block: Gauge,

    /// The last L1 block number of the most recent backfill run.
    pub backfill_end_block: Gauge,

    /// Duration of a backfill run in seconds.
    pub backfill_duration_seconds: Histogram,

    /// Most recent L1 block number observed by the subscriber.
    pub latest_l1_block_seen: Gauge,

    /// Current lag between the subscriber and the observed L1 tip, in blocks.
    pub current_l1_lag_blocks: Gauge,

    /// Number of L1 blocks accepted into the deposit queue.
    pub blocks_enqueued_total: Counter,

    /// Number of regular deposit events observed on L1.
    pub regular_deposit_events_total: Counter,

    /// Number of encrypted deposit events observed on L1.
    pub encrypted_deposit_events_total: Counter,

    /// Number of `TokenEnabled` events observed on L1.
    pub token_enabled_events_total: Counter,

    /// Number of reorgs detected by the subscriber.
    pub reorgs_detected_total: Counter,

    /// Number of failed L1 receipt fetches.
    pub receipt_fetch_failures_total: Counter,

    /// Time spent waiting for the next live L1 block from the stream.
    pub stream_try_next_duration_seconds: Histogram,

    /// Number of reconnect attempts after the subscriber exits or errors.
    pub reconnects_total: Counter,
}
