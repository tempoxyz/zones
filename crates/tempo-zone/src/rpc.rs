//! [`ZoneRpcApi`] implementation backed by reth's EthApi (in-process reth-backed).
//!
//! Re-exports the standalone `zone-rpc` crate so everything is accessible
//! via `zone::rpc::*`.

pub use zone_rpc::*;

use std::{collections::HashMap, sync::Arc};

use alloy_network::{ReceiptResponse, TransactionResponse};
use alloy_primitives::{Address, B256, Bloom, Bytes, U64, U256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_rpc_types_eth::{
    Block, BlockId, BlockNumberOrTag, BlockTransactions, Filter, FilterChanges, FilterId,
    TransactionRequest,
    state::{EvmOverrides, StateOverride},
};
use alloy_sol_types::{SolCall, SolEvent, SolEventInterface};
use eyre::WrapErr;
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
use tempo_contracts::precompiles::{
    ACCOUNT_KEYCHAIN_ADDRESS,
    account_keychain::IAccountKeychain::{self, KeyInfo, getKeyCall},
};
use tempo_primitives::TempoTxEnvelope;
use tokio::sync::Mutex;

use crate::abi::{
    TEMPO_STATE_ADDRESS, ZONE_INBOX_ADDRESS, ZONE_TOKEN_ADDRESS, ZoneInbox, ZonePortal,
};
use zone_rpc::{
    auth::AuthContext,
    types::{
        AuthorizationTokenInfoResponse, BoxEyreFut, BoxFut, DepositKind, DepositState,
        DepositStatusEntry, DepositStatusResponse, JsonRpcError, ZoneInfoResponse, internal,
        raw_null, raw_zero, to_raw,
    },
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
    config: zone_rpc::PrivateRpcConfig,
    l1_provider: DynProvider<TempoNetwork>,
    zone_provider: DynProvider<TempoNetwork>,
    tempo_state:
        crate::abi::TempoState::TempoStateInstance<DynProvider<TempoNetwork>, TempoNetwork>,
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
    pub async fn new(
        eth: EthHandlers<Api>,
        config: zone_rpc::PrivateRpcConfig,
    ) -> eyre::Result<Self> {
        let l1_rpc_url = config.l1_rpc_url.clone();
        let zone_rpc_url = config.zone_rpc_url.clone();
        let l1_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&l1_rpc_url)
            .await
            .wrap_err("failed to connect private RPC L1 provider")?
            .erased();
        let zone_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&zone_rpc_url)
            .await
            .wrap_err("failed to connect private RPC zone provider")?
            .erased();
        let tempo_state = crate::abi::TempoState::new(TEMPO_STATE_ADDRESS, zone_provider.clone());
        Ok(Self {
            eth,
            config,
            l1_provider,
            zone_provider,
            tempo_state,
            filter_owners: Arc::new(Mutex::new(HashMap::new())),
        })
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

    async fn portal_deposits_for_block(
        &self,
        tempo_block_number: u64,
    ) -> Result<Vec<PortalDepositRecord>, JsonRpcError> {
        if self.config.zone_portal.is_zero() {
            return Err(JsonRpcError::internal("zone portal not configured"));
        }

        let filter = Filter::new()
            .address(self.config.zone_portal)
            .from_block(tempo_block_number)
            .to_block(tempo_block_number)
            .event_signature(vec![
                ZonePortal::DepositMade::SIGNATURE_HASH,
                ZonePortal::EncryptedDepositMade::SIGNATURE_HASH,
            ]);

        let logs = self.l1_provider.get_logs(&filter).await.map_err(internal)?;
        let mut deposits = Vec::with_capacity(logs.len());

        for log in logs {
            match ZonePortal::ZonePortalEvents::decode_log(&log.inner)
                .map_err(internal)?
                .data
            {
                ZonePortal::ZonePortalEvents::DepositMade(event) => {
                    deposits.push(PortalDepositRecord::Regular {
                        deposit_hash: event.newCurrentDepositQueueHash,
                        sender: event.sender,
                        recipient: event.to,
                        token: event.token,
                        amount: event.netAmount,
                        memo: event.memo,
                    });
                }
                ZonePortal::ZonePortalEvents::EncryptedDepositMade(event) => {
                    deposits.push(PortalDepositRecord::Encrypted {
                        deposit_hash: event.newCurrentDepositQueueHash,
                        sender: event.sender,
                        token: event.token,
                        amount: event.netAmount,
                    });
                }
                _ => {}
            }
        }

        Ok(deposits)
    }

    async fn terminal_event_for_deposit(
        &self,
        deposit_hash: B256,
    ) -> Result<Option<TerminalDepositEvent>, JsonRpcError> {
        let filter = Filter::new()
            .address(ZONE_INBOX_ADDRESS)
            .from_block(0)
            .event_signature(vec![
                ZoneInbox::DepositProcessed::SIGNATURE_HASH,
                ZoneInbox::EncryptedDepositProcessed::SIGNATURE_HASH,
                ZoneInbox::EncryptedDepositFailed::SIGNATURE_HASH,
            ])
            .topic1(deposit_hash);

        let logs = self
            .zone_provider
            .get_logs(&filter)
            .await
            .map_err(internal)?;
        let Some(log) = logs.last() else {
            return Ok(None);
        };

        let Some(signature) = log.topics().first().copied() else {
            return Ok(None);
        };

        if signature == ZoneInbox::DepositProcessed::SIGNATURE_HASH {
            ZoneInbox::DepositProcessed::decode_log(&log.inner).map_err(internal)?;
            return Ok(Some(TerminalDepositEvent::RegularProcessed));
        }

        if signature == ZoneInbox::EncryptedDepositProcessed::SIGNATURE_HASH {
            let event =
                ZoneInbox::EncryptedDepositProcessed::decode_log(&log.inner).map_err(internal)?;
            return Ok(Some(TerminalDepositEvent::EncryptedProcessed {
                recipient: event.to,
                memo: event.memo,
            }));
        }

        if signature == ZoneInbox::EncryptedDepositFailed::SIGNATURE_HASH {
            ZoneInbox::EncryptedDepositFailed::decode_log(&log.inner).map_err(internal)?;
            return Ok(Some(TerminalDepositEvent::EncryptedFailed));
        }

        Ok(None)
    }
}

