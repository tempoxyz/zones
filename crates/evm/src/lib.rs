//! Zone-specific EVM configuration.
//!
//! Wraps [`TempoEvmConfig`] with a custom [`ZoneEvmFactory`] that registers
//! zone-specific native precompiles.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![allow(unnameable_types)]

mod executor;
pub mod precompiles;
mod tx_context;

use crate::{
    executor::ZoneBlockExecutor,
    precompiles::{
        AES_GCM_DECRYPT_ADDRESS, AesGcmDecrypt, CHAUM_PEDERSEN_VERIFY_ADDRESS, ChaumPedersenVerify,
        SequencerExt, TempoState, ZONE_TIP20_FACTORY_ADDRESS, ZONE_TIP403_PROXY_ADDRESS,
        ZoneTip20Token, ZoneTip403ProxyRegistry, ZoneTokenFactory,
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
use reth_evm::{
    ConfigureEngineEvm, ConfigureEvm, EvmEnvFor, ExecutableTxIterator, ExecutionCtxFor,
    block::StateDB,
    execute::{BlockAssembler, BlockAssemblerInput},
};
use reth_primitives_traits::{SealedBlock, SealedHeader};
use revm::{
    bytecode::opcode::{CREATE, CREATE2},
    context::{
        Transaction,
        result::{EVMError, ResultAndState},
    },
    interpreter::{
        Instruction, InstructionContext, InstructionResult, interpreter::EthInterpreter,
    },
};
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
    ACCOUNT_KEYCHAIN_ADDRESS, NONCE_PRECOMPILE_ADDRESS, PrecompileEnv, STABLECOIN_DEX_ADDRESS,
    TIP_FEE_MANAGER_ADDRESS, account_keychain::AccountKeychain, nonce::NonceManager,
    storage::actions::StorageActions, storage_credits::NonCreditableSlots,
    tip_fee_manager::TipFeeManager, tip20::is_tip20_prefix,
};
use tempo_primitives::{
    Block, TempoHeader, TempoPrimitives, TempoReceipt, TempoTxEnvelope, TempoTxType,
};
use tempo_revm::{TempoInvalidTransaction, TempoTxEnv};
use tempo_zone_contracts::{TEMPO_STATE_ADDRESS, ZONE_TX_CONTEXT_ADDRESS};
use zone_l1::state::{L1StateCache, L1StateProvider, L1StateProviderConfig, PolicyProvider};

type TempoCtx<DB> = <TempoEvmFactory as EvmFactory>::Context<DB>;
type ZoneInstructionCtx<'a, DB> = InstructionContext<'a, TempoCtx<DB>, EthInterpreter>;

/// Zone runtime EVM.
///
/// Wraps Tempo (L1) EVM to enforce Zone-specific rules, like disabling contract creation.
pub struct ZoneEvm<DB: Database, I> {
    inner: TempoEvm<DB, I>,
}

impl<DB: Database, I> ZoneEvm<DB, I> {
    /// Creates a new `ZoneEvm` by disabling `CREATE` and `CREATE2` opcodes.
    fn new(mut evm: TempoEvm<DB, I>) -> Self {
        fn disabled<DB: Database>(_: ZoneInstructionCtx<'_, DB>) -> Result<(), InstructionResult> {
            Err(InstructionResult::NotActivated)
        }

        let instructions = &mut evm.inner_mut().inner.instruction;
        instructions.insert_instruction(CREATE, Instruction::new(disabled::<DB>), 0);
        instructions.insert_instruction(CREATE2, Instruction::new(disabled::<DB>), 0);

        Self { inner: evm }
    }

    /// Provides a reference to the EVM context.
    pub fn ctx(&self) -> &TempoCtx<DB> {
        self.inner.ctx()
    }

    /// Provides a mutable reference to the EVM context.
    pub fn ctx_mut(&mut self) -> &mut TempoCtx<DB> {
        self.inner.ctx_mut()
    }

    /// Provides a mutable reference to the inner Tempo EVM.
    pub fn inner_mut(&mut self) -> &mut TempoEvm<DB, I> {
        &mut self.inner
    }
}

impl<DB, I> Evm for ZoneEvm<DB, I>
where
    DB: Database,
    I: Inspector<TempoCtx<DB>>,
{
    type DB = DB;
    type Tx = TempoTxEnv;
    type Error = EVMError<DB::Error, TempoInvalidTransaction>;
    type HaltReason = TempoHaltReason;
    type Spec = tempo_chainspec::hardfork::TempoHardfork;
    type BlockEnv = TempoBlockEnv;
    type Precompiles = PrecompilesMap;
    type Inspector = I;

    fn block(&self) -> &Self::BlockEnv {
        self.inner.block()
    }

    fn cfg_env(&self) -> &revm::context::CfgEnv<Self::Spec> {
        self.inner.cfg_env()
    }

    fn chain_id(&self) -> u64 {
        self.inner.chain_id()
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        // Ensure that none of the transaction top-level calls attempt contract creation.
        if tx.kind().is_create()
            || tx
                .tempo_tx_env
                .as_ref()
                .is_some_and(|aa| aa.aa_calls.iter().any(|call| call.to.is_create()))
        {
            return Err(EVMError::Custom(
                "contract creation not supported on zones".to_string(),
            ));
        }
        self.inner.transact_raw(tx)
    }

    fn transact_system_call(
        &mut self,
        caller: alloy_primitives::Address,
        contract: alloy_primitives::Address,
        data: alloy_primitives::Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.inner.transact_system_call(caller, contract, data)
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec, Self::BlockEnv>) {
        self.inner.finish()
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inner.set_inspector_enabled(enabled);
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        self.inner.components()
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        self.inner.components_mut()
    }
}

/// Zone EVM factory — wraps [`TempoEvmFactory`] and registers the
/// zone-native precompiles.
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
        let cfg = evm.ctx().cfg.clone();
        let (_, _, precompiles) = evm.components_mut();
        precompiles.apply_precompile(&TEMPO_STATE_ADDRESS, |_| {
            Some(TempoState::create(self.l1_provider.clone(), &cfg))
        });
        precompiles.apply_precompile(&ZONE_TX_CONTEXT_ADDRESS, |_| Some(ZoneTxContext::create()));
        precompiles.apply_precompile(&CHAUM_PEDERSEN_VERIFY_ADDRESS, |_| {
            Some(ChaumPedersenVerify::create(&cfg))
        });
        precompiles.apply_precompile(&AES_GCM_DECRYPT_ADDRESS, |_| {
            Some(AesGcmDecrypt::create(&cfg))
        });
        precompiles.apply_precompile(&ZONE_TIP20_FACTORY_ADDRESS, |_| {
            Some(ZoneTokenFactory::create(&cfg))
        });
        let registry = self
            .policy_provider
            .clone()
            .map(ZoneTip403ProxyRegistry::new);
        let sequencer: Arc<dyn SequencerExt> = Arc::new(self.l1_provider.clone());

        if let Some(provider) = self.policy_provider.clone() {
            precompiles.apply_precompile(&ZONE_TIP403_PROXY_ADDRESS, |_| {
                Some(ZoneTip403ProxyRegistry::create(provider.clone(), &cfg))
            });
        }

        // Override the TIP-20 precompile lookup so that all TIP-20 token
        // calls go through ZoneTip20Token. When a live policy provider is
        // available, the wrapper also enforces TIP-403 policy checks; without
        // one, it still applies privacy, fixed-gas, and bridge-auth rules.
        //
        // This replaces the upstream `extend_tempo_precompiles` lookup, so we
        // must also handle the non-TIP-20 Tempo precompiles that are zone-relevant
        // (FeeManager, NonceManager, AccountKeychain).
        // Zone-specific overrides (TIP20Factory, TIP403Proxy) are in the
        // static map via `apply_precompile` and take priority over this.
        let zone_cfg = cfg.clone();
        let zone_env = PrecompileEnv::new(
            &cfg,
            StorageActions::disabled(),
            Rc::new(RefCell::new(NonCreditableSlots::empty())),
        );
        precompiles.set_precompile_lookup(move |address: &alloy_primitives::Address| {
            if is_tip20_prefix(*address) {
                Some(ZoneTip20Token::create(
                    *address,
                    &zone_cfg,
                    registry.clone(),
                    sequencer.clone(),
                ))
            } else if *address == TIP_FEE_MANAGER_ADDRESS {
                Some(TipFeeManager::create_precompile(&zone_env))
            } else if *address == STABLECOIN_DEX_ADDRESS {
                None
            } else if *address == NONCE_PRECOMPILE_ADDRESS {
                Some(NonceManager::create_precompile(&zone_env))
            } else if *address == ACCOUNT_KEYCHAIN_ADDRESS {
                Some(AccountKeychain::create_precompile(&zone_env))
            } else {
                None
            }
        });
        evm
    }
}

