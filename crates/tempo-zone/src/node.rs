//! Tempo Zone Node configuration.
//!
//! This is a lightweight L2 node built on reth's node builder infrastructure.
//! It reuses Tempo's EVM, primitives, and pool, but with noop consensus/network/payload.

use alloy_consensus::{Signed, TxLegacy};
use alloy_primitives::U256;
use alloy_sol_types::{SolCall, sol};
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, MissingPayloadBehaviour, PayloadBuilder, PayloadConfig,
};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks};
use reth_engine_local::LocalPayloadAttributesBuilder;
use reth_errors::ProviderError;
use reth_eth_wire_types::primitives::BasicNetworkPrimitives;
use reth_evm::{
    ConfigureEvm, Database, NextBlockEnvAttributes,
    execute::{BlockBuilder, BlockBuilderOutcome},
};
use reth_node_api::{
    AddOnsContext, FullNodeComponents, FullNodeTypes, InvalidPayloadAttributesError,
    NewPayloadError, NodeAddOns, NodeTypes, PayloadAttributesBuilder, PayloadTypes,
    PayloadValidator,
};
use reth_node_builder::{
    BuilderContext, DebugNode, Node, NodeAdapter,
    components::{
        BasicPayloadServiceBuilder, ComponentsBuilder, ExecutorBuilder, NoopConsensusBuilder,
        NoopNetworkBuilder, PayloadBuilderBuilder, PoolBuilder, TxPoolBuilder,
        spawn_maintenance_tasks,
    },
    rpc::{
        BasicEngineValidatorBuilder, EngineValidatorAddOn, EthApiBuilder, EthApiCtx,
        NoopEngineApiBuilder, PayloadValidatorBuilder, RethRpcAddOns, RpcAddOns,
    },
};
use reth_payload_builder::{EthBuiltPayload, PayloadBuilderError};
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_primitives_traits::{AlloyBlockHeader as _, Recovered, SealedBlock, SealedHeader};
use reth_provider::EthStorage;
use reth_revm::{
    State,
    database::StateProviderDatabase,
};
use reth_rpc::DynRpcConverter;
use reth_rpc_builder::Identity;
use reth_rpc_eth_api::RpcConverter;
use reth_storage_api::{StateProvider, StateProviderFactory};
use reth_tracing::tracing::{debug, error, info, warn};
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, TransactionPool,
    TransactionValidationTaskExecutor,
    blobstore::InMemoryBlobStore,
    error::InvalidPoolTransactionError,
};
use std::{default::Default, sync::Arc, time::Instant};
use tempo_alloy::TempoNetwork;
use tempo_chainspec::spec::{TEMPO_BASE_FEE, TempoChainSpec};
use tempo_consensus::{TEMPO_GENERAL_GAS_DIVISOR, TEMPO_SHARED_GAS_DIVISOR};
use tempo_evm::{TempoEvmConfig, TempoNextBlockEnvAttributes, evm::TempoEvmFactory};
use tempo_node::{DEFAULT_AA_VALID_AFTER_MAX_SECS, rpc::TempoReceiptConverter};
use tempo_payload_types::{
    TempoExecutionData, TempoPayloadAttributes, TempoPayloadBuilderAttributes, TempoPayloadTypes,
};
use tempo_primitives::{
    Block, TempoHeader, TempoPrimitives, TempoTxEnvelope, TempoTxType,
    transaction::envelope::{TEMPO_SYSTEM_TX_SENDER, TEMPO_SYSTEM_TX_SIGNATURE},
};
use tempo_transaction_pool::{
    AA2dPool, AA2dPoolConfig, TempoTransactionPool, amm::AmmLiquidityCache,
    validator::TempoTransactionValidator,
};

sol! {
    function mint(address to, uint256 amount);
}

/// TEMPO TIP-20 token address.
const TEMPO_TOKEN_ADDRESS: alloy_primitives::Address =
    alloy_primitives::address!("0x20C0000000000000000000000000000000000000");

/// Network primitives for Zone.
type ZoneNetworkPrimitives = BasicNetworkPrimitives<TempoPrimitives, TempoTxEnvelope>;

/// Tempo Zone node type configuration.
///
/// Uses Tempo primitives, EVM, and pool, but with noop consensus/network/payload.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ZoneNode {
    deposit_queue: crate::DepositQueue,
}