impl<Api> zone_rpc::ZoneRpcApi for TempoZoneRpc<Api>
where
    Api: FullEthApi + EthApiTypes<NetworkTypes = TempoNetwork> + Send + Sync + 'static,
{
    fn get_keychain_key(&self, account: Address, key_id: Address) -> BoxEyreFut<'_, KeyInfo> {
        Box::pin(async move {
            let request = TempoTransactionRequest {
                inner: TransactionRequest {
                    to: Some(ACCOUNT_KEYCHAIN_ADDRESS.into()),
                    input: getKeyCall {
                        account,
                        keyId: key_id,
                    }
                    .abi_encode()
                    .into(),
                    ..Default::default()
                },
                ..Default::default()
            };

            let output = EthCall::call(&self.eth.api, request, None, EvmOverrides::default())
                .await
                .wrap_err("AccountKeychain.getKey eth_call failed")?;

            IAccountKeychain::getKeyCall::abi_decode_returns(output.as_ref()).map_err(Into::into)
        })
    }

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

    fn zone_get_authorization_token_info(&self, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            to_raw(&AuthorizationTokenInfoResponse {
                account: auth.caller,
                expires_at: U64::from(auth.expires_at),
            })
        })
    }

    fn zone_get_zone_info(&self, _auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            to_raw(&ZoneInfoResponse {
                zone_id: U64::from(self.config.zone_id),
                zone_token: ZONE_TOKEN_ADDRESS,
                sequencer: self.config.sequencer,
                chain_id: U64::from(self.config.chain_id),
            })
        })
    }

    fn zone_get_deposit_status(&self, tempo_block_number: u64, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let zone_processed_through = self
                .tempo_state
                .tempoBlockNumber()
                .call()
                .await
                .map_err(internal)?;
            let portal_deposits = self.portal_deposits_for_block(tempo_block_number).await?;

            let mut deposits = Vec::new();
            for deposit in portal_deposits {
                match deposit {
                    PortalDepositRecord::Regular {
                        deposit_hash,
                        sender,
                        recipient,
                        token,
                        amount,
                        memo,
                    } => {
                        if sender != auth.caller && recipient != auth.caller {
                            continue;
                        }

                        let terminal = self.terminal_event_for_deposit(deposit_hash).await?;
                        let status = if terminal.is_some() {
                            DepositState::Processed
                        } else {
                            DepositState::Pending
                        };

                        deposits.push(DepositStatusEntry {
                            deposit_hash,
                            kind: DepositKind::Regular,
                            token,
                            sender,
                            recipient: Some(recipient),
                            amount: U256::from(amount),
                            memo: Some(memo),
                            status,
                        });
                    }
                    PortalDepositRecord::Encrypted {
                        deposit_hash,
                        sender,
                        token,
                        amount,
                    } => {
                        let terminal = self.terminal_event_for_deposit(deposit_hash).await?;

                        let include = match (&terminal, sender == auth.caller) {
                            (_, true) => true,
                            (
                                Some(TerminalDepositEvent::EncryptedProcessed {
                                    recipient, ..
                                }),
                                false,
                            ) => *recipient == auth.caller,
                            _ => false,
                        };

                        if !include {
                            continue;
                        }

                        let (recipient, memo, status) = match terminal {
                            Some(TerminalDepositEvent::EncryptedProcessed {
                                recipient,
                                memo,
                                ..
                            }) => (Some(recipient), Some(memo), DepositState::Processed),
                            Some(TerminalDepositEvent::EncryptedFailed) => {
                                (None, None, DepositState::Failed)
                            }
                            Some(TerminalDepositEvent::RegularProcessed) => {
                                return Err(JsonRpcError::internal(
                                    "regular deposit event matched encrypted deposit hash",
                                ));
                            }
                            None => (None, None, DepositState::Pending),
                        };

                        deposits.push(DepositStatusEntry {
                            deposit_hash,
                            kind: DepositKind::Encrypted,
                            token,
                            sender,
                            recipient,
                            amount: U256::from(amount),
                            memo,
                            status,
                        });
                    }
                }
            }

            let processed = zone_processed_through >= tempo_block_number
                && deposits
                    .iter()
                    .all(|deposit| deposit.status != DepositState::Pending);

            to_raw(&DepositStatusResponse {
                tempo_block_number: U64::from(tempo_block_number),
                zone_processed_through: U64::from(zone_processed_through),
                processed,
                deposits,
            })
        })
    }
}

#[derive(Debug, Clone)]
enum PortalDepositRecord {
    Regular {
        deposit_hash: B256,
        sender: Address,
        recipient: Address,
        token: Address,
        amount: u128,
        memo: B256,
    },
    Encrypted {
        deposit_hash: B256,
        sender: Address,
        token: Address,
        amount: u128,
    },
}

#[derive(Debug, Clone)]
enum TerminalDepositEvent {
    RegularProcessed,
    EncryptedProcessed { recipient: Address, memo: B256 },
    EncryptedFailed,
}

/// Strip privacy-sensitive fields from a block for non-sequencer callers.
fn redact_block(block: &mut RpcBlock) {
    // header.inner = alloy Header, .inner = Sealed wrapper, .inner = TempoHeader (contains logs_bloom)
    block.header.inner.inner.inner.logs_bloom = Bloom::ZERO;
    block.transactions = BlockTransactions::Hashes(Vec::new());
}
