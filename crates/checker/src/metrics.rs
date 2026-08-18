//! Checker progress and alert metrics exposed by the node metrics endpoint.

use reth_metrics::{
    Metrics,
    metrics::{Counter, Gauge},
};

#[derive(Metrics, Clone)]
#[metrics(scope = "tempo_zone_checker")]
pub(crate) struct CheckerMetrics {
    /// Highest durably verified Zone block.
    pub(crate) verified_zone_height: Gauge,
    /// Tempo block imported by the verified Zone tip.
    pub(crate) imported_tempo_height: Gauge,
    /// Highest Zone block delivered to the checker.
    pub(crate) observed_zone_height: Gauge,
    /// Delivered blocks not yet verified.
    pub(crate) verification_lag_blocks: Gauge,
    /// One when a deterministic finding has stopped verification.
    pub(crate) divergence_active: Gauge,
    /// Number of transient acquisition retries.
    pub(crate) acquisition_retries_total: Counter,
    /// Number of verified Zone blocks.
    pub(crate) verified_zone_blocks_total: Counter,
    /// Number of deep reorg rebuilds.
    pub(crate) recovery_rebuilds_total: Counter,
}