impl ZoneNode {
    /// Create a new zone node with a deposit queue.
    pub fn new(deposit_queue: crate::DepositQueue) -> Self {
        Self { deposit_queue }
    }

    /// Returns a [`ComponentsBuilder`] configured for a Zone node.
    pub fn components<N>(
        deposit_queue: crate::DepositQueue,
    ) -> ComponentsBuilder<
        N,
        ZonePoolBuilder,
        BasicPayloadServiceBuilder<ZonePayloadBuilderBuilder>,
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
            .executor(ZoneExecutorBuilder::default())
            .payload(BasicPayloadServiceBuilder::new(
                ZonePayloadBuilderBuilder::new(deposit_queue),
            ))
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
pub struct ZoneAddOns<N: FullNodeComponents<Types = ZoneNode, Evm = TempoEvmConfig>> {
    inner: RpcAddOns<
        N,
        ZoneEthApiBuilder,
        ZoneEngineValidatorBuilder,
        NoopEngineApiBuilder,
        BasicEngineValidatorBuilder<ZoneEngineValidatorBuilder>,
        Identity,
    >,
}

impl<N: FullNodeComponents<Types = ZoneNode, Evm = TempoEvmConfig>> std::fmt::Debug
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
    pub fn new() -> Self {
        Self {
            inner: RpcAddOns::new(
                ZoneEthApiBuilder::default(),
                ZoneEngineValidatorBuilder,
                NoopEngineApiBuilder::default(),
                BasicEngineValidatorBuilder::default(),
                Identity::default(),
            ),
        }
    }
}

impl<N> Default for ZoneAddOns<NodeAdapter<N>>
where
    N: FullNodeTypes<Types = ZoneNode>,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<N> NodeAddOns<N> for ZoneAddOns<N>
where
    N: FullNodeComponents<Types = ZoneNode, Evm = TempoEvmConfig>,
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
        self.inner.launch_add_ons(ctx).await
    }
}

impl<N> RethRpcAddOns<N> for ZoneAddOns<N>
where
    N: FullNodeComponents<Types = ZoneNode, Evm = TempoEvmConfig>,
    ZoneEthApiBuilder: EthApiBuilder<N>,
{
    type EthApi = <ZoneEthApiBuilder as EthApiBuilder<N>>::EthApi;

    fn hooks_mut(&mut self) -> &mut reth_node_builder::rpc::RpcHooks<N, Self::EthApi> {
        self.inner.hooks_mut()
    }
}

impl<N> EngineValidatorAddOn<N> for ZoneAddOns<N>
where
    N: FullNodeComponents<Types = ZoneNode, Evm = TempoEvmConfig>,
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
        BasicPayloadServiceBuilder<ZonePayloadBuilderBuilder>,
        NoopNetworkBuilder<ZoneNetworkPrimitives>,
        ZoneExecutorBuilder,
        NoopConsensusBuilder,
    >;
    type AddOns = ZoneAddOns<NodeAdapter<N>>;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        Self::components(self.deposit_queue.clone())
    }

    fn add_ons(&self) -> Self::AddOns {
        ZoneAddOns::new()
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

/// Executor builder for Zone - uses Tempo EVM with precompiles.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct ZoneExecutorBuilder;

impl<Node> ExecutorBuilder<Node> for ZoneExecutorBuilder
where
    Node: FullNodeTypes<Types = ZoneNode>,
{
    type EVM = TempoEvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        let evm_config = TempoEvmConfig::new(ctx.chain_spec(), TempoEvmFactory::default());
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

/// Payload builder builder for Zone.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ZonePayloadBuilderBuilder {
    deposit_queue: crate::DepositQueue,
}

impl ZonePayloadBuilderBuilder {
    /// Create a new zone payload builder builder with a deposit queue.
    pub fn new(deposit_queue: crate::DepositQueue) -> Self {
        Self { deposit_queue }
    }
}

impl<Node> PayloadBuilderBuilder<Node, TempoTransactionPool<Node::Provider>, TempoEvmConfig>
    for ZonePayloadBuilderBuilder
where
    Node: FullNodeTypes<Types = ZoneNode>,
{
    type PayloadBuilder = ZonePayloadBuilder<Node::Provider>;

    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: TempoTransactionPool<Node::Provider>,
        evm_config: TempoEvmConfig,
    ) -> eyre::Result<Self::PayloadBuilder> {
        Ok(ZonePayloadBuilder {
            pool,
            provider: ctx.provider().clone(),
            evm_config,
            deposit_queue: self.deposit_queue,
        })
    }
}

