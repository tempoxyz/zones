use reth_metrics::{
    Metrics,
    metrics::{Counter, Gauge, Histogram},
};

use crate::auth::AuthError;

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
    /// Count of RPC calls started.
    pub(crate) started_total: Counter,
    /// Count of successful RPC calls.
    pub(crate) successful_total: Counter,
    /// Count of failed RPC calls.
    pub(crate) failed_total: Counter,
    /// End-to-end RPC call latency.
    pub(crate) time_seconds: Histogram,
}

impl PrivateRpcCallMetrics {
    pub(crate) fn new_for(transport: RpcTransport, method: &str) -> Self {
        Self::new_with_labels(&[
            ("transport", transport.as_str()),
            ("method", canonical_method_label(method)),
        ])
    }
}

#[derive(Metrics, Clone)]
#[metrics(scope = "zone_private_rpc.auth")]
pub(crate) struct PrivateRpcAuthMetrics {
    /// Count of authentication failures grouped by reason.
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
    /// Number of active WebSocket sessions.
    pub(crate) sessions_active: Gauge,
    /// Count of WebSocket sessions opened.
    pub(crate) sessions_opened_total: Counter,
}

#[derive(Metrics, Clone)]
#[metrics(scope = "zone_private_rpc.ws")]
pub(crate) struct PrivateRpcWsDisconnectMetrics {
    /// Count of WebSocket disconnects grouped by reason.
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
    /// Count of authorization-token refresh attempts.
    pub(crate) token_refresh_attempts_total: Counter,
    /// Count of authorization-token refresh failures.
    pub(crate) token_refresh_failures_total: Counter,
}

pub(crate) fn canonical_method_label(method: &str) -> &'static str {
    match method {
        "eth_blockNumber" => "eth_blockNumber",
        "eth_chainId" => "eth_chainId",
        "eth_gasPrice" => "eth_gasPrice",
        "eth_getBalance" => "eth_getBalance",
        "eth_getTransactionCount" => "eth_getTransactionCount",
        "eth_call" => "eth_call",
        "eth_estimateGas" => "eth_estimateGas",
        "eth_feeHistory" => "eth_feeHistory",
        "eth_maxPriorityFeePerGas" => "eth_maxPriorityFeePerGas",
        "eth_getBlockByNumber" => "eth_getBlockByNumber",
        "eth_getBlockByHash" => "eth_getBlockByHash",
        "eth_syncing" => "eth_syncing",
        "eth_coinbase" => "eth_coinbase",
        "net_version" => "net_version",
        "net_listening" => "net_listening",
        "web3_clientVersion" => "web3_clientVersion",
        "web3_sha3" => "web3_sha3",
        "zone_getAuthorizationTokenInfo" => "zone_getAuthorizationTokenInfo",
        "zone_getZoneInfo" => "zone_getZoneInfo",
        "zone_getDepositStatus" => "zone_getDepositStatus",
        "eth_getTransactionByHash" => "eth_getTransactionByHash",
        "eth_getTransactionReceipt" => "eth_getTransactionReceipt",
        "eth_getLogs" => "eth_getLogs",
        "eth_getFilterLogs" => "eth_getFilterLogs",
        "eth_getFilterChanges" => "eth_getFilterChanges",
        "eth_newFilter" => "eth_newFilter",
        "eth_newBlockFilter" => "eth_newBlockFilter",
        "eth_uninstallFilter" => "eth_uninstallFilter",
        "eth_fillTransaction" => "eth_fillTransaction",
        "eth_sendRawTransaction" => "eth_sendRawTransaction",
        "eth_sendRawTransactionSync" => "eth_sendRawTransactionSync",
        "eth_getCode" => "eth_getCode",
        "eth_getStorageAt" => "eth_getStorageAt",
        "eth_getBlockReceipts" => "eth_getBlockReceipts",
        "eth_sendTransaction" => "eth_sendTransaction",
        "debug_traceTransaction" => "debug_traceTransaction",
        "debug_traceBlockByNumber" => "debug_traceBlockByNumber",
        "debug_traceBlockByHash" => "debug_traceBlockByHash",
        "eth_createAccessList" => "eth_createAccessList",
        "eth_getBlockTransactionCountByNumber" => "eth_getBlockTransactionCountByNumber",
        "eth_getBlockTransactionCountByHash" => "eth_getBlockTransactionCountByHash",
        "eth_getTransactionByBlockNumberAndIndex" => "eth_getTransactionByBlockNumberAndIndex",
        "eth_getTransactionByBlockHashAndIndex" => "eth_getTransactionByBlockHashAndIndex",
        "eth_getUncleCountByBlockNumber" => "eth_getUncleCountByBlockNumber",
        "eth_getUncleCountByBlockHash" => "eth_getUncleCountByBlockHash",
        "txpool_content" => "txpool_content",
        "txpool_status" => "txpool_status",
        "txpool_inspect" => "txpool_inspect",
        "eth_mining" => "eth_mining",
        "eth_hashrate" => "eth_hashrate",
        "eth_submitWork" => "eth_submitWork",
        "eth_submitHashrate" => "eth_submitHashrate",
        "eth_subscribe" => "eth_subscribe",
        "eth_unsubscribe" => "eth_unsubscribe",
        _ if method.starts_with("admin_") => "admin_*",
        _ => "unknown",
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
        AuthError::UnsupportedSignatureType => "unsupported_signature_type",
        AuthError::UnauthorizedKeychainKey => "unauthorized_keychain_key",
        AuthError::RevokedKeychainKey => "revoked_keychain_key",
        AuthError::ExpiredKeychainKey => "expired_keychain_key",
        AuthError::KeychainSignatureTypeMismatch => "keychain_signature_type_mismatch",
    }
}
