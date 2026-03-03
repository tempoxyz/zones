//! Tempo Zone Node configuration.
//!
//! This is a lightweight L2 node built on reth's node builder infrastructure.
//! It reuses Tempo's EVM, primitives, and pool, but with noop consensus/network/payload.

use crate::{
    ext::TempoStateExt,
    payload::{ZonePayloadAttributes, ZonePayloadTypes},
};
use alloy_primitives::U256;
use reth_eth_wire_types::primitives::BasicNetworkPrimitives;
use reth_node_api::{
    AddOnsContext, FullNodeComponents, FullNodeTypes, InvalidPayloadAttributesError,
    NewPayloadError, NodeAddOns, NodeTypes, PayloadAttributesBuilder, PayloadTypes,
    PayloadValidator,
};
use reth_node_builder::{
    BuilderContext, DebugNode, Node, NodeAdapter,
    components::{
        BasicPayloadServiceBuilder, ComponentsBuilder, ExecutorBuilder, NoopConsensusBuilder,
        NoopNetworkBuilder, PoolBuilder, TxPoolBuilder, spawn_maintenance_tasks,
    },
    rpc::{
        BasicEngineValidatorBuilder, EngineValidatorAddOn, EthApiBuilder, EthApiCtx,
        NoopEngineApiBuilder, PayloadValidatorBuilder, RethRpcAddOns, RpcAddOns,
    },
};
use reth_provider::ChainSpecProvider;
use reth_rpc::DynRpcConverter;
use reth_rpc_builder::Identity;
use reth_rpc_eth_api::RpcConverter;
use reth_storage_api::{EmptyBodyStorage, StateProviderFactory};
use reth_transaction_pool::{TransactionValidationTaskExecutor, blobstore::InMemoryBlobStore};
use std::{default::Default, sync::Arc};
use tempo_alloy::TempoNetwork;
use tempo_chainspec::spec::TempoChainSpec;
use tempo_evm::TempoEvmConfig;
use tempo_node::{
    DEFAULT_AA_VALID_AFTER_MAX_SECS, engine::TempoEngineValidator, rpc::TempoReceiptConverter,
};
use tempo_primitives::{TempoHeader, TempoPrimitives, TempoTxEnvelope, TempoTxType};
use tempo_transaction_pool::{
    AA2dPool, AA2dPoolConfig, TempoTransactionPool,
    amm::AmmLiquidityCache,
    validator::{DEFAULT_MAX_TEMPO_AUTHORIZATIONS, TempoTransactionValidator},
};
use tracing::{debug, error, info};

use alloy_provider::Provider as _;

use crate::{
    ZoneEngine,
    evm::ZoneEvmConfig,
    l1::L1Subscriber,
    l1_state::{
        L1StateListenerConfig, L1StateProvider, L1StateProviderConfig, SharedL1StateCache,
        spawn_l1_state_listener,
    },
};

use crate::builder::ZonePayloadFactory;

/// Network primitives for Zone.
type ZoneNetworkPrimitives = BasicNetworkPrimitives<TempoPrimitives, TempoTxEnvelope>;

/// Tempo Zone node type configuration.
///
/// Uses Tempo primitives, EVM, and pool, but with noop consensus/network/payload.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ZoneNode {
    /// Queue of L1 deposit messages to be included in the next zone block.
    deposit_queue: crate::DepositQueue,
    /// Configuration for the L1 event subscriber (RPC endpoint, poll interval, etc.).
    l1_config: crate::L1SubscriberConfig,
    /// Configuration for the L1 state provider (contract addresses, query parameters).
    l1_state_provider_config: L1StateProviderConfig,
    /// Configuration for the L1 state listener (WebSocket URL, tracked events).
    l1_state_listener_config: L1StateListenerConfig,
    /// Shared L1 state cache (enabled tokens, zone metadata, etc.).
    l1_state_cache: SharedL1StateCache,
    /// Shared TIP-403 policy cache, populated by the [`PolicyListener`](crate::l1_state::PolicyListener)
    /// and read by the precompile during block building.
    policy_cache: crate::SharedPolicyCache,
    /// Address of the zone sequencer (fee recipient, authorized block producer).
    sequencer: alloy_primitives::Address,
    /// Sequencer's secp256k1 secret key for ECIES decryption of encrypted deposits.
    sequencer_key: k256::SecretKey,
    /// Address of the L1 deposit portal contract.
    portal_address: alloy_primitives::Address,
    /// Optional pre-configured list of enabled token addresses. When set, the
    /// startup L1 RPC query for `enabledTokenCount`/`enabledTokens` is skipped.
    initial_tokens: Option<Vec<alloy_primitives::Address>>,
}

