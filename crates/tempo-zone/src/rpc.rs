//! [`ZoneRpcApi`] implementation backed by reth's EthApi (in-process reth-backed).
//!
//! Re-exports the standalone `zone-rpc` crate so everything is accessible
//! via `zone::rpc::*`.

pub use zone_rpc::*;

use std::{collections::HashMap, sync::Arc};

use alloy_network::{ReceiptResponse, TransactionResponse};
use alloy_primitives::{Address, B256, Bloom, Bytes, U256};
use alloy_rpc_types_eth::{
    Block, BlockId, BlockNumberOrTag, BlockTransactions, Filter, FilterChanges, FilterId,
    state::{EvmOverrides, StateOverride},
};
use futures::StreamExt;
use reth_rpc::EthFilter;
use reth_rpc_builder::EthHandlers;
use reth_rpc_eth_api::{
    EthApiTypes, EthFilterApiServer,
    helpers::{EthApiSpec, EthBlocks, EthCall, EthFees, EthState, EthTransactions, FullEthApi},
};
use serde_json::value::RawValue;
use tempo_alloy::{
    TempoNetwork,
    rpc::{TempoHeaderResponse, TempoTransactionRequest},
};
use tempo_primitives::TempoTxEnvelope;
use tokio::sync::{Mutex, mpsc};

use zone_rpc::{
    auth::AuthContext,
    subscription::{BoxWsSubscriptionFut, WsSubscription, WsSubscriptionStream},
    types::{BoxFut, JsonRpcError, internal, raw_null, raw_zero, to_raw},
};

type RpcBlock = Block<alloy_rpc_types_eth::Transaction<TempoTxEnvelope>, TempoHeaderResponse>;

/// [`ZoneRpcApi`] implementation backed by reth's [`EthHandlers`].
///
/// This is the privacy enforcement layer for the zone's JSON-RPC surface.
/// Only methods explicitly routed through [`ZoneRpcApi`] are reachable —
/// everything else is rejected by the dispatcher's [`classify_method`]
/// whitelist, so this struct effectively acts as an **enforced allowlist**
/// of Ethereum JSON-RPC endpoints.
///
/// For every allowed endpoint it applies typed privacy checks *before*
/// serializing to JSON:
///
/// - **Block redaction** — zeroing `logsBloom` and clearing transaction
///   lists for non-sequencer callers.
/// - **Sender-scoped access** — returning `null` for transactions and
///   receipts not owned by the authenticated caller.
/// - **`from`-enforcement** — `eth_call` / `eth_estimateGas` may only
///   simulate from the authenticated account (`-32004` on mismatch,
///   auto-set when omitted); state overrides are rejected for
///   non-sequencers (`-32602`).
/// - **Sender verification** — `eth_sendRawTransaction` checks that the
///   recovered transaction sender matches the authenticated account
///   (`-32003` on mismatch).
///
/// [`classify_method`]: zone_rpc::types::classify_method
pub struct TempoZoneRpc<Api: EthApiTypes> {
    eth: EthHandlers<Api>,
    /// Maps filter IDs to the authenticated account that created them.
    /// Ensures filters can only be accessed by their creator.
    ///
    /// TODO: entries are never cleaned up when reth reaps stale filters internally.
    /// After bumping reth, use its `ActiveFilters` API to periodically sync this
    /// map and remove entries for filters that no longer exist on the reth side.
    filter_owners: Arc<Mutex<HashMap<FilterId, Address>>>,
}

impl<Api: EthApiTypes> TempoZoneRpc<Api> {
    /// Wrap reth's [`EthHandlers`] (api + filter + pubsub).
    pub fn new(eth: EthHandlers<Api>) -> Self {
        Self {
            eth,
            filter_owners: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Returns a reference to the inner [`EthFilter`] handler.
    #[allow(dead_code)]
    pub fn filter(&self) -> &EthFilter<Api> {
        &self.eth.filter
    }

    /// Verify that the filter belongs to the authenticated caller.
    ///
    /// Returns `Ok(())` if the caller owns the filter or is the sequencer.
    /// Returns an error indistinguishable from "filter not found" to avoid
    /// leaking filter existence to non-owners.
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

impl<Api> zone_rpc::ZoneRpcApi for TempoZoneRpc<Api>
where
    Api: FullEthApi + EthApiTypes<NetworkTypes = TempoNetwork> + Send + Sync + 'static,
{
    fn block_number(&self) -> BoxFut<'_> {
        Box::pin(async move {
            let info = EthApiSpec::chain_info(&self.eth.api).map_err(internal)?;
            to_raw(&U256::from(info.best_number))
        })
    }

    fn chain_id(&self) -> BoxFut<'_> {
        Box::pin(async move {
            let chain_id = EthApiSpec::chain_id(&self.eth.api);
            to_raw(&Some(chain_id))
        })
    }

    fn net_version(&self) -> BoxFut<'_> {
        Box::pin(async move {
            let chain_id = EthApiSpec::chain_id(&self.eth.api);
            to_raw(&chain_id.to_string())
        })
    }

