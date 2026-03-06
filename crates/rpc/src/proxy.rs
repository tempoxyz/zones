//! HTTP proxy implementation of [`ZoneRpcApi`].
//!
//! [`ProxyZoneRpc`] forwards JSON-RPC requests to an upstream zone node and
//! applies privacy redactions on the responses. This allows the private RPC
//! service to run as a standalone process without linking against reth.

use std::{collections::HashMap, sync::Arc};

use alloy_primitives::{Address, Bytes};
use alloy_rpc_types_eth::{BlockId, BlockNumberOrTag, Filter, FilterId, Log, state::StateOverride};
use serde::Deserialize;
use serde_json::value::RawValue;
use tempo_alloy::rpc::TempoTransactionRequest;
use tokio::sync::Mutex;

use crate::{
    auth::AuthContext,
    filter,
    handlers::ZoneRpcApi,
    policy,
    types::{BoxFut, JsonRpcError, internal, raw_null, raw_zero, to_raw},
};

/// Upstream JSON-RPC response envelope.
#[derive(Deserialize)]
struct UpstreamResponse {
    result: Option<Box<RawValue>>,
    error: Option<JsonRpcError>,
}

/// HTTP proxy implementation of [`ZoneRpcApi`].
///
/// Forwards requests to an upstream zone node's standard (non-private) RPC
/// endpoint and applies per-caller privacy redactions on the responses.
pub struct ProxyZoneRpc {
    client: reqwest::Client,
    upstream_url: String,
    /// Maps filter IDs to the authenticated account that created them.
    filter_owners: Arc<Mutex<HashMap<FilterId, Address>>>,
}

impl ProxyZoneRpc {
    /// Create a new proxy targeting the given upstream RPC URL.
    pub fn new(upstream_url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            upstream_url,
            filter_owners: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Forward a JSON-RPC call to the upstream node.
    async fn forward(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<Box<RawValue>, JsonRpcError> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": 1
        });

        let response = self
            .client
            .post(&self.upstream_url)
            .json(&request)
            .send()
            .await
            .map_err(|e| JsonRpcError::internal(e.to_string()))?;

        let upstream: UpstreamResponse = response
            .json()
            .await
            .map_err(|e| JsonRpcError::internal(e.to_string()))?;

        if let Some(err) = upstream.error {
            return Err(err);
        }

        upstream
            .result
            .ok_or_else(|| JsonRpcError::internal("missing result in upstream response"))
    }

    /// Verify that the filter belongs to the authenticated caller.
    async fn ensure_filter_owner(
        &self,
        id: &FilterId,
        auth: &AuthContext,
    ) -> Result<(), JsonRpcError> {
        if auth.is_sequencer {
            return Ok(());
        }
        let owners = self.filter_owners.lock().await;
        match owners.get(id) {
            Some(owner) if *owner == auth.caller => Ok(()),
            _ => Err(JsonRpcError::invalid_params("filter not found")),
        }
    }
}

/// Strip privacy-sensitive fields from a block JSON object for non-sequencer callers.
///
/// Zeroes `logsBloom` and replaces `transactions` with an empty array.
fn redact_block_json(value: &mut serde_json::Value) {
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "logsBloom".to_string(),
            serde_json::Value::String(format!("0x{}", "0".repeat(512))),
        );
        obj.insert("transactions".to_string(), serde_json::Value::Array(vec![]));
    }
}

/// Extract the `from` address from a JSON transaction or receipt object.
fn json_from(value: &serde_json::Value) -> Option<Address> {
    value.get("from")?.as_str()?.parse().ok()
}