impl ZoneNode {
    /// Create a new zone node from minimal configuration.
    ///
    /// Internally constructs the [`DepositQueue`], [`L1SubscriberConfig`],
    /// [`L1StateProviderConfig`], [`L1StateListenerConfig`], and [`SharedL1StateCache`]
    /// from the provided parameters — callers don't need to know about these internals.
    ///
    /// The L1 subscriber is spawned automatically as part of the node lifecycle via
    /// [`ZoneAddOns::launch_add_ons`], ensuring it cannot be accidentally omitted.
    /// The L1 state listener is spawned by [`ZoneExecutorBuilder`] during EVM construction.
    pub fn new(
        l1_rpc_url: String,
        portal_address: alloy_primitives::Address,
        genesis_tempo_block_number: Option<u64>,
        sequencer: alloy_primitives::Address,
        sequencer_key: k256::SecretKey,
    ) -> Self {
        let deposit_queue = crate::DepositQueue::default();

        let l1_config = crate::L1SubscriberConfig {
            l1_rpc_url: l1_rpc_url.clone(),
            portal_address,
            genesis_tempo_block_number,
            local_tempo_block_number: 0, // resolved at launch from local state
        };

        let l1_state_provider_config = L1StateProviderConfig {
            l1_rpc_url: l1_rpc_url.clone(),
            ..Default::default()
        };

        let l1_state_listener_config = L1StateListenerConfig {
            l1_ws_url: l1_rpc_url,
            ..Default::default()
        };

        let l1_state_cache =
            SharedL1StateCache::new(std::collections::HashSet::from([portal_address]));

        Self {
            deposit_queue,
            l1_config,
            l1_state_provider_config,
            l1_state_listener_config,
            l1_state_cache,
            policy_cache: crate::SharedPolicyCache::default(),
            sequencer,
            sequencer_key,
            portal_address,
            initial_tokens: None,
        }
    }

    /// Set the initial list of enabled token addresses.
    ///
    /// When set, the startup L1 RPC query for enabled tokens is skipped —
    /// useful for tests or environments where no L1 node is available at launch.
    pub fn with_initial_tokens(mut self, tokens: Vec<alloy_primitives::Address>) -> Self {
        self.initial_tokens = Some(tokens);
        self
    }

    /// Returns a clone of the deposit queue handle for external use (e.g. sequencer tasks).
    pub fn deposit_queue(&self) -> crate::DepositQueue {
        self.deposit_queue.clone()
    }

    /// Returns a clone of the shared L1 state cache handle.
    ///
    /// Allows pre-populating the cache for testing scenarios where no real L1 is available.
    pub fn l1_state_cache(&self) -> SharedL1StateCache {
        self.l1_state_cache.clone()
    }

    /// Returns a clone of the shared policy cache handle.
    pub fn policy_cache(&self) -> crate::SharedPolicyCache {
        self.policy_cache.clone()
    }

