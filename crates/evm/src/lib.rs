//! Zone-specific EVM configuration.
//!
//! Wraps [`TempoEvmConfig`] with a [`ZoneEvmFactory`] that installs the L1-anchored database,
//! registers Zone-native precompiles, and preserves the original database at the [`Evm`] boundary.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![allow(unnameable_types)]

mod database;
mod executor;
pub mod precompiles;
mod tx_context;
mod zone_evm;

pub use database::{AnchoredZoneDb, ZoneDbError};
pub use executor::ZoneBlockExecutor;
pub use zone_evm::{ZoneEvm, contract_creation::validate_transaction};

use crate::{
    precompiles::{
        AES_GCM_DECRYPT_ADDRESS, AesGcmDecrypt, CHAUM_PEDERSEN_VERIFY_ADDRESS, ChaumPedersenVerify,
        L1State, L1StorageReader, SequencerExt, TIP403_REGISTRY_ADDRESS, TempoState,
        ZONE_TIP20_FACTORY_ADDRESS, ZonePrecompileEnv, ZoneTokenFactory, create_tip20_precompile,
        create_tip403_precompile,
    },
    tx_context::ZoneTxContext,
};
use alloy_evm::{
    Database, Evm, EvmEnv, EvmFactory,
    block::BlockExecutorFactory,
    eth::EthTxResult,
    precompiles::PrecompilesMap,
    revm::{Inspector, context::DBErrorMarker, inspector::NoOpInspector},
};
use alloy_provider::{Provider, ProviderBuilder};
use reth_chainspec::EthChainSpec;
use reth_evm::{
    ConfigureEngineEvm, ConfigureEvm, EvmEnvFor, ExecutableTxIterator, ExecutionCtxFor,
    block::StateDB,
    execute::{BlockAssembler, BlockAssemblerInput},
};
use reth_primitives_traits::{SealedBlock, SealedHeader};
use std::{cell::RefCell, rc::Rc, sync::Arc};
use tempo_alloy::TempoNetwork;
use tempo_chainspec::TempoChainSpec;
use tempo_evm::{
    TempoBlockAssembler, TempoBlockEnv, TempoBlockExecutionCtx, TempoEvmConfig, TempoEvmError,
    TempoHaltReason, TempoNextBlockEnvAttributes,
    evm::{TempoEvm, TempoEvmFactory},
};
use tempo_payload_types::TempoExecutionData;
use tempo_precompiles::{
    ACCOUNT_KEYCHAIN_ADDRESS, NONCE_PRECOMPILE_ADDRESS, PrecompileEnv,
    RECEIVE_POLICY_GUARD_ADDRESS, STABLECOIN_DEX_ADDRESS, TIP_FEE_MANAGER_ADDRESS,
    account_keychain::AccountKeychain, nonce::NonceManager,
    receive_policy_guard::ReceivePolicyGuard, storage::actions::StorageActions,
    storage_credits::NonCreditableSlots, tip_fee_manager::TipFeeManager, tip20::is_tip20_prefix,
};
use tempo_primitives::{
    Block, TempoHeader, TempoPrimitives, TempoReceipt, TempoTxEnvelope, TempoTxType,
};
use tempo_zone_contracts::{TEMPO_STATE_ADDRESS, ZONE_TX_CONTEXT_ADDRESS};
use zone_chainspec::ZoneChainSpec;
use zone_l1::state::{L1StateCache, L1StateProvider, L1StateProviderConfig};

type TempoCtx<DB> = <TempoEvmFactory as EvmFactory>::Context<DB>;

/// Zone EVM factory that adapts caller databases and registers the zone-native precompiles.
#[derive(Debug, Clone)]
pub struct ZoneEvmFactory<L1 = L1StateProvider> {
    l1_reader: L1,
}