impl EvmFactory for ZoneEvmFactory {
    type Evm<DB: Database, I: Inspector<Self::Context<DB>>> = ZoneEvm<DB, I>;
    type Context<DB: Database> = TempoCtx<DB>;
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
        let evm = TempoEvm::new(db, input);
        ZoneEvm::new(self.register_precompiles(evm))
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<Self::Spec, Self::BlockEnv>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        let evm = TempoEvm::new(db, input).with_inspector(inspector);
        ZoneEvm::new(self.register_precompiles(evm))
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

/// Zone EVM configuration — wraps [`TempoEvmConfig`] with a [`ZoneEvmFactory`].
#[derive(Debug, Clone)]
pub struct ZoneEvmConfig {
    inner: TempoEvmConfig,
    zone_factory: ZoneEvmFactory,
    block_assembler: ZoneBlockAssembler,
}

impl ZoneEvmConfig {
    /// Create a new zone EVM config with the given chain spec, L1 state
    /// provider.
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

    /// Create a zone EVM config without a usable L1 provider.
    ///
    /// Intended for CLI subcommands (import, stage, re-execute) that need a type-compatible
    /// EVM config but don't have access to an L1 RPC connection. The portal address defaults to
    /// the zero address in this mode, so sequencer reads are treated as unavailable.
    pub fn new_without_l1(chain_spec: Arc<TempoChainSpec>) -> Self {
        let cache = L1StateCache::default();
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_http("http://127.0.0.1:1".parse().expect("valid fallback URL"))
            .erased();
        let runtime_handle = tokio::runtime::Handle::current();
        let config = L1StateProviderConfig::default();
        let l1_provider = L1StateProvider::new_raw(config, cache, provider, runtime_handle);
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
    type TxExecutionResult = EthTxResult<TempoHaltReason, TempoTxType>;
    type Executor<'a, DB: StateDB, I: Inspector<TempoCtx<DB>>> = ZoneBlockExecutor<'a, DB, I>;

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
        I: Inspector<TempoCtx<DB>>,
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
    use alloy_primitives::{Address, Bytes, bytes};
    use revm::{
        bytecode::Bytecode,
        context::{TxEnv, result::ExecutionResult},
        database::{EmptyDB, in_memory_db::CacheDB},
        inspector::NoOpInspector,
        state::AccountInfo,
    };
    use tempo_evm::TempoBlockEnv;
    use tempo_revm::TempoTxEnv;

    fn evm_with_contract(addr: Address, code: &[u8]) -> ZoneEvm<CacheDB<EmptyDB>, NoOpInspector> {
        let bytecode = Bytes::copy_from_slice(code);
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            addr,
            AccountInfo {
                code_hash: alloy_primitives::keccak256(&bytecode),
                code: Some(Bytecode::new_raw(bytecode)),
                ..Default::default()
            },
        );

        let input: EvmEnv<tempo_chainspec::hardfork::TempoHardfork, TempoBlockEnv> =
            EvmEnv::default();
        let evm = TempoEvm::new(db, input);
        ZoneEvm::new(evm)
    }

