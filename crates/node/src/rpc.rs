//! [`ZoneRpcApi`] implementation backed by reth's EthApi.
//!
//! Re-exports the standalone `zone-rpc` crate so everything is accessible
//! via `zone_node::rpc::*`.

pub use zone_rpc::*;

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Weak},
    time::Duration,
};

use alloy_consensus::BlockHeader;
use alloy_eips::eip2935::{HISTORY_SERVE_WINDOW, HISTORY_STORAGE_ADDRESS};
use alloy_network::{ReceiptResponse, TransactionBuilder, TransactionResponse};
use alloy_primitives::{Address, B256, Bloom, Bytes, U64, U256, keccak256};
use alloy_provider::{DynProvider, Provider};
use alloy_rpc_types_eth::{
    Block, BlockId, BlockNumberOrTag, BlockTransactions, FeeHistory, Filter, FilterChanges,
    FilterId, TransactionRequest,
    state::{EvmOverrides, StateOverride},
};
use alloy_sol_types::SolCall;
use eyre::WrapErr;
use futures::StreamExt;
use jsonrpsee::{RpcModule, core::RpcResult, proc_macros::rpc, types::ErrorObjectOwned};
use reth_evm::{ConfigureEvm as _, execute::Executor as _};
use reth_provider::{CanonStateSubscriptions, HeaderProvider};
use reth_revm::{db::State, witness::ExecutionWitnessRecord};
use reth_rpc::{EthFilter, eth::filter::EthFilterError};
use reth_rpc_builder::EthHandlers;
use reth_rpc_eth_api::{
    EthApiTypes, EthFilterApiServer, RpcConvert,
    helpers::{EthApiSpec, EthBlocks, EthCall, EthFees, EthState, EthTransactions, FullEthApi},
};
use reth_rpc_eth_types::{EthApiError, logs_utils};
use reth_storage_api::{BlockNumReader, StateProviderFactory};
use reth_trie_common::{ExecutionWitnessMode, HashedStorage};
use tempo_alloy::{
    TempoNetwork,
    provider::ext::TempoProviderExt as _,
    rpc::{TempoCallBuilderExt as _, TempoHeaderResponse, TempoTransactionRequest},
};
use tempo_chainspec::spec::{TEMPO_T0_BASE_FEE, TEMPO_T1_BASE_FEE};
use tempo_contracts::precompiles::{
    ACCOUNT_KEYCHAIN_ADDRESS,
    account_keychain::IAccountKeychain::{self, KeyInfo, getKeyCall},
};
use tempo_primitives::{TempoPrimitives, TempoTxEnvelope};
use tokio::{
    sync::Mutex,
    time::{MissedTickBehavior, interval},
};
use zone_l1::{TempoStateExt as _, state::EnabledTokenRegistry};

use alloy_rpc_client::{ConnectionConfig, WebSocketConfig};
use tempo_zone_contracts::{ZONE_TOKEN_ADDRESS, ZonePortal};
use zone_evm::ZoneEvmConfig;
use zone_p2p::{LeadershipSchedule, PeerTip, ZoneManifest};
use zone_rpc::{
    auth::AuthContext,
    types::{
        ActiveLeaderInfo, AuthorizationTokenInfoResponse, BoxEyreFut, BoxFut, JsonRpcError,
        LocalSequencerInfo, PeerTipInfo, SequencerInfoResponse, SequencerPeerInfo,
        SequencerProgress, SequencerReadiness, SetLeaderResponse,
        TempoStorageRead as RpcTempoStorageRead, ZoneExecutionWitness, ZoneInfoResponse, internal,
        raw_null, raw_zero, to_raw,
    },
};

use crate::{replication::PeerTipRegistry, role::SharedRoleStatus};

/// Multi-sequencer handles for the sequencer RPC methods.
///
/// The RPC servers launch before the role controller, so the node installs this context
/// through an [`std::sync::OnceLock`] indirection once the leadership machinery exists.
#[derive(Debug)]
pub struct SequencerRpcContext {
    /// Shared finalized leadership schedule.
    pub schedule: LeadershipSchedule,
    /// Live role and promotion-readiness snapshot from the role controller.
    pub status: SharedRoleStatus,
    /// Hash-carrying peer tip evidence.
    pub(crate) peer_tips: PeerTipRegistry,
    /// Validated static topology manifest.
    pub manifest: Arc<ZoneManifest>,
    /// This node's individual secp256k1 address (the `setLeader` relayer identity).
    ///
    /// `None` on an rpc-only member: it holds no individual key, so it cannot relay.
    pub local_secp256k1_address: Option<Address>,
    /// This node's Ed25519 public key.
    pub local_ed25519_public_key: zone_p2p::P2pPeerId,
    /// Wallet-backed L1 provider signing with the individual key, when this node holds one.
    pub relayer: Option<DynProvider<TempoNetwork>>,
}

impl SequencerRpcContext {
    /// Create the RPC context for a multi-sequencer node.
    pub(crate) fn new(
        schedule: LeadershipSchedule,
        status: SharedRoleStatus,
        peer_tips: PeerTipRegistry,
        manifest: Arc<ZoneManifest>,
        local_secp256k1_address: Option<Address>,
        local_ed25519_public_key: zone_p2p::P2pPeerId,
        relayer: Option<DynProvider<TempoNetwork>>,
    ) -> Self {
        Self {
            schedule,
            status,
            peer_tips,
            manifest,
            local_secp256k1_address,
            local_ed25519_public_key,
            relayer,
        }
    }
}

/// Public, authentication-independent Zone metadata methods.
#[rpc(server, namespace = "zone")]
pub(crate) trait ZoneApi {
    /// Returns metadata for this Zone.
    #[method(name = "getZoneInfo")]
    async fn get_zone_info(&self) -> RpcResult<ZoneInfoResponse>;

    /// Returns the encryption key active at the current Tempo L1 head.
    #[method(name = "getEncryptionKey")]
    async fn get_encryption_key(&self) -> RpcResult<ZonePortal::encryptionKeyAtBlockReturn>;
}

/// Public Zone API backed directly by the node and Tempo L1 providers.
#[derive(Clone)]
pub(crate) struct OperatorZoneApi<P> {
    zone_id: u32,
    chain_id: u64,
    portal_address: Address,
    l1_provider: DynProvider<TempoNetwork>,
    zone_provider: P,
}

