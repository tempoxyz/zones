//! JSON-RPC types for the private zone RPC.

use std::{future::Future, pin::Pin};

use alloy_primitives::U256;
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};

/// Shorthand for the boxed future returned by [`ZoneRpcApi`](crate::handlers::ZoneRpcApi) methods.
///
/// Returns pre-serialized JSON ([`RawValue`]) to avoid an intermediate
/// `serde_json::Value` allocation — the result is embedded verbatim in
/// the JSON-RPC response.
pub type BoxFut<'a> =
    Pin<Box<dyn Future<Output = Result<Box<RawValue>, JsonRpcError>> + Send + 'a>>;

/// A JSON-RPC 2.0 request.
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcRequest {
    /// The JSON-RPC version (must be "2.0").
    pub jsonrpc: String,
    /// The method name.
    pub method: String,
    /// The parameters (raw JSON).
    pub params: Option<Box<serde_json::value::RawValue>>,
    /// The request ID.
    pub id: Value,
}

/// A JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcResponse {
    /// The JSON-RPC version.
    pub jsonrpc: &'static str,
    /// The result, if successful (embedded as pre-serialized JSON).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Box<RawValue>>,
    /// The error, if failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
    /// The request ID.
    pub id: Value,
}

impl JsonRpcResponse {
    /// Create a successful response from a pre-serialized result.
    pub fn success(id: Value, result: Box<RawValue>) -> Self {
        Self {
            jsonrpc: "2.0",
            result: Some(result),
            error: None,
            id,
        }
    }

    /// Create an error response.
    pub fn error(id: Value, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0",
            result: None,
            error: Some(error),
            id,
        }
    }
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// The error code.
    pub code: i64,
    /// The error message.
    pub message: String,
    /// Optional additional data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (code {})", self.message, self.code)
    }
}

impl JsonRpcError {
    /// Method not found (-32601).
    pub fn method_not_found() -> Self {
        Self {
            code: -32601,
            message: "Method not found".to_string(),
            data: None,
        }
    }

    /// Method disabled (-32006).
    pub fn method_disabled() -> Self {
        Self {
            code: -32006,
            message: "Method disabled".to_string(),
            data: None,
        }
    }

    /// Sequencer-only method (-32005).
    pub fn sequencer_only() -> Self {
        Self {
            code: -32005,
            message: "Sequencer only".to_string(),
            data: None,
        }
    }

    /// Invalid params (-32602).
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: msg.into(),
            data: None,
        }
    }

    /// Transaction rejected — sender mismatch (-32003).
    pub fn transaction_rejected() -> Self {
        Self {
            code: -32003,
            message: "Transaction rejected".to_string(),
            data: None,
        }
    }

    /// Account mismatch — `from` does not match authenticated account (-32004).
    pub fn account_mismatch() -> Self {
        Self {
            code: -32004,
            message: "Account mismatch".to_string(),
            data: None,
        }
    }

    /// Internal error (-32603).
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: msg.into(),
            data: None,
        }
    }
}

/// Method access tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodTier {
    /// Available to all authenticated callers.
    Public,
    /// Only available to the sequencer.
    Restricted,
    /// Disabled on the private RPC.
    Disabled,
}

/// Classify a JSON-RPC method into its access tier.
///
/// Returns `None` if the method is unknown.
pub fn classify_method(method: &str) -> Option<MethodTier> {
    match method {
        // Public read methods — no privacy redaction needed
        "eth_blockNumber"
        | "eth_chainId"
        | "eth_gasPrice"
        | "eth_getBalance"
        | "eth_getTransactionCount"
        | "eth_call"
        | "eth_estimateGas"
        | "eth_feeHistory"
        | "eth_maxPriorityFeePerGas"
        | "eth_getBlockByNumber"
        | "eth_getBlockByHash"
        | "eth_syncing"
        | "eth_coinbase"
        | "net_version"
        | "net_listening"
        | "web3_clientVersion"
        | "web3_sha3" => Some(MethodTier::Public),

        // Fetch-then-check: public but redacted based on caller identity
        "eth_getTransactionByHash"
        | "eth_getTransactionReceipt"
        | "eth_getLogs"
        | "eth_getFilterLogs"
        | "eth_getFilterChanges"
        | "eth_newFilter"
        | "eth_newBlockFilter"
        | "eth_uninstallFilter" => Some(MethodTier::Public),

        // Transaction preparation: public (scoped to caller's account)
        "eth_fillTransaction" => Some(MethodTier::Public),

        // Transaction submission: public (caller sends their own txs)
        "eth_sendRawTransaction" | "eth_sendRawTransactionSync" => Some(MethodTier::Public),

        // Sequencer-only — raw state inspection and full block data bypass privacy scoping
        "eth_getCode"
        | "eth_getStorageAt"
        | "eth_getBlockReceipts"
        | "eth_sendTransaction"
        | "debug_traceTransaction"
        | "debug_traceBlockByNumber"
        | "debug_traceBlockByHash"
        | "eth_createAccessList"
        | "eth_getBlockTransactionCountByNumber"
        | "eth_getBlockTransactionCountByHash"
        | "eth_getTransactionByBlockNumberAndIndex"
        | "eth_getTransactionByBlockHashAndIndex"
        | "eth_getUncleCountByBlockNumber"
        | "eth_getUncleCountByBlockHash"
        | "txpool_content"
        | "txpool_status"
        | "txpool_inspect" => Some(MethodTier::Restricted),

        // Disabled (mining, subscriptions not supported via HTTP proxy)
        "eth_mining" | "eth_hashrate" | "eth_submitWork" | "eth_submitHashrate"
        | "eth_subscribe" | "eth_unsubscribe" => Some(MethodTier::Disabled),

        // Zone-specific sequencer-only methods
        "zone_getBatchWitness" => Some(MethodTier::Restricted),

        _ if method.starts_with("admin_") => Some(MethodTier::Restricted),
        _ => None,
    }
}

/// Pre-serialized JSON `null`.
pub fn raw_null() -> Box<RawValue> {
    RawValue::from_string("null".to_string()).unwrap()
}

/// Pre-serialized JSON `"0x0"` — returned as a silent dummy for scoped queries
/// about non-caller accounts (e.g. `eth_getBalance`, `eth_getTransactionCount`).
pub fn raw_zero() -> Box<RawValue> {
    serde_json::value::to_raw_value(&U256::ZERO).unwrap()
}

/// Serialize a value directly to [`RawValue`], skipping the intermediate
/// `serde_json::Value` allocation.
pub fn to_raw<T: serde::Serialize>(value: &T) -> Result<Box<RawValue>, JsonRpcError> {
    serde_json::value::to_raw_value(value).map_err(|e| JsonRpcError::internal(e.to_string()))
}

/// Shorthand for wrapping any `Display` error into a [`JsonRpcError::internal`].
pub fn internal(e: impl std::fmt::Display) -> JsonRpcError {
    JsonRpcError::internal(e.to_string())
}
