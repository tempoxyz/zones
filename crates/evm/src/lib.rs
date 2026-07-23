//! Zone-specific EVM configuration.
//!
//! Wraps [`TempoEvmConfig`] with a [`ZoneEvmFactory`] that installs the L1-anchored database,
//! registers Zone-native precompiles, and preserves the original database at the [`Evm`] boundary.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![allow(unnameable_types)]

mod database;
mod executor;
mod fee_manager;
pub mod precompiles;
mod zone_evm;

pub use database::{L1OverlayDB, ZoneDbError};
pub use executor::{ZoneBlockExecutor, ZoneTxResult};
pub use zone_evm::{ZoneEvm, contract_creation::validate_transaction};

use crate::{
    fee_manager::ZoneProtocolFeeManager,
    precompiles::{
        AES_GCM_DECRYPT_ADDRESS, AesGcmDecrypt, CHAUM_PEDERSEN_VERIFY_ADDRESS, ChaumPedersenVerify,
        L1State, L1StorageReader, TIP403_REGISTRY_ADDRESS, TempoState, ZONE_FEE_MANAGER_ADDRESS,
        ZONE_TIP20_FACTORY_ADDRESS, ZoneInbox, ZonePrecompileEnv, ZoneTokenFactory,
        create_tip20_precompile, create_tip403_precompile, create_zone_fee_manager_precompile,
        tx_context::ZoneTxContext,
    },
};
use alloy_evm::{
    Database, Evm, EvmEnv, EvmFactory,
    block::BlockExecutorFactory,
    precompiles::PrecompilesMap,
    revm::{Inspector, context::DBErrorMarker, inspector::NoOpInspector},
};
use alloy_primitives::Address;
use alloy_provider::{Provider, ProviderBuilder};
use reth_chainspec::EthChainSpec;
use reth_evm::{
    ConfigureEngineEvm, ConfigureEvm, EvmEnvFor, ExecutableTxIterator, ExecutionCtxFor,
    block::StateDB,
    execute::{BlockAssembler, BlockAssemblerInput},
};
use reth_primitives_traits::{SealedBlock, SealedHeader};
use std::{fmt, num::NonZeroU32, sync::Arc};
use tempo_alloy::TempoNetwork;
use tempo_chainspec::{TempoChainSpec, hardfork::TempoHardfork};
use tempo_evm::{
    FeeTokenResolver, TempoBlockAssembler, TempoBlockEnv, TempoBlockExecutionCtx, TempoEvmConfig,
    TempoEvmError, TempoHaltReason, TempoNextBlockEnvAttributes, TempoStateAccess,
    evm::{TempoEvm, TempoEvmFactory},
};
use tempo_payload_types::TempoExecutionData;
use tempo_precompiles::{
    ACCOUNT_KEYCHAIN_ADDRESS, NONCE_PRECOMPILE_ADDRESS, PrecompileEnv,
    RECEIVE_POLICY_GUARD_ADDRESS, STABLECOIN_DEX_ADDRESS, TIP_FEE_MANAGER_ADDRESS,
    account_keychain::AccountKeychain, error::Result as TempoResult, nonce::NonceManager,
    receive_policy_guard::ReceivePolicyGuard, storage::actions::StorageActions,
    tip20::is_tip20_prefix,
};
use tempo_primitives::{
    Block, TempoHeader, TempoPrimitives, TempoReceipt, TempoTxEnvelope, TempoTxType,
};
use tempo_revm::TempoTxEnv;
use tempo_zone_contracts::{
    TEMPO_STATE_ADDRESS, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS, ZONE_TX_CONTEXT_ADDRESS,
};
use zone_chainspec::ZoneChainSpec;
use zone_l1::state::{L1StateCache, L1StateProvider, L1StateProviderConfig};
use zone_precompiles::create_outbox_precompile;

type TempoCtx<DB> = <TempoEvmFactory as EvmFactory>::Context<DB>;

/// Zone EVM factory that adapts caller databases and registers the zone-native precompiles.
#[derive(Debug, Clone)]
pub struct ZoneEvmFactory<L1 = L1StateProvider> {
    l1_reader: L1,
    portal_address: Address,
}

