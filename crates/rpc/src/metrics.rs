//! Private RPC metric definitions and label helpers.
//!
//! The helpers in this module keep label cardinality bounded so the in-process
//! recorder stays safe for long-running nodes.

use reth_metrics::{
    Metrics,
    metrics::{Counter, Gauge, Histogram},
};

use crate::{auth::AuthError, types::classify_method};

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum RpcTransport {
    Http,
    Ws,
}

impl RpcTransport {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Ws => "ws",
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum WsDisconnectReason {
    ClientClose,
    StreamEnded,
    RecvError,
    SendError,
}

impl WsDisconnectReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ClientClose => "client_close",
            Self::StreamEnded => "stream_ended",
            Self::RecvError => "recv_error",
            Self::SendError => "send_error",
        }
    }
}

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
    pub(crate) fn new_for(transport: RpcTransport, method: &str) -> Self {
        Self::new_with_labels(&[
            ("transport", transport.as_str().to_string()),
            ("method", canonical_method_label(method).to_string()),
        ])
    }
}

#[derive(Metrics, Clone)]
#[metrics(scope = "zone_private_rpc.auth")]
pub(crate) struct PrivateRpcAuthMetrics {
    /// Number of authentication failures.
    pub(crate) failures_total: Counter,
}

impl PrivateRpcAuthMetrics {
    fn new_for(transport: RpcTransport, reason: &'static str) -> Self {
        Self::new_with_labels(&[("transport", transport.as_str()), ("reason", reason)])
    }
}

#[derive(Metrics, Clone)]
#[metrics(scope = "zone_private_rpc.ws")]
pub(crate) struct PrivateRpcWsSessionMetrics {
    /// Number of active private RPC WebSocket sessions.
    pub(crate) sessions_active: Gauge,
    /// Number of private RPC WebSocket sessions opened.
    pub(crate) sessions_opened_total: Counter,
}

#[derive(Metrics, Clone)]
#[metrics(scope = "zone_private_rpc.ws")]
pub(crate) struct PrivateRpcWsDisconnectMetrics {
    /// Number of private RPC WebSocket session disconnects.
    pub(crate) disconnects_total: Counter,
}

impl PrivateRpcWsDisconnectMetrics {
    pub(crate) fn new_for(reason: WsDisconnectReason) -> Self {
        Self::new_with_labels(&[("reason", reason.as_str())])
    }
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

pub(crate) fn record_auth_failure(transport: RpcTransport, error: &AuthError) {
    PrivateRpcAuthMetrics::new_for(transport, auth_reason_label(error))
        .failures_total
        .increment(1);
}

fn auth_reason_label(error: &AuthError) -> &'static str {
    match error {
        AuthError::Missing => "missing",
        AuthError::InvalidHex => "invalid_hex",
        AuthError::TooShort => "too_short",
        AuthError::UnsupportedVersion(_) => "unsupported_version",
        AuthError::ZoneIdMismatch => "zone_id_mismatch",
        AuthError::ChainIdMismatch => "chain_id_mismatch",
        AuthError::ZonePortalMismatch => "zone_portal_mismatch",
        AuthError::WindowTooLarge => "window_too_large",
        AuthError::Expired => "expired",
        AuthError::IssuedInFuture => "issued_in_future",
        AuthError::InvalidSignature => "invalid_signature",
        AuthError::UnauthorizedKeychainKey => "unauthorized_keychain_key",
        AuthError::RevokedKeychainKey => "revoked_keychain_key",
        AuthError::ExpiredKeychainKey => "expired_keychain_key",
        AuthError::KeychainSignatureTypeMismatch => "keychain_signature_type_mismatch",
    }
}
