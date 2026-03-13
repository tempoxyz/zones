//! Private RPC method handlers.
//!
//! Each handler calls the underlying EthApi via the [`ZoneRpcApi`] trait,
//! which performs typed privacy redactions internally before serialization.

use alloy_primitives::{Address, B256, Bytes};
use alloy_rpc_types_eth::{BlockId, BlockNumberOrTag, Filter, FilterId, state::StateOverride};
use serde_json::{Value, value::RawValue};
use tempo_alloy::rpc::TempoTransactionRequest;
use tracing::warn;

use crate::{
    auth::AuthContext,
    subscription::BoxWsSubscriptionFut,
    types::{BoxFut, JsonRpcError, JsonRpcRequest, JsonRpcResponse, MethodTier, classify_method},
};

/// Interface to the underlying reth EthApi for the private zone RPC.
///
/// Implementations are responsible for:
/// - **Access control**: restricting responses based on the [`AuthContext`]
///   (e.g. returning `null` for transactions not owned by the caller).
/// - **Redaction**: scrubbing privacy-sensitive fields (e.g. zeroing
///   `logsBloom`, clearing transaction lists) on typed responses *before*
///   serializing to JSON.
pub trait ZoneRpcApi: Send + Sync + 'static {
    /// `eth_blockNumber` — returns the latest block number.
    fn block_number(&self) -> BoxFut<'_>;

    /// `eth_chainId` — returns the chain ID.
    fn chain_id(&self) -> BoxFut<'_>;

    /// `net_version` — returns the network ID as a decimal string.
    fn net_version(&self) -> BoxFut<'_>;

    /// `eth_gasPrice` — returns the current gas price.
    fn gas_price(&self) -> BoxFut<'_>;

    /// `eth_maxPriorityFeePerGas` — returns the current max priority fee.
    fn max_priority_fee_per_gas(&self) -> BoxFut<'_>;

    /// `eth_feeHistory(blockCount, newestBlock, rewardPercentiles)` — returns fee history.
    fn fee_history(
        &self,
        block_count: u64,
        newest_block: BlockNumberOrTag,
        reward_percentiles: Option<Vec<f64>>,
    ) -> BoxFut<'_>;

    /// `eth_getBalance(address, block)` — returns the balance of an account.
    ///
    /// Returns `0x0` for non-sequencer callers querying an address that does
    /// not match `auth.caller`.
    fn get_balance(
        &self,
        address: Address,
        block: Option<BlockId>,
        auth: AuthContext,
    ) -> BoxFut<'_>;

    /// `eth_getTransactionCount(address, block)` — returns the nonce.
    ///
    /// Returns `0x0` for non-sequencer callers querying an address that does
    /// not match `auth.caller`.
    fn get_transaction_count(
        &self,
        address: Address,
        block: Option<BlockId>,
        auth: AuthContext,
    ) -> BoxFut<'_>;

    /// `eth_getBlockByNumber(number, full)` — returns a block by number.
    fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
        auth: AuthContext,
    ) -> BoxFut<'_>;

    /// `eth_getBlockByHash(hash, full)` — returns a block by hash.
    fn block_by_hash(&self, hash: B256, full: bool, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_getTransactionByHash(hash)` — returns a transaction by hash.
    fn transaction_by_hash(&self, hash: B256, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_getTransactionReceipt(hash)` — returns a transaction receipt.
    fn transaction_receipt(&self, hash: B256, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_call(request, block, state_override)` — executes a call without
    /// creating a transaction.
    ///
    /// Enforces that `from` equals the authenticated account (sets it if omitted,
    /// rejects with `-32004` on mismatch). State/block overrides are rejected
    /// for non-sequencer callers (`-32602`).
    fn call(
        &self,
        request: TempoTransactionRequest,
        block: Option<BlockId>,
        state_override: Option<StateOverride>,
        auth: AuthContext,
    ) -> BoxFut<'_>;

    /// `eth_estimateGas(request, block, state_override)` — estimates gas for a transaction.
    ///
    /// Same `from`-enforcement as [`call`](Self::call). State overrides are
    /// rejected for non-sequencer callers (`-32602`).
    fn estimate_gas(
        &self,
        request: TempoTransactionRequest,
        block: Option<BlockId>,
        state_override: Option<StateOverride>,
        auth: AuthContext,
    ) -> BoxFut<'_>;

    /// `eth_sendRawTransaction(data)` — submits a signed transaction to the pool.
    ///
    /// Verifies that the recovered tx sender matches the authenticated account;
    /// rejects with `-32003` on mismatch.
    fn send_raw_transaction(&self, data: Bytes, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_sendRawTransactionSync(data)` — submits a signed transaction and
    /// waits for inclusion, returning the receipt.
    ///
    /// Same sender verification as [`send_raw_transaction`](Self::send_raw_transaction).
    fn send_raw_transaction_sync(&self, data: Bytes, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_fillTransaction(request)` — fills defaults on an unsigned transaction
    /// (nonce, gas limit, fees, chain ID) and returns the filled + RLP-encoded
    /// result without signing or submitting.
    ///
    /// Same `from`-enforcement as [`call`](Self::call).
    fn fill_transaction(&self, request: TempoTransactionRequest, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_getLogs(filter)` — returns logs matching the filter, scoped to the caller.
    fn get_logs(&self, filter: Filter, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_newFilter(filter)` — creates a new log filter, scoped to the caller.
    fn new_filter(&self, filter: Filter, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_getFilterLogs(id)` — returns all logs for a filter.
    fn get_filter_logs(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_getFilterChanges(id)` — returns new logs since last poll.
    fn get_filter_changes(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_newBlockFilter` — creates a new block filter.
    fn new_block_filter(&self, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_uninstallFilter(id)` — removes a filter.
    fn uninstall_filter(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_>;

    /// `eth_subscribe("newHeads")` — opens a stream of new block headers.
    fn ws_subscribe_new_heads(&self, _auth: AuthContext) -> BoxWsSubscriptionFut<'_> {
        Box::pin(async { Err(JsonRpcError::method_disabled()) })
    }

    /// `eth_subscribe("logs", filter)` — opens a stream of matching logs.
    fn ws_subscribe_logs(&self, _filter: Filter, _auth: AuthContext) -> BoxWsSubscriptionFut<'_> {
        Box::pin(async { Err(JsonRpcError::method_disabled()) })
    }
}

/// Deserialize JSON-RPC params, returning an error response on failure.
#[allow(clippy::result_large_err)]
fn parse_params<T: serde::de::DeserializeOwned>(
    raw: &str,
    id: &Value,
    msg: &'static str,
) -> Result<T, JsonRpcResponse> {
    serde_json::from_str(raw)
        .map_err(|_| JsonRpcResponse::error(id.clone(), JsonRpcError::invalid_params(msg)))
}

/// Params for `eth_call` / `eth_estimateGas`: `[request, block?, stateOverride?]`.
///
/// Supports 1–3 element arrays with null-as-absent semantics for trailing optionals.
struct CallParams(
    TempoTransactionRequest,
    Option<BlockId>,
    Option<StateOverride>,
);

impl<'de> serde::Deserialize<'de> for CallParams {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Vis;
        impl<'de> serde::de::Visitor<'de> for Vis {
            type Value = CallParams;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("[request, block?, stateOverride?]")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let request = seq
                    .next_element()?
                    .ok_or_else(|| serde::de::Error::invalid_length(0, &self))?;
                let block = seq.next_element::<Option<BlockId>>()?.flatten();
                let state_override = seq.next_element::<Option<StateOverride>>()?.flatten();
                Ok(CallParams(request, block, state_override))
            }
        }
        deserializer.deserialize_seq(Vis)
    }
}

/// Convert an API result into a JSON-RPC response, logging failures.
fn api_result(
    id: Value,
    method: &str,
    res: Result<Box<RawValue>, JsonRpcError>,
) -> JsonRpcResponse {
    match res {
        Ok(v) => JsonRpcResponse::success(id, v),
        Err(e) => {
            warn!(target: "zone::rpc", err = %e, method, "RPC call failed");
            JsonRpcResponse::error(id, e)
        }
    }
}

/// Dispatch a single JSON-RPC request through the private zone RPC pipeline.
///
/// Enforces a strict whitelist of allowed methods (see [`classify_method`]) and
/// rejects anything unknown or disabled. Restricted methods are gated on
/// [`AuthContext::is_sequencer`]. Individual handlers may apply additional
/// per-method access checks (e.g. `full` block requests are sequencer-only).
pub async fn dispatch(
    req: &JsonRpcRequest,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let id = req.id.clone();

    let tier = match classify_method(&req.method) {
        Some(tier) => tier,
        None => return JsonRpcResponse::error(id, JsonRpcError::method_not_found()),
    };

    match tier {
        MethodTier::Disabled => {
            return JsonRpcResponse::error(id, JsonRpcError::method_disabled());
        }
        MethodTier::Restricted if !auth.is_sequencer => {
            return JsonRpcResponse::error(id, JsonRpcError::sequencer_only());
        }
        _ => {}
    }

    // Raw params JSON — handlers deserialize directly, no intermediate Vec<Value>.
    let raw = req.params.as_deref().map(|p| p.get()).unwrap_or("[]");

    match req.method.as_str() {
        // Simple passthrough methods (no params, no auth scoping)
        "eth_blockNumber" => api_result(id, "eth_blockNumber", api.block_number().await),
        "eth_chainId" => api_result(id, "eth_chainId", api.chain_id().await),
        "eth_gasPrice" => api_result(id, "eth_gasPrice", api.gas_price().await),
        "eth_maxPriorityFeePerGas" => api_result(
            id,
            "eth_maxPriorityFeePerGas",
            api.max_priority_fee_per_gas().await,
        ),
        "net_version" => api_result(id, "net_version", api.net_version().await),
        "net_listening" => api_result(id, "net_listening", crate::types::to_raw(&true)),
        "web3_clientVersion" => api_result(
            id,
            "web3_clientVersion",
            crate::types::to_raw(&"tempo-zone/v0.1.0"),
        ),

        // Fee history
        "eth_feeHistory" => handle_fee_history(id, raw, api).await,

        // Scoped state queries
        "eth_getBalance" => handle_get_balance(id, raw, auth, api).await,
        "eth_getTransactionCount" => handle_get_transaction_count(id, raw, auth, api).await,

        // Block queries
        "eth_getBlockByNumber" => handle_get_block_by_number(id, raw, auth, api).await,
        "eth_getBlockByHash" => handle_get_block_by_hash(id, raw, auth, api).await,

        // Transaction queries
        "eth_getTransactionByHash" => handle_get_transaction_by_hash(id, raw, auth, api).await,
        "eth_getTransactionReceipt" => handle_get_transaction_receipt(id, raw, auth, api).await,

        // Simulation
        "eth_call" => handle_call(id, raw, auth, api).await,
        "eth_estimateGas" => handle_estimate_gas(id, raw, auth, api).await,

        // Transaction preparation & submission
        "eth_fillTransaction" => handle_fill_transaction(id, raw, auth, api).await,
        "eth_sendRawTransaction" => handle_send_raw_transaction(id, raw, auth, api).await,
        "eth_sendRawTransactionSync" => handle_send_raw_transaction_sync(id, raw, auth, api).await,

        // Log & filter queries
        "eth_getLogs" => handle_get_logs(id, raw, auth, api).await,
        "eth_newFilter" => handle_new_filter(id, raw, auth, api).await,
        "eth_getFilterLogs" => handle_get_filter_logs(id, raw, auth, api).await,
        "eth_getFilterChanges" => handle_get_filter_changes(id, raw, auth, api).await,
        "eth_newBlockFilter" => handle_new_block_filter(id, auth, api).await,
        "eth_uninstallFilter" => handle_uninstall_filter(id, raw, auth, api).await,
        _ => {
            // Method is whitelisted but not yet implemented via direct API
            JsonRpcResponse::error(
                id,
                JsonRpcError::internal("method not yet implemented in private RPC"),
            )
        }
    }
}

/// Handle `eth_getBlockByNumber`. Rejects `full=true` for non-sequencer callers.
async fn handle_get_block_by_number(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (number, full) = match parse_params::<(BlockNumberOrTag, bool)>(
        raw,
        &id,
        "expected [blockNumberOrTag, full]",
    ) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    if full && !auth.is_sequencer {
        return JsonRpcResponse::error(id, JsonRpcError::sequencer_only());
    }

    api_result(
        id,
        "eth_getBlockByNumber",
        api.block_by_number(number, full, auth.clone()).await,
    )
}

/// Handle `eth_getBlockByHash`. Rejects `full=true` for non-sequencer callers.
async fn handle_get_block_by_hash(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (hash, full) = match parse_params::<(B256, bool)>(raw, &id, "expected [blockHash, full]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    if full && !auth.is_sequencer {
        return JsonRpcResponse::error(id, JsonRpcError::sequencer_only());
    }

    api_result(
        id,
        "eth_getBlockByHash",
        api.block_by_hash(hash, full, auth.clone()).await,
    )
}

/// Handle `eth_getTransactionByHash`. Access control is delegated to the API impl.
async fn handle_get_transaction_by_hash(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (hash,) = match parse_params::<(B256,)>(raw, &id, "expected [txHash]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    api_result(
        id,
        "eth_getTransactionByHash",
        api.transaction_by_hash(hash, auth.clone()).await,
    )
}

/// Handle `eth_getTransactionReceipt`. Access control is delegated to the API impl.
async fn handle_get_transaction_receipt(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (hash,) = match parse_params::<(B256,)>(raw, &id, "expected [txHash]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    api_result(
        id,
        "eth_getTransactionReceipt",
        api.transaction_receipt(hash, auth.clone()).await,
    )
}

/// Handle `eth_call`. Enforces `from` matches the authenticated account and
/// rejects state overrides for non-sequencer callers.
async fn handle_call(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let CallParams(request, block, state_override) =
        match parse_params(raw, &id, "expected [request, block?, stateOverride?]") {
            Ok(v) => v,
            Err(resp) => return resp,
        };

    if !auth.is_sequencer && state_override.is_some() {
        return JsonRpcResponse::error(
            id,
            JsonRpcError::invalid_params("state overrides not allowed"),
        );
    }

    api_result(
        id,
        "eth_call",
        api.call(request, block, state_override, auth.clone()).await,
    )
}

/// Handle `eth_estimateGas`. Same `from`-enforcement as `eth_call`.
/// Rejects state overrides for non-sequencer callers.
async fn handle_estimate_gas(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let CallParams(request, block, state_override) =
        match parse_params(raw, &id, "expected [request, block?, stateOverride?]") {
            Ok(v) => v,
            Err(resp) => return resp,
        };

    if !auth.is_sequencer && state_override.is_some() {
        return JsonRpcResponse::error(
            id,
            JsonRpcError::invalid_params("state overrides not allowed"),
        );
    }

    api_result(
        id,
        "eth_estimateGas",
        api.estimate_gas(request, block, state_override, auth.clone())
            .await,
    )
}

/// Handle `eth_fillTransaction`. `from`-enforcement is delegated to the API impl.
async fn handle_fill_transaction(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (request,) =
        match parse_params::<(TempoTransactionRequest,)>(raw, &id, "expected [request]") {
            Ok(v) => v,
            Err(resp) => return resp,
        };

    api_result(
        id,
        "eth_fillTransaction",
        api.fill_transaction(request, auth.clone()).await,
    )
}

/// Handle `eth_sendRawTransaction`. Sender verification is delegated to the API impl.
async fn handle_send_raw_transaction(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (data,) = match parse_params::<(Bytes,)>(raw, &id, "expected [data]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    api_result(
        id,
        "eth_sendRawTransaction",
        api.send_raw_transaction(data, auth.clone()).await,
    )
}

/// Handle `eth_sendRawTransactionSync`. Sender verification is delegated to
/// the API impl.
async fn handle_send_raw_transaction_sync(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (data,) = match parse_params::<(Bytes,)>(raw, &id, "expected [data]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    api_result(
        id,
        "eth_sendRawTransactionSync",
        api.send_raw_transaction_sync(data, auth.clone()).await,
    )
}

/// Handle `eth_feeHistory`. Public method, no auth scoping needed.
async fn handle_fee_history(id: Value, raw: &str, api: &dyn ZoneRpcApi) -> JsonRpcResponse {
    let (block_count, newest_block, reward_percentiles) =
        match parse_params::<(u64, BlockNumberOrTag, Option<Vec<f64>>)>(
            raw,
            &id,
            "expected [blockCount, newestBlock, rewardPercentiles?]",
        ) {
            Ok(v) => v,
            Err(resp) => return resp,
        };

    api_result(
        id,
        "eth_feeHistory",
        api.fee_history(block_count, newest_block, reward_percentiles)
            .await,
    )
}

/// Handle `eth_getBalance`. Returns `0x0` for non-sequencer callers querying
/// a different address (checked in API impl, no timing leak since check is pre-fetch).
async fn handle_get_balance(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (address, block) =
        match parse_params::<(Address, Option<BlockId>)>(raw, &id, "expected [address, block?]") {
            Ok(v) => v,
            Err(resp) => return resp,
        };

    api_result(
        id,
        "eth_getBalance",
        api.get_balance(address, block, auth.clone()).await,
    )
}

/// Handle `eth_getTransactionCount`. Returns `0x0` for non-sequencer callers
/// querying a different address (checked in API impl, no timing leak).
async fn handle_get_transaction_count(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (address, block) =
        match parse_params::<(Address, Option<BlockId>)>(raw, &id, "expected [address, block?]") {
            Ok(v) => v,
            Err(resp) => return resp,
        };

    api_result(
        id,
        "eth_getTransactionCount",
        api.get_transaction_count(address, block, auth.clone())
            .await,
    )
}

/// Handle `eth_getLogs`.
async fn handle_get_logs(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (filter,) = match parse_params::<(Filter,)>(raw, &id, "expected [filter]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    api_result(id, "eth_getLogs", api.get_logs(filter, auth.clone()).await)
}

/// Handle `eth_newFilter`.
async fn handle_new_filter(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (filter,) = match parse_params::<(Filter,)>(raw, &id, "expected [filter]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    api_result(
        id,
        "eth_newFilter",
        api.new_filter(filter, auth.clone()).await,
    )
}

/// Handle `eth_getFilterLogs`.
async fn handle_get_filter_logs(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (filter_id,) = match parse_params::<(FilterId,)>(raw, &id, "expected [filterId]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    api_result(
        id,
        "eth_getFilterLogs",
        api.get_filter_logs(filter_id, auth.clone()).await,
    )
}

/// Handle `eth_getFilterChanges`.
async fn handle_get_filter_changes(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (filter_id,) = match parse_params::<(FilterId,)>(raw, &id, "expected [filterId]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    api_result(
        id,
        "eth_getFilterChanges",
        api.get_filter_changes(filter_id, auth.clone()).await,
    )
}

/// Handle `eth_newBlockFilter`.
async fn handle_new_block_filter(
    id: Value,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    api_result(
        id,
        "eth_newBlockFilter",
        api.new_block_filter(auth.clone()).await,
    )
}

/// Handle `eth_uninstallFilter`.
async fn handle_uninstall_filter(
    id: Value,
    raw: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let (filter_id,) = match parse_params::<(FilterId,)>(raw, &id, "expected [filterId]") {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    api_result(
        id,
        "eth_uninstallFilter",
        api.uninstall_filter(filter_id, auth.clone()).await,
    )
}