    /// Returns a [`ComponentsBuilder`] configured for a Zone node.
    pub fn components<N>(
        executor_builder: ZoneExecutorBuilder,
    ) -> ComponentsBuilder<
        N,
        ZonePoolBuilder,
        BasicPayloadServiceBuilder<ZonePayloadFactory>,
        NoopNetworkBuilder<ZoneNetworkPrimitives>,
        ZoneExecutorBuilder,
        NoopConsensusBuilder,
    >
    where
        N: FullNodeTypes<Types = Self>,
    {
        ComponentsBuilder::default()
            .node_types::<N>()
            .pool(ZonePoolBuilder)
            .executor(executor_builder)
            .payload(BasicPayloadServiceBuilder::new(ZonePayloadFactory))
            .network(NoopNetworkBuilder::<ZoneNetworkPrimitives>::default())
            .noop_consensus()
    }
}

impl NodeTypes for ZoneNode {
    type Primitives = TempoPrimitives;
    type ChainSpec = TempoChainSpec;
    type Storage = EmptyBodyStorage<TempoTxEnvelope, TempoHeader>;
    type Payload = ZonePayloadTypes;
}

/// Zone node add-ons — spawns all background tasks and wires the block production pipeline.
///
/// [`launch_add_ons`](NodeAddOns::launch_add_ons) is the central wiring point for the zone
/// sequencer. It spawns the following tasks:
///
/// ## Spawned tasks
///
/// - **[`L1Subscriber`](crate::L1Subscriber)** *(critical)* — subscribes to L1 blocks via
///   WebSocket, extracts deposit and token events from the ZonePortal, and enqueues them
///   into the [`DepositQueue`](crate::DepositQueue).
///
/// - **[`PolicyListener`](crate::l1_state::PolicyListener)** *(critical)* — subscribes to L1
///   for TIP-403 policy events (membership changes, policy type changes, token policy
///   assignments) and writes them into the [`SharedPolicyCache`](crate::SharedPolicyCache).
///
/// - **[`PolicyResolutionTask`](crate::l1_state::PolicyResolutionTask)** *(critical)* —
///   receives pre-fetch requests via a channel and resolves them concurrently against L1
///   RPC, populating the [`SharedPolicyCache`](crate::SharedPolicyCache) so the engine
///   hits the cache at block-building time.
///
/// - **Pool prefetch task** *(background)* — watches incoming pool transactions, extracts
///   sender/recipient/fee-payer addresses, and submits them to the
///   [`PolicyResolutionTask`](crate::l1_state::PolicyResolutionTask) for pre-fetching.
///
/// - **[`ZoneEngine`](crate::ZoneEngine)** *(critical)* — drives L1-event-driven block
///   production. Peeks the [`DepositQueue`](crate::DepositQueue), calls
///   [`prepare_l1_block`](crate::ZoneEngine::prepare_l1_block) (ECIES decryption +
///   TIP-403 policy checks via [`PolicyProvider`](crate::PolicyProvider)), then triggers
///   payload building via FCU and submits via `newPayload`.
///
/// ## Shared state
///
/// The [`SharedPolicyCache`](crate::SharedPolicyCache) connects all policy-aware
/// components: the listener writes L1 events, the resolution task pre-fetches on
/// cache miss, and the engine's [`PolicyProvider`](crate::PolicyProvider) reads
/// during block preparation (cache-first, L1 RPC fallback).
pub struct ZoneAddOns<N: FullNodeComponents<Types = ZoneNode, Evm = ZoneEvmConfig>> {
    inner: RpcAddOns<
        N,
        ZoneEthApiBuilder,
        ZoneEngineValidatorBuilder,
        NoopEngineApiBuilder,
        BasicEngineValidatorBuilder<ZoneEngineValidatorBuilder>,
        Identity,
    >,
    /// Queue of L1 deposit messages to be included in the next zone block.
    deposit_queue: crate::DepositQueue,
    /// Configuration for the L1 event subscriber (RPC endpoint, poll interval, etc.).
    l1_config: crate::L1SubscriberConfig,
    /// Address that receives zone block fees (the sequencer's fee vault).
    fee_recipient: alloy_primitives::Address,
    /// Shared TIP-403 policy cache, populated by the [`PolicyListener`](crate::l1_state::PolicyListener)
    /// and read by the precompile during block building.
    policy_cache: crate::SharedPolicyCache,
    /// Sequencer's secp256k1 secret key for ECIES decryption of encrypted deposits.
    sequencer_key: k256::SecretKey,
    /// ZonePortal address on L1 — used as context in HKDF key derivation.
    portal_address: alloy_primitives::Address,
    /// Optional pre-configured list of enabled token addresses.
    initial_tokens: Option<Vec<alloy_primitives::Address>>,
}