/// Simple zone payload builder that executes deposit mint txs + pool txs.
///
/// TODO: Integrate with TempoPayloadBuilder for shared metrics, subblock support, etc.
#[derive(Debug, Clone)]
pub struct ZonePayloadBuilder<Provider> {
    pool: TempoTransactionPool<Provider>,
    provider: Provider,
    evm_config: TempoEvmConfig,
    deposit_queue: crate::DepositQueue,
}

impl<Provider> ZonePayloadBuilder<Provider>
where
    Provider: StateProviderFactory + ChainSpecProvider<ChainSpec = TempoChainSpec>,
{
    /// Build mint system transactions from pending deposits.
    fn build_deposit_mint_txs(
        &self,
        deposits: &[crate::Deposit],
    ) -> Vec<Recovered<TempoTxEnvelope>> {
        let chain_id = Some(self.provider.chain_spec().chain().id());

        deposits
            .iter()
            .map(|deposit| {
                let calldata = mintCall {
                    to: deposit.to,
                    amount: U256::from(deposit.amount),
                }
                .abi_encode();

                Recovered::new_unchecked(
                    TempoTxEnvelope::Legacy(Signed::new_unhashed(
                        TxLegacy {
                            chain_id,
                            nonce: 0,
                            gas_price: 0,
                            gas_limit: 0,
                            to: TEMPO_TOKEN_ADDRESS.into(),
                            value: U256::ZERO,
                            input: calldata.into(),
                        },
                        TEMPO_SYSTEM_TX_SIGNATURE,
                    )),
                    TEMPO_SYSTEM_TX_SENDER,
                )
            })
            .collect()
    }
}

