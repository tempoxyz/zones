//! JSON-RPC types for the private zone RPC.

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    /// The result, if successful.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// The error, if failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
    /// The request ID.
    pub id: Value,
}

impl JsonRpcResponse {
    /// Create a successful response.
    pub fn success(id: Value, result: Value) -> Self {
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

    /// Method disabled (-32601).
    pub fn method_disabled() -> Self {
        Self {
            code: -32601,
            message: "Method disabled on private RPC".to_string(),
            data: None,
        }
    }

    /// Sequencer-only method (-32604).
    pub fn sequencer_only() -> Self {
        Self {
            code: -32604,
            message: "Method restricted to sequencer".to_string(),
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
        // Public read methods
        "eth_blockNumber" | "eth_chainId" | "eth_gasPrice" | "eth_getBalance"
        | "eth_getTransactionCount" | "eth_getCode" | "eth_getStorageAt" | "eth_call"
        | "eth_estimateGas" | "eth_feeHistory" | "eth_maxPriorityFeePerGas"
        | "eth_getBlockByNumber" | "eth_getBlockByHash" | "eth_getBlockReceipts"
        | "net_version" | "net_listening" | "web3_clientVersion" => Some(MethodTier::Public),

        // Fetch-then-check: public but redacted based on caller identity
        "eth_getTransactionByHash" | "eth_getTransactionReceipt" | "eth_getLogs"
        | "eth_getFilterLogs" | "eth_getFilterChanges" => Some(MethodTier::Public),

        // Transaction submission: public (caller sends their own txs)
        "eth_sendRawTransaction" => Some(MethodTier::Public),

        // Sequencer-only
        "eth_sendTransaction" | "debug_traceTransaction" | "debug_traceBlockByNumber"
        | "debug_traceBlockByHash" | "txpool_content" | "txpool_status" | "txpool_inspect" => {
            Some(MethodTier::Restricted)
        }

        // Disabled (mining, subscriptions not supported via HTTP proxy)
        "eth_mining" | "eth_hashrate" | "eth_submitWork" | "eth_submitHashrate"
        | "eth_subscribe" | "eth_unsubscribe" => Some(MethodTier::Disabled),

        _ => None,
    }
}