impl<N: FullNodeComponents<Types = ZoneNode, Evm = ZoneEvmConfig>> std::fmt::Debug
    for ZoneAddOns<N>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZoneAddOns").finish_non_exhaustive()
    }
}

impl<N> ZoneAddOns<NodeAdapter<N>>
where
    N: FullNodeTypes<Types = ZoneNode>,
{
    /// Creates a new instance.
    pub fn new(
        deposit_queue: crate::DepositQueue,
        l1_config: crate::L1SubscriberConfig,
        fee_recipient: alloy_primitives::Address,
        policy_cache: crate::SharedPolicyCache,
        sequencer_key: k256::SecretKey,
        portal_address: alloy_primitives::Address,
        initial_tokens: Option<Vec<alloy_primitives::Address>>,
    ) -> Self {
        Self {
            inner: RpcAddOns::new(
                ZoneEthApiBuilder::default(),
                ZoneEngineValidatorBuilder,
                NoopEngineApiBuilder::default(),
                BasicEngineValidatorBuilder::default(),
                Identity::default(),
            ),
            deposit_queue,
            l1_config,
            fee_recipient,
            policy_cache,
            sequencer_key,
            portal_address,
            initial_tokens,
        }
    }
}

impl<N> NodeAddOns<N> for ZoneAddOns<N>
where
    N: FullNodeComponents<Types = ZoneNode, Evm = ZoneEvmConfig>,
    N::Pool: reth_transaction_pool::TransactionPool<
            Transaction = tempo_transaction_pool::transaction::TempoPooledTransaction,
        >,
    ZoneEthApiBuilder: EthApiBuilder<N>,
{
    type Handle = <RpcAddOns<
        N,
        ZoneEthApiBuilder,
        ZoneEngineValidatorBuilder,
        NoopEngineApiBuilder,
        BasicEngineValidatorBuilder<ZoneEngineValidatorBuilder>,
        Identity,
    > as NodeAddOns<N>>::Handle;

    async fn launch_add_ons(mut self, ctx: AddOnsContext<'_, N>) -> eyre::Result<Self::Handle> {
        // Read the zone's current tempoBlockNumber from local state so the L1
        // subscriber knows exactly where to resume backfill.
        let sp = ctx.node.provider().latest()?;
        let tempo_block_number = sp.tempo_block_number()?;
        self.l1_config.local_tempo_block_number = tempo_block_number;
        self.policy_cache.set_last_l1_block(tempo_block_number);
        info!(target: "reth::cli", tempo_block_number, "Read local tempoBlockNumber for L1 subscriber");

        // Capture fields needed after `self.l1_config` is moved into the subscriber.
        let l1_url = self.l1_config.l1_rpc_url.clone();
        let portal_address = self.l1_config.portal_address;

        // Spawn L1 deposit subscriber
        L1Subscriber::spawn(
            self.l1_config,
            self.deposit_queue.clone(),
            ctx.node.task_executor().clone(),
        );
        info!(target: "reth::cli", "L1 deposit subscriber started as part of node lifecycle");

        // Resolve enabled tokens — use pre-configured list if available, otherwise
        // query L1 via RPC as a fallback.
        let tracked_tokens = if let Some(tokens) = self.initial_tokens.take() {
            info!(target: "reth::cli", count = tokens.len(), ?tokens, "Using pre-configured initial tokens");
            tokens
        } else {
            let l1_provider = alloy_provider::ProviderBuilder::new_with_network::<TempoNetwork>()
                .connect(&l1_url)
                .await?;
            let tokens = crate::bindings::enabled_tokens(portal_address, &l1_provider).await?;
            info!(target: "reth::cli", count = tokens.len(), ?tokens, "Discovered enabled tokens from L1");
            tokens
        };

        // Spawn TIP-403 policy listener to keep policy cache in sync with L1
        let engine_l1_url = l1_url.clone();
        crate::l1_state::spawn_policy_listener(
            crate::l1_state::PolicyListenerConfig {
                l1_ws_url: l1_url.clone(),
                portal_address,
                tracked_tokens,
            },
            self.policy_cache.clone(),
            ctx.node.task_executor().clone(),
        );
        info!(target: "reth::cli", "TIP-403 policy listener started");

        // Spawn policy resolution task for pre-fetching authorization data from L1.
        // The pool prefetch task feeds it with sender/recipient addresses from
        // incoming transactions so the cache is warm by the time we build blocks.
        let policy_task_handle = crate::l1_state::spawn_policy_resolution_task(
            self.policy_cache.clone(),
            l1_url.clone(),
            16,  // max concurrent RPC resolutions
            256, // channel capacity
            ctx.node.task_executor().clone(),
        );
        crate::l1_state::spawn_pool_prefetch_task(
            ctx.node.pool().clone(),
            policy_task_handle,
            ctx.node.task_executor().clone(),
        );
        info!(target: "reth::cli", "TIP-403 policy prefetch tasks started");

        // Spawn the ZoneEngine — L1-event-driven block production
        {
            let provider = ctx.node.provider().clone();
            let to_engine = ctx.beacon_engine_handle.clone();
            let payload_builder = ctx.node.payload_builder_handle().clone();
            let deposit_queue = self.deposit_queue;
            let fee_recipient = self.fee_recipient;
            let sequencer_key = self.sequencer_key;
            let portal_address = self.portal_address;
            let l1_url = engine_l1_url;
            let policy_cache = self.policy_cache;

            ctx.node
                .task_executor()
                .spawn_critical("zone-engine", async move {
                    // Connect to L1 for policy resolution (cache-first, RPC fallback).
                    let l1_provider =
                        match alloy_provider::ProviderBuilder::new_with_network::<TempoNetwork>()
                            .connect(&l1_url)
                            .await
                        {
                            Ok(p) => p.erased(),
                            Err(e) => {
                                error!(target: "reth::cli", %e, "Failed to connect to L1 for policy provider in ZoneEngine");
                                return;
                            }
                        };
                    let policy_provider = crate::l1_state::PolicyProvider::new(
                        policy_cache,
                        l1_provider,
                        tokio::runtime::Handle::current(),
                    );

                    ZoneEngine::new(
                        provider,
                        to_engine,
                        payload_builder,
                        deposit_queue,
                        fee_recipient,
                        sequencer_key,
                        portal_address,
                        policy_provider,
                    )
                    .run()
                    .await
                });
            info!(target: "reth::cli", "ZoneEngine spawned — L1-driven block production active");
        }

        self.inner.launch_add_ons(ctx).await
    }
}

