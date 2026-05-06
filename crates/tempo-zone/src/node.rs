//! Tempo Zone Node configuration.
//!
//! This is a lightweight L2 node built on reth's node builder infrastructure.
//! It reuses Tempo's EVM, primitives, and pool, but with noop consensus/network/payload.

use crate::{
    BatchAnchorConfig, DepositQueue, L1SubscriberConfig, SharedPolicyCache, ZoneEngine,
    ZoneSequencerConfig,
    abi::{TEMPO_STATE_ADDRESS, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS, ZonePortal},
    builder::ZonePayloadFactory,
    evm::ZoneEvmConfig,
    ext::TempoStateExt,
    l1::L1Subscriber,
    l1_state::{
        L1StateProvider, L1StateProviderConfig, PolicyProvider, SharedL1StateCache,
        spawn_policy_resolution_task, spawn_pool_prefetch_task,
    },
    payload::{ZonePayloadAttributes, ZonePayloadTypes},
    rpc::{TempoZoneRpc, ZoneRpcApi, start_private_rpc},
    rpc_connection_config, spawn_zone_sequencer,
};
use alloy_primitives::{Address, U256};
use alloy_provider::Provider as _;
use alloy_signer_local::PrivateKeySigner;
use k256::SecretKey;
use reth_eth_wire_types::primitives::BasicNetworkPrimitives;
use reth_node_api::{
    AddOnsContext, FullNodeComponents, FullNodeTypes, InvalidPayloadAttributesError,
    NewPayloadError, NodeAddOns, NodeTypes, PayloadAttributes, PayloadAttributesBuilder,
    PayloadTypes, PayloadValidator,
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
use reth_primitives_traits::{
    AlloyBlockHeader, SealedBlock, SealedHeader, transaction::error::InvalidTransactionError,
};
use reth_provider::ChainSpecProvider;
use reth_rpc::DynRpcConverter;
use reth_rpc_builder::Identity;
use reth_rpc_eth_api::{EthApiTypes, RpcConverter};
use reth_storage_api::{BlockNumReader, EmptyBodyStorage, HeaderProvider, StateProviderFactory};
use reth_transaction_pool::{
    TransactionValidationTaskExecutor, blobstore::InMemoryBlobStore,
    error::InvalidPoolTransactionError,
};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tempo_alloy::TempoNetwork;
use tempo_chainspec::spec::TempoChainSpec;
use tempo_evm::TempoEvmConfig;
use tempo_node::{
    DEFAULT_AA_VALID_AFTER_MAX_SECS, engine::TempoEngineValidator, rpc::TempoReceiptConverter,
};
use tempo_payload_types::TempoExecutionData;
use tempo_primitives::{
    self as primitives, TempoHeader, TempoPrimitives, TempoTxEnvelope, TempoTxType,
};
use tempo_transaction_pool::{
    AA2dPool, AA2dPoolConfig, TempoTransactionPool,
    amm::AmmLiquidityCache,
    transaction::TempoPooledTransaction,
    validator::{DEFAULT_MAX_TEMPO_AUTHORIZATIONS, TempoTransactionValidator},
};
use tracing::{debug, info};

/// Network primitives for Zone Nodes
type ZoneNetworkPrimitives = BasicNetworkPrimitives<TempoPrimitives, TempoTxEnvelope>;

/// Configuration for the sequencer background tasks
#[derive(Debug, Clone)]
pub struct ZoneSequencerAddOnsConfig {
    /// Sequencer private key signer for signing L1 transactions.
    pub sequencer_signer: PrivateKeySigner,
    /// Zone ID for chain ID validation.
    pub zone_id: u32,
    /// How often the zone monitor polls for new L2 blocks.
    pub zone_poll_interval: Duration,
    /// Maximum time to accumulate zone blocks before batch submission.
    pub batch_interval: Duration,
    /// EIP-2935 history and safety-margin limits used by the batch submitter.
    pub batch_anchor_config: BatchAnchorConfig,
    /// How often the withdrawal processor polls the L1 queue.
    pub withdrawal_poll_interval: Duration,
}

/// Configuration for the Zone private RPC server extension.
#[derive(Debug, Clone, Default)]
pub struct ZonePrivateRpcConfig {
    /// Port for RPC traffic.
    pub private_rpc_port: u16,
    /// Zone ID for chain ID validation and private RPC auth.
    pub zone_id: u32,
    /// Max duration for private RPC auth.
    pub max_auth_token_validity: Duration,
}

/// Tempo Zone node type configuration.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ZoneNode {
    /// Queue of L1 deposit messages to be included in the next zone block.
    deposit_queue: DepositQueue,
    /// Configuration for the L1 event subscriber (RPC endpoint, poll interval, etc.).
    l1_config: L1SubscriberConfig,
    /// Configuration for the L1 state provider (contract addresses, query parameters).
    l1_state_provider_config: L1StateProviderConfig,
    /// Shared L1 state cache (enabled tokens, zone metadata, etc.).
    l1_state_cache: SharedL1StateCache,
    /// Shared TIP-403 policy cache, populated by the unified [`L1Subscriber`](crate::l1::L1Subscriber)
    /// and read by the precompile during block building.
    policy_cache: SharedPolicyCache,
    /// Address of the L1 deposit portal contract.
    portal_address: Address,
    /// Optional pre-configured list of enabled token addresses. When set, the
    /// startup L1 RPC query for `enabledTokenCount`/`enabledTokens` is skipped.
    initial_tokens: Option<Vec<Address>>,
    /// Private RPC config.
    private_rpc_config: ZonePrivateRpcConfig,
    /// Optional sequencer config. When set, sequencer tasks are spawned.
    sequencer_config: Option<ZoneSequencerAddOnsConfig>,
}

