//! Redacted RPC metric definitions and label helpers.
//!
//! The helpers in this module keep label cardinality bounded so the in-process
//! recorder stays safe for long-running nodes.

use reth_metrics::{
    Metrics,
    metrics::{Counter, Histogram},
};

use crate::types::Method;

#[derive(Metrics, Clone)]
#[metrics(scope = "tempo_zone_redacted_rpc_calls")]
pub(crate) struct RedactedRpcCallMetrics {
    /// Number of redacted RPC calls that started.
    pub(crate) started_total: Counter,
    /// Number of redacted RPC calls that returned success.
    pub(crate) successful_total: Counter,
    /// Number of redacted RPC calls that returned an error response.
    pub(crate) failed_total: Counter,
    /// Time spent processing a redacted RPC call.
    pub(crate) time_seconds: Histogram,
}

impl RedactedRpcCallMetrics {
    pub(crate) fn new_for(method: &str) -> Self {
        Self::new_with_labels(&[("method", canonical_method_label(method).to_string())])
    }
}

#[derive(Metrics, Clone)]
#[metrics(scope = "tempo_zone_redacted_rpc")]
pub(crate) struct RedactedRpcAuthMetrics {
    /// Number of authentication failures.
    pub(crate) auth_failures_total: Counter,
}

#[derive(Metrics, Clone)]
#[metrics(scope = "tempo_zone_redacted_rpc_provider")]
pub(crate) struct ZoneProviderMetrics {
    /// Number of redacted RPC provider token refresh attempts.
    pub(crate) token_refresh_attempts_total: Counter,
    /// Number of redacted RPC provider token refresh failures.
    pub(crate) token_refresh_failures_total: Counter,
}

/// Normalize JSON-RPC method names into the fixed label set used by metrics.
pub(crate) fn canonical_method_label(method: &str) -> &str {
    Method::from_name(method)
        .map(Method::name)
        .unwrap_or("unknown")
}