impl<N> RethRpcAddOns<N> for ZoneAddOns<N>
where
    N: FullNodeComponents<Types = ZoneNode, Evm = ZoneEvmConfig>,
    N::Pool: reth_transaction_pool::TransactionPool<
            Transaction = tempo_transaction_pool::transaction::TempoPooledTransaction,
        >,
    ZoneEthApiBuilder: EthApiBuilder<N>,
{
    type EthApi = <ZoneEthApiBuilder as EthApiBuilder<N>>::EthApi;

    fn hooks_mut(&mut self) -> &mut reth_node_builder::rpc::RpcHooks<N, Self::EthApi> {
        self.inner.hooks_mut()
    }
}

impl<N> EngineValidatorAddOn<N> for ZoneAddOns<N>
where
    N: FullNodeComponents<Types = ZoneNode, Evm = ZoneEvmConfig>,
    N::Pool: reth_transaction_pool::TransactionPool<
            Transaction = tempo_transaction_pool::transaction::TempoPooledTransaction,
        >,
    ZoneEthApiBuilder: EthApiBuilder<N>,
{
    type ValidatorBuilder = BasicEngineValidatorBuilder<ZoneEngineValidatorBuilder>;

    fn engine_validator_builder(&self) -> Self::ValidatorBuilder {
        self.inner.engine_validator_builder()
    }
}