impl<L1> ZoneEvmFactory<L1>
where
    L1: L1StorageReader + SequencerExt,
{
    /// Create a new factory with the given L1 state reader.
    pub fn new(l1_reader: L1) -> Self {
        Self { l1_reader }
    }

    fn register_precompiles<DB: Database, I: Inspector<TempoCtx<AnchoredZoneDb<DB, L1>>>>(
        &self,
        mut evm: TempoEvm<AnchoredZoneDb<DB, L1>, I>,
        l1: L1State<L1>,
    ) -> TempoEvm<AnchoredZoneDb<DB, L1>, I> {
        let cfg = evm.ctx().cfg.clone();
        let (_, _, precompiles) = evm.components_mut();
        let actions = StorageActions::disabled();
        let non_creditable_slots = Rc::new(RefCell::new(NonCreditableSlots::empty()));
        let env = ZonePrecompileEnv::new(&cfg, actions.clone(), non_creditable_slots.clone());
        precompiles.apply_precompile(&TEMPO_STATE_ADDRESS, |_| {
            Some(TempoState::create(l1.clone(), &env))
        });
        precompiles.apply_precompile(&ZONE_TX_CONTEXT_ADDRESS, |_| Some(ZoneTxContext::create()));
        precompiles.apply_precompile(&CHAUM_PEDERSEN_VERIFY_ADDRESS, |_| {
            Some(ChaumPedersenVerify::create(&env))
        });
        precompiles.apply_precompile(&AES_GCM_DECRYPT_ADDRESS, |_| {
            Some(AesGcmDecrypt::create(&env))
        });
        precompiles.apply_precompile(&ZONE_TIP20_FACTORY_ADDRESS, |_| {
            Some(ZoneTokenFactory::create(&env))
        });
        let tip403_env = env.clone();
        precompiles.apply_precompile(&TIP403_REGISTRY_ADDRESS, move |_| {
            Some(create_tip403_precompile(&tip403_env))
        });
        let sequencer: Arc<dyn SequencerExt> = Arc::new(self.l1_reader.clone());
        let tempo_env = PrecompileEnv::new(&cfg, actions, non_creditable_slots);
        precompiles.set_precompile_lookup(move |address: &alloy_primitives::Address| {
            if is_tip20_prefix(*address) {
                Some(create_tip20_precompile(*address, &env, sequencer.clone()))
            } else if *address == TIP_FEE_MANAGER_ADDRESS {
                Some(TipFeeManager::create_precompile(&tempo_env))
            } else if *address == STABLECOIN_DEX_ADDRESS {
                None
            } else if *address == NONCE_PRECOMPILE_ADDRESS {
                Some(NonceManager::create_precompile(&tempo_env))
            } else if *address == ACCOUNT_KEYCHAIN_ADDRESS {
                Some(AccountKeychain::create_precompile(&tempo_env))
            } else if *address == RECEIVE_POLICY_GUARD_ADDRESS {
                Some(ReceivePolicyGuard::create_precompile(&tempo_env))
            } else {
                None
            }
        });
        evm
    }
}

impl<L1> EvmFactory for ZoneEvmFactory<L1>
where
    L1: L1StorageReader + SequencerExt,
{
    type Evm<DB: Database, I: Inspector<Self::Context<DB>>> = ZoneEvm<DB, I, L1>;
    type Context<DB: Database> = TempoCtx<AnchoredZoneDb<DB, L1>>;
    type Tx = <TempoEvmFactory as EvmFactory>::Tx;
    type Error<DBError: DBErrorMarker> = <TempoEvmFactory as EvmFactory>::Error<DBError>;
    type HaltReason = TempoHaltReason;
    type Spec = tempo_chainspec::hardfork::TempoHardfork;
    type BlockEnv = TempoBlockEnv;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
    ) -> Self::Evm<DB, NoOpInspector> {
        let db = AnchoredZoneDb::new(db, self.l1_reader.clone());
        let l1 = db.l1_state().clone();
        let evm = TempoEvm::new(db, input);
        ZoneEvm::new(self.register_precompiles(evm, l1))
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let db = AnchoredZoneDb::new(db, self.l1_reader.clone());
        let l1 = db.l1_state().clone();
        let evm = TempoEvm::new(db, input).with_inspector(inspector);
        ZoneEvm::new(self.register_precompiles(evm, l1))
    }
}

