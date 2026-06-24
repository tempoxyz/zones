//! HTTP proxy implementation of [`ZoneRpcApi`].
//!
//! [`ProxyZoneRpc`] forwards JSON-RPC requests to an upstream zone node and
//! applies privacy redactions on the responses. This allows the private RPC
//! service to run as a standalone process without linking against reth.

use std::{collections::HashMap, sync::Arc};

use alloy_network::ReceiptResponse;
use alloy_primitives::{Address, B256, Bytes, hex};
use alloy_rpc_types_eth::{BlockId, BlockNumberOrTag, Filter, FilterId, Log, state::StateOverride};
use alloy_sol_types::SolCall;
use eyre::WrapErr;
use serde::Deserialize;
use serde_json::value::RawValue;
use tempo_alloy::rpc::{TempoTransactionReceipt, TempoTransactionRequest};
use tempo_contracts::precompiles::{
    ACCOUNT_KEYCHAIN_ADDRESS,
    account_keychain::IAccountKeychain::{self, KeyInfo, getKeyCall},
};
use tokio::sync::Mutex;

use crate::{
    auth::AuthContext,
    filter,
    handlers::ZoneRpcApi,
    policy,
    types::{BoxEyreFut, BoxFut, JsonRpcError, internal, raw_null, raw_zero, to_raw},
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
    /// Maps filter IDs to the authenticated account and filter type that created them.
    filter_owners: Arc<Mutex<HashMap<FilterId, TrackedFilter>>>,
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
    ) -> Result<TrackedFilterKind, JsonRpcError> {
        let owners = self.filter_owners.lock().await;
        match owners.get(id) {
            Some(filter) if filter.owner == auth.caller => Ok(filter.kind),
            _ => Err(JsonRpcError::invalid_params("filter not found")),
        }
    }
}

/// Metadata for filters created through the private RPC.
///
/// The upstream filter ID alone does not tell us whether
/// `eth_getFilterChanges` should return logs or block hashes. Keeping the
/// locally-created filter kind lets the proxy validate the upstream result shape
/// before returning anything to the caller.
#[derive(Clone, Copy)]
struct TrackedFilter {
    /// Authenticated account that created the filter.
    owner: Address,
    /// Expected shape for subsequent `eth_getFilterChanges` responses.
    kind: TrackedFilterKind,
}