impl ZoneRpcApi for ProxyZoneRpc {
    fn block_number(&self) -> BoxFut<'_> {
        Box::pin(async move { self.forward("eth_blockNumber", serde_json::json!([])).await })
    }

    fn chain_id(&self) -> BoxFut<'_> {
        Box::pin(async move { self.forward("eth_chainId", serde_json::json!([])).await })
    }

    fn net_version(&self) -> BoxFut<'_> {
        Box::pin(async move { self.forward("net_version", serde_json::json!([])).await })
    }

    fn gas_price(&self) -> BoxFut<'_> {
        Box::pin(async move { self.forward("eth_gasPrice", serde_json::json!([])).await })
    }

    fn max_priority_fee_per_gas(&self) -> BoxFut<'_> {
        Box::pin(async move {
            self.forward("eth_maxPriorityFeePerGas", serde_json::json!([]))
                .await
        })
    }

    fn fee_history(
        &self,
        block_count: u64,
        newest_block: BlockNumberOrTag,
        reward_percentiles: Option<Vec<f64>>,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            self.forward(
                "eth_feeHistory",
                serde_json::json!([block_count, newest_block, reward_percentiles]),
            )
            .await
        })
    }

    fn get_balance(
        &self,
        address: Address,
        block: Option<BlockId>,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            if !auth.is_sequencer && address != auth.caller {
                return Ok(raw_zero());
            }
            self.forward("eth_getBalance", serde_json::json!([address, block]))
                .await
        })
    }

    fn get_transaction_count(
        &self,
        address: Address,
        block: Option<BlockId>,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            if !auth.is_sequencer && address != auth.caller {
                return Ok(raw_zero());
            }
            self.forward(
                "eth_getTransactionCount",
                serde_json::json!([address, block]),
            )
            .await
        })
    }

    fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            let result = self
                .forward("eth_getBlockByNumber", serde_json::json!([number, full]))
                .await?;

            if auth.is_sequencer {
                return Ok(result);
            }

            let mut block: serde_json::Value =
                serde_json::from_str(result.get()).map_err(internal)?;

            if block.is_null() {
                return Ok(result);
            }

            redact_block_json(&mut block);
            to_raw(&block)
        })
    }

    fn block_by_hash(
        &self,
        hash: alloy_primitives::B256,
        full: bool,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            let result = self
                .forward("eth_getBlockByHash", serde_json::json!([hash, full]))
                .await?;

            if auth.is_sequencer {
                return Ok(result);
            }

            let mut block: serde_json::Value =
                serde_json::from_str(result.get()).map_err(internal)?;

            if block.is_null() {
                return Ok(result);
            }

            redact_block_json(&mut block);
            to_raw(&block)
        })
    }

    fn transaction_by_hash(&self, hash: alloy_primitives::B256, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let result = self
                .forward("eth_getTransactionByHash", serde_json::json!([hash]))
                .await?;

            let tx: serde_json::Value = serde_json::from_str(result.get()).map_err(internal)?;

            if tx.is_null() {
                return Ok(result);
            }

            if !auth.is_sequencer && json_from(&tx) != Some(auth.caller) {
                return Ok(raw_null());
            }

            Ok(result)
        })
    }

    fn transaction_receipt(&self, hash: alloy_primitives::B256, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let result = self
                .forward("eth_getTransactionReceipt", serde_json::json!([hash]))
                .await?;

            let receipt: serde_json::Value =
                serde_json::from_str(result.get()).map_err(internal)?;

            if receipt.is_null() {
                return Ok(result);
            }

            if !auth.is_sequencer && json_from(&receipt) != Some(auth.caller) {
                return Ok(raw_null());
            }

            Ok(result)
        })
    }

    fn call(
        &self,
        mut request: TempoTransactionRequest,
        block: Option<BlockId>,
        state_override: Option<StateOverride>,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            if !auth.is_sequencer && state_override.is_some() {
                return Err(JsonRpcError::invalid_params("state overrides not allowed"));
            }

            if !auth.is_sequencer {
                policy::enforce_from(&mut request, &auth)?;
            }

            self.forward(
                "eth_call",
                serde_json::json!([request, block, state_override]),
            )
            .await
        })
    }

    fn estimate_gas(
        &self,
        mut request: TempoTransactionRequest,
        block: Option<BlockId>,
        state_override: Option<StateOverride>,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            if !auth.is_sequencer && state_override.is_some() {
                return Err(JsonRpcError::invalid_params("state overrides not allowed"));
            }

            if !auth.is_sequencer {
                policy::enforce_from(&mut request, &auth)?;
            }

            self.forward(
                "eth_estimateGas",
                serde_json::json!([request, block, state_override]),
            )
            .await
        })
    }

    fn send_raw_transaction(&self, data: Bytes, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            if !auth.is_sequencer {
                policy::verify_raw_tx_sender(&data, &auth)?;
            }

            self.forward("eth_sendRawTransaction", serde_json::json!([data]))
                .await
        })
    }

    fn send_raw_transaction_sync(&self, data: Bytes, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            if !auth.is_sequencer {
                policy::verify_raw_tx_sender(&data, &auth)?;
            }

            self.forward("eth_sendRawTransactionSync", serde_json::json!([data]))
                .await
        })
    }

    fn fill_transaction(
        &self,
        mut request: TempoTransactionRequest,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            if !auth.is_sequencer {
                policy::enforce_from(&mut request, &auth)?;
            }

            self.forward("eth_fillTransaction", serde_json::json!([request]))
                .await
        })
    }

    fn get_logs(&self, mut filter: Filter, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            if auth.is_sequencer {
                return self
                    .forward("eth_getLogs", serde_json::json!([filter]))
                    .await;
            }

            filter::scope_filter(&mut filter);
            let result = self
                .forward("eth_getLogs", serde_json::json!([filter]))
                .await?;
            let logs: Vec<Log> = serde_json::from_str(result.get()).map_err(internal)?;
            let filtered = filter::filter_logs(logs, &auth.caller);
            to_raw(&filtered)
        })
    }

    fn new_filter(&self, mut filter: Filter, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            if !auth.is_sequencer {
                filter::scope_filter(&mut filter);
            }
            let result = self
                .forward("eth_newFilter", serde_json::json!([filter]))
                .await?;
            let id: FilterId = serde_json::from_str(result.get()).map_err(internal)?;
            self.filter_owners.lock().await.insert(id, auth.caller);
            Ok(result)
        })
    }

    fn get_filter_logs(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            self.ensure_filter_owner(&id, &auth).await?;

            let result = self
                .forward("eth_getFilterLogs", serde_json::json!([id]))
                .await?;

            if auth.is_sequencer {
                return Ok(result);
            }

            let logs: Vec<Log> = serde_json::from_str(result.get()).map_err(internal)?;
            let filtered = filter::filter_logs(logs, &auth.caller);
            to_raw(&filtered)
        })
    }

    fn get_filter_changes(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            self.ensure_filter_owner(&id, &auth).await?;

            let result = self
                .forward("eth_getFilterChanges", serde_json::json!([id]))
                .await?;

            if auth.is_sequencer {
                return Ok(result);
            }

            // Try to parse as logs for filtering. If the result is block hashes
            // (from a block filter) or empty, the parse will fail and we pass through.
            if let Ok(logs) = serde_json::from_str::<Vec<Log>>(result.get()) {
                let filtered = filter::filter_logs(logs, &auth.caller);
                return to_raw(&filtered);
            }

            Ok(result)
        })
    }

    fn new_block_filter(&self, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let result = self
                .forward("eth_newBlockFilter", serde_json::json!([]))
                .await?;
            let id: FilterId = serde_json::from_str(result.get()).map_err(internal)?;
            self.filter_owners.lock().await.insert(id, auth.caller);
            Ok(result)
        })
    }

    fn uninstall_filter(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            self.ensure_filter_owner(&id, &auth).await?;

            let result = self
                .forward("eth_uninstallFilter", serde_json::json!([id]))
                .await?;

            let removed: bool = serde_json::from_str(result.get()).unwrap_or(false);
            if removed {
                self.filter_owners.lock().await.remove(&id);
            }

            Ok(result)
        })
    }
}