impl<N> Node<N> for ZoneNode
where
    N: FullNodeTypes<Types = Self>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        ZonePoolBuilder,
        BasicPayloadServiceBuilder<ZonePayloadFactory>,
        NoopNetworkBuilder<ZoneNetworkPrimitives>,
        ZoneExecutorBuilder,
        NoopConsensusBuilder,
    >;
    type AddOns = ZoneAddOns<NodeAdapter<N>>;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        let executor_builder = ZoneExecutorBuilder::new(
            self.l1_state_provider_config.clone(),
            self.l1_state_listener_config.clone(),
            self.l1_state_cache.clone(),
            self.policy_cache.clone(),
        );
        Self::components(executor_builder)
    }

    fn add_ons(&self) -> Self::AddOns {
        ZoneAddOns::new(
            self.deposit_queue.clone(),
            self.l1_config.clone(),
            self.sequencer,
            self.policy_cache.clone(),
            self.sequencer_key.clone(),
            self.portal_address,
            self.initial_tokens.clone(),
        )
    }
}

impl<N: FullNodeComponents<Types = Self>> DebugNode<N> for ZoneNode {
    type RpcBlock =
        alloy_rpc_types_eth::Block<alloy_rpc_types_eth::Transaction<TempoTxEnvelope>, TempoHeader>;

    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> tempo_primitives::Block {
        rpc_block
            .into_consensus_block()
            .map_transactions(|tx| tx.into_inner())
    }

    fn local_payload_attributes_builder(
        chain_spec: &Self::ChainSpec,
    ) -> impl PayloadAttributesBuilder<<Self::Payload as PayloadTypes>::PayloadAttributes, TempoHeader>
    {
        ZonePayloadAttributesBuilder::new(Arc::new(chain_spec.clone()))
    }
}

/// Builds [`ZonePayloadAttributes`] with `l1_block: None` — suitable for
/// debug/test scenarios where no L1 data is available.
#[derive(Debug)]
pub(crate) struct ZonePayloadAttributesBuilder;

impl ZonePayloadAttributesBuilder {
    pub(crate) fn new(_chain_spec: Arc<TempoChainSpec>) -> Self {
        Self
    }
}

impl PayloadAttributesBuilder<ZonePayloadAttributes, TempoHeader> for ZonePayloadAttributesBuilder {
    fn build(
        &self,
        _parent: &reth_primitives_traits::SealedHeader<TempoHeader>,
    ) -> ZonePayloadAttributes {
        unimplemented!("zone blocks require L1 data — use ZoneEngine instead")
    }
}

/// Executor builder for Zone — constructs the [`ZoneEvmConfig`] used during block execution.
///
/// Called once during node startup by the reth component builder. It:
///
/// 1. Creates the [`L1StateProvider`] (cache-first, RPC-fallback reader for L1 contract
///    storage) and connects it to the [`SharedL1StateCache`].
/// 2. Spawns the [`L1StateListener`](crate::l1_state::L1StateListener), which subscribes
///    to L1 chain notifications and keeps the storage cache up to date.
/// 3. Creates a [`PolicyProvider`](crate::PolicyProvider) for the TIP-403 proxy precompile,
///    giving the EVM synchronous access to policy authorization checks during execution.
/// 4. Returns a [`ZoneEvmConfig`] with the TempoStateReader and TIP-403 proxy precompiles
///    wired in.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ZoneExecutorBuilder {
    l1_state_provider_config: L1StateProviderConfig,
    l1_state_listener_config: L1StateListenerConfig,
    l1_state_cache: SharedL1StateCache,
    policy_cache: crate::SharedPolicyCache,
}