/// Assembler for Zone blocks - delegates to [`TempoBlockAssembler`] after converting input types.
#[derive(Debug, Clone)]
pub struct ZoneBlockAssembler {
    inner: TempoBlockAssembler,
}

impl ZoneBlockAssembler {
    /// Create a new [`ZoneBlockAssembler`] with the given chain spec.
    pub fn new(chain_spec: Arc<ZoneChainSpec>) -> Self {
        Self {
            inner: TempoBlockAssembler::new(chain_spec.inner.clone()),
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
            block_access_list_hash,
            ..
        } = input;

        self.inner.assemble_block(
            BlockAssemblerInput::<TempoEvmConfig, TempoHeader>::new(
                evm_env,
                execution_ctx,
                parent,
                transactions,
                output,
                bundle_state,
                state_provider,
                state_root,
                block_access_list_hash,
            ),
            None,
            None,
            None,
        )
    }
}

/// Zone EVM configuration with Zone precompiles and parent Tempo hardfork conditions.
#[derive(Debug, Clone)]
pub struct ZoneEvmConfig {
    inner: TempoEvmConfig,
    chain_spec: Arc<ZoneChainSpec>,
    zone_factory: ZoneEvmFactory,
    block_assembler: ZoneBlockAssembler,
}

impl ZoneEvmConfig {
    /// Creates a Zone EVM config using Tempo hardfork conditions from the parent L1 spec.
    pub fn new(
        zone_chain_spec: Arc<ZoneChainSpec>,
        tempo_chain_spec: Arc<TempoChainSpec>,
        l1_provider: L1StateProvider,
    ) -> Self {
        let chain_spec = Self::compose_chain_spec(&zone_chain_spec, &tempo_chain_spec);
        Self::from_chain_spec(chain_spec, l1_provider)
    }

    /// Copies the Zone chain spec and applies the Tempo hardfork conditions from its parent chain.
    fn compose_chain_spec(zone: &ZoneChainSpec, tempo: &TempoChainSpec) -> Arc<ZoneChainSpec> {
        Arc::new(zone.clone().with_tempo_hardforks_from(tempo))
    }

    fn from_chain_spec(chain_spec: Arc<ZoneChainSpec>, l1_provider: L1StateProvider) -> Self {
        let zone_factory = ZoneEvmFactory::new(l1_provider);
        let tempo_chain_spec = chain_spec.inner.clone();
        let inner = TempoEvmConfig::new(tempo_chain_spec);
        let block_assembler = ZoneBlockAssembler::new(chain_spec.clone());
        Self {
            inner,
            chain_spec,
            zone_factory,
            block_assembler,
        }
    }

    /// Creates a Zone EVM config without a usable L1 provider.
    ///
    /// Intended for CLI subcommands (import, stage, re-execute) that need a type-compatible
    /// EVM config but don't have access to an L1 RPC connection. Tempo hardfork conditions come
    /// from `chain_spec` because the parent L1 spec cannot be resolved in this mode. The portal
    /// address defaults to zero, so sequencer reads are unavailable.
    pub fn new_without_l1(chain_spec: Arc<ZoneChainSpec>) -> Self {
        let cache = L1StateCache::default();
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_http("http://127.0.0.1:1".parse().expect("valid fallback URL"))
            .erased();
        let runtime_handle = tokio::runtime::Handle::current();
        let config = L1StateProviderConfig {
            max_sync_attempts: Some(1),
            ..Default::default()
        };
        let l1_provider = L1StateProvider::new_raw(config, cache, provider, runtime_handle);
        Self::from_chain_spec(chain_spec, l1_provider)
    }

    /// Returns the Zone chain specification.
    pub fn chain_spec(&self) -> &Arc<ZoneChainSpec> {
        &self.chain_spec
    }