impl<P> OperatorZoneApi<P> {
    pub(crate) const fn new(
        zone_id: u32,
        chain_id: u64,
        portal_address: Address,
        l1_provider: DynProvider<TempoNetwork>,
        zone_provider: P,
    ) -> Self {
        Self {
            zone_id,
            chain_id,
            portal_address,
            l1_provider,
            zone_provider,
        }
    }
}

#[jsonrpsee::core::async_trait]
impl<P> ZoneApiServer for OperatorZoneApi<P>
where
    P: StateProviderFactory + Clone + Send + Sync + 'static,
{
    async fn get_zone_info(&self) -> RpcResult<ZoneInfoResponse> {
        let tempo_block_number = self
            .zone_provider
            .latest()
            .map_err(internal)
            .and_then(|state| state.tempo_block_number().map_err(internal))
            .map_err(operator_rpc_error)?;

        zone_info(
            self.zone_id,
            self.chain_id,
            self.portal_address,
            tempo_block_number,
            &self.l1_provider,
        )
        .await
        .map_err(operator_rpc_error)
    }

    async fn get_encryption_key(&self) -> RpcResult<ZonePortal::encryptionKeyAtBlockReturn> {
        encryption_key(self.portal_address, &self.l1_provider)
            .await
            .map_err(operator_rpc_error)
    }
}

/// Build the unauthenticated Zone extension installed on the node's operator HTTP RPC.
pub(crate) fn operator_zone_rpc_module<P>(
    portal_address: Address,
    sequencer: Arc<std::sync::OnceLock<SequencerRpcContext>>,
    provider: P,
) -> Result<RpcModule<()>, jsonrpsee::core::RegisterMethodError>
where
    P: BlockNumReader + HeaderProvider + StateProviderFactory + Clone + Send + Sync + 'static,
{
    let mut module = RpcModule::new(());
    let set_leader_sequencer = sequencer.clone();
    module.register_async_method("zone_setLeader", move |params, _, _| {
        let sequencer = set_leader_sequencer.clone();
        async move {
            let (target,) = params.parse::<(Address,)>()?;
            set_leader(portal_address, sequencer.as_ref(), target)
                .await
                .map_err(operator_rpc_error)
        }
    })?;
    module.register_async_method("zone_getSequencerInfo", move |_, _, _| {
        let sequencer = sequencer.clone();
        let provider = provider.clone();
        async move {
            get_sequencer_info(portal_address, sequencer.as_ref(), &provider)
                .map_err(operator_rpc_error)
        }
    })?;
    Ok(module)
}

/// Zone-specific debug API.
#[derive(Clone)]
pub(crate) struct NodeZoneDebugApi<E> {
    eth_api: E,
}

impl<E> NodeZoneDebugApi<E> {
    pub(crate) const fn new(eth_api: E) -> Self {
        Self { eth_api }
    }
}

#[jsonrpsee::core::async_trait]
impl<E> ZoneDebugApi for NodeZoneDebugApi<E>
where
    E: FullEthApi<Evm = ZoneEvmConfig, Primitives = TempoPrimitives>,
{
    async fn zone_execution_witness(
        &self,
        block_id: BlockNumberOrTag,
    ) -> RpcResult<ZoneExecutionWitness> {
        let _permit = self
            .eth_api
            .tracing_task_guard()
            .clone()
            .acquire_owned()
            .await;

        let block = self
            .eth_api
            .recovered_block(block_id.into())
            .await
            .map_err(|error| operator_rpc_error(internal(error)))?
            .ok_or_else(|| operator_rpc_error(internal(format!("block {block_id} not found"))))?;
        let block_number = block.header().number();

        self.eth_api
            .spawn_with_state_at_block(block.parent_hash(), move |eth_api, mut db| {
                let (evm_config, recorder) = eth_api.evm_config().with_l1_storage_recorder();
                let block_executor = evm_config.executor(&mut db);
                let mode = ExecutionWitnessMode::default();
                let mut witness_record = ExecutionWitnessRecord::default();

                let _ = block_executor
                    .execute_with_state_closure(&block, |statedb: &State<_>| {
                        witness_record.record_executed_state(statedb, mode);
                        record_block_hash_storage_proofs(&mut witness_record, statedb);
                    })
                    .map_err(|error| EthApiError::Internal(error.into()))?;

                let witness = witness_record
                    .into_execution_witness(&db.database.0, eth_api.provider(), block_number, mode)
                    .map_err(EthApiError::from)?;
                Ok(ZoneExecutionWitness {
                    execution_witness: witness,
                    tempo_reads: recorder
                        .take_reads()
                        .into_iter()
                        .map(|read| RpcTempoStorageRead {
                            account: read.account,
                            slot: read.slot,
                        })
                        .collect(),
                })
            })
            .await
            .map_err(|error| operator_rpc_error(internal(error)))
    }
}

/// Add EIP-2935 history-contract storage paths for every BLOCKHASH value read during replay.
///
/// Reth records these reads in REVM's block-hash cache and normally proves them with ancestor
/// headers. Zones already commit the EIP-2935 history contract in state, so adding the matching
/// storage targets lets the SPF authenticate the same values against the parent state root.
fn record_block_hash_storage_proofs<DB>(witness: &mut ExecutionWitnessRecord, state: &State<DB>) {
    let block_hashes = state.block_hashes.iter().collect::<Vec<_>>();
    if block_hashes.is_empty() {
        return;
    }

    let history_storage = witness
        .hashed_state
        .storages
        .entry(keccak256(HISTORY_STORAGE_ADDRESS))
        .or_insert_with(|| HashedStorage::new(false));
    for (number, hash) in block_hashes {
        let slot = U256::from(number % HISTORY_SERVE_WINDOW as u64);
        history_storage.storage.insert(
            keccak256(slot.to_be_bytes::<32>()),
            U256::from_be_bytes(hash.0),
        );
    }
}

fn operator_rpc_error(error: JsonRpcError) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(error.code as i32, error.message, error.data)
}

async fn zone_tokens(
    portal_address: Address,
    l1_provider: &DynProvider<TempoNetwork>,
) -> Result<Vec<Address>, JsonRpcError> {
    if portal_address.is_zero() {
        return Ok(vec![ZONE_TOKEN_ADDRESS]);
    }

    ZonePortal::new(portal_address, l1_provider)
        .enabled_tokens()
        .await
        .map_err(internal)
}

async fn zone_sequencers(
    portal_address: Address,
    l1_provider: &DynProvider<TempoNetwork>,
) -> Result<Vec<Address>, JsonRpcError> {
    ZonePortal::new(portal_address, l1_provider)
        .sequencers()
        .await
        .map_err(internal)
}

