//! Tempo Zone Node configuration.
//!
//! This is a lightweight L2 node built on reth's node builder infrastructure.
//! It reuses Tempo's EVM, primitives, and pool, but with noop consensus/network/payload.

use alloy_primitives::U256;
use reth_engine_local::LocalPayloadAttributesBuilder;
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
use reth_primitives_traits::{AlloyBlockHeader as _, SealedBlock, SealedHeader};
use reth_provider::{ChainSpecProvider, EthStorage};
use reth_rpc::DynRpcConverter;
use reth_rpc_builder::Identity;
use reth_rpc_eth_api::RpcConverter;
use reth_transaction_pool::{TransactionValidationTaskExecutor, blobstore::InMemoryBlobStore};
use std::{default::Default, sync::Arc};
use tempo_alloy::TempoNetwork;
use tempo_chainspec::spec::{TEMPO_BASE_FEE, TempoChainSpec};
use tempo_node::{DEFAULT_AA_VALID_AFTER_MAX_SECS, rpc::TempoReceiptConverter};
use tempo_payload_types::{TempoExecutionData, TempoPayloadAttributes, TempoPayloadTypes};
use tempo_primitives::{Block, TempoHeader, TempoPrimitives, TempoTxEnvelope, TempoTxType};
use tempo_transaction_pool::{
    AA2dPool, AA2dPoolConfig, TempoTransactionPool, amm::AmmLiquidityCache,
    validator::TempoTransactionValidator,
};
use tracing::{debug, info};

use crate::{
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
    deposit_queue: crate::DepositQueue,
    l1_config: crate::L1SubscriberConfig,
    l1_state_provider_config: L1StateProviderConfig,
    l1_state_listener_config: L1StateListenerConfig,
    l1_state_cache: SharedL1StateCache,
    sequencer: Option<alloy_primitives::Address>,
}

impl ZoneNode {
    /// Create a new zone node with a deposit queue, L1 subscriber config,
    /// and L1 state infrastructure configs.
    ///
    /// The L1 subscriber is spawned automatically as part of the node lifecycle via
    /// [`ZoneAddOns::launch_add_ons`], ensuring it cannot be accidentally omitted.
    /// The L1 state listener is spawned by [`ZoneExecutorBuilder`] during EVM construction.
    pub fn new(
        deposit_queue: crate::DepositQueue,
        l1_config: crate::L1SubscriberConfig,
        l1_state_provider_config: L1StateProviderConfig,
        l1_state_listener_config: L1StateListenerConfig,
        l1_state_cache: SharedL1StateCache,
        sequencer: Option<alloy_primitives::Address>,
    ) -> Self {
        Self {
            deposit_queue,
            l1_config,
            l1_state_provider_config,
            l1_state_listener_config,
            l1_state_cache,
            sequencer,
        }
    }

    /// Returns a [`ComponentsBuilder`] configured for a Zone node.
    pub fn components<N>(
        deposit_queue: crate::DepositQueue,
        executor_builder: ZoneExecutorBuilder,
        sequencer: Option<alloy_primitives::Address>,
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
            .payload(BasicPayloadServiceBuilder::new(ZonePayloadFactory::new(
                deposit_queue,
                sequencer,
            )))
            .network(NoopNetworkBuilder::<ZoneNetworkPrimitives>::default())
            .noop_consensus()
    }
}

impl NodeTypes for ZoneNode {
    type Primitives = TempoPrimitives;
    type ChainSpec = TempoChainSpec;
    type Storage = EthStorage<TempoTxEnvelope, TempoHeader>;
    type Payload = TempoPayloadTypes;
}

/// Zone node add-ons (RPC, etc.)
pub struct ZoneAddOns<N: FullNodeComponents<Types = ZoneNode, Evm = ZoneEvmConfig>> {
    inner: RpcAddOns<
        N,
        ZoneEthApiBuilder,
        ZoneEngineValidatorBuilder,
        NoopEngineApiBuilder,
        BasicEngineValidatorBuilder<ZoneEngineValidatorBuilder>,
        Identity,
    >,
    deposit_queue: crate::DepositQueue,
    l1_config: crate::L1SubscriberConfig,
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
    pub fn new(deposit_queue: crate::DepositQueue, l1_config: crate::L1SubscriberConfig) -> Self {
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
        }
    }
}

impl<N> NodeAddOns<N> for ZoneAddOns<N>
where
    N: FullNodeComponents<Types = ZoneNode, Evm = ZoneEvmConfig>,
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

    async fn launch_add_ons(self, ctx: AddOnsContext<'_, N>) -> eyre::Result<Self::Handle> {
        L1Subscriber::spawn(
            self.l1_config,
            self.deposit_queue,
            ctx.node.task_executor().clone(),
        );

        info!(target: "reth::cli", "L1 deposit subscriber started as part of node lifecycle");

        self.inner.launch_add_ons(ctx).await
    }
}

