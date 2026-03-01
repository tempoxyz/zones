//! Zone-specific EVM configuration.
//!
//! Wraps [`TempoEvmConfig`] with a custom [`ZoneEvmFactory`] that registers the
//! [`TempoStateReader`](crate::l1_state::TempoStateReader) precompile at
//! [`TEMPO_STATE_READER_ADDRESS`](crate::abi::TEMPO_STATE_READER_ADDRESS).

use std::sync::Arc;

use alloy_evm::{
    Database, Evm, EvmEnv, EvmFactory,
    block::{BlockExecutorFactory, BlockExecutorFor},
    precompiles::PrecompilesMap,
    revm::{Inspector, database::State, inspector::NoOpInspector},
};
use alloy_provider::{Provider, ProviderBuilder};
use reth_evm::{
    ConfigureEngineEvm, ConfigureEvm, EvmEnvFor, ExecutableTxIterator, ExecutionCtxFor,
    execute::{BlockAssembler, BlockAssemblerInput},
};
use reth_primitives_traits::{SealedBlock, SealedHeader};
use tempo_alloy::TempoNetwork;
use tempo_chainspec::TempoChainSpec;
use tempo_evm::{
    TempoBlockAssembler, TempoBlockEnv, TempoBlockExecutionCtx, TempoEvmConfig, TempoEvmError,
    TempoHaltReason, TempoNextBlockEnvAttributes,
    evm::{TempoEvm, TempoEvmFactory},
};
use tempo_payload_types::TempoExecutionData;
use tempo_primitives::{Block, TempoHeader, TempoPrimitives, TempoReceipt, TempoTxEnvelope};

use crate::executor::ZoneBlockExecutor;

use crate::{
    abi::TEMPO_STATE_READER_ADDRESS,
    l1_state::{L1StateProvider, PolicyProvider, SharedL1StateCache, TempoStateReader},
    precompiles::{
        AES_GCM_DECRYPT_ADDRESS, AesGcmDecrypt, CHAUM_PEDERSEN_VERIFY_ADDRESS, ChaumPedersenVerify,
        ZONE_TIP403_PROXY_ADDRESS, ZoneTip403ProxyRegistry,
    },
};

type TempoCtx<DB> = <TempoEvmFactory as EvmFactory>::Context<DB>;

/// Zone EVM factory — wraps [`TempoEvmFactory`] and registers the
/// [`TempoStateReader`] precompile for reading Tempo L1 storage from zone contracts.
#[derive(Debug, Clone)]
pub struct ZoneEvmFactory {
    l1_provider: L1StateProvider,
    policy_provider: Option<PolicyProvider>,
}

impl ZoneEvmFactory {
    /// Create a new factory with the given L1 state provider.
    pub fn new(l1_provider: L1StateProvider) -> Self {
        Self {
            l1_provider,
            policy_provider: None,
        }
    }

    /// Set the policy provider for the TIP-403 proxy precompile.
    pub fn with_policy_provider(mut self, policy_provider: PolicyProvider) -> Self {
        self.policy_provider = Some(policy_provider);
        self
    }

    fn register_precompiles<DB: Database, I: Inspector<TempoCtx<DB>>>(
        &self,
        mut evm: TempoEvm<DB, I>,
    ) -> TempoEvm<DB, I> {
        let (_, _, precompiles) = evm.components_mut();
        precompiles.apply_precompile(&TEMPO_STATE_READER_ADDRESS, |_| {
            Some(TempoStateReader::create(self.l1_provider.clone()))
        });
        precompiles.apply_precompile(&CHAUM_PEDERSEN_VERIFY_ADDRESS, |_| {
            Some(ChaumPedersenVerify.into())
        });
        precompiles.apply_precompile(&AES_GCM_DECRYPT_ADDRESS, |_| Some(AesGcmDecrypt.into()));
        if let Some(ref policy_provider) = self.policy_provider {
            precompiles.apply_precompile(&ZONE_TIP403_PROXY_ADDRESS, |_| {
                Some(ZoneTip403ProxyRegistry::create(policy_provider.clone()))
            });
        }
        evm
    }
}

impl EvmFactory for ZoneEvmFactory {
    type Evm<DB: Database, I: Inspector<Self::Context<DB>>> = TempoEvm<DB, I>;
    type Context<DB: Database> = TempoCtx<DB>;
    type Tx = <TempoEvmFactory as EvmFactory>::Tx;
    type Error<DBError: std::error::Error + Send + Sync + 'static> =
        <TempoEvmFactory as EvmFactory>::Error<DBError>;
    type HaltReason = TempoHaltReason;
    type Spec = tempo_chainspec::hardfork::TempoHardfork;
    type BlockEnv = TempoBlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let evm = TempoEvm::new(db, input);
        self.register_precompiles(evm)
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let evm = TempoEvm::new(db, input).with_inspector(inspector);
        self.register_precompiles(evm)
    }
}

/// Assembler for Zone blocks — delegates to [`TempoBlockAssembler`] after converting input types.
#[derive(Debug, Clone)]
pub struct ZoneBlockAssembler {
    inner: TempoBlockAssembler,
}

impl ZoneBlockAssembler {
    /// Create a new [`ZoneBlockAssembler`] with the given chain spec.
    pub fn new(chain_spec: Arc<TempoChainSpec>) -> Self {
        Self {
            inner: TempoBlockAssembler::new(chain_spec),
        }
    }
}

impl BlockAssembler<ZoneEvmConfig> for ZoneBlockAssembler {
    type Block = Block;