    #[test]
    fn top_level_create_transaction_is_disabled() {
        let mut evm = evm_with_contract(Address::ZERO, &[]);
        let err = evm
            .transact_raw(TempoTxEnv {
                inner: TxEnv {
                    caller: Address::repeat_byte(0x01),
                    gas_price: 0,
                    gas_limit: 1_000_000,
                    kind: alloy_primitives::TxKind::Create,
                    data: Bytes::from_static(&[0x00]),
                    ..Default::default()
                },
                ..Default::default()
            })
            .expect_err("top-level create must be rejected");

        assert!(
            matches!(err, EVMError::Custom(message) if message == "contract creation not supported on zones")
        );
    }

    #[test]
    fn runtime_create_opcodes_are_disabled() {
        let (contract, caller) = (Address::random(), Address::random());
        for bytecode in [
            // PUSH0 PUSH0 PUSH0 CREATE STOP
            bytes!("0x5f5f5ff000"),
            // PUSH0 PUSH0 PUSH0 PUSH0 CREATE2 STOP
            bytes!("0x5f5f5f5ff500"),
        ] {
            let mut evm = evm_with_contract(contract, &bytecode);
            let result = evm
                .transact_raw(TempoTxEnv {
                    inner: TxEnv {
                        caller,
                        gas_price: 0,
                        gas_limit: 1_000_000,
                        kind: alloy_primitives::TxKind::Call(contract),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .expect("transaction should execute")
                .result;

            assert!(matches!(
                result,
                ExecutionResult::Halt {
                    reason: TempoHaltReason::Ethereum(
                        revm::context::result::HaltReason::NotActivated
                    ),
                    ..
                }
            ));
        }
    }
}
