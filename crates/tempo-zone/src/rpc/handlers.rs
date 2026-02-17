//! Private RPC method handlers.
//!
//! Each handler calls the underlying EthApi via the [`ZoneRpcApi`] trait,
//! which performs typed privacy redactions internally before serialization.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use alloy_primitives::B256;
use alloy_rpc_types_eth::BlockNumberOrTag;
use serde_json::Value;
use tracing::warn;

use super::{
    auth::AuthContext,
    config::PrivateRpcConfig,
    types::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, MethodTier, classify_method},
};

/// Type-erased interface to the underlying reth EthApi.
///
/// Implementations perform privacy redactions on typed responses
/// *before* serializing to `serde_json::Value`.
pub trait ZoneRpcApi: Send + Sync + 'static {
    /// `eth_getBlockByNumber(number, full)` — returns a block by number.
    fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
        auth: AuthContext,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + '_>>;

    /// `eth_getBlockByHash(hash, full)` — returns a block by hash.
    fn block_by_hash(
        &self,
        hash: B256,
        full: bool,
        auth: AuthContext,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + '_>>;

    /// `eth_getTransactionByHash(hash)` — returns a transaction by hash.
    fn transaction_by_hash(
        &self,
        hash: B256,
        auth: AuthContext,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + '_>>;

    /// `eth_getTransactionReceipt(hash)` — returns a transaction receipt.
    fn transaction_receipt(
        &self,
        hash: B256,
        auth: AuthContext,
    ) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send + '_>>;
}

/// Methods that require a timing floor to prevent side-channel leaks.
const TIMING_FLOOR_METHODS: &[&str] = &[
    "eth_getTransactionByHash",
    "eth_getTransactionReceipt",
    "eth_getLogs",
    "eth_getFilterLogs",
    "eth_getFilterChanges",
];

/// Minimum response time for fetch-then-check methods.
const TIMING_FLOOR: Duration = Duration::from_millis(100);

/// Dispatch a single JSON-RPC request through the access control pipeline.
pub async fn dispatch(
    req: &JsonRpcRequest,
    auth: &AuthContext,
    config: &PrivateRpcConfig,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let start = Instant::now();
    let needs_timing_floor = TIMING_FLOOR_METHODS.contains(&req.method.as_str());

    let response = dispatch_inner(req, auth, config, api).await;

    // Enforce timing floor for fetch-then-check methods
    if needs_timing_floor {
        let elapsed = start.elapsed();
        if elapsed < TIMING_FLOOR {
            tokio::time::sleep(TIMING_FLOOR - elapsed).await;
        }
    }

    response
}

async fn dispatch_inner(
    req: &JsonRpcRequest,
    auth: &AuthContext,
    _config: &PrivateRpcConfig,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let tier = match classify_method(&req.method) {
        Some(tier) => tier,
        None => return JsonRpcResponse::error(req.id.clone(), JsonRpcError::method_not_found()),
    };

    match tier {
        MethodTier::Disabled => {
            return JsonRpcResponse::error(req.id.clone(), JsonRpcError::method_disabled());
        }
        MethodTier::Restricted if !auth.is_sequencer => {
            return JsonRpcResponse::error(req.id.clone(), JsonRpcError::sequencer_only());
        }
        _ => {}
    }

    // Raw params JSON — handlers deserialize directly, no intermediate Vec<Value>.
    let raw = req.params.as_deref().map(|p| p.get()).unwrap_or("[]");

    match req.method.as_str() {
        "eth_getBlockByNumber" => handle_get_block_by_number(raw, auth, api, &req.id).await,
        "eth_getBlockByHash" => handle_get_block_by_hash(raw, auth, api, &req.id).await,
        "eth_getTransactionByHash" => {
            handle_get_transaction_by_hash(raw, auth, api, &req.id).await
        }
        "eth_getTransactionReceipt" => {
            handle_get_transaction_receipt(raw, auth, api, &req.id).await
        }
        _ => {
            // Method is whitelisted but not yet implemented via direct API
            JsonRpcResponse::error(
                req.id.clone(),
                JsonRpcError::internal("method not yet implemented in private RPC"),
            )
        }
    }
}

async fn handle_get_block_by_number(
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
    id: &Value,
) -> JsonRpcResponse {
    let (number, full): (BlockNumberOrTag, bool) =
        serde_json::from_str(raw).unwrap_or((BlockNumberOrTag::Latest, false));

    if full && !auth.is_sequencer {
        return JsonRpcResponse::error(id.clone(), JsonRpcError::sequencer_only());
    }

    match api.block_by_number(number, full, auth.clone()).await {
        Ok(result) => JsonRpcResponse::success(id.clone(), result),
        Err(e) => {
            warn!(target: "zone::rpc", err = %e, "eth_getBlockByNumber failed");
            JsonRpcResponse::error(id.clone(), JsonRpcError::internal(e))
        }
    }
}

async fn handle_get_block_by_hash(
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
    id: &Value,
) -> JsonRpcResponse {
    let (hash, full): (B256, bool) = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => {
            return JsonRpcResponse::error(
                id.clone(),
                JsonRpcError::invalid_params("missing block hash"),
            )
        }
    };

    if full && !auth.is_sequencer {
        return JsonRpcResponse::error(id.clone(), JsonRpcError::sequencer_only());
    }

    match api.block_by_hash(hash, full, auth.clone()).await {
        Ok(result) => JsonRpcResponse::success(id.clone(), result),
        Err(e) => {
            warn!(target: "zone::rpc", err = %e, "eth_getBlockByHash failed");
            JsonRpcResponse::error(id.clone(), JsonRpcError::internal(e))
        }
    }
}

async fn handle_get_transaction_by_hash(
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
    id: &Value,
) -> JsonRpcResponse {
    let (hash,): (B256,) = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => {
            return JsonRpcResponse::error(
                id.clone(),
                JsonRpcError::invalid_params("missing tx hash"),
            )
        }
    };

    match api.transaction_by_hash(hash, auth.clone()).await {
        Ok(result) => JsonRpcResponse::success(id.clone(), result),
        Err(e) => {
            warn!(target: "zone::rpc", err = %e, "eth_getTransactionByHash failed");
            JsonRpcResponse::error(id.clone(), JsonRpcError::internal(e))
        }
    }
}

async fn handle_get_transaction_receipt(
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
    id: &Value,
) -> JsonRpcResponse {
    let (hash,): (B256,) = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => {
            return JsonRpcResponse::error(
                id.clone(),
                JsonRpcError::invalid_params("missing tx hash"),
            )
        }
    };

    match api.transaction_receipt(hash, auth.clone()).await {
        Ok(result) => JsonRpcResponse::success(id.clone(), result),
        Err(e) => {
            warn!(target: "zone::rpc", err = %e, "eth_getTransactionReceipt failed");
            JsonRpcResponse::error(id.clone(), JsonRpcError::internal(e))
        }
    }
}