impl<N> RethRpcAddOns<N> for ZoneAddOns<N>
where
    N: FullNodeComponents<Types = ZoneNode, Evm = ZoneEvmConfig>,
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
        );
        Self::components(self.deposit_queue.clone(), executor_builder, self.sequencer)
    }

    fn add_ons(&self) -> Self::AddOns {
        ZoneAddOns::new(self.deposit_queue.clone(), self.l1_config.clone())
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

/// Payload attributes builder for Zone.
#[derive(Debug)]
#[non_exhaustive]
struct ZonePayloadAttributesBuilder {
    inner: LocalPayloadAttributesBuilder<TempoChainSpec>,
}

impl ZonePayloadAttributesBuilder {
    fn new(chain_spec: Arc<TempoChainSpec>) -> Self {
        Self {
            inner: LocalPayloadAttributesBuilder::new(chain_spec).without_increasing_timestamp(),
        }
    }
}

impl PayloadAttributesBuilder<TempoPayloadAttributes, TempoHeader>
    for ZonePayloadAttributesBuilder
{
    fn build(&self, parent: &SealedHeader<TempoHeader>) -> TempoPayloadAttributes {
        let mut inner = self.inner.build(parent);
        inner.suggested_fee_recipient = alloy_primitives::Address::ZERO;

        let timestamp_millis_part = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            % 1000;

        TempoPayloadAttributes {
            inner,
            timestamp_millis_part,
        }
    }
}

/// Executor builder for Zone — constructs [`ZoneEvmConfig`] with the TempoStateReader precompile
/// and spawns the L1 state listener for cache updates.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ZoneExecutorBuilder {
    l1_state_provider_config: L1StateProviderConfig,
    l1_state_listener_config: L1StateListenerConfig,
    l1_state_cache: SharedL1StateCache,
}

impl ZoneExecutorBuilder {
    pub fn new(
        l1_state_provider_config: L1StateProviderConfig,
        l1_state_listener_config: L1StateListenerConfig,
        l1_state_cache: SharedL1StateCache,
    ) -> Self {
        Self {
            l1_state_provider_config,
            l1_state_listener_config,
            l1_state_cache,
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
            self.l1_state_provider_config,
            self.l1_state_cache.clone(),
            runtime_handle,
        )
        .await;

        spawn_l1_state_listener(
            self.l1_state_listener_config,
            self.l1_state_cache,
            ctx.task_executor().clone(),
        );

        let evm_config = ZoneEvmConfig::new(ctx.chain_spec(), l1_provider);
        info!(target: "reth::cli", "Zone EVM initialized with TempoStateReader precompile");
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
    type Validator = ZonePayloadValidator;

    async fn build(self, _ctx: &AddOnsContext<'_, Node>) -> eyre::Result<Self::Validator> {
        Ok(ZonePayloadValidator)
    }
}

/// Payload validator for Zone.
#[derive(Debug, Clone, Copy, Default)]
pub struct ZonePayloadValidator;

impl PayloadValidator<TempoPayloadTypes> for ZonePayloadValidator {
    type Block = Block;

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
        attr: &TempoPayloadAttributes,
        header: &TempoHeader,
    ) -> Result<(), InvalidPayloadAttributesError> {
        if attr.inner.timestamp < header.timestamp() {
            return Err(InvalidPayloadAttributesError::InvalidTimestamp);
        }
        Ok(())
    }
}

/// Transaction pool builder for Zone - uses Tempo pool with defaults.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct ZonePoolBuilder;

impl<Node> PoolBuilder<Node> for ZonePoolBuilder
where
    Node: FullNodeTypes<Types = ZoneNode>,
{
    type Pool = TempoTransactionPool<Node::Provider>;

    async fn build_pool(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Pool> {
        let mut pool_config = ctx.pool_config();
        pool_config.minimal_protocol_basefee = TEMPO_BASE_FEE;
        pool_config.max_inflight_delegated_slot_limit = pool_config.max_account_slots;

        let blob_store = InMemoryBlobStore::default();
        let validator = TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone())
            .with_head_timestamp(ctx.head().timestamp)
            .with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
            .with_local_transactions_config(pool_config.local_transactions_config.clone())
            .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
            .with_max_tx_gas_limit(ctx.config().txpool.max_tx_gas_limit)
            .disable_balance_check()
            .with_minimum_priority_fee(ctx.config().txpool.minimum_priority_fee)
            .with_additional_tasks(ctx.config().txpool.additional_validation_tasks)
            .with_custom_tx_type(TempoTxType::AA as u8)
            .no_eip4844()
            .build_with_tasks(ctx.task_executor().clone(), blob_store.clone());

        let aa_2d_config = AA2dPoolConfig {
            price_bump_config: pool_config.price_bumps,
            aa_2d_limit: pool_config.pending_limit,
        };
        let aa_2d_pool = AA2dPool::new(aa_2d_config);
        let amm_liquidity_cache = AmmLiquidityCache::new(ctx.provider())?;

        let validator = validator.map(|v| {
            TempoTransactionValidator::new(
                v,
                DEFAULT_AA_VALID_AFTER_MAX_SECS,
                amm_liquidity_cache.clone(),
            )
        });
        let protocol_pool = TxPoolBuilder::new(ctx)
            .with_validator(validator)
            .build(blob_store, pool_config.clone());

        let transaction_pool = TempoTransactionPool::new(protocol_pool, aa_2d_pool);

        spawn_maintenance_tasks(ctx, transaction_pool.clone(), &pool_config)?;

        let task_pool = transaction_pool.clone();
        let task_provider = ctx.provider().clone();
        ctx.task_executor().spawn_critical(
            "txpool maintenance (protocol) - evict expired AA txs",
            tempo_transaction_pool::maintain::evict_expired_aa_txs(task_pool, task_provider),
        );

        ctx.task_executor().spawn_critical(
            "txpool maintenance - 2d nonce AA txs",
            tempo_transaction_pool::maintain::maintain_2d_nonce_pool(transaction_pool.clone()),
        );

        ctx.task_executor().spawn_critical(
            "txpool maintenance - amm liquidity cache",
            tempo_transaction_pool::maintain::maintain_amm_cache(transaction_pool.clone()),
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
