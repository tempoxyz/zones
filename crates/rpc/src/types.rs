//! JSON-RPC types for the private zone RPC.

use std::{future::Future, pin::Pin};

use alloy_primitives::{Address, B256, U64, U256};
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};

/// Shorthand for the boxed future returned by [`ZoneRpcApi`](crate::handlers::ZoneRpcApi) methods.
///
/// Returns pre-serialized JSON ([`RawValue`]) to avoid an intermediate
/// `serde_json::Value` allocation — the result is embedded verbatim in
/// the JSON-RPC response.
pub type BoxFut<'a> =
    Pin<Box<dyn Future<Output = Result<Box<RawValue>, JsonRpcError>> + Send + 'a>>;

/// Shorthand for typed boxed futures returned by internal async helpers.
pub type BoxEyreFut<'a, T> = Pin<Box<dyn Future<Output = eyre::Result<T>> + Send + 'a>>;

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

    /// Transaction rejected (-32003).
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

    /// Parse error — invalid JSON (-32700).
    pub fn parse_error(msg: impl Into<String>) -> Self {
        Self {
            code: -32700,
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

/// Response payload for `zone_getAuthorizationTokenInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationTokenInfoResponse {
    /// Authenticated account derived from the authorization token.
    pub account: Address,
    /// Expiration timestamp encoded as a JSON-RPC quantity.
    pub expires_at: U64,
}

/// Response payload for `zone_getZoneInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ZoneInfoResponse {
    /// The zone's numeric identifier.
    pub zone_id: U64,
    /// Whether account allowlist enforcement is enabled.
    pub is_access_enforced: bool,
    /// Whether callback gateway registration enforcement is disabled.
    pub is_gateway_open: bool,
    /// The enabled zone token contract addresses.
    pub zone_tokens: Vec<Address>,
    /// The active sequencer addresses.
    pub sequencers: Vec<Address>,
    /// The zone chain ID.
    pub chain_id: U64,
    /// The latest Tempo block imported into the zone.
    pub tempo_block_number: U64,
}

/// Local view of one sequencer node for `zone_getSequencerInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalSequencerInfo {
    /// Manifest node name.
    pub name: String,
    /// Individual secp256k1 address.
    pub sequencer_address: Address,
    /// Hex-encoded Ed25519 Commonware public key.
    pub p2p_public_key: String,
    /// Current role: `leader`, `follower`, or `fenced`.
    pub role: String,
}

/// Active finalized leader for `zone_getSequencerInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveLeaderInfo {
    /// Manifest node name, when the leader maps to a manifest member.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Individual secp256k1 address registered on the portal.
    pub sequencer_address: Address,
    /// Hex-encoded Ed25519 Commonware public key.
    pub p2p_public_key: String,
    /// Leadership epoch.
    pub epoch: U64,
    /// Tempo block at which this leader's authorization begins.
    pub activation_tempo_block: U64,
}

/// A configured peer with its most recently advertised tip evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SequencerPeerInfo {
    /// Manifest node name.
    pub name: String,
    /// Individual secp256k1 address.
    pub sequencer_address: Address,
    /// Whether this entry describes the local node.
    pub is_local: bool,
    /// Most recent hash-carrying tip evidence, when observed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tip: Option<PeerTipInfo>,
}

/// Hash-carrying tip evidence advertised by a peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerTipInfo {
    /// Height of the peer's canonical zone head.
    pub zone_height: U64,
    /// Hash of the peer's canonical zone head.
    pub zone_hash: B256,
    /// Tempo anchor embedded in that head.
    pub tempo_block_number: U64,
    /// Hash of that Tempo anchor.
    pub tempo_block_hash: B256,
}

/// Consumption and observation progress for `zone_getSequencerInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SequencerProgress {
    /// Local canonical zone height.
    pub zone_height: U64,
    /// Local canonical Tempo checkpoint.
    pub tempo_block_number: U64,
    /// Highest leadership epoch finalized L1 has shown this node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_observed_leadership_epoch: Option<U64>,
    /// Epoch whose activation boundary local consumption has crossed (observability only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locally_applied_leadership_epoch: Option<U64>,
    /// Observed transitions whose activation boundary is still ahead of local consumption.
    pub pending_transitions: U64,
}

/// Promotion-readiness snapshot for `zone_getSequencerInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SequencerReadiness {
    /// Whether the promotion barrier is currently satisfied.
    pub ready_for_promotion: bool,
    /// Unsatisfied readiness reasons (empty when ready).
    pub reasons: Vec<String>,
}

/// Response payload for `zone_getSequencerInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SequencerInfoResponse {
    /// `multi` in manifest mode, `single` otherwise.
    pub mode: String,
    /// ZonePortal address on Tempo L1.
    pub portal: Address,
    /// Local node identity and role (multi-sequencer mode only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local: Option<LocalSequencerInfo>,
    /// Active finalized leader (multi-sequencer mode only, once observed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_leader: Option<ActiveLeaderInfo>,
    /// All configured manifest members with observed tip evidence.
    pub peers: Vec<SequencerPeerInfo>,
    /// Consumption and observation progress (multi-sequencer mode only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<SequencerProgress>,
    /// Promotion-readiness snapshot (multi-sequencer mode only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readiness: Option<SequencerReadiness>,
}

/// Response payload for `zone_setLeader`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetLeaderResponse {
    /// `submitted` when a transaction was relayed, `alreadyActive` for a finalized no-op.
    pub status: String,
    /// Hash of the relayed L1 transaction, when one was submitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_hash: Option<B256>,
    /// Individual sequencer address that relayed the transaction.
    pub relayer: Address,
    /// The requested leader's individual sequencer address.
    pub requested_leader: Address,
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
        | "web3_sha3"
        | "zone_getAuthorizationTokenInfo"
        | "zone_getZoneInfo"
        | "zone_getSequencerInfo"
        | "zone_getEncryptionKey" => Some(MethodTier::Public),

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
        | "eth_createAccessList"
        | "eth_getBlockTransactionCountByNumber"
        | "eth_getBlockTransactionCountByHash"
        | "eth_getTransactionByBlockNumberAndIndex"
        | "eth_getTransactionByBlockHashAndIndex"
        | "eth_getUncleCountByBlockNumber"
        | "eth_getUncleCountByBlockHash" => Some(MethodTier::Restricted),

        // Disabled (mempool observation, mining, subscriptions not supported via HTTP)
        "eth_getProof"
        | "eth_newPendingTransactionFilter"
        | "eth_getUncleByBlockNumberAndIndex"
        | "eth_getUncleByBlockHashAndIndex"
        | "eth_mining"
        | "eth_hashrate"
        | "eth_getWork"
        | "eth_submitWork"
        | "eth_submitHashrate"
        | "eth_subscribe"
        | "eth_unsubscribe" => Some(MethodTier::Disabled),

        _ if method.starts_with("admin_") => Some(MethodTier::Restricted),
        _ if method.starts_with("debug_") => Some(MethodTier::Restricted),
        _ if method.starts_with("txpool_") => Some(MethodTier::Restricted),
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
