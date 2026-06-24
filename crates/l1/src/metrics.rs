//! Internal metrics definitions for L1 ingestion.

use reth_metrics::{
    Metrics,
    metrics::{Counter, Gauge, Histogram},
};

/// Metrics emitted by the L1 subscriber / deposit ingestion pipeline.
#[derive(Metrics, Clone)]
#[metrics(scope = "tempo_zone_l1_subscriber")]
pub(crate) struct L1SubscriberMetrics {
    /// Duration of a backfill run in seconds.
    pub backfill_duration_seconds: Histogram,

    /// Most recent L1 block number observed by the subscriber.
    pub latest_l1_block_seen: Gauge,

    /// Current lag between the subscriber and the observed L1 tip, in blocks.
    pub current_l1_lag_blocks: Gauge,

    /// Number of L1 blocks accepted into the deposit queue.
    pub blocks_enqueued: Counter,

    /// Number of regular deposit events observed on L1.
    pub regular_deposit_events: Counter,

    /// Number of encrypted deposit events observed on L1.
    pub encrypted_deposit_events: Counter,

    /// Number of `TokenEnabled` events observed on L1.
    pub token_enabled_events: Counter,

    /// Number of `SequencerTransferStarted` events observed on L1.
    pub sequencer_transfer_started_events: Counter,

    /// Number of `SequencerTransferred` events observed on L1.
    pub sequencer_transferred_events: Counter,

    /// Number of reorgs detected by the subscriber.
    pub reorgs_detected: Counter,

    /// Number of failed L1 block preparation fetches.
    pub fetch_failures: Counter,

    /// Time spent waiting for the next live L1 block from the stream.
    pub stream_try_next_duration_seconds: Histogram,

    /// Number of reconnect attempts after the subscriber exits or errors.
    pub reconnects: Counter,
}
