//! Redacted RPC metric definitions and label helpers.
//!
//! The helpers in this module keep label cardinality bounded so the in-process
//! recorder stays safe for long-running nodes.

use reth_metrics::{
    Metrics,
    metrics::{Counter, Histogram},
};

use crate::types::classify_method;

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
    match classify_method(method) {
        Some(_) if method.starts_with("admin_") => "admin_*",
        Some(_) if method.starts_with("debug_") => "debug_*",
        Some(_) if method.starts_with("txpool_") => "txpool_*",
        Some(_) => method,
        None => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::canonical_method_label;

    #[test]
    fn canonicalizes_restricted_wildcard_method_families() {
        for (method, expected) in [
            ("admin_trace_0", "admin_*"),
            ("admin_trace_1", "admin_*"),
            ("debug_trace_0", "debug_*"),
            ("debug_trace_1", "debug_*"),
            ("txpool_content_0", "txpool_*"),
            ("txpool_content_1", "txpool_*"),
        ] {
            assert_eq!(canonical_method_label(method), expected);
        }
    }

    #[test]
    fn preserves_known_methods_and_buckets_unknown_methods() {
        assert_eq!(canonical_method_label("eth_call"), "eth_call");
        assert_eq!(canonical_method_label("missing_method"), "unknown");
    }
}