impl<L1> ZoneEvmFactory<L1>
where
    L1: L1StorageReader,
{
    /// Create a new factory with the given L1 state reader and Zone portal address.
    pub fn new(l1_reader: L1, portal_address: Address) -> Self {
        Self {
            l1_reader,
            portal_address,
        }
    }

    fn register_precompiles<DB: Database, I: Inspector<TempoCtx<L1OverlayDB<DB, L1>>>>(
        &self,
        evm: TempoEvm<L1OverlayDB<DB, L1>, I>,
        l1: L1State<L1>,
    ) -> TempoEvm<L1OverlayDB<DB, L1>, I> {
        let mut evm = evm.with_fee_manager(ZoneProtocolFeeManager::new());
        let cfg = evm.ctx().cfg.clone();
        let actions = StorageActions::disabled();
        let non_creditable_slots = evm.non_creditable_slots();
        let (_, _, precompiles) = evm.components_mut();
        let env = ZonePrecompileEnv::new(&cfg, actions.clone(), non_creditable_slots.clone());
        precompiles.apply_precompile(&TEMPO_STATE_ADDRESS, |_| {
            Some(TempoState::create(l1.clone(), &env))
        });
        precompiles.apply_precompile(&ZONE_TX_CONTEXT_ADDRESS, |_| Some(ZoneTxContext::create()));
        let inbox_env = env.clone();
        let inbox_l1 = l1.clone();
        precompiles.apply_precompile(&ZONE_INBOX_ADDRESS, move |_| {
            Some(ZoneInbox::create(inbox_l1.clone(), &inbox_env))
        });
        let outbox_env = env.clone();
        let outbox_l1 = l1.clone();
        precompiles.apply_precompile(&ZONE_OUTBOX_ADDRESS, move |_| {
            Some(create_outbox_precompile(outbox_l1.clone(), &outbox_env))
        });
        precompiles.apply_precompile(&CHAUM_PEDERSEN_VERIFY_ADDRESS, |_| {
            Some(ChaumPedersenVerify::create(&env))
        });
        precompiles.apply_precompile(&AES_GCM_DECRYPT_ADDRESS, |_| {
            Some(AesGcmDecrypt::create(&env))
        });
        precompiles.apply_precompile(&ZONE_TIP20_FACTORY_ADDRESS, |_| {
            Some(ZoneTokenFactory::create(&env))
        });
        precompiles.apply_precompile(&TIP_FEE_MANAGER_ADDRESS, |_| None);
        let fee_env = env.clone();
        precompiles.apply_precompile(&ZONE_FEE_MANAGER_ADDRESS, move |_| {
            Some(create_zone_fee_manager_precompile(&fee_env))
        });
        let tip403_env = env.clone();
        precompiles.apply_precompile(&TIP403_REGISTRY_ADDRESS, move |_| {
            Some(create_tip403_precompile(&tip403_env))
        });
        let tip20_l1 = l1.clone();
        let tempo_env = PrecompileEnv::new(&cfg, actions, non_creditable_slots);
        precompiles.set_precompile_lookup(move |address: &alloy_primitives::Address| {
            if is_tip20_prefix(*address) {
                Some(create_tip20_precompile(*address, &env, tip20_l1.clone()))
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
    L1: L1StorageReader,
{
    type Evm<DB: Database, I: Inspector<Self::Context<DB>>> = ZoneEvm<DB, I, L1>;
    type Context<DB: Database> = TempoCtx<L1OverlayDB<DB, L1>>;
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
        let db = L1OverlayDB::new(db, self.l1_reader.clone(), self.portal_address);
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
        let db = L1OverlayDB::new(db, self.l1_reader.clone(), self.portal_address);
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

impl<L1> BlockAssembler<ZoneEvmConfig<L1>> for ZoneBlockAssembler
where
    L1: L1StorageReader,
{
    type Block = Block;

    fn assemble_block(
        &self,
        input: BlockAssemblerInput<'_, '_, ZoneEvmConfig<L1>, TempoHeader>,
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
#[derive(Clone)]
pub struct ZoneEvmConfig<L1 = L1StateProvider> {
    inner: TempoEvmConfig,
    chain_spec: Arc<ZoneChainSpec>,
    zone_factory: ZoneEvmFactory<L1>,
    block_assembler: ZoneBlockAssembler,
}

impl<L1> FeeTokenResolver for ZoneEvmConfig<L1> {
    fn resolve_fee_token<S, M>(
        &self,
        state: &mut S,
        tx: &TempoTxEnv,
        _fee_payer: Address,
        spec: TempoHardfork,
        actions: StorageActions,
    ) -> TempoResult<Address>
    where
        S: TempoStateAccess<M>,
    {
        fee_manager::resolve_fee_token(state, tx, spec, actions)
    }
}

impl<L1> ZoneEvmConfig<L1>
where
    L1: L1StorageReader,
{
    /// Creates a Zone EVM config using Tempo hardfork conditions from the parent L1 spec.
    pub fn new(
        zone_chain_spec: Arc<ZoneChainSpec>,
        tempo_chain_spec: Arc<TempoChainSpec>,
        l1_provider: L1,
        portal_address: Address,
    ) -> Self {
        let chain_spec = compose_chain_spec(&zone_chain_spec, &tempo_chain_spec);
        Self::from_chain_spec(chain_spec, l1_provider, portal_address)
    }

    fn from_chain_spec(
        chain_spec: Arc<ZoneChainSpec>,
        l1_provider: L1,
        portal_address: Address,
    ) -> Self {
        let zone_factory = ZoneEvmFactory::new(l1_provider, portal_address);
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

    /// Returns the Zone chain specification.
    pub fn chain_spec(&self) -> &Arc<ZoneChainSpec> {
        &self.chain_spec
    }

    /// Returns the underlying chain specification used by Tempo execution.
    pub fn tempo_chain_spec(&self) -> &Arc<TempoChainSpec> {
        self.inner.chain_spec()
    }
}

impl ZoneEvmConfig {
    /// Creates a Zone EVM config without a usable L1 provider.
    ///
    /// Intended for CLI subcommands (import, stage, re-execute) that need a type-compatible
    /// EVM config but don't have access to an L1 RPC connection. Tempo hardfork conditions come
    /// from `chain_spec` because the parent L1 spec cannot be resolved in this mode.
    pub fn new_without_l1(chain_spec: Arc<ZoneChainSpec>) -> Self {
        let cache = L1StateCache::default();
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_http("http://127.0.0.1:1".parse().expect("valid fallback URL"))
            .erased();
        let runtime_handle = tokio::runtime::Handle::current();
        let config = L1StateProviderConfig {
            max_sync_attempts: Some(NonZeroU32::MIN),
            ..Default::default()
        };
        let l1_provider = L1StateProvider::new_raw(config, cache, provider, runtime_handle);
        Self::from_chain_spec(chain_spec, l1_provider, Address::ZERO)
    }
}

impl<L1> fmt::Debug for ZoneEvmConfig<L1> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZoneEvmConfig")
            .field("inner", &self.inner)
            .field("chain_spec", &self.chain_spec)
            .field("block_assembler", &self.block_assembler)
            .finish_non_exhaustive()
    }
}

impl<L1> BlockExecutorFactory for ZoneEvmConfig<L1>
where
    L1: L1StorageReader,
{
    type EvmFactory = ZoneEvmFactory<L1>;
    type ExecutionCtx<'a> = TempoBlockExecutionCtx<'a>;
    type Transaction = TempoTxEnvelope;
    type Receipt = TempoReceipt;
    type TxExecutionResult = ZoneTxResult<TempoHaltReason, TempoTxType>;
    type Executor<'a, DB: StateDB, I: Inspector<TempoCtx<L1OverlayDB<DB, L1>>>> =
        ZoneBlockExecutor<'a, DB, I, L1>;

    fn evm_factory(&self) -> &Self::EvmFactory {
        &self.zone_factory
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: ZoneEvm<DB, I, L1>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> Self::Executor<'a, DB, I>
    where
        DB: StateDB,
        I: Inspector<TempoCtx<L1OverlayDB<DB, L1>>>,
    {
        ZoneBlockExecutor::new(evm, ctx, self.chain_spec())
    }
}

impl<L1> ConfigureEvm for ZoneEvmConfig<L1>
where
    L1: L1StorageReader + Unpin,
{
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

impl<L1> ConfigureEngineEvm<TempoExecutionData> for ZoneEvmConfig<L1>
where
    L1: L1StorageReader + Unpin,
{
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

/// Copies the Zone chain spec and applies the Tempo hardfork conditions from its parent chain.
fn compose_chain_spec(zone: &ZoneChainSpec, tempo: &TempoChainSpec) -> Arc<ZoneChainSpec> {
    Arc::new(zone.clone().with_tempo_hardforks_from(tempo))
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloy_primitives::{B256, Bytes, U256, address, keccak256};
    use alloy_rlp::Encodable;
    use alloy_sol_types::SolCall;
    use reth_chainspec::{EthChainSpec, ForkCondition};
    use revm::{
        context::result::ExecutionResult,
        database::{CacheDB, EmptyDB},
    };
    use tempo_chainspec::{
        hardfork::TempoHardfork,
        spec::{DEV, MODERATO, TempoHardforks},
    };
    use tempo_precompiles::{
        TIP403_REGISTRY_ADDRESS, storage::StorageKey, tip403_registry::tip403_registry_slots,
    };
    use tempo_zone_contracts::IZoneInbox;
    use zone_precompiles::{tempo_state::TEMPO_BLOCK_NUMBER_SLOT, test_utils::MockL1Reader};
    use zone_primitives::constants::{
        PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, PORTAL_IS_SEQUENCER_SLOT, TEMPO_STATE_ADDRESS,
        ZONE_INBOX_ADDRESS,
    };

    #[test]
    fn composed_chain_spec_uses_zone_identity_and_parent_tempo_forks() {
        let zone = ZoneChainSpec::from(DEV.clone());
        let composed = compose_chain_spec(&zone, &MODERATO);

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
    fn advance_tempo_keeps_overlay_reads_on_child_anchor() {
        const PARENT: u64 = 0;
        const CHILD: u64 = 1;
        let portal = Address::repeat_byte(0x42);
        let sequencer = Address::repeat_byte(0xa1);
        let token = address!("0x20C00000000000000000000000000000000000AA");
        let reader = MockL1Reader::default();

        let membership_slot = sequencer.mapping_slot(PORTAL_IS_SEQUENCER_SLOT.into());
        reader.set_u256(portal, membership_slot, PARENT, U256::ZERO);
        reader.set_u256(portal, membership_slot, CHILD, U256::ONE);
        reader.set_u256(
            portal,
            U256::from_be_bytes(PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT.0),
            CHILD,
            U256::ZERO,
        );

        let policy_slot = token.mapping_slot(tip403_registry_slots::TOKEN_TRANSFER_POLICIES);
        let parent_policy = U256::from(0xaaaa);
        let child_policy = U256::from(0xbbbb);
        reader.set_u256(TIP403_REGISTRY_ADDRESS, policy_slot, PARENT, parent_policy);
        reader.set_u256(TIP403_REGISTRY_ADDRESS, policy_slot, CHILD, child_policy);

        let genesis = TempoHeader::default();
        let mut genesis_rlp = Vec::new();
        genesis.encode(&mut genesis_rlp);
        let genesis_hash = keccak256(&genesis_rlp);
        let child = TempoHeader {
            inner: alloy_consensus::Header {
                parent_hash: genesis_hash,
                number: CHILD,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut child_rlp = Vec::new();
        child.encode(&mut child_rlp);

        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_storage(
            TEMPO_STATE_ADDRESS,
            U256::ZERO,
            U256::from_be_bytes(genesis_hash.0),
        )
        .unwrap();
        db.insert_account_storage(
            TEMPO_STATE_ADDRESS,
            TEMPO_BLOCK_NUMBER_SLOT,
            U256::from(PARENT),
        )
        .unwrap();

        let factory = ZoneEvmFactory::new(reader.clone(), portal);
        let mut evm = factory.create_evm(db, EvmEnv::default());
        let calldata = IZoneInbox::advanceTempoCall {
            header: Bytes::from(child_rlp),
            deposits: Vec::new(),
            decryptions: Vec::new(),
            enabledTokens: vec![IZoneInbox::EnabledToken {
                token,
                name: "Adversarial Token".into(),
                symbol: "ADV".into(),
                currency: "USD".into(),
            }],
        }
        .abi_encode();

        let result = evm
            .transact_system_call(Address::ZERO, ZONE_INBOX_ADDRESS, calldata.into())
            .expect("advanceTempo execution must not fail");
        assert!(matches!(result.result, ExecutionResult::Success { .. }));
        assert_eq!(
            evm.ctx().journaled_state.database.l1_state().get_anchor(),
            None,
            "transaction completion must clear the shared L1 anchor"
        );

        let requests = reader.storage_requests();
        let child_membership_request = (portal, B256::from(membership_slot.to_be_bytes()), CHILD);
        let parent_membership_request = (portal, B256::from(membership_slot.to_be_bytes()), PARENT);
        let child_policy_request = (
            TIP403_REGISTRY_ADDRESS,
            B256::from(policy_slot.to_be_bytes()),
            CHILD,
        );
        let parent_policy_request = (
            TIP403_REGISTRY_ADDRESS,
            B256::from(policy_slot.to_be_bytes()),
            PARENT,
        );
        let queue_head_request = (portal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, CHILD);

        assert!(!requests.contains(&child_membership_request));
        assert!(!requests.contains(&parent_membership_request));
        assert!(requests.contains(&child_policy_request));
        assert!(!requests.contains(&parent_policy_request));
        assert!(requests.contains(&queue_head_request));
    }

    #[test]
    fn tempo_evm_selects_parent_fork_from_zone_block_timestamp() {
        let zone = ZoneChainSpec::from(DEV.clone());
        let composed = compose_chain_spec(&zone, &MODERATO);
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
