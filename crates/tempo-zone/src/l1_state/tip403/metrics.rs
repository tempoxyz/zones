//! Metrics for the TIP-403 policy cache and resolution system.

use reth_metrics::{
    Metrics,
    metrics::{Counter, Gauge, Histogram},
};

/// Metrics for the TIP-403 policy cache, provider, and resolution task.
#[derive(Metrics, Clone)]
#[metrics(scope = "tip403")]
pub struct Tip403Metrics {
    /// Total authorization checks performed (cache + RPC).
    pub authorization_checks_total: Counter,

    /// Authorization checks served from cache (hits).
    pub cache_hits: Counter,

    /// Authorization checks that required L1 RPC fallback (misses).
    pub cache_misses: Counter,

    /// L1 RPC calls that failed during authorization resolution.
    pub rpc_errors: Counter,

    /// Duration of L1 RPC authorization resolution in seconds.
    pub rpc_resolution_duration_seconds: Histogram,

    /// Total pre-fetch requests submitted to the resolution task.
    pub prefetch_requests_total: Counter,

    /// Pre-fetch requests that completed successfully.
    pub prefetch_successes: Counter,

    /// Pre-fetch requests that failed.
    pub prefetch_failures: Counter,

    /// Number of in-flight concurrent resolution futures in the task.
    pub prefetch_in_flight: Gauge,
}
