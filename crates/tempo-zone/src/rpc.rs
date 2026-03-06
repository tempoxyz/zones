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
use reth_rpc::EthFilter;
use reth_rpc_builder::EthHandlers;
use reth_rpc_eth_api::{
    EthApiTypes, EthFilterApiServer,
    helpers::{EthApiSpec, EthBlocks, EthCall, EthFees, EthState, EthTransactions, FullEthApi},
};
use tempo_alloy::{
    TempoNetwork,
    rpc::{TempoHeaderResponse, TempoTransactionRequest},
};
use tempo_primitives::TempoTxEnvelope;
use tokio::sync::Mutex;

use zone_rpc::{
    auth::AuthContext,
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
    proof_generator: Arc<dyn crate::proof::BatchProofGenerator>,
    witness_store: crate::witness::SharedWitnessStore,
    state_provider_factory: Arc<dyn reth_provider::StateProviderFactory>,
}

impl<Api: EthApiTypes> TempoZoneRpc<Api> {
    /// Wrap reth's [`EthHandlers`] (api + filter + pubsub).
    pub fn new(
        eth: EthHandlers<Api>,
        proof_generator: Arc<dyn crate::proof::BatchProofGenerator>,
        witness_store: crate::witness::SharedWitnessStore,
        state_provider_factory: Arc<dyn reth_provider::StateProviderFactory>,
    ) -> Self {
        Self {
            eth,
            filter_owners: Arc::new(Mutex::new(HashMap::new())),
            proof_generator,
            witness_store,
            state_provider_factory,
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

            let Some(receipt) = receipt else {
                return Ok(raw_null());
            };

            if !auth.is_sequencer && receipt.from() != auth.caller {
                return Ok(raw_null());
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

            let receipt = EthTransactions::send_raw_transaction_sync(&self.eth.api, data)
                .await
                .map_err(internal)?;
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

    fn get_batch_witness(&self) -> BoxFut<'_> {
        Box::pin(async move {
            use crate::abi::{TEMPO_PACKED_SLOT, TEMPO_STATE_ADDRESS, ZONE_OUTBOX_ADDRESS};
            use reth_storage_api::StateProvider;

            // 1. Read the available block range from the store.
            let (from, to, prev_block_hash) = {
                let store = self.witness_store.lock().expect("witness store poisoned");
                let (Some(from), Some(to)) = (store.first_block(), store.last_block()) else {
                    return Err(JsonRpcError::internal("no unsubmitted batch available"));
                };
                let first = store
                    .get_range(from, from)
                    .map_err(|b| JsonRpcError::internal(format!("missing block {b}")))?;
                let prev_block_hash = first[0].1.parent_block_hash;
                (from, to, prev_block_hash)
            };

            // 2. Read tempo_block_number and withdrawal_batch_index from zone state.
            let (tempo_block_number, expected_withdrawal_batch_index) = {
                let sp = self
                    .state_provider_factory
                    .latest()
                    .map_err(|e| {
                        JsonRpcError::internal(format!(
                            "failed to open latest state provider: {e}"
                        ))
                    })?;

                let packed = sp
                    .storage(TEMPO_STATE_ADDRESS, TEMPO_PACKED_SLOT)
                    .map_err(|e| {
                        JsonRpcError::internal(format!(
                            "failed to read tempo block number: {e}"
                        ))
                    })?
                    .unwrap_or_default();
                let tempo_block_number = (packed & U256::from(u64::MAX)).to::<u64>();

                let slot = B256::from(
                    (zone_prover::execute::storage::ZONE_OUTBOX_LAST_BATCH_BASE_SLOT
                        + U256::from(1))
                    .to_be_bytes(),
                );
                let value = sp
                    .storage(ZONE_OUTBOX_ADDRESS, slot)
                    .map_err(|e| {
                        JsonRpcError::internal(format!(
                            "failed to read withdrawal batch index: {e}"
                        ))
                    })?
                    .unwrap_or_default();
                let withdrawal_batch_index = value.to::<u64>();

                (tempo_block_number, withdrawal_batch_index + 1)
            };

            tracing::info!(
                target: "zone::rpc",
                from, to, tempo_block_number, expected_withdrawal_batch_index,
                "Building batch witness for RPC"
            );

            let witness = self
                .proof_generator
                .generate_batch_witness(
                    from,
                    to,
                    tempo_block_number,
                    prev_block_hash,
                    expected_withdrawal_batch_index,
                )
                .await
                .map_err(|e| JsonRpcError::internal(format!("failed to build witness: {e}")))?;

            to_raw(&witness)
        })
    }
}

/// Strip privacy-sensitive fields from a block for non-sequencer callers.
fn redact_block(block: &mut RpcBlock) {
    // header.inner = alloy Header, .inner = Sealed wrapper, .inner = TempoHeader (contains logs_bloom)
    block.header.inner.inner.inner.logs_bloom = Bloom::ZERO;
    block.transactions = BlockTransactions::Hashes(Vec::new());
}