/// Builds the Zone metadata shared by the operator and redacted RPC surfaces.
///
/// The caller supplies the local Zone's processed Tempo block number; the
/// remaining dynamic fields are read directly from the ZonePortal on Tempo L1.
async fn zone_info(
    zone_id: u32,
    chain_id: u64,
    portal_address: Address,
    tempo_block_number: u64,
    l1_provider: &DynProvider<TempoNetwork>,
) -> Result<ZoneInfoResponse, JsonRpcError> {
    let portal = ZonePortal::new(portal_address, l1_provider);
    let (zone_tokens, sequencers, is_access_enforced, is_gateway_open) = tokio::try_join!(
        zone_tokens(portal_address, l1_provider),
        zone_sequencers(portal_address, l1_provider),
        async {
            if portal_address.is_zero() {
                Ok(false)
            } else {
                portal.isAccessEnforced().call().await.map_err(internal)
            }
        },
        async {
            if portal_address.is_zero() {
                Ok(true)
            } else {
                portal.isGatewayOpen().call().await.map_err(internal)
            }
        },
    )?;

    Ok(ZoneInfoResponse {
        zone_id: U64::from(zone_id),
        is_access_enforced,
        is_gateway_open,
        zone_tokens,
        sequencers,
        chain_id: U64::from(chain_id),
        tempo_block_number: U64::from(tempo_block_number),
    })
}

/// Reads the encryption key active at the current Tempo L1 head.
async fn encryption_key(
    portal_address: Address,
    l1_provider: &DynProvider<TempoNetwork>,
) -> Result<ZonePortal::encryptionKeyAtBlockReturn, JsonRpcError> {
    let block_number = l1_provider.get_block_number().await.map_err(internal)?;
    ZonePortal::new(portal_address, l1_provider)
        .encryptionKeyAtBlock(block_number)
        .block(BlockId::number(block_number))
        .call()
        .await
        .map_err(internal)
}

fn get_sequencer_info<P>(
    portal_address: Address,
    sequencer: &std::sync::OnceLock<SequencerRpcContext>,
    provider: &P,
) -> Result<SequencerInfoResponse, JsonRpcError>
where
    P: BlockNumReader + HeaderProvider + StateProviderFactory,
{
    let Some(context) = sequencer.get() else {
        // Single-sequencer (or not yet initialized) node: report the minimal view.
        return Ok(SequencerInfoResponse {
            mode: "single".to_owned(),
            portal: portal_address,
            local: None,
            active_leader: None,
            local_tip: None,
            peers: Vec::new(),
            progress: None,
            readiness: None,
        });
    };

    let status = context.status.lock().expect("poisoned").clone();
    let latest = context.schedule.latest();
    let active_leader = latest.as_ref().map(|record| {
        let node = context.manifest.node_by_ed25519_public_key(&record.leader);
        ActiveLeaderInfo {
            name: node.map(|node| node.name().to_owned()),
            sequencer_address: node.and_then(|node| node.secp256k1_address()),
            p2p_public_key: record.leader.to_string(),
            epoch: U64::from(record.epoch),
            activation_tempo_block: U64::from(record.activation_tempo_block),
        }
    });

    let tips: HashMap<_, _> = context
        .peer_tips
        .snapshot()
        .into_iter()
        .map(|(peer, tip, _)| (peer, tip))
        .collect();
    let peers = context
        .manifest
        .nodes()
        .iter()
        .map(|node| SequencerPeerInfo {
            name: node.name().to_owned(),
            sequencer_address: node.secp256k1_address(),
            rpc_only: node.is_rpc_only(),
            is_local: node.ed25519_public_key() == &context.local_ed25519_public_key,
            tip: tips.get(node.ed25519_public_key()).map(|tip| PeerTipInfo {
                zone_height: U64::from(tip.zone_height),
                zone_hash: tip.zone_hash,
                tempo_block_number: U64::from(tip.tempo_block_number),
                tempo_block_hash: tip.tempo_block_hash,
            }),
        })
        .collect();

    let local_tip = local_recovery_tip(provider)?;

    let local_node = context
        .manifest
        .node_by_ed25519_public_key(&context.local_ed25519_public_key);
    Ok(SequencerInfoResponse {
        mode: "multi".to_owned(),
        portal: portal_address,
        local: Some(LocalSequencerInfo {
            name: local_node
                .map(|node| node.name().to_owned())
                .unwrap_or_default(),
            sequencer_address: context.local_secp256k1_address,
            p2p_public_key: context.local_ed25519_public_key.to_string(),
            role: status.role.to_owned(),
        }),
        active_leader,
        local_tip: Some(PeerTipInfo {
            zone_height: U64::from(local_tip.zone_height),
            zone_hash: local_tip.zone_hash,
            tempo_block_number: U64::from(local_tip.tempo_block_number),
            tempo_block_hash: local_tip.tempo_block_hash,
        }),
        peers,
        progress: Some(SequencerProgress {
            zone_height: U64::from(local_tip.zone_height),
            tempo_block_number: U64::from(local_tip.tempo_block_number),
            latest_observed_leadership_epoch: context
                .schedule
                .latest_observed_epoch()
                .map(U64::from),
            locally_applied_leadership_epoch: context
                .schedule
                .locally_applied_epoch()
                .map(U64::from),
            pending_transitions: U64::from(context.schedule.pending_transitions() as u64),
        }),
        readiness: Some(SequencerReadiness {
            ready_for_promotion: status.ready_for_promotion,
            reasons: status.promotion_reasons,
        }),
    })
}

type RpcBlock = Block<alloy_rpc_types_eth::Transaction<TempoTxEnvelope>, TempoHeaderResponse>;
const FILTER_OWNER_PRUNE_INTERVAL: Duration = Duration::from_secs(60);
const MAX_WS_FRAME_AND_MESSAGE_SIZE: usize = 128 * 1024 * 1024;

fn filter_not_found_error() -> JsonRpcError {
    JsonRpcError::invalid_params("filter not found")
}

fn map_eth_filter_error(err: EthFilterError) -> JsonRpcError {
    match err {
        EthFilterError::FilterNotFound(_) => filter_not_found_error(),
        other => internal(other),
    }
}

fn stale_filter_owner_ids(
    owner_ids: impl IntoIterator<Item = FilterId>,
    active_ids: &HashSet<FilterId>,
) -> Vec<FilterId> {
    owner_ids
        .into_iter()
        .filter(|id| !active_ids.contains(id))
        .collect()
}