impl ZoneExecutorBuilder {
    pub fn new(
        l1_state_provider_config: L1StateProviderConfig,
        l1_state_listener_config: L1StateListenerConfig,
        l1_state_cache: SharedL1StateCache,
        policy_cache: crate::SharedPolicyCache,
    ) -> Self {
        Self {
            l1_state_provider_config,
            l1_state_listener_config,
            l1_state_cache,
            policy_cache,
        }
    }
}

impl<Node> ExecutorBuilder<Node> for ZoneExecutorBuilder
where
    Node: FullNodeTypes<Types = ZoneNode>,
{
    type EVM = ZoneEvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        let runtime_handle = tokio::runtime::Handle::current();
        let l1_provider = L1StateProvider::new(
            self.l1_state_provider_config.clone(),
            self.l1_state_cache.clone(),
            runtime_handle.clone(),
        )
        .await?;

        spawn_l1_state_listener(
            self.l1_state_listener_config,
            self.l1_state_cache,
            ctx.task_executor().clone(),
        );

        let mut evm_config = ZoneEvmConfig::new(ctx.chain_spec(), l1_provider);

        // Create PolicyProvider for the TIP-403 proxy precompile.
        let policy_l1 = alloy_provider::ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&self.l1_state_provider_config.l1_rpc_url)
            .await?
            .erased();

        let policy_provider = crate::l1_state::PolicyProvider::new(
            self.policy_cache,
            policy_l1,
            runtime_handle,
        );
        evm_config = evm_config.with_policy_provider(policy_provider);
        info!(target: "reth::cli", "Zone EVM initialized with TempoStateReader + TIP-403 proxy precompiles");

        Ok(evm_config)
    }
}

/// Engine validator builder for Zone.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct ZoneEngineValidatorBuilder;

impl<Node> PayloadValidatorBuilder<Node> for ZoneEngineValidatorBuilder
where
    Node: FullNodeComponents<Types = ZoneNode>,
{
    type Validator = TempoEngineValidator;

    async fn build(self, _ctx: &AddOnsContext<'_, Node>) -> eyre::Result<Self::Validator> {
        Ok(TempoEngineValidator::new())
    }
}

impl PayloadValidator<ZonePayloadTypes> for TempoEngineValidator {
    type Block = tempo_primitives::Block;

    fn convert_payload_to_block(
        &self,
        payload: tempo_payload_types::TempoExecutionData,
    ) -> Result<reth_primitives_traits::SealedBlock<Self::Block>, NewPayloadError> {
        let tempo_payload_types::TempoExecutionData {
            block,
            validator_set: _,
        } = payload;
        Ok(Arc::unwrap_or_clone(block))
    }

    fn validate_payload_attributes_against_header(
        &self,
        attr: &crate::payload::ZonePayloadAttributes,
        header: &TempoHeader,
    ) -> Result<(), InvalidPayloadAttributesError> {
        if reth_node_api::PayloadAttributes::timestamp(attr)
            < reth_primitives_traits::AlloyBlockHeader::timestamp(header)
        {
            return Err(InvalidPayloadAttributesError::InvalidTimestamp);
        }
        Ok(())
    }
}

/// Transaction pool builder for Zone - uses Tempo pool with defaults.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct ZonePoolBuilder;

