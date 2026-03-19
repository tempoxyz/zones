//! Private RPC metric definitions and label helpers.
//!
//! The helpers in this module keep label cardinality bounded so the in-process
//! recorder stays safe for long-running nodes.

use reth_metrics::{
    Metrics,
    metrics::{Counter, Histogram},
};

use crate::types::classify_method;

#[derive(Metrics, Clone)]
#[metrics(scope = "zone_private_rpc.calls")]
pub(crate) struct PrivateRpcCallMetrics {
    /// Number of private RPC calls that started.
    pub(crate) started_total: Counter,
    /// Number of private RPC calls that returned success.
    pub(crate) successful_total: Counter,
    /// Number of private RPC calls that returned an error response.
    pub(crate) failed_total: Counter,
    /// Time spent processing a private RPC call.
    pub(crate) time_seconds: Histogram,
}

impl PrivateRpcCallMetrics {
    pub(crate) fn new_for(method: &str) -> Self {
        Self::new_with_labels(&[("method", canonical_method_label(method).to_string())])
    }
}

#[derive(Metrics, Clone)]
#[metrics(scope = "zone_private_rpc")]
pub(crate) struct PrivateRpcAuthMetrics {
    /// Number of authentication failures.
    pub(crate) auth_failures_total: Counter,
}

#[derive(Metrics, Clone)]
#[metrics(scope = "zone_private_rpc.provider")]
pub(crate) struct ZoneProviderMetrics {
    /// Number of private RPC provider token refresh attempts.
    pub(crate) token_refresh_attempts_total: Counter,
    /// Number of private RPC provider token refresh failures.
    pub(crate) token_refresh_failures_total: Counter,
}

/// Normalize JSON-RPC method names into the fixed label set used by metrics.
pub(crate) fn canonical_method_label(method: &str) -> &str {
    match classify_method(method) {
        Some(_) if method.starts_with("admin_") => "admin_*",
        Some(_) => method,
        None => "unknown",
    }
}