async fn prune_filter_owners<Api: EthApiTypes + 'static>(
    filter: &EthFilter<Api>,
    owners: &Mutex<HashMap<FilterId, Address>>,
) {
    let owner_ids = {
        let owners = owners.lock().await;
        owners.keys().cloned().collect::<Vec<_>>()
    };
    if owner_ids.is_empty() {
        return;
    }

    let active_ids = filter
        .active_filters()
        .ids()
        .await
        .into_iter()
        .collect::<HashSet<_>>();
    let stale_ids = stale_filter_owner_ids(owner_ids, &active_ids);
    if stale_ids.is_empty() {
        return;
    }

    let mut owners = owners.lock().await;
    for id in stale_ids {
        owners.remove(&id);
    }
}

/// [`ZoneRpcApi`] implementation backed by reth's [`EthHandlers`].
///
/// This is the privacy enforcement layer for the zone's JSON-RPC surface.
/// Only methods explicitly routed through [`ZoneRpcApi`] are reachable —
/// everything else is rejected by the dispatcher's typed method registry,
/// so this struct effectively acts as an **enforced allowlist**
/// of Ethereum JSON-RPC endpoints.
///
/// For every allowed endpoint it applies typed privacy checks *before*
/// serializing to JSON:
///
/// - **Block redaction** — zeroing `logsBloom` and clearing transaction
///   lists on the redacted RPC.
/// - **Sender-scoped access** — returning `null` for transactions and
///   receipts not owned by the authenticated caller.
/// - **`from`-enforcement** — `eth_call` / `eth_estimateGas` may only
///   simulate from the authenticated account (`-32004` on mismatch,
///   auto-set when omitted); state overrides are rejected (`-32602`).
/// - **Sender verification** — `eth_sendRawTransaction` checks that the
///   recovered transaction sender matches the authenticated account
///   (`-32003` on mismatch).
pub struct ZoneRpc<Api: EthApiTypes> {
    eth: EthHandlers<Api>,
    config: zone_rpc::RedactedRpcConfig,
    enabled_tokens: EnabledTokenRegistry,
    l1_provider: DynProvider<TempoNetwork>,
    /// Maps filter IDs to the authenticated account that created them.
    /// The reth filter registry remains the source of truth for filter liveness.
    filter_owners: Arc<Mutex<HashMap<FilterId, Address>>>,
}

impl<Api: EthApiTypes + 'static> ZoneRpc<Api> {
    /// Wrap reth's [`EthHandlers`] (api + filter + pubsub) and an L1 provider.
    pub fn new(
        eth: EthHandlers<Api>,
        config: zone_rpc::RedactedRpcConfig,
        enabled_tokens: EnabledTokenRegistry,
        l1_provider: DynProvider<TempoNetwork>,
    ) -> Self {
        let rpc = Self {
            eth,
            config,
            enabled_tokens,
            l1_provider,
            filter_owners: Arc::new(Mutex::new(HashMap::new())),
        };
        rpc.spawn_filter_owner_pruner();
        rpc
    }

    /// Returns a reference to the inner [`EthFilter`] handler.
    pub fn filter(&self) -> &EthFilter<Api> {
        &self.eth.filter
    }

    async fn filter_is_active(&self, id: &FilterId) -> bool {
        self.filter().active_filters().contains(id).await
    }

    fn spawn_filter_owner_pruner(&self)
    where
        Api: Send + Sync + 'static,
    {
        let filter = self.filter().clone();
        let owners: Weak<Mutex<HashMap<FilterId, Address>>> = Arc::downgrade(&self.filter_owners);
        tokio::spawn(async move {
            let mut prune_interval = interval(FILTER_OWNER_PRUNE_INTERVAL);
            prune_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

            loop {
                prune_interval.tick().await;

                let Some(owners) = owners.upgrade() else {
                    break;
                };

                prune_filter_owners(&filter, &owners).await;
            }
        });
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
        let owner_matches = {
            let owners = self.filter_owners.lock().await;
            matches!(owners.get(id), Some(owner) if *owner == auth.caller)
        };
        if !owner_matches {
            return Err(filter_not_found_error());
        }
        if self.filter_is_active(id).await {
            Ok(())
        } else {
            self.filter_owners.lock().await.remove(id);
            Err(filter_not_found_error())
        }
    }

    fn zone_tokens(&self) -> Vec<Address> {
        // Preserve the default token when running without an L1 portal.
        if self.config.zone_portal.is_zero() {
            return vec![ZONE_TOKEN_ADDRESS];
        }

        self.enabled_tokens.read().iter().copied().collect()
    }

    fn enforce_authorized(
        &self,
        request: &mut TempoTransactionRequest,
        auth: &AuthContext,
    ) -> Result<(), JsonRpcError> {
        zone_rpc::policy::enforce_authorized(request, auth)
    }
}

impl<Api> ZoneRpc<Api>
where
    Api: FullEthApi + EthApiTypes<NetworkTypes = TempoNetwork> + Send + Sync + 'static,
{
    fn block_by_id(&self, id: BlockId) -> BoxFut<'_> {
        Box::pin(async move {
            let block = EthBlocks::rpc_block(&self.eth.api, id, false)
                .await
                .map_err(internal)?;

            let Some(mut block) = block else {
                return Ok(raw_null());
            };

            redact_block(&mut block);

            to_raw(&block)
        })
    }
}