impl<Provider> PayloadBuilder for ZonePayloadBuilder<Provider>
where
    Provider:
        StateProviderFactory + ChainSpecProvider<ChainSpec = TempoChainSpec> + Clone + 'static,
{
    type Attributes = TempoPayloadBuilderAttributes;
    type BuiltPayload = EthBuiltPayload<TempoPrimitives>;

    fn try_build(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> Result<BuildOutcome<Self::BuiltPayload>, PayloadBuilderError> {
        let BuildArguments {
            mut cached_reads,
            config,
            cancel,
            best_payload: _,
        } = args;
        let PayloadConfig {
            parent_header,
            attributes,
        } = config;

        let start = Instant::now();

        // Drain pending deposits
        let pending_deposits = self.deposit_queue.drain();

        if !pending_deposits.is_empty() {
            info!(
                target: "zone::payload",
                count = pending_deposits.len(),
                "Including deposit mint txs in block"
            );
            for deposit in &pending_deposits {
                debug!(
                    target: "zone::payload",
                    sender = %deposit.sender,
                    to = %deposit.to,
                    amount = %deposit.amount,
                    l1_block = deposit.l1_block_number,
                    "Deposit -> mint"
                );
            }
        }

        // Set up state
        let state_provider = self.provider.state_by_block_hash(parent_header.hash())?;
        let state_provider: Box<dyn StateProvider> = state_provider;
        let state = StateProviderDatabase::new(&state_provider);
        let mut db = State::builder()
            .with_database(Box::new(cached_reads.as_db_mut(state))
                as Box<dyn Database<Error = ProviderError>>)
            .with_bundle_update()
            .build();

        let chain_spec = self.provider.chain_spec();

        let block_gas_limit = parent_header.gas_limit();
        let shared_gas_limit = block_gas_limit / TEMPO_SHARED_GAS_DIVISOR;
        let non_shared_gas_limit = block_gas_limit - shared_gas_limit;
        let general_gas_limit = non_shared_gas_limit / TEMPO_GENERAL_GAS_DIVISOR;

        let mut cumulative_gas_used = 0u64;
        let total_fees = U256::ZERO;

        // Build block
        let mut builder = self
            .evm_config
            .builder_for_next_block(
                &mut db,
                &parent_header,
                TempoNextBlockEnvAttributes {
                    inner: NextBlockEnvAttributes {
                        timestamp: attributes.timestamp(),
                        suggested_fee_recipient: attributes.suggested_fee_recipient(),
                        prev_randao: attributes.prev_randao(),
                        gas_limit: block_gas_limit,
                        parent_beacon_block_root: attributes.parent_beacon_block_root(),
                        withdrawals: Some(attributes.withdrawals().clone()),
                        extra_data: attributes.extra_data().clone(),
                    },
                    general_gas_limit,
                    shared_gas_limit,
                    timestamp_millis_part: attributes.timestamp_millis_part(),
                    subblock_fee_recipients: Default::default(),
                },
            )
            .map_err(PayloadBuilderError::other)?;

        builder.apply_pre_execution_changes().map_err(|err| {
            warn!(%err, "failed to apply pre-execution changes");
            PayloadBuilderError::Internal(err.into())
        })?;

        // Execute deposit mint system transactions
        let deposit_txs = self.build_deposit_mint_txs(&pending_deposits);
        for tx in deposit_txs {
            if let Err(err) = builder.execute_transaction(tx) {
                error!(?err, "deposit mint system tx failed");
                return Err(PayloadBuilderError::evm(err));
            }
        }

        // Execute pool transactions
        // TODO: Use gas accounting from TempoPayloadBuilder (payment vs non-payment limits, etc.)
        let base_fee = builder.evm_mut().block.basefee;
        let mut best_txs = self.pool.best_transactions_with_attributes(
            BestTransactionsAttributes::new(base_fee, None),
        );

        while let Some(pool_tx) = best_txs.next() {
            if cumulative_gas_used + pool_tx.gas_limit() > non_shared_gas_limit {
                best_txs.mark_invalid(
                    &pool_tx,
                    &InvalidPoolTransactionError::ExceedsGasLimit(
                        pool_tx.gas_limit(),
                        non_shared_gas_limit - cumulative_gas_used,
                    ),
                );
                continue;
            }

            if cancel.is_cancelled() {
                return Ok(BuildOutcome::Cancelled);
            }

            let tx_with_env = pool_tx.transaction.clone().into_with_tx_env();
            match builder.execute_transaction(tx_with_env) {
                Ok(gas_used) => {
                    cumulative_gas_used += gas_used;
                }
                Err(reth_evm::block::BlockExecutionError::Validation(
                    reth_evm::block::BlockValidationError::InvalidTx { error, .. },
                )) => {
                    if !error.is_nonce_too_low() {
                        best_txs.mark_invalid(
                            &pool_tx,
                            &InvalidPoolTransactionError::Consensus(
                                reth_primitives_traits::transaction::error::InvalidTransactionError::TxTypeNotSupported,
                            ),
                        );
                    }
                    continue;
                }
                Err(err) => return Err(PayloadBuilderError::evm(err)),
            }
        }

        // Finish block
        let BlockBuilderOutcome {
            execution_result,
            block,
            ..
        } = builder.finish(&state_provider)?;

        let requests = chain_spec
            .is_prague_active_at_timestamp(attributes.timestamp())
            .then_some(execution_result.requests);

        let sealed_block = Arc::new(block.sealed_block().clone());
        let elapsed = start.elapsed();

        info!(
            number = sealed_block.number(),
            hash = ?sealed_block.hash(),
            gas_used = sealed_block.gas_used(),
            deposits = pending_deposits.len(),
            tx_count = sealed_block.body().transactions.len(),
            ?elapsed,
            "Built zone payload"
        );

        let payload =
            EthBuiltPayload::new(attributes.payload_id(), sealed_block, total_fees, requests);

        drop(db);
        Ok(BuildOutcome::Better {
            payload,
            cached_reads,
        })
    }

    fn on_missing_payload(
        &self,
        _args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> MissingPayloadBehaviour<Self::BuiltPayload> {
        MissingPayloadBehaviour::AwaitInProgress
    }

    fn build_empty_payload(
        &self,
        config: PayloadConfig<Self::Attributes, TempoHeader>,
    ) -> Result<Self::BuiltPayload, PayloadBuilderError> {
        self.try_build(BuildArguments::new(
            Default::default(),
            config,
            Default::default(),
            Default::default(),
        ))?
        .into_payload()
        .ok_or_else(|| PayloadBuilderError::MissingPayload)
    }
}

/// EthApi builder for Zone - uses Tempo RPC types.
#[derive(Debug, Default, Clone)]
pub struct ZoneEthApiBuilder;

impl<N> EthApiBuilder<N> for ZoneEthApiBuilder
where
    N: FullNodeComponents<Types = ZoneNode, Evm = TempoEvmConfig>,
{
    type EthApi = reth_rpc::EthApi<N, DynRpcConverter<TempoEvmConfig, TempoNetwork>>;

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