    fn gas_price(&self) -> BoxFut<'_> {
        Box::pin(async move {
            let price = EthFees::gas_price(&self.eth.api).await.map_err(internal)?;
            to_raw(&price)
        })
    }

    fn max_priority_fee_per_gas(&self) -> BoxFut<'_> {
        Box::pin(async move {
            let fee = EthFees::suggested_priority_fee(&self.eth.api)
                .await
                .map_err(internal)?;
            to_raw(&fee)
        })
    }

    fn fee_history(
        &self,
        block_count: u64,
        newest_block: BlockNumberOrTag,
        reward_percentiles: Option<Vec<f64>>,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            let history =
                EthFees::fee_history(&self.eth.api, block_count, newest_block, reward_percentiles)
                    .await
                    .map_err(internal)?;
            to_raw(&history)
        })
    }

    fn get_balance(
        &self,
        address: Address,
        block: Option<BlockId>,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            // Silent dummy: non-caller addresses get "0x0" to avoid leaking account existence.
            if !auth.is_sequencer && address != auth.caller {
                return Ok(raw_zero());
            }
            let balance = EthState::balance(&self.eth.api, address, block)
                .await
                .map_err(internal)?;
            to_raw(&balance)
        })
    }

    fn get_transaction_count(
        &self,
        address: Address,
        block: Option<BlockId>,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            // Silent dummy: non-caller addresses get "0x0" to avoid leaking account existence.
            if !auth.is_sequencer && address != auth.caller {
                return Ok(raw_zero());
            }
            let count = EthState::transaction_count(&self.eth.api, address, block)
                .await
                .map_err(internal)?;
            to_raw(&count)
        })
    }

    fn block_by_number(
        &self,
        number: BlockNumberOrTag,
        full: bool,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            let block = EthBlocks::rpc_block(&self.eth.api, number.into(), full)
                .await
                .map_err(internal)?;

            let Some(mut block) = block else {
                return Ok(raw_null());
            };

            if !auth.is_sequencer {
                redact_block(&mut block);
            }

            to_raw(&block)
        })
    }

    fn block_by_hash(&self, hash: B256, full: bool, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let block = EthBlocks::rpc_block(&self.eth.api, hash.into(), full)
                .await
                .map_err(internal)?;

            let Some(mut block) = block else {
                return Ok(raw_null());
            };

            if !auth.is_sequencer {
                redact_block(&mut block);
            }

            to_raw(&block)
        })
    }

    fn transaction_by_hash(&self, hash: B256, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let tx = EthTransactions::transaction_by_hash(&self.eth.api, hash)
                .await
                .map_err(internal)?
                .map(|src| src.into_transaction(self.eth.api.converter()))
                .transpose()
                .map_err(internal)?;

            let Some(tx) = tx else { return Ok(raw_null()) };

            if !auth.is_sequencer && tx.from() != auth.caller {
                return Ok(raw_null());
            }

            to_raw(&tx)
        })
    }

    fn transaction_receipt(&self, hash: B256, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let receipt = EthTransactions::transaction_receipt(&self.eth.api, hash)
                .await
                .map_err(internal)?;

            let Some(mut receipt) = receipt else {
                return Ok(raw_null());
            };

            if !auth.is_sequencer {
                if receipt.from() != auth.caller {
                    return Ok(raw_null());
                }

                receipt = zone_rpc::filter::filter_receipt_logs(receipt);
            }

            to_raw(&receipt)
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
            // Defense-in-depth: handlers.rs also rejects this, but enforce here too.
            if !auth.is_sequencer && state_override.is_some() {
                return Err(JsonRpcError::invalid_params("state overrides not allowed"));
            }

            if !auth.is_sequencer {
                zone_rpc::policy::enforce_from(&mut request, &auth)?;
            }

            let result = EthCall::call(
                &self.eth.api,
                request,
                block,
                EvmOverrides::state(state_override),
            )
            .await
            .map_err(internal)?;
            to_raw(&result)
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
            // Defense-in-depth: handlers.rs also rejects this, but enforce here too.
            if !auth.is_sequencer && state_override.is_some() {
                return Err(JsonRpcError::invalid_params("state overrides not allowed"));
            }

            if !auth.is_sequencer {
                zone_rpc::policy::enforce_from(&mut request, &auth)?;
            }

            let result = EthCall::estimate_gas_at(
                &self.eth.api,
                request,
                block.unwrap_or_default(),
                state_override,
            )
            .await
            .map_err(internal)?;
            to_raw(&result)
        })
    }

    fn send_raw_transaction(&self, data: Bytes, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            if !auth.is_sequencer {
                zone_rpc::policy::verify_raw_tx_sender(&data, &auth)?;
            }

            let hash = EthTransactions::send_raw_transaction(&self.eth.api, data)
                .await
                .map_err(internal)?;
            to_raw(&hash)
        })
    }

    fn send_raw_transaction_sync(&self, data: Bytes, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            if !auth.is_sequencer {
                zone_rpc::policy::verify_raw_tx_sender(&data, &auth)?;
            }

            let mut receipt = EthTransactions::send_raw_transaction_sync(&self.eth.api, data)
                .await
                .map_err(internal)?;

            if !auth.is_sequencer {
                receipt = zone_rpc::filter::filter_receipt_logs(receipt);
            }

            to_raw(&receipt)
        })
    }

    fn fill_transaction(
        &self,
        mut request: TempoTransactionRequest,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            if !auth.is_sequencer {
                zone_rpc::policy::enforce_from(&mut request, &auth)?;
            }

            let result = EthTransactions::fill_transaction(&self.eth.api, request)
                .await
                .map_err(internal)?;
            to_raw(&result)
        })
    }

    fn get_logs(&self, mut filter: Filter, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            if auth.is_sequencer {
                let logs = EthFilterApiServer::logs(&self.eth.filter, filter)
                    .await
                    .map_err(internal)?;
                return to_raw(&logs);
            }

            zone_rpc::filter::scope_filter(&mut filter);
            let logs = EthFilterApiServer::logs(&self.eth.filter, filter)
                .await
                .map_err(internal)?;
            let filtered = zone_rpc::filter::filter_logs(logs, &auth.caller);
            to_raw(&filtered)
        })
    }

    fn new_filter(&self, mut filter: Filter, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            if !auth.is_sequencer {
                zone_rpc::filter::scope_filter(&mut filter);
            }
            let id = EthFilterApiServer::new_filter(&self.eth.filter, filter)
                .await
                .map_err(internal)?;
            self.filter_owners
                .lock()
                .await
                .insert(id.clone(), auth.caller);
            to_raw(&id)
        })
    }

    fn get_filter_logs(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            self.ensure_filter_owner(&id, &auth).await?;

            let logs = self.eth.filter.filter_logs(id).await.map_err(internal)?;

            if auth.is_sequencer {
                return to_raw(&logs);
            }

            let filtered = zone_rpc::filter::filter_logs(logs, &auth.caller);
            to_raw(&filtered)
        })
    }

    fn get_filter_changes(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            self.ensure_filter_owner(&id, &auth).await?;

            let changes = self.eth.filter.filter_changes(id).await.map_err(internal)?;

            if auth.is_sequencer {
                return to_raw(&changes);
            }

            match changes {
                FilterChanges::Logs(logs) => {
                    let filtered = zone_rpc::filter::filter_logs(logs, &auth.caller);
                    to_raw(&FilterChanges::<
                        alloy_rpc_types_eth::Transaction<TempoTxEnvelope>,
                    >::Logs(filtered))
                }
                FilterChanges::Hashes(hashes) => to_raw(&FilterChanges::<
                    alloy_rpc_types_eth::Transaction<TempoTxEnvelope>,
                >::Hashes(hashes)),
                // Pending transaction filters are disabled — return empty if one somehow exists
                FilterChanges::Transactions(_) => to_raw(
                    &FilterChanges::<alloy_rpc_types_eth::Transaction<TempoTxEnvelope>>::Empty,
                ),
                FilterChanges::Empty => to_raw(
                    &FilterChanges::<alloy_rpc_types_eth::Transaction<TempoTxEnvelope>>::Empty,
                ),
            }
        })
    }

    fn new_block_filter(&self, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let id = EthFilterApiServer::new_block_filter(&self.eth.filter)
                .await
                .map_err(internal)?;
            self.filter_owners
                .lock()
                .await
                .insert(id.clone(), auth.caller);
            to_raw(&id)
        })
    }

    fn uninstall_filter(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            self.ensure_filter_owner(&id, &auth).await?;

            let result = EthFilterApiServer::uninstall_filter(&self.eth.filter, id.clone())
                .await
                .map_err(internal)?;

            if result {
                self.filter_owners.lock().await.remove(&id);
            }

            to_raw(&result)
        })
    }

    fn ws_subscribe_new_heads(&self, auth: AuthContext) -> BoxWsSubscriptionFut<'_> {
        Box::pin(async move {
            let pubsub = self.eth.pubsub.clone();
            let redact_logs_bloom = !auth.is_sequencer;
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                let mut stream = Box::pin(pubsub.new_headers_stream());
                while let Some(header) = stream.next().await {
                    if tx
                        .send(serialize_ws_header(&header, redact_logs_bloom))
                        .is_err()
                    {
                        break;
                    }
                }
            });
            Ok(WsSubscription::new(ws_stream_from_receiver(rx)))
        })
    }

    fn ws_subscribe_logs(&self, mut filter: Filter, auth: AuthContext) -> BoxWsSubscriptionFut<'_> {
        Box::pin(async move {
            let pubsub = self.eth.pubsub.clone();

            if auth.is_sequencer {
                let (tx, rx) = mpsc::unbounded_channel();
                tokio::spawn(async move {
                    let mut stream = Box::pin(pubsub.log_stream(filter));
                    while let Some(log) = stream.next().await {
                        if tx.send(to_raw(&log)).is_err() {
                            break;
                        }
                    }
                });
                return Ok(WsSubscription::new(ws_stream_from_receiver(rx)));
            }

            zone_rpc::filter::scope_filter(&mut filter);
            let caller = auth.caller;
            let (tx, rx) = mpsc::unbounded_channel();
            tokio::spawn(async move {
                let mut stream = Box::pin(pubsub.log_stream(filter));
                while let Some(log) = stream.next().await {
                    let allowed = log
                        .topic0()
                        .is_some_and(|topic| zone_rpc::filter::WHITELISTED_TOPICS.contains(topic))
                        && zone_rpc::filter::is_caller_eligible(&log, &caller);
                    if !allowed {
                        continue;
                    }

                    if tx.send(to_raw(&log)).is_err() {
                        break;
                    }
                }
            });

            Ok(WsSubscription::new(ws_stream_from_receiver(rx)))
        })
    }
}

/// Strip privacy-sensitive fields from a block for non-sequencer callers.
fn redact_block(block: &mut RpcBlock) {
    // header.inner = alloy Header, .inner = Sealed wrapper, .inner = TempoHeader (contains logs_bloom)
    block.header.inner.inner.inner.logs_bloom = Bloom::ZERO;
    block.transactions = BlockTransactions::Hashes(Vec::new());
}

/// Serialize a `newHeads` item, optionally redacting `logsBloom`.
fn serialize_ws_header<T: serde::Serialize>(
    header: &T,
    redact_logs_bloom: bool,
) -> Result<Box<RawValue>, JsonRpcError> {
    if !redact_logs_bloom {
        return to_raw(header);
    }

    let mut value = serde_json::to_value(header).map_err(internal)?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "logsBloom".to_string(),
            serde_json::Value::String(format!("0x{}", "0".repeat(512))),
        );
    }
    to_raw(&value)
}

fn ws_stream_from_receiver(
    rx: mpsc::UnboundedReceiver<Result<Box<RawValue>, JsonRpcError>>,
) -> WsSubscriptionStream {
    Box::pin(futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
}