impl<Api> zone_rpc::ZoneRpcApi for ZoneRpc<Api>
where
    Api: FullEthApi + EthApiTypes<NetworkTypes = TempoNetwork> + Send + Sync + 'static,
{
    fn get_keychain_key(&self, account: Address, key_id: Address) -> BoxEyreFut<'_, KeyInfo> {
        Box::pin(async move {
            let request = TempoTransactionRequest {
                inner: TransactionRequest {
                    from: Some(account),
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

    fn client_version(&self) -> BoxFut<'_> {
        Box::pin(async { to_raw(&crate::version::client_version()) })
    }

    fn syncing(&self) -> BoxFut<'_> {
        Box::pin(async move {
            let status = EthApiSpec::sync_status(&self.eth.api).map_err(internal)?;
            to_raw(&status)
        })
    }

    fn coinbase(&self) -> BoxFut<'_> {
        Box::pin(async move {
            let header = EthBlocks::rpc_block_header(&self.eth.api, BlockId::latest())
                .await
                .map_err(internal)?
                .ok_or_else(|| JsonRpcError::internal("latest block not found"))?;
            to_raw(&header.beneficiary())
        })
    }

    fn gas_price(&self) -> BoxFut<'_> {
        Box::pin(async move { to_raw(&U256::from(TEMPO_T1_BASE_FEE)) })
    }

    fn max_priority_fee_per_gas(&self) -> BoxFut<'_> {
        Box::pin(async move { to_raw(&U256::ZERO) })
    }

    fn fee_history(
        &self,
        block_count: u64,
        newest_block: BlockNumberOrTag,
        reward_percentiles: Option<Vec<f64>>,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            let mut history =
                EthFees::fee_history(&self.eth.api, block_count, newest_block, reward_percentiles)
                    .await
                    .map_err(internal)?;
            // Redact gas fields (like `gas_used_ratio`) that can be used to guess tx counts
            redact_fee_history(&mut history);
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
            if address != auth.caller {
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
            if address != auth.caller {
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
        _full: bool,
        _auth: AuthContext,
    ) -> BoxFut<'_> {
        self.block_by_id(number.into())
    }

    fn block_by_hash(&self, hash: B256, _full: bool, _auth: AuthContext) -> BoxFut<'_> {
        self.block_by_id(hash.into())
    }

    fn transaction_by_hash(&self, hash: B256, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let tx = EthTransactions::transaction_by_hash(&self.eth.api, hash)
                .await
                .map_err(internal)?
                .map(|src| src.into_transaction(self.eth.api.converter()))
                .transpose()
                .map_err(internal)?;

            let Some(mut tx) = tx else {
                return Ok(raw_null());
            };

            if tx.from() != auth.caller {
                return Ok(raw_null());
            }

            // transaction_index leaks how many txns were in this block, so redact
            tx.transaction_index = Some(0);

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

            if receipt.from() != auth.caller {
                return Ok(raw_null());
            }

            receipt = zone_rpc::filter::filter_receipt_logs(receipt);

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
            if state_override.is_some() {
                return Err(JsonRpcError::invalid_params("state overrides not allowed"));
            }

            self.enforce_authorized(&mut request, &auth)?;

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
            if state_override.is_some() {
                return Err(JsonRpcError::invalid_params("state overrides not allowed"));
            }

            self.enforce_authorized(&mut request, &auth)?;

            let result = EthCall::estimate_gas_at(
                &self.eth.api,
                request,
                block.unwrap_or_default(),
                EvmOverrides::state(state_override),
            )
            .await
            .map_err(internal)?;
            to_raw(&result)
        })
    }

    fn send_raw_transaction(&self, data: Bytes, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            zone_rpc::policy::verify_raw_tx_sender(&data, &auth)?;

            let hash = EthTransactions::send_raw_transaction(&self.eth.api, data)
                .await
                .map_err(internal)?;
            to_raw(&hash)
        })
    }

    fn send_raw_transaction_sync(&self, data: Bytes, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            zone_rpc::policy::verify_raw_tx_sender(&data, &auth)?;

            let mut receipt = EthTransactions::send_raw_transaction_sync(&self.eth.api, data, None)
                .await
                .map_err(internal)?;

            receipt = zone_rpc::filter::filter_receipt_logs(receipt);

            to_raw(&receipt)
        })
    }

    fn fill_transaction(
        &self,
        mut request: TempoTransactionRequest,
        auth: AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            self.enforce_authorized(&mut request, &auth)?;

            // Prefill the users request so the `fill_transaction` doesnt leak dynamic fee estimates via
            // missing fee fields.
            apply_public_fee_policy(&mut request);

            let result = EthTransactions::fill_transaction(&self.eth.api, request)
                .await
                .map_err(internal)?;
            to_raw(&result)
        })
    }

    fn get_logs(&self, mut filter: Filter, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let zone_tokens = self.zone_tokens();
            zone_rpc::filter::scope_filter_addresses(&mut filter, &zone_tokens)?;
            zone_rpc::filter::scope_filter_for_caller(&mut filter, &auth.caller)?;
            let logs = EthFilterApiServer::logs(&self.eth.filter, filter)
                .await
                .map_err(internal)?;
            let filtered = zone_rpc::filter::filter_logs(logs, &auth.caller);
            to_raw(&filtered)
        })
    }

    fn new_filter(&self, mut filter: Filter, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let zone_tokens = self.zone_tokens();
            zone_rpc::filter::scope_filter_addresses(&mut filter, &zone_tokens)?;
            zone_rpc::filter::scope_filter_for_caller(&mut filter, &auth.caller)?;
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

            let logs = self
                .filter()
                .filter_logs(id)
                .await
                .map_err(map_eth_filter_error)?;

            let filtered = zone_rpc::filter::filter_logs(logs, &auth.caller);
            to_raw(&filtered)
        })
    }

    fn get_filter_changes(&self, id: FilterId, auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            self.ensure_filter_owner(&id, &auth).await?;

            let changes = self
                .filter()
                .filter_changes(id)
                .await
                .map_err(map_eth_filter_error)?;

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

            if result || !self.filter_is_active(&id).await {
                self.filter_owners.lock().await.remove(&id);
            }

            to_raw(&result)
        })
    }

    fn ws_subscribe_new_heads(&self, _auth: AuthContext) -> BoxWsSubscriptionFut<'_> {
        Box::pin(async move {
            let api = self.eth.api.clone();
            let provider = self.eth.api.provider().clone();
            let stream = provider
                .canonical_state_stream()
                .flat_map(move |new_chain| {
                    let api = api.clone();
                    let headers = new_chain
                        .committed()
                        .blocks_iter()
                        .filter_map(move |block| {
                            match api
                                .converter()
                                .convert_header(block.clone_sealed_header(), block.rlp_length())
                            {
                                Ok(header) => Some(header),
                                Err(err) => {
                                    tracing::error!(
                                        target: "rpc",
                                        %err,
                                        "Failed to convert header"
                                    );
                                    None
                                }
                            }
                        })
                        .collect::<Vec<_>>();
                    futures::stream::iter(headers)
                })
                .map(move |mut header| {
                    redact_header(&mut header);
                    to_raw(&header)
                });
            let stream: zone_rpc::WsSubscriptionStream = Box::pin(stream);
            Ok(stream)
        })
    }

    fn ws_subscribe_logs(&self, mut filter: Filter, auth: AuthContext) -> BoxWsSubscriptionFut<'_> {
        Box::pin(async move {
            let provider = self.eth.api.provider().clone();
            let caller = auth.caller;

            let zone_tokens = self.zone_tokens();
            zone_rpc::filter::scope_filter_addresses(&mut filter, &zone_tokens)?;
            zone_rpc::filter::scope_filter_for_caller(&mut filter, &caller)?;

            let stream = provider
                .canonical_state_stream()
                .flat_map(|canon_state| futures::stream::iter(canon_state.block_receipts()))
                .flat_map(move |(block_receipts, removed)| {
                    let all_logs = logs_utils::matching_block_logs_with_tx_hashes(
                        &filter,
                        block_receipts.block,
                        block_receipts.timestamp,
                        block_receipts
                            .tx_receipts
                            .iter()
                            .map(|(tx, receipt)| (*tx, receipt)),
                        removed,
                    );
                    futures::stream::iter(all_logs)
                });

            // Renumber `log_index` per-transaction so a log seen live over the
            // subscription carries the same `(transactionHash, logIndex)` it would
            // via `eth_getLogs`/`eth_getTransactionReceipt`.
            // Logs arrive in block order grouped by tx, which is what `LogOrderingRedactor` needs.
            let mut log_redactor = zone_rpc::filter::LogOrderingRedactor::default();
            let stream = stream.filter_map(move |log| {
                std::future::ready(
                    zone_rpc::filter::is_log_visible(&log, &caller)
                        .then(|| to_raw(&log_redactor.redact(log))),
                )
            });
            let stream: zone_rpc::WsSubscriptionStream = Box::pin(stream);
            Ok(stream)
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
            let tempo_block_number = self
                .eth
                .api
                .provider()
                .latest()
                .map_err(internal)?
                .tempo_block_number()
                .map_err(internal)?;
            let info = zone_info(
                self.config.zone_id,
                self.config.chain_id,
                self.config.zone_portal,
                tempo_block_number,
                &self.l1_provider,
            )
            .await?;
            to_raw(&info)
        })
    }

    fn zone_get_encryption_key(&self, _auth: AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let key = encryption_key(self.config.zone_portal, &self.l1_provider).await?;
            to_raw(&key)
        })
    }
}