impl ZoneNode {
    // Creates a new ZoneNode
    pub fn new(
        l1_rpc_url: String,
        portal_address: Address,
        genesis_tempo_block_number: Option<u64>,
        l1_fetch_concurrency: usize,
        retry_connection_interval: Duration,
    ) -> Self {
        let deposit_queue = DepositQueue::default();

        let policy_cache = SharedPolicyCache::default();
        let l1_state_cache = SharedL1StateCache::new(HashSet::from([portal_address]));
        let l1_config = L1SubscriberConfig {
            l1_rpc_url: l1_rpc_url.clone(),
            portal_address,
            genesis_tempo_block_number,
            policy_cache: policy_cache.clone(),
            l1_state_cache: l1_state_cache.clone(),
            l1_fetch_concurrency,
            retry_connection_interval,
        };

        let l1_state_provider_config = L1StateProviderConfig {
            l1_rpc_url,
            portal_address,
            retry_connection_interval,
            ..Default::default()
        };

        Self {
            deposit_queue,
            l1_config,
            l1_state_provider_config,
            l1_state_cache,
            policy_cache,
            portal_address,
            initial_tokens: None,
            private_rpc_config: ZonePrivateRpcConfig::default(),
            sequencer_config: None,
        }
    }

    /// Set the private RPC configuration.
    pub fn with_private_rpc(mut self, config: ZonePrivateRpcConfig) -> Self {
        self.private_rpc_config = config;
        self
    }

    /// Set the sequencer configuration. When set, batch submission and
    /// withdrawal processing tasks are spawned during node launch.
    pub fn with_sequencer(mut self, config: ZoneSequencerAddOnsConfig) -> Self {
        self.sequencer_config = Some(config);
        self
    }

    /// Set the initial list of enabled token addresses.
    /// When set, the startup L1 RPC query for enabled tokens is skipped.
    pub fn with_initial_tokens(mut self, tokens: Vec<Address>) -> Self {
        self.initial_tokens = Some(tokens);
        self
    }

    /// Returns the current deposit queue
    pub fn deposit_queue(&self) -> DepositQueue {
        self.deposit_queue.clone()
    }

    /// Returns the current l1 state cache
    pub fn l1_state_cache(&self) -> SharedL1StateCache {
        self.l1_state_cache.clone()
    }