impl<Node> PoolBuilder<Node, ZoneEvmConfig> for ZonePoolBuilder
where
    Node: FullNodeTypes<Types = ZoneNode>,
{
    type Pool = TempoTransactionPool<Node::Provider>;

    async fn build_pool(
        self,
        ctx: &BuilderContext<Node>,
        _evm_config: ZoneEvmConfig,
    ) -> eyre::Result<Self::Pool> {
        let mut pool_config = ctx.pool_config();
        pool_config.max_inflight_delegated_slot_limit = pool_config.max_account_slots;

        // this store is effectively a noop
        let blob_store = InMemoryBlobStore::default();
        let tempo_evm_config = TempoEvmConfig::new(ctx.chain_spec());
        let validator = TransactionValidationTaskExecutor::eth_builder(
            ctx.provider().clone(),
            tempo_evm_config,
        )
        .with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
        .with_local_transactions_config(pool_config.local_transactions_config.clone())
        .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
        .with_max_tx_gas_limit(ctx.config().txpool.max_tx_gas_limit)
        .set_block_gas_limit(ctx.chain_spec().inner.genesis().gas_limit)
        .disable_balance_check()
        .with_minimum_priority_fee(ctx.config().txpool.minimum_priority_fee)
        .with_additional_tasks(ctx.config().txpool.additional_validation_tasks)
        .with_custom_tx_type(TempoTxType::AA as u8)
        .no_eip4844()
        .build_with_tasks(ctx.task_executor().clone(), blob_store.clone());

        let aa_2d_config = AA2dPoolConfig {
            price_bump_config: pool_config.price_bumps,
            pending_limit: pool_config.pending_limit,
            queued_limit: pool_config.queued_limit,
            max_txs_per_sender: pool_config.max_account_slots,
        };
        let aa_2d_pool = AA2dPool::new(aa_2d_config);
        let amm_liquidity_cache = AmmLiquidityCache::new(ctx.provider())?;

        let validator = validator.map(|v| {
            TempoTransactionValidator::new(
                v,
                DEFAULT_AA_VALID_AFTER_MAX_SECS,
                DEFAULT_MAX_TEMPO_AUTHORIZATIONS,
                amm_liquidity_cache.clone(),
            )
        });
        let protocol_pool = TxPoolBuilder::new(ctx)
            .with_validator(validator)
            .build(blob_store, pool_config.clone());

        let transaction_pool = TempoTransactionPool::new(protocol_pool, aa_2d_pool);

        spawn_maintenance_tasks(ctx, transaction_pool.clone(), &pool_config)?;

        // Spawn unified Tempo pool maintenance task
        // This consolidates: expired AA txs, 2D nonce updates, AMM cache, and keychain revocations
        ctx.task_executor().spawn_critical(
            "txpool maintenance - tempo pool",
            tempo_transaction_pool::maintain::maintain_tempo_pool(transaction_pool.clone()),
        );

        info!(target: "reth::cli", "Transaction pool initialized");
        debug!(target: "reth::cli", "Spawned txpool maintenance task");

        Ok(transaction_pool)
    }
}

/// EthApi builder for Zone - uses Tempo RPC types.
#[derive(Debug, Default, Clone)]
pub struct ZoneEthApiBuilder;

impl<N> EthApiBuilder<N> for ZoneEthApiBuilder
where
    N: FullNodeComponents<Types = ZoneNode, Evm = ZoneEvmConfig>,
{
    type EthApi = reth_rpc::EthApi<N, DynRpcConverter<ZoneEvmConfig, TempoNetwork>>;

    async fn build_eth_api(self, ctx: EthApiCtx<'_, N>) -> eyre::Result<Self::EthApi> {
        let chain_spec = ctx.components.provider().chain_spec();
        let eth_api = ctx
            .eth_api_builder()
            .modify_gas_oracle_config(|config| config.default_suggested_fee = Some(U256::ZERO))
            .map_converter(|_| RpcConverter::new(TempoReceiptConverter::new(chain_spec)).erased())
            .build();

        Ok(eth_api)
    }
}