fn local_recovery_tip<P>(provider: &P) -> Result<PeerTip, JsonRpcError>
where
    P: BlockNumReader + HeaderProvider + StateProviderFactory,
{
    let zone_height = provider.last_block_number().map_err(internal)?;
    let zone_header = provider
        .sealed_header(zone_height)
        .map_err(internal)?
        .ok_or_else(|| JsonRpcError::internal("local canonical zone header is missing"))?;
    let tempo_tip = provider
        .state_by_block_hash(zone_header.hash())
        .map_err(internal)?
        .tempo_num_hash()
        .map_err(internal)?;
    Ok(PeerTip {
        zone_height,
        zone_hash: zone_header.hash(),
        tempo_block_number: tempo_tip.number,
        tempo_block_hash: tempo_tip.hash,
    })
}

async fn set_leader(
    portal_address: Address,
    sequencer: &std::sync::OnceLock<SequencerRpcContext>,
    target: Address,
) -> Result<SetLeaderResponse, JsonRpcError> {
    let Some(context) = sequencer.get() else {
        return Err(JsonRpcError::invalid_params(
            "zone_setLeader requires multi-sequencer mode",
        ));
    };
    if portal_address.is_zero() {
        return Err(JsonRpcError::invalid_params(
            "zone_setLeader requires a nonzero portal",
        ));
    }

    // The transaction is signed by this node's individual sequencer key, and the portal enforces
    // relayer authority. An rpc-only node holds no such key, so it cannot relay: operators call
    // this on a quorum member instead.
    let (Some(relayer), Some(relayer_address)) =
        (context.relayer.as_ref(), context.local_secp256k1_address)
    else {
        return Err(JsonRpcError::invalid_params(
            "zone_setLeader requires a node holding an individual secp256k1 key; this node is rpc-only",
        ));
    };
    let portal = ZonePortal::new(portal_address, relayer);

    // The target must be a manifest member and a registered portal sequencer.
    if context.manifest.node_by_secp256k1_address(target).is_none() {
        return Err(JsonRpcError::invalid_params(
            "target is not a manifest member",
        ));
    }
    let is_sequencer = portal
        .isSequencer(target)
        .block(BlockId::finalized())
        .call()
        .await
        .map_err(internal)?;
    if !is_sequencer {
        return Err(JsonRpcError::invalid_params(
            "target is not a registered portal sequencer",
        ));
    }

    // Read the finalized epoch for the compare-and-set guard. A duplicate fanout to
    // the already-active leader is answered without a transaction; races remain safe
    // because same-target calls no-op on chain and the epoch guard rejects delayed
    // stale calls.
    let leader_call = portal.leader().block(BlockId::finalized());
    let epoch_call = portal.leaderEpoch().block(BlockId::finalized());
    let (leader, expected_epoch) =
        tokio::try_join!(leader_call.call(), epoch_call.call()).map_err(internal)?;

    if leader == target {
        return Ok(SetLeaderResponse {
            status: "alreadyActive".to_owned(),
            tx_hash: None,
            relayer: relayer_address,
            requested_leader: target,
        });
    }

    // Refetch the committed admin-lane nonce for every attempt. The provider's process-local
    // nonce cache advances after a send, even when that transaction never lands, which would
    // otherwise leave every retry queued behind an unfillable 2D-nonce gap.
    let nonce = relayer
        .get_transaction_count_with_nonce_key(
            relayer_address,
            zone_sequencer::nonce_keys::ADMIN_OPS_NONCE_KEY,
        )
        .await
        .map_err(internal)?;
    let receipt = tokio::time::timeout(
        Duration::from_secs(30),
        portal
            .setLeader(target, expected_epoch)
            .nonce_key(zone_sequencer::nonce_keys::ADMIN_OPS_NONCE_KEY)
            .nonce(nonce)
            .max_fee_per_gas(tempo_chainspec::constants::gas::TEMPO_T1_BASE_FEE as u128)
            .max_priority_fee_per_gas(0)
            .send_sync(),
    )
    .await
    .map_err(|_| JsonRpcError::internal("setLeader confirmation timed out after 30 seconds"))?
    .map_err(internal)?;
    let tx_hash = receipt.transaction_hash();
    if !receipt.status() {
        metrics::counter!("zone_set_leader_submissions_total", "result" => "reverted").increment(1);
        return Err(JsonRpcError::internal(format!(
            "setLeader transaction {tx_hash} reverted on L1"
        )));
    }
    metrics::counter!("zone_set_leader_submissions_total", "result" => "submitted").increment(1);
    tracing::info!(
        target: "zone::rpc",
        %target,
        %tx_hash,
        nonce,
        expected_epoch,
        "Confirmed setLeader on the ZonePortal"
    );
    Ok(SetLeaderResponse {
        status: "submitted".to_owned(),
        tx_hash: Some(tx_hash),
        relayer: relayer_address,
        requested_leader: target,
    })
}