    /// Returns the current TIP-403 policy cache
    pub fn policy_cache(&self) -> SharedPolicyCache {
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

/// Addons for Tempo Zone nodes.
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
    deposit_queue: DepositQueue,
    /// Configuration for the L1 event subscriber
    l1_config: L1SubscriberConfig,
    /// TIP-403 policy cache
    policy_cache: SharedPolicyCache,
    /// ZonePortal address on L1.
    portal_address: Address,
    /// Pre-configured list of initial tokens.
    initial_tokens: Option<Vec<Address>>,
    /// Private RPC configuration.
    private_rpc_config: ZonePrivateRpcConfig,
    /// Sequencer configuration.
    sequencer_config: Option<ZoneSequencerAddOnsConfig>,
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
    /// Creates a new ZoneAddOns instance.
    pub fn new(
        deposit_queue: DepositQueue,
        l1_config: L1SubscriberConfig,
        policy_cache: SharedPolicyCache,
        portal_address: Address,
        initial_tokens: Option<Vec<Address>>,
        private_rpc_config: ZonePrivateRpcConfig,
        sequencer_config: Option<ZoneSequencerAddOnsConfig>,
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
            policy_cache,
            portal_address,
            initial_tokens,
            private_rpc_config,
            sequencer_config,
        }
    }
}

impl<N> NodeAddOns<N> for ZoneAddOns<N>
where
    N: FullNodeComponents<Types = ZoneNode, Evm = ZoneEvmConfig>,
    N::Pool: reth_transaction_pool::TransactionPool<
            Transaction = tempo_transaction_pool::transaction::TempoPooledTransaction,
        >,
    ZoneEthApiBuilder: EthApiBuilder<N, EthApi: EthApiTypes<NetworkTypes = TempoNetwork>>,
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
        let sp = ctx.node.provider().latest()?;
        let tempo_block_number = sp.tempo_block_number()?;
        self.policy_cache.set_last_l1_block(tempo_block_number);
        info!(target: "reth::cli", tempo_block_number, "Read local tempoBlockNumber for L1 subscriber");

        let l1_provider = alloy_provider::ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_with_config(
                &self.l1_config.l1_rpc_url,
                rpc_connection_config(self.l1_config.retry_connection_interval),
            )
            .await?
            .erased();

        self.resolve_and_seed_tokens(&l1_provider).await?;
        self.spawn_l1_subscriber(&ctx);
        self.spawn_policy_tasks(&l1_provider, &ctx);

        if let Some(ref config) = self.sequencer_config {
            let sequencer_addr = config.sequencer_signer.address();
            let sequencer_key = SecretKey::from(config.sequencer_signer.credential());
            self.spawn_zone_engine(l1_provider, &ctx, sequencer_addr, sequencer_key)?;
        }

        let task_executor = ctx.node.task_executor().clone();

        let chain_id = ctx
            .node
            .provider()
            .chain_spec()
            .inner
            .genesis()
            .config
            .chain_id;
        let handle = self.inner.launch_add_ons(ctx).await?;

        Self::launch_private_rpc(
            self.private_rpc_config,
            &handle,
            self.l1_config.l1_rpc_url.clone(),
            self.l1_config.retry_connection_interval,
            self.l1_config.portal_address,
            chain_id,
        )
        .await?;

        if let Some(config) = self.sequencer_config.take() {
            let sequencer_addr = config.sequencer_signer.address();

            Self::launch_sequencer_tasks(
                config,
                &handle,
                &task_executor,
                self.l1_config.l1_rpc_url,
                self.l1_config.portal_address,
                self.l1_config.retry_connection_interval,
                sequencer_addr,
                chain_id,
            )
            .await?;
        }

        Ok(handle)
    }
}