    /// Returns the underlying chain specification used by Tempo execution.
    pub fn tempo_chain_spec(&self) -> &Arc<TempoChainSpec> {
        self.inner.chain_spec()
    }
}

impl BlockExecutorFactory for ZoneEvmConfig {
    type EvmFactory = ZoneEvmFactory;
    type ExecutionCtx<'a> = TempoBlockExecutionCtx<'a>;
    type Transaction = TempoTxEnvelope;
    type Receipt = TempoReceipt;
    type TxExecutionResult = EthTxResult<TempoHaltReason, TempoTxType>;
    type Executor<'a, DB: StateDB, I: Inspector<TempoCtx<AnchoredZoneDb<DB, L1StateProvider>>>> =
        ZoneBlockExecutor<'a, DB, I>;

    fn evm_factory(&self) -> &Self::EvmFactory {
        &self.zone_factory
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: ZoneEvm<DB, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> Self::Executor<'a, DB, I>
    where
        DB: StateDB,
        I: Inspector<TempoCtx<AnchoredZoneDb<DB, L1StateProvider>>>,
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
        let mut env = self.inner.next_evm_env(parent, attributes)?;
        // TempoEvmConfig is concrete over TempoChainSpec, so apply the Zone fee policy after
        // delegating the rest of the environment construction.
        env.block_env.inner.basefee = self
            .chain_spec
            .next_block_base_fee(parent, attributes.timestamp)
            .unwrap_or_default();
        Ok(env)
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
                withdrawals: block
                    .body()
                    .withdrawals
                    .as_ref()
                    .map(|withdrawals| Cow::Borrowed(withdrawals.as_slice())),
                extra_data: block.header().extra_data().clone(),
                tx_count_hint: Some(block.body().transactions.len()),
                slot_number: block.slot_number(),
            },
            general_gas_limit: 0,
            shared_gas_limit: 0,
            validator_set: None,
            consensus_context: block.header().consensus_context,
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

#[cfg(test)]
mod tests {
    use super::*;
    use reth_chainspec::{EthChainSpec, ForkCondition};
    use tempo_chainspec::{
        hardfork::TempoHardfork,
        spec::{DEV, MODERATO, TempoHardforks},
    };

    #[test]
    fn composed_chain_spec_uses_zone_identity_and_parent_tempo_forks() {
        let zone = ZoneChainSpec::from(DEV.clone());
        let composed = ZoneEvmConfig::compose_chain_spec(&zone, &MODERATO);

        assert_eq!(composed.chain().id(), DEV.chain().id());
        assert_eq!(composed.genesis_hash(), DEV.genesis_hash());
        for &hardfork in TempoHardfork::VARIANTS {
            assert_eq!(
                composed.tempo_fork_activation(hardfork),
                MODERATO.tempo_fork_activation(hardfork)
            );
        }
    }

    #[test]
    fn tempo_evm_selects_parent_fork_from_zone_block_timestamp() {
        let zone = ZoneChainSpec::from(DEV.clone());
        let composed = ZoneEvmConfig::compose_chain_spec(&zone, &MODERATO);
        let activation_timestamp = TempoHardfork::VARIANTS
            .iter()
            .find_map(|&hardfork| match MODERATO.tempo_fork_activation(hardfork) {
                ForkCondition::Timestamp(timestamp) if timestamp > 0 => Some(timestamp),
                _ => None,
            })
            .expect("Moderato must have a post-genesis Tempo hardfork");
        let header = TempoHeader {
            inner: alloy_consensus::Header {
                timestamp: activation_timestamp,
                ..Default::default()
            },
            ..Default::default()
        };

        let config = TempoEvmConfig::new(composed.inner.clone());
        let env = config.evm_env(&header).expect("valid EVM environment");

        assert_eq!(
            env.cfg_env.spec,
            MODERATO.tempo_hardfork_at(activation_timestamp)
        );
    }
}