/// Clear RPC header fields that reveal private execution state from the header
fn redact_header(header: &mut TempoHeaderResponse) {
    header.inner.size = header.inner.size.map(|_| U256::ZERO);
    let inner = &mut header.inner.inner.inner;
    inner.gas_used = 0;
    inner.state_root = B256::ZERO;
    inner.transactions_root = B256::ZERO;
    inner.receipts_root = B256::ZERO;
    inner.logs_bloom = Bloom::ZERO;
    inner.extra_data = Bytes::new();
    inner.blob_gas_used = inner.blob_gas_used.map(|_| 0);
    inner.excess_blob_gas = inner.excess_blob_gas.map(|_| 0);
    inner.withdrawals_root = inner.withdrawals_root.map(|_| B256::ZERO);
}

/// Clear gas related fields that leak the size (and therefore tx counts)
fn redact_fee_history(history: &mut FeeHistory) {
    history.base_fee_per_gas.fill(u128::from(TEMPO_T0_BASE_FEE));
    history.gas_used_ratio.fill(0.0);
    history.base_fee_per_blob_gas.fill(0);
    history.blob_gas_used_ratio.fill(0.0);
    if let Some(rewards) = &mut history.reward {
        for block_rewards in rewards {
            block_rewards.fill(0);
        }
    }
}

/// Prefill missing transaction fee fields with public, deterministic values before calling reth's
/// transaction filler, so `eth_fillTransaction` does not expose dynamic fee estimates derived from
/// private zone activity.
fn apply_public_fee_policy(request: &mut TempoTransactionRequest) {
    if request.inner.has_eip4844_fields() && request.inner.max_fee_per_blob_gas.is_none() {
        request.inner.max_fee_per_blob_gas = Some(0);
    }

    if request.gas_price().is_some() {
        return;
    }

    if matches!(request.inner.transaction_type, Some(0 | 1)) {
        request.set_gas_price(u128::from(TEMPO_T0_BASE_FEE));
        return;
    }

    let priority_fee = request.max_priority_fee_per_gas().unwrap_or(0);
    if request.max_priority_fee_per_gas().is_none() {
        request.set_max_priority_fee_per_gas(0);
    }
    if request.max_fee_per_gas().is_none() {
        request.set_max_fee_per_gas(u128::from(TEMPO_T0_BASE_FEE) + priority_fee);
    }
}

/// Strip privacy-sensitive fields from a block returned by the redacted RPC.
fn redact_block(block: &mut RpcBlock) {
    redact_header(&mut block.header);
    block.transactions = BlockTransactions::Hashes(Vec::new());
    block.withdrawals = block.withdrawals.take().map(|_| Default::default());
}