impl<N> ZoneAddOns<N>
where
    N: FullNodeComponents<Types = ZoneNode, Evm = ZoneEvmConfig>,
    N::Pool: reth_transaction_pool::TransactionPool<
            Transaction = tempo_transaction_pool::transaction::TempoPooledTransaction,
        >,
    ZoneEthApiBuilder: EthApiBuilder<N, EthApi: EthApiTypes<NetworkTypes = TempoNetwork>>,
{
    /// Resolve enabled tokens and seed the policy cache.
    async fn resolve_and_seed_tokens(
        &mut self,
        l1_provider: &alloy_provider::DynProvider<TempoNetwork>,
    ) -> eyre::Result<()> {
        let portal = self.portal_address;
        let tracked_tokens = if let Some(tokens) = self.initial_tokens.take() {
            info!(target: "reth::cli", count = tokens.len(), ?tokens, "Using pre-configured initial tokens");
            tokens
        } else {
            let tokens = ZonePortal::new(portal, l1_provider)
                .enabled_tokens()
                .await?;
            info!(target: "reth::cli", count = tokens.len(), ?tokens, "Discovered enabled tokens from L1");
            tokens
        };

        self.policy_cache
            .seed_token_policies(portal, &tracked_tokens, l1_provider)
            .await?;
        info!(target: "reth::cli", "Seeded token policies from L1");
        Ok(())
    }

    /// Spawn the L1 subscriber. Listens for new blocks and deposit events.
    fn spawn_l1_subscriber(&mut self, ctx: &AddOnsContext<'_, N>) {
        L1Subscriber::spawn(
            self.l1_config.clone(),
            ctx.node.provider().clone(),
            self.deposit_queue.clone(),
            ctx.node.task_executor().clone(),
        );
        info!(target: "reth::cli", "Unified L1 subscriber started");
    }

    /// Spawn TIP-403 policy resolution and pool prefetch tasks.
    fn spawn_policy_tasks(
        &self,
        l1_provider: &alloy_provider::DynProvider<TempoNetwork>,
        ctx: &AddOnsContext<'_, N>,
    ) {
        let policy_task_handle = spawn_policy_resolution_task(
            self.policy_cache.clone(),
            l1_provider.clone(),
            16,
            256,
            ctx.node.task_executor().clone(),
        );
        spawn_pool_prefetch_task(
            ctx.node.pool().clone(),
            policy_task_handle,
            ctx.node.task_executor().clone(),
        );
        info!(target: "reth::cli", "TIP-403 policy prefetch tasks started");
    }

    /// Spawn the [`ZoneEngine`] for L1-event-driven block production.
    fn spawn_zone_engine(
        &self,
        l1_provider: alloy_provider::DynProvider<TempoNetwork>,
        ctx: &AddOnsContext<'_, N>,
        fee_recipient: Address,
        sequencer_key: SecretKey,
    ) -> eyre::Result<()> {
        let policy_provider = PolicyProvider::new(
            self.policy_cache.clone(),
            l1_provider,
            tokio::runtime::Handle::current(),
        );
        let provider = ctx.node.provider();
        let last_header = provider
            .sealed_header(provider.best_block_number()?)?
            .ok_or_else(|| eyre::eyre!("no latest block header"))?;
        let engine = ZoneEngine::new(
            provider.chain_spec(),
            ctx.beacon_engine_handle.clone(),
            ctx.node.payload_builder_handle().clone(),
            self.deposit_queue.clone(),
            last_header,
            fee_recipient,
            sequencer_key,
            self.portal_address,
            policy_provider,
        );
        ctx.node
            .task_executor()
            .spawn_critical_task("zone-engine", engine.run());
        info!(target: "reth::cli", "ZoneEngine spawned");
        Ok(())
    }

    /// Launch the private RPC server.
    async fn launch_private_rpc(
        config: ZonePrivateRpcConfig,
        handle: &<Self as NodeAddOns<N>>::Handle,
        l1_rpc_url: String,
        retry_connection_interval: Duration,
        portal_address: Address,
        chain_id: u64,
    ) -> eyre::Result<()> {
        if config.zone_id != 0 {
            let expected = zone_primitives::constants::zone_chain_id(config.zone_id);
            if chain_id != expected {
                eyre::bail!(
                    "chain ID mismatch: zone.id={} requires chain_id={}, but genesis has {}",
                    config.zone_id,
                    expected,
                    chain_id,
                );
            }
        }

        let eth_handlers = handle.eth_handlers().clone();
        let zone_rpc_url = handle
            .rpc_server_handles
            .rpc
            .http_url()
            .expect("HTTP RPC server must be enabled for private RPC");
        let private_rpc_config = zone_rpc::PrivateRpcConfig {
            listen_addr: ([0, 0, 0, 0], config.private_rpc_port).into(),
            l1_rpc_url,
            zone_rpc_url,
            retry_connection_interval,
            zone_id: config.zone_id,
            chain_id,
            max_auth_token_validity: config.max_auth_token_validity,
            zone_portal: portal_address,
        };
        let api: Arc<dyn ZoneRpcApi> =
            Arc::new(TempoZoneRpc::new(eth_handlers, private_rpc_config.clone()).await?);
        let local_addr = start_private_rpc(private_rpc_config, api).await?;
        info!(target: "reth::cli", %local_addr, "Private zone RPC server started");

        Ok(())
    }

    /// Launch sequencer background tasks: batch submission, withdrawal processing,
    /// and engine shutdown hook.
    async fn launch_sequencer_tasks(
        config: ZoneSequencerAddOnsConfig,
        handle: &<Self as NodeAddOns<N>>::Handle,
        task_executor: &reth_tasks::TaskExecutor,
        l1_rpc_url: String,
        portal_address: Address,
        retry_connection_interval: Duration,
        sequencer_addr: Address,
        chain_id: u64,
    ) -> eyre::Result<()> {
        if config.zone_id != 0 {
            let expected = zone_primitives::constants::zone_chain_id(config.zone_id);
            if chain_id != expected {
                eyre::bail!(
                    "chain ID mismatch: zone.id={} requires chain_id={}, but genesis has {}",
                    config.zone_id,
                    expected,
                    chain_id,
                );
            }
        }

        let zone_rpc_url = handle
            .rpc_server_handles
            .rpc
            .http_url()
            .expect("HTTP RPC server must be enabled for sequencer mode");

        info!(target: "reth::cli", %sequencer_addr, "Starting sequencer background tasks");
        let sequencer_config = ZoneSequencerConfig {
            portal_address,
            l1_rpc_url,
            retry_connection_interval,
            withdrawal_poll_interval: config.withdrawal_poll_interval,
            outbox_address: ZONE_OUTBOX_ADDRESS,
            inbox_address: ZONE_INBOX_ADDRESS,
            tempo_state_address: TEMPO_STATE_ADDRESS,
            zone_rpc_url,
            zone_poll_interval: config.zone_poll_interval,
            batch_interval: config.batch_interval,
            batch_anchor_config: config.batch_anchor_config,
        };
        let seq_handle = spawn_zone_sequencer(sequencer_config, config.sequencer_signer).await;
        info!(target: "reth::cli", "Sequencer tasks spawned");

        // Critical task — node shuts down if either exits.
        task_executor.spawn_critical_task("zone-monitor", async move {
            tokio::select! {
                res = seq_handle.withdrawal_handle => {
                    tracing::error!(target: "reth::cli", ?res, "Withdrawal processor task exited");
                }
                res = seq_handle.monitor_handle => {
                    tracing::error!(target: "reth::cli", ?res, "Zone monitor task exited");
                }
            }
        });

        // Flush unpersisted blocks on shutdown.
        let engine_shutdown = handle.engine_shutdown.clone();
        task_executor.spawn_critical_with_graceful_shutdown_signal(
            "zone-engine-shutdown",
            |shutdown| async move {
                let _guard = shutdown.await;
                info!(target: "reth::cli", "Shutdown signal received — flushing engine state");
                if let Some(done) = engine_shutdown.shutdown() {
                    let _ = done.await;
                }
            },
        );

        Ok(())
    }
}