/// Filter types exposed by the private RPC.
#[derive(Clone, Copy)]
enum TrackedFilterKind {
    /// Log filter created by `eth_newFilter`; changes are `Vec<Log>` and must be filtered.
    Log,
    /// Block filter created by `eth_newBlockFilter`; changes are `Vec<B256>` block hashes.
    Block,
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
    fn get_keychain_key(&self, account: Address, key_id: Address) -> BoxEyreFut<'_, KeyInfo> {
        Box::pin(async move {
            let call_data = getKeyCall {
                account,
                keyId: key_id,
            }
            .abi_encode();

            let result = self
                .forward(
                    "eth_call",
                    serde_json::json!([
                        {
                            "to": format!("{ACCOUNT_KEYCHAIN_ADDRESS:#x}"),
                            "input": format!("0x{}", hex::encode(call_data)),
                        },
                        "latest"
                    ]),
                )
                .await
                .map_err(|err| eyre::eyre!("AccountKeychain.getKey eth_call failed: {err}"))?;
            let output: Bytes = serde_json::from_str(result.get())
                .wrap_err("AccountKeychain.getKey returned invalid bytes")?;

            IAccountKeychain::getKeyCall::abi_decode_returns(output.as_ref()).map_err(Into::into)
        })
    }

    fn block_number(&self) -> BoxFut<'_> {
        Box::pin(async move { self.forward("eth_blockNumber", serde_json::json!([])).await })
    }

    fn chain_id(&self) -> BoxFut<'_> {
        Box::pin(async move { self.forward("eth_chainId", serde_json::json!([])).await })
    }

    fn net_version(&self) -> BoxFut<'_> {
        Box::pin(async move { self.forward("net_version", serde_json::json!([])).await })
    }

    fn syncing(&self) -> BoxFut<'_> {
        Box::pin(async move { self.forward("eth_syncing", serde_json::json!([])).await })
    }

    fn coinbase(&self) -> BoxFut<'_> {
        Box::pin(async move { self.forward("eth_coinbase", serde_json::json!([])).await })
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
            if address != auth.caller {
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
            if address != auth.caller {
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
        _auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            let result = self
                .forward("eth_getBlockByNumber", serde_json::json!([number, full]))
                .await?;

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
        _auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            let result = self
                .forward("eth_getBlockByHash", serde_json::json!([hash, full]))
                .await?;

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

            if json_from(&tx) != Some(auth.caller) {
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

            let receipt: Option<TempoTransactionReceipt> =
                serde_json::from_str(result.get()).map_err(internal)?;

            let Some(receipt) = receipt else {
                return Ok(result);
            };

            if receipt.from() != auth.caller {
                return Ok(raw_null());
            }

            to_raw(&filter::filter_receipt_logs(receipt))
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
            if state_override.is_some() {
                return Err(JsonRpcError::invalid_params("state overrides not allowed"));
            }

            policy::enforce_from(&mut request, &auth)?;
            policy::enforce_no_contract_creation(&request)?;

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
            if state_override.is_some() {
                return Err(JsonRpcError::invalid_params("state overrides not allowed"));
            }

            policy::enforce_from(&mut request, &auth)?;
            policy::enforce_no_contract_creation(&request)?;

            self.forward(
                "eth_estimateGas",
                serde_json::json!([request, block, state_override]),
            )
            .await
        })
    }

    fn send_raw_transaction(&self, data: Bytes, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            policy::verify_raw_tx_sender(&data, &auth)?;

            self.forward("eth_sendRawTransaction", serde_json::json!([data]))
                .await
        })
    }

    fn send_raw_transaction_sync(&self, data: Bytes, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            policy::verify_raw_tx_sender(&data, &auth)?;

            let result = self
                .forward("eth_sendRawTransactionSync", serde_json::json!([data]))
                .await?;

            let receipt: TempoTransactionReceipt =
                serde_json::from_str(result.get()).map_err(internal)?;
            to_raw(&filter::filter_receipt_logs(receipt))
        })
    }

    fn fill_transaction(
        &self,
        mut request: TempoTransactionRequest,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            policy::enforce_from(&mut request, &auth)?;

            policy::enforce_no_contract_creation(&request)?;

            self.forward("eth_fillTransaction", serde_json::json!([request]))
                .await
        })
    }

    fn get_logs(&self, mut filter: Filter, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
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
            filter::scope_filter(&mut filter);
            let result = self
                .forward("eth_newFilter", serde_json::json!([filter]))
                .await?;
            let id: FilterId = serde_json::from_str(result.get()).map_err(internal)?;
            self.filter_owners.lock().await.insert(
                id,
                TrackedFilter {
                    owner: auth.caller,
                    kind: TrackedFilterKind::Log,
                },
            );
            Ok(result)
        })
    }

    fn get_filter_logs(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            self.ensure_filter_owner(&id, &auth).await?;

            let result = self
                .forward("eth_getFilterLogs", serde_json::json!([id]))
                .await?;

            let logs: Vec<Log> = serde_json::from_str(result.get()).map_err(internal)?;
            let filtered = filter::filter_logs(logs, &auth.caller);
            to_raw(&filtered)
        })
    }

    fn get_filter_changes(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let kind = self.ensure_filter_owner(&id, &auth).await?;

            let result = self
                .forward("eth_getFilterChanges", serde_json::json!([id]))
                .await?;

            match kind {
                TrackedFilterKind::Log => {
                    let logs: Vec<Log> = serde_json::from_str(result.get()).map_err(|_| {
                        internal("unexpected eth_getFilterChanges log filter result shape")
                    })?;
                    let filtered = filter::filter_logs(logs, &auth.caller);
                    to_raw(&filtered)
                }
                TrackedFilterKind::Block => {
                    serde_json::from_str::<Vec<B256>>(result.get()).map_err(|_| {
                        internal("unexpected eth_getFilterChanges block filter result shape")
                    })?;
                    Ok(result)
                }
            }
        })
    }

    fn new_block_filter(&self, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let result = self
                .forward("eth_newBlockFilter", serde_json::json!([]))
                .await?;
            let id: FilterId = serde_json::from_str(result.get()).map_err(internal)?;
            self.filter_owners.lock().await.insert(
                id,
                TrackedFilter {
                    owner: auth.caller,
                    kind: TrackedFilterKind::Block,
                },
            );
            Ok(result)
        })
    }

    fn uninstall_filter(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            self.ensure_filter_owner(&id, &auth).await?;

            let result = self
                .forward("eth_uninstallFilter", serde_json::json!([id]))
                .await?;

            self.filter_owners.lock().await.remove(&id);

            Ok(result)
        })
    }

    fn zone_get_authorization_token_info(&self, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            to_raw(&serde_json::json!({
                "account": auth.caller,
                "expiresAt": alloy_primitives::U64::from(auth.expires_at),
            }))
        })
    }

    fn zone_get_zone_info(&self, _auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            Err(JsonRpcError::internal(
                "zone-specific methods are not supported by the proxy backend",
            ))
        })
    }

    fn zone_get_deposit_status(&self, _tempo_block_number: u64, _auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            Err(JsonRpcError::internal(
                "zone-specific methods are not supported by the proxy backend",
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::ReceiptWithBloom;
    use alloy_primitives::{B256, Bytes as PrimitiveBytes, LogData, TxHash, address};
    use alloy_rpc_types_eth::TransactionReceipt;
    use axum::{Json, Router, routing::post};
    use tempo_primitives::{TempoReceipt, TempoTxType};

    fn make_log(emitter: Address, topics: Vec<B256>) -> Log {
        Log {
            inner: alloy_primitives::Log {
                address: emitter,
                data: LogData::new_unchecked(topics, PrimitiveBytes::new()),
            },
            block_hash: None,
            block_number: None,
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        }
    }

    fn caller_word(addr: &Address) -> B256 {
        B256::left_padding_from(addr.as_slice())
    }

    fn make_receipt(from: Address, logs: Vec<Log>) -> TempoTransactionReceipt {
        let receipt = TempoReceipt {
            tx_type: TempoTxType::Legacy,
            success: true,
            cumulative_gas_used: 21_000,
            logs,
        };

        TempoTransactionReceipt {
            inner: TransactionReceipt {
                inner: ReceiptWithBloom::from(receipt),
                transaction_hash: TxHash::with_last_byte(1),
                transaction_index: Some(0),
                block_hash: Some(B256::with_last_byte(2)),
                block_number: Some(1),
                gas_used: 21_000,
                effective_gas_price: 1,
                blob_gas_used: None,
                blob_gas_price: None,
                from,
                to: Some(Address::ZERO),
                contract_address: None,
            },
            fee_token: None,
            fee_payer: from,
        }
    }

    async fn spawn_upstream(result: serde_json::Value) -> String {
        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": result,
        });

        let app = Router::new().route(
            "/",
            post(move || {
                let response = response.clone();
                async move { Json(response) }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream test server");
        let addr = listener.local_addr().expect("read upstream addr");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve upstream test server");
        });

        format!("http://{addr}")
    }

    #[tokio::test]
    async fn transaction_receipt_filters_logs() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let other = address!("0x0000000000000000000000000000000000000002");
        let third = address!("0x0000000000000000000000000000000000000003");

        let visible = make_log(
            Address::ZERO,
            vec![
                filter::TRANSFER_TOPIC,
                caller_word(&caller),
                caller_word(&other),
            ],
        );
        let hidden = make_log(
            Address::ZERO,
            vec![
                filter::TRANSFER_TOPIC,
                caller_word(&other),
                caller_word(&third),
            ],
        );
        let upstream = make_receipt(caller, vec![visible.clone(), hidden]);
        let proxy =
            ProxyZoneRpc::new(spawn_upstream(serde_json::to_value(&upstream).unwrap()).await);

        let raw = proxy
            .transaction_receipt(
                TxHash::with_last_byte(1),
                AuthContext {
                    caller,
                    expires_at: u64::MAX,
                    keychain_key_id: None,
                },
            )
            .await
            .expect("proxy should return receipt");

        let receipt: TempoTransactionReceipt =
            serde_json::from_str(raw.get()).expect("deserialize filtered receipt");
        assert_eq!(receipt.inner.logs(), std::slice::from_ref(&visible));
        assert_eq!(
            receipt.inner.inner.logs_bloom,
            alloy_primitives::logs_bloom(receipt.inner.logs().iter().map(|log| log.as_ref())),
        );
        assert_ne!(
            receipt.inner.inner.logs_bloom,
            upstream.inner.inner.logs_bloom
        );
    }

    #[tokio::test]
    async fn filter_changes_filters_log_results() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let other = address!("0x0000000000000000000000000000000000000002");
        let third = address!("0x0000000000000000000000000000000000000003");

        let visible = make_log(
            Address::ZERO,
            vec![
                filter::TRANSFER_TOPIC,
                caller_word(&caller),
                caller_word(&other),
            ],
        );
        let hidden = make_log(
            Address::ZERO,
            vec![
                filter::TRANSFER_TOPIC,
                caller_word(&other),
                caller_word(&third),
            ],
        );
        let result = serde_json::to_value(vec![visible.clone(), hidden]).unwrap();
        let proxy = ProxyZoneRpc::new(spawn_upstream(result).await);

        let id = FilterId::Num(1);
        proxy.filter_owners.lock().await.insert(
            id.clone(),
            TrackedFilter {
                owner: caller,
                kind: TrackedFilterKind::Log,
            },
        );

        let raw = proxy
            .get_filter_changes(
                id,
                AuthContext {
                    caller,
                    expires_at: u64::MAX,
                    keychain_key_id: None,
                },
            )
            .await
            .expect("proxy should return filtered changes");

        let logs: Vec<Log> = serde_json::from_str(raw.get()).expect("deserialize filtered logs");
        assert_eq!(logs, vec![visible]);
    }

    #[tokio::test]
    async fn filter_changes_passes_through_block_hashes() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let hashes = vec![B256::with_last_byte(7), B256::with_last_byte(8)];
        let result = serde_json::to_value(&hashes).unwrap();
        let proxy = ProxyZoneRpc::new(spawn_upstream(result).await);

        let id = FilterId::Num(2);
        proxy.filter_owners.lock().await.insert(
            id.clone(),
            TrackedFilter {
                owner: caller,
                kind: TrackedFilterKind::Block,
            },
        );

        let raw = proxy
            .get_filter_changes(
                id,
                AuthContext {
                    caller,
                    expires_at: u64::MAX,
                    keychain_key_id: None,
                },
            )
            .await
            .expect("block-hash changes should pass through");

        let out: Vec<B256> = serde_json::from_str(raw.get()).expect("deserialize block hashes");
        assert_eq!(out, hashes);
    }

    #[tokio::test]
    async fn filter_changes_fails_closed_on_pending_transaction_results() {
        let caller = address!("0x0000000000000000000000000000000000000001");
        let recipient = address!("0x0000000000000000000000000000000000000002");
        let result = serde_json::json!([
            {
                "hash": format!("{:#x}", TxHash::with_last_byte(9)),
                "nonce": "0x0",
                "blockHash": null,
                "blockNumber": null,
                "transactionIndex": null,
                "from": format!("{caller:#x}"),
                "to": format!("{recipient:#x}"),
                "value": "0x0",
                "gas": "0x5208",
                "gasPrice": "0x1",
                "input": "0x"
            }
        ]);
        let proxy = ProxyZoneRpc::new(spawn_upstream(result).await);

        let id = FilterId::Num(3);
        proxy.filter_owners.lock().await.insert(
            id.clone(),
            TrackedFilter {
                owner: caller,
                kind: TrackedFilterKind::Block,
            },
        );

        let err = proxy
            .get_filter_changes(
                id,
                AuthContext {
                    caller,
                    expires_at: u64::MAX,
                    keychain_key_id: None,
                },
            )
            .await
            .expect_err("pending transaction changes must fail closed");

        assert_eq!(err.code, JsonRpcError::internal("").code);
        assert!(err.message.contains("block filter result shape"));
    }
}