pub(crate) fn rpc_connection_config(retry_connection_interval: Duration) -> ConnectionConfig {
    ConnectionConfig::new()
        .with_max_retries(u32::MAX)
        .with_retry_interval(retry_connection_interval)
        .with_ws_config(
            WebSocketConfig::default()
                // Large blocks can exceed tungstenite's default 16 MiB frame limit.
                .max_frame_size(Some(MAX_WS_FRAME_AND_MESSAGE_SIZE))
                .max_message_size(Some(MAX_WS_FRAME_AND_MESSAGE_SIZE)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_provider::ProviderBuilder;

    #[test]
    fn records_block_hashes_as_eip2935_storage_targets() {
        let number = 42;
        let hash = B256::repeat_byte(0x42);
        let mut state = State::builder()
            .with_database(revm::database::EmptyDB::default())
            .build();
        state.block_hashes.insert(number, hash);
        let mut witness = ExecutionWitnessRecord::default();

        record_block_hash_storage_proofs(&mut witness, &state);

        let storage = witness
            .hashed_state
            .storages
            .get(&keccak256(HISTORY_STORAGE_ADDRESS))
            .unwrap();
        let slot = U256::from(number % HISTORY_SERVE_WINDOW as u64);
        assert_eq!(
            storage.storage.get(&keccak256(slot.to_be_bytes::<32>())),
            Some(&U256::from_be_bytes(hash.0))
        );
    }

    #[test]
    fn zone_execution_witness_serializes_tempo_reads() {
        let account = Address::repeat_byte(0xaa);
        let slot = B256::repeat_byte(0xbb);
        let value = serde_json::to_value(ZoneExecutionWitness {
            execution_witness: Default::default(),
            tempo_reads: vec![RpcTempoStorageRead { account, slot }],
        })
        .unwrap();

        assert!(value.get("state").is_some());
        assert_eq!(
            value["tempo_reads"],
            serde_json::json!([{ "account": account, "slot": slot }])
        );
    }

    #[tokio::test]
    async fn operator_rpc_module_exposes_sequencer_methods_without_auth() {
        let module = operator_zone_rpc_module(
            Address::repeat_byte(0x11),
            Arc::new(std::sync::OnceLock::new()),
            Arc::new(reth_provider::test_utils::MockEthProvider::default()),
        )
        .expect("operator zone RPC module should register");

        let methods = module.method_names().collect::<HashSet<_>>();
        assert_eq!(methods.len(), 2);
        assert!(methods.contains("zone_getSequencerInfo"));
        assert!(methods.contains("zone_setLeader"));

        let info = module
            .call::<_, SequencerInfoResponse>(
                "zone_getSequencerInfo",
                jsonrpsee::core::EmptyServerParams::new(),
            )
            .await
            .expect("single-sequencer info should be available without authentication");
        assert_eq!(info.mode, "single");
        assert_eq!(info.portal, Address::repeat_byte(0x11));

        let error = module
            .call::<_, SetLeaderResponse>("zone_setLeader", [Address::repeat_byte(0x22)])
            .await
            .expect_err("an uninitialized sequencer context should reject the call");
        assert!(
            error
                .to_string()
                .contains("zone_setLeader requires multi-sequencer mode")
        );

        let error = module
            .call::<_, SetLeaderResponse>(
                "zone_setLeader",
                (
                    Address::repeat_byte(0x22),
                    serde_json::json!({
                        "force": true,
                        "expectedEpoch": "0x7",
                        "recoveryBlockHash": B256::repeat_byte(0x33),
                    }),
                ),
            )
            .await
            .expect_err("zone_setLeader no longer accepts runtime recovery options");
        assert!(error.to_string().contains("Invalid params"));
    }

    #[test]
    fn operator_zone_api_exposes_only_metadata_methods_without_auth() {
        let l1_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_http("http://127.0.0.1:1".parse().expect("valid URL"))
            .erased();
        let module = OperatorZoneApi::new(
            1,
            42431,
            Address::repeat_byte(0x11),
            l1_provider,
            Arc::new(reth_provider::test_utils::MockEthProvider::default()),
        )
        .into_rpc();

        let methods = module.method_names().collect::<HashSet<_>>();
        assert_eq!(methods.len(), 2);
        assert!(methods.contains("zone_getZoneInfo"));
        assert!(methods.contains("zone_getEncryptionKey"));
        assert!(!methods.contains("zone_getAuthorizationTokenInfo"));
    }

    #[test]
    fn redact_fee_history_preserves_shape_and_public_values() {
        let mut history = FeeHistory {
            base_fee_per_gas: vec![1, 2, 3],
            gas_used_ratio: vec![0.25, 0.75],
            base_fee_per_blob_gas: vec![4, 5, 6],
            blob_gas_used_ratio: vec![0.5, 1.0],
            oldest_block: 42,
            reward: Some(vec![vec![7, 8], vec![9, 10]]),
        };

        redact_fee_history(&mut history);

        assert_eq!(history.oldest_block, 42);
        assert_eq!(
            history.base_fee_per_gas,
            vec![u128::from(TEMPO_T0_BASE_FEE); 3]
        );
        assert_eq!(history.gas_used_ratio, vec![0.0; 2]);
        assert_eq!(history.base_fee_per_blob_gas, vec![0; 3]);
        assert_eq!(history.blob_gas_used_ratio, vec![0.0; 2]);
        assert_eq!(history.reward, Some(vec![vec![0, 0], vec![0, 0]]));
    }

    #[test]
    fn apply_public_fee_policy_prefills_missing_fees() {
        let mut request = TempoTransactionRequest::default();

        apply_public_fee_policy(&mut request);

        assert_eq!(request.gas_price(), None);
        assert_eq!(
            request.max_fee_per_gas(),
            Some(u128::from(TEMPO_T0_BASE_FEE))
        );
        assert_eq!(request.max_priority_fee_per_gas(), Some(0));
    }

    #[test]
    fn apply_public_fee_policy_prefills_legacy_gas_price() {
        let mut request = TempoTransactionRequest::default();
        request.inner.transaction_type = Some(0);

        apply_public_fee_policy(&mut request);

        assert_eq!(request.gas_price(), Some(u128::from(TEMPO_T0_BASE_FEE)));
        assert_eq!(request.max_fee_per_gas(), None);
        assert_eq!(request.max_priority_fee_per_gas(), None);
    }

    #[test]
    fn apply_public_fee_policy_preserves_supplied_priority_fee() {
        let mut request = TempoTransactionRequest::default();
        request.set_max_priority_fee_per_gas(7);

        apply_public_fee_policy(&mut request);

        assert_eq!(request.max_priority_fee_per_gas(), Some(7));
        assert_eq!(
            request.max_fee_per_gas(),
            Some(u128::from(TEMPO_T0_BASE_FEE) + 7)
        );
    }

    #[test]
    fn apply_public_fee_policy_prefills_blob_fee() {
        let mut request = TempoTransactionRequest::default();
        request.inner.blob_versioned_hashes = Some(Vec::new());

        apply_public_fee_policy(&mut request);

        assert_eq!(request.inner.max_fee_per_blob_gas, Some(0));
    }

    #[test]
    fn redact_header_clears_activity_metadata() {
        let mut header = TempoHeaderResponse {
            inner: alloy_rpc_types_eth::Header {
                hash: B256::with_last_byte(7),
                inner: tempo_primitives::TempoHeader {
                    inner: alloy_consensus::Header {
                        gas_used: 123,
                        state_root: B256::with_last_byte(1),
                        transactions_root: B256::with_last_byte(2),
                        receipts_root: B256::with_last_byte(3),
                        extra_data: Bytes::from_static(b"private"),
                        blob_gas_used: Some(4),
                        excess_blob_gas: Some(5),
                        withdrawals_root: Some(B256::with_last_byte(6)),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                size: Some(U256::from(8)),
                ..Default::default()
            },
            timestamp_millis: 123_000,
        };

        redact_header(&mut header);

        let inner = &header.inner.inner.inner;
        assert_eq!(header.inner.hash, B256::with_last_byte(7));
        assert_eq!(header.inner.size, Some(U256::ZERO));
        assert_eq!(header.timestamp_millis, 123_000);
        assert_eq!(inner.gas_used, 0);
        assert_eq!(inner.state_root, B256::ZERO);
        assert_eq!(inner.transactions_root, B256::ZERO);
        assert_eq!(inner.receipts_root, B256::ZERO);
        assert_eq!(inner.logs_bloom, Bloom::ZERO);
        assert!(inner.extra_data.is_empty());
        assert_eq!(inner.blob_gas_used, Some(0));
        assert_eq!(inner.excess_blob_gas, Some(0));
        assert_eq!(inner.withdrawals_root, Some(B256::ZERO));
    }

    #[test]
    fn stale_filter_owner_ids_removes_only_inactive_entries() {
        let active_ids = HashSet::from([
            FilterId::Str("0xactive".to_string()),
            FilterId::Str("0xkeep".to_string()),
        ]);
        let owner_ids = vec![
            FilterId::Str("0xactive".to_string()),
            FilterId::Str("0xstale".to_string()),
            FilterId::Str("0xkeep".to_string()),
        ];

        let stale_ids = stale_filter_owner_ids(owner_ids, &active_ids);

        assert_eq!(stale_ids, vec![FilterId::Str("0xstale".to_string())]);
    }

    #[test]
    fn stale_filter_owner_ids_is_noop_for_empty_owner_set() {
        let stale_ids = stale_filter_owner_ids(Vec::new(), &HashSet::new());

        assert!(stale_ids.is_empty());
    }
}