impl<N> RethRpcAddOns<N> for ZoneAddOns<N>
where
    N: FullNodeComponents<Types = ZoneNode, Evm = ZoneEvmConfig>,
    N::Pool: reth_transaction_pool::TransactionPool<
            Transaction = tempo_transaction_pool::transaction::TempoPooledTransaction,
        >,
    ZoneEthApiBuilder:
        EthApiBuilder<N, EthApi: reth_rpc_eth_api::EthApiTypes<NetworkTypes = TempoNetwork>>,
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
    ZoneEthApiBuilder: EthApiBuilder<N, EthApi: EthApiTypes<NetworkTypes = TempoNetwork>>,
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
            self.l1_state_cache.clone(),
            self.policy_cache.clone(),
        );
        Self::components(executor_builder)
    }

    fn add_ons(&self) -> Self::AddOns {
        ZoneAddOns::new(
            self.deposit_queue.clone(),
            self.l1_config.clone(),
            self.policy_cache.clone(),
            self.portal_address,
            self.initial_tokens.clone(),
            self.private_rpc_config.clone(),
            self.sequencer_config.clone(),
        )
    }
}

impl<N: FullNodeComponents<Types = Self>> DebugNode<N> for ZoneNode {
    type RpcBlock =
        alloy_rpc_types_eth::Block<alloy_rpc_types_eth::Transaction<TempoTxEnvelope>, TempoHeader>;

    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> primitives::Block {
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
    fn build(&self, _parent: &SealedHeader<TempoHeader>) -> ZonePayloadAttributes {
        unimplemented!("zone blocks require L1 data — use ZoneEngine instead")
    }
}

/// Builder that constructs the [`ZoneEvmConfig`] used during block execution.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ZoneExecutorBuilder {
    l1_state_provider_config: L1StateProviderConfig,
    l1_state_cache: SharedL1StateCache,
    policy_cache: SharedPolicyCache,
}