    fn assemble_block(
        &self,
        input: BlockAssemblerInput<'_, '_, ZoneEvmConfig, TempoHeader>,
    ) -> Result<Self::Block, alloy_evm::block::BlockExecutionError> {
        let BlockAssemblerInput {
            evm_env,
            execution_ctx,
            parent,
            transactions,
            output,
            bundle_state,
            state_provider,
            state_root,
            ..
        } = input;

        self.inner
            .assemble_block(BlockAssemblerInput::<TempoEvmConfig, TempoHeader>::new(
                evm_env,
                execution_ctx,
                parent,
                transactions,
                output,
                bundle_state,
                state_provider,
                state_root,
            ))
    }
}

/// Zone EVM configuration — wraps [`TempoEvmConfig`] with a [`ZoneEvmFactory`].
#[derive(Debug, Clone)]
pub struct ZoneEvmConfig {
    inner: TempoEvmConfig,
    zone_factory: ZoneEvmFactory,
    block_assembler: ZoneBlockAssembler,
}

impl ZoneEvmConfig {
    /// Create a new zone EVM config with the given chain spec and L1 state provider.
    pub fn new(chain_spec: Arc<TempoChainSpec>, l1_provider: L1StateProvider) -> Self {
        let zone_factory = ZoneEvmFactory::new(l1_provider);
        let inner = TempoEvmConfig::new(chain_spec.clone());
        let block_assembler = ZoneBlockAssembler::new(chain_spec);
        Self {
            inner,
            zone_factory,
            block_assembler,
        }
    }

    /// Create a zone EVM config **without** the TempoStateReader precompile.
    ///
    /// Intended for CLI subcommands (import, stage, re-execute) that need a type-compatible
    /// EVM config but don't have access to an L1 RPC connection. Transactions calling the
    /// TempoStateReader precompile will get a reverted / empty response.
    pub fn new_without_l1(chain_spec: Arc<TempoChainSpec>) -> Self {
        let cache = SharedL1StateCache::default();
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_http("http://127.0.0.1:1".parse().expect("valid fallback URL"))
            .erased();
        let runtime_handle = tokio::runtime::Handle::current();
        let l1_provider = L1StateProvider::new_raw(cache, provider, runtime_handle);
        Self::new(chain_spec, l1_provider)
    }

    /// Set the policy provider for the TIP-403 proxy precompile.
    pub fn with_policy_provider(mut self, policy_provider: PolicyProvider) -> Self {
        self.zone_factory = self.zone_factory.with_policy_provider(policy_provider);
        self
    }

    /// Returns the chain spec.
    pub fn chain_spec(&self) -> &Arc<TempoChainSpec> {
        self.inner.chain_spec()
    }
}

impl BlockExecutorFactory for ZoneEvmConfig {
    type EvmFactory = ZoneEvmFactory;
    type ExecutionCtx<'a> = TempoBlockExecutionCtx<'a>;
    type Transaction = TempoTxEnvelope;
    type Receipt = TempoReceipt;

    fn evm_factory(&self) -> &Self::EvmFactory {
        &self.zone_factory
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: TempoEvm<&'a mut State<DB>, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> impl BlockExecutorFor<'a, Self, DB, I>
    where
        DB: Database + 'a,
        I: Inspector<TempoCtx<&'a mut State<DB>>> + 'a,
    {
        ZoneBlockExecutor::new(evm, ctx, self.chain_spec())
    }
}

impl ConfigureEvm for ZoneEvmConfig {
    type Primitives = TempoPrimitives;
    type Error = TempoEvmError;
    type NextBlockEnvCtx = TempoNextBlockEnvAttributes;
    type BlockExecutorFactory = Self;
    type BlockAssembler = ZoneBlockAssembler;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        self
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        &self.block_assembler
    }

    fn evm_env(&self, header: &TempoHeader) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.inner.evm_env(header)
    }

    fn next_evm_env(
        &self,
        parent: &TempoHeader,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.inner.next_evm_env(parent, attributes)
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<Block>,
    ) -> Result<TempoBlockExecutionCtx<'a>, Self::Error> {
        use alloy_consensus::BlockHeader;
        use alloy_evm::eth::EthBlockExecutionCtx;
        use std::borrow::Cow;

        Ok(TempoBlockExecutionCtx {
            inner: EthBlockExecutionCtx {
                parent_hash: block.header().parent_hash(),
                parent_beacon_block_root: block.header().parent_beacon_block_root(),
                ommers: &[],
                withdrawals: block.body().withdrawals.as_ref().map(Cow::Borrowed),
                extra_data: block.header().extra_data().clone(),
                tx_count_hint: Some(block.body().transactions.len()),
            },
            general_gas_limit: 0,
            shared_gas_limit: 0,
            validator_set: None,
            subblock_fee_recipients: Default::default(),
        })
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader<TempoHeader>,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<TempoBlockExecutionCtx<'_>, Self::Error> {
        self.inner.context_for_next_block(parent, attributes)
    }
}

impl ConfigureEngineEvm<TempoExecutionData> for ZoneEvmConfig {
    fn evm_env_for_payload(
        &self,
        payload: &TempoExecutionData,
    ) -> Result<EvmEnvFor<Self>, Self::Error> {
        self.inner.evm_env_for_payload(payload)
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a TempoExecutionData,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        let mut context = self.context_for_block(&payload.block)?;
        context.validator_set = payload.validator_set.clone();
        Ok(context)
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &TempoExecutionData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        self.inner.tx_iterator_for_payload(payload)
    }
}