impl ZoneExecutorBuilder {
    /// Create a zone executor builder with the shared L1 state/policy caches.
    pub fn new(
        l1_state_provider_config: L1StateProviderConfig,
        l1_state_cache: SharedL1StateCache,
        policy_cache: SharedPolicyCache,
    ) -> Self {
        Self {
            l1_state_provider_config,
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
            self.l1_state_cache,
            runtime_handle.clone(),
        )
        .await?;

        let mut evm_config = ZoneEvmConfig::new(ctx.chain_spec(), l1_provider);

        // Create PolicyProvider for the TIP-403 proxy precompile.
        let policy_l1 = alloy_provider::ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_with_config(
                &self.l1_state_provider_config.l1_rpc_url,
                rpc_connection_config(self.l1_state_provider_config.retry_connection_interval),
            )
            .await?
            .erased();

        let policy_provider = PolicyProvider::new(self.policy_cache, policy_l1, runtime_handle);
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
    type Block = primitives::Block;

    fn convert_payload_to_block(
        &self,
        payload: TempoExecutionData,
    ) -> Result<SealedBlock<Self::Block>, NewPayloadError> {
        let TempoExecutionData {
            block,
            validator_set: _,
        } = payload;
        Ok(Arc::unwrap_or_clone(block))
    }

    fn validate_payload_attributes_against_header(
        &self,
        attr: &ZonePayloadAttributes,
        header: &TempoHeader,
    ) -> Result<(), InvalidPayloadAttributesError> {
        if PayloadAttributes::timestamp(attr) < AlloyBlockHeader::timestamp(header) {
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
        let additional_tasks = ctx.config().txpool.additional_validation_tasks;
        let task_executor = ctx.task_executor().clone();
        let mut validator = TransactionValidationTaskExecutor::eth_builder(
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
        .with_custom_tx_type(TempoTxType::AA as u8)
        .no_eip4844()
        .build::<TempoPooledTransaction, _>(blob_store.clone());

        validator.set_additional_stateless_validation(|_origin, tx| {
            use alloy_consensus::Transaction;
            if tx.is_create() {
                return Err(InvalidPoolTransactionError::Consensus(
                    InvalidTransactionError::TxTypeNotSupported,
                ));
            }
            Ok(())
        });

        let validator =
            TransactionValidationTaskExecutor::spawn(validator, &task_executor, additional_tasks);

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
        ctx.task_executor().spawn_critical_task(
            "txpool maintenance - tempo pool",
            tempo_transaction_pool::maintain::maintain_tempo_pool(transaction_pool.clone()),
        );

        info!(target: "reth::cli", "Transaction pool initialized");
        debug!(target: "reth::cli", "Spawned txpool maintenance task");

        Ok(transaction_pool)
    }
}

/// EthApi builder for Zone
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
