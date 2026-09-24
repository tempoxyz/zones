//! Zone opcode configuration.

use alloy_consensus::transaction::Recovered;
use alloy_primitives::B256;
use evm2::{
    EvmConfig, ExecutionConfig, OpcodeConfig, SpecId, TxResult, Version,
    interpreter::{
        InstrStop, InterpreterState, Pc, Result, StackMut,
        instructions::create as CreateInstruction, op, private::Instruction,
    },
    registry::{AnyTxHandler, HandlerError, HandlerResult, TxHandler, TxRegistry, TxRequest},
};
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_evm::{
    ExecutionContext, TempoEvmTypes, TempoTxEnv, tempo_opcode_config, tempo_tx_registry,
};
use zone_precompiles::{
    L1State, L1StorageReader, tempo_state::TEMPO_BLOCK_NUMBER_SLOT, tx_context,
};
use zone_primitives::constants::{CONTRACT_DEPLOYER_ALLOWLIST, TEMPO_STATE_ADDRESS};

/// Zone opcode configuration over Tempo and an inherited Ethereum specification.
pub(crate) struct ZoneConfig<const BASE_SPEC_ID: u32>(());

impl<const BASE_SPEC_ID: u32> EvmConfig<TempoEvmTypes> for ZoneConfig<BASE_SPEC_ID> {
    const BASE_SPEC_ID: SpecId =
        SpecId::try_from_u32(BASE_SPEC_ID).expect("invalid Zone base spec id");
    const OPCODE_CONFIG: &'static OpcodeConfig<TempoEvmTypes> =
        &zone_opcode_config::<BASE_SPEC_ID>();
}

/// Selects the Zone opcode table for the active Tempo specification.
pub(crate) fn zone_execution_config(
    spec: TempoHardfork,
    version: Version,
) -> ExecutionConfig<TempoEvmTypes> {
    let base_spec = SpecId::from(spec);
    evm2::spec_to_generic!(base_spec, |BASE_SPEC_ID| ExecutionConfig::for_config::<
        ZoneConfig<BASE_SPEC_ID>,
    >()
    .with_version(version))
}

/// Wraps Tempo's handlers with Zone transaction validation and transaction-local L1 context.
pub(crate) fn zone_tx_registry<L1: L1StorageReader>(
    spec: TempoHardfork,
    l1: L1State<L1>,
) -> TxRegistry<TempoEvmTypes, TxResult<TempoEvmTypes>> {
    let tempo = tempo_tx_registry(spec.into());
    let mut zone = TxRegistry::new();

    for type_id in [0, 1, 2, 4, 0x76] {
        let Some(handler) = tempo.get_by_type(type_id) else {
            continue;
        };
        zone.register(
            type_id,
            |tx: &TempoTxEnv| Some(tx),
            ZoneTxHandler {
                inner: handler,
                l1: l1.clone(),
            },
        );
    }

    zone
}

const fn zone_opcode_config<const BASE_SPEC_ID: u32>() -> OpcodeConfig<TempoEvmTypes> {
    let mut config = tempo_opcode_config::<BASE_SPEC_ID>();
    config.set_instruction::<ZoneCreate<false>>(op::CREATE, 0);
    config.set_instruction::<ZoneCreate<true>>(op::CREATE2, 0);
    config
}

struct ZoneCreate<const IS_CREATE2: bool>;

impl<const IS_CREATE2: bool> Instruction<TempoEvmTypes> for ZoneCreate<IS_CREATE2> {
    const DYNAMIC_GAS: bool = true;

    fn execute(
        pc: &mut Pc,
        stack: StackMut<'_>,
        state: &mut InterpreterState<'_, '_, TempoEvmTypes>,
    ) -> Result {
        if !CONTRACT_DEPLOYER_ALLOWLIST.contains(&state.message().destination) {
            return Err(InstrStop::NotActivated);
        }

        CreateInstruction::<TempoEvmTypes, IS_CREATE2>::execute(pc, stack, state)
    }
}

/// Applies Zone policies and context around either phase of a Tempo handler.
struct ZoneTxHandler<L1> {
    inner: AnyTxHandler<TempoEvmTypes, TxResult<TempoEvmTypes>>,
    l1: L1State<L1>,
}

impl<L1: L1StorageReader> ZoneTxHandler<L1> {
    fn enter(
        &self,
        request: &mut TxRequest<'_, '_, TempoEvmTypes, TempoTxEnv>,
    ) -> HandlerResult<ZoneTxContext<L1>> {
        self.l1.reset_transaction_state();
        crate::validate_transaction(request.envelope, CONTRACT_DEPLOYER_ALLOWLIST)
            .map_err(HandlerError::external)?;

        // The accepted EVM overlay sits above L1OverlayDB. Snapshot its checkpoint
        // here so mirrored reads cannot fall back to the backing database's old value.
        let initial_anchor = request
            .host
            .state_mut()
            .storage_slot_untracked(&TEMPO_STATE_ADDRESS, &TEMPO_BLOCK_NUMBER_SLOT)?;
        self.l1.begin_transaction(initial_anchor);

        let tx_hash = match request.envelope.execution_context() {
            ExecutionContext::Transaction { tx_hash } => tx_hash,
            ExecutionContext::Simulation => B256::repeat_byte(0xff),
        };
        let fee_payer = request
            .envelope
            .fee_payer()
            .unwrap_or_else(|_| request.tx.signer());
        Ok(ZoneTxContext {
            l1: self.l1.clone(),
            _transaction: tx_context::set_current_transaction(tx_hash, fee_payer),
        })
    }
}

impl<L1: L1StorageReader> TxHandler<TempoEvmTypes, TempoTxEnv, TxResult<TempoEvmTypes>>
    for ZoneTxHandler<L1>
{
    fn validate(
        &self,
        mut request: TxRequest<'_, '_, TempoEvmTypes, TempoTxEnv>,
    ) -> HandlerResult<()> {
        let _context = self.enter(&mut request)?;
        let transaction = Recovered::new_unchecked(request.envelope.clone(), request.tx.signer());
        self.inner.validate(&transaction, request.host)
    }

    fn execute(
        &self,
        mut request: TxRequest<'_, '_, TempoEvmTypes, TempoTxEnv>,
    ) -> HandlerResult<TxResult<TempoEvmTypes>> {
        let _context = self.enter(&mut request)?;
        let transaction = Recovered::new_unchecked(request.envelope.clone(), request.tx.signer());
        self.inner.execute(&transaction, request.host)
    }
}

struct ZoneTxContext<L1: L1StorageReader> {
    l1: L1State<L1>,
    _transaction: tx_context::TransactionContextGuard,
}

impl<L1: L1StorageReader> Drop for ZoneTxContext<L1> {
    fn drop(&mut self) {
        self.l1.reset_transaction_state();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Signed, TxLegacy};
    use alloy_primitives::{Address, Signature, TxKind, U256};
    use evm2::{
        Evm,
        evm::{InMemoryDB, precompile::NoPrecompiles},
        registry::handler,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tempo_evm::{TempoBlockEnv, TempoEvmExt, TempoInvalidTransaction, tempo_execution_config};
    use tempo_primitives::TempoTxEnvelope;
    use zone_precompiles::test_utils::MockL1Reader;

    fn transaction(to: TxKind) -> Recovered<TempoTxEnv> {
        let signer = Address::repeat_byte(0xab);
        let tx = Recovered::new_unchecked(
            TempoTxEnvelope::Legacy(Signed::new_unhashed(
                TxLegacy {
                    to,
                    ..Default::default()
                },
                Signature::test_signature(),
            )),
            signer,
        );
        Recovered::new_unchecked(tx.into(), signer)
    }

    #[test]
    fn validation_runs_zone_policy_checks() {
        let spec = TempoHardfork::T7;
        let l1 = L1State::new(MockL1Reader::default(), Address::ZERO);
        let mut evm = Evm::new_with_execution_config_and_ext(
            tempo_execution_config(spec, 1),
            spec,
            TempoBlockEnv::default(),
            zone_tx_registry(spec, l1.clone()),
            InMemoryDB::default(),
            NoPrecompiles::default(),
            TempoEvmExt::default(),
        );
        let error = evm.validate_tx(&transaction(TxKind::Create)).unwrap_err();
        assert!(matches!(
            error.external_ref::<TempoInvalidTransaction>(),
            Some(TempoInvalidTransaction::CallsValidation(
                "contract creation is not supported"
            ))
        ));
        assert_eq!(l1.initial_anchor(), None);
    }

    #[test]
    fn validation_sets_and_clears_l1_context() {
        let spec = TempoHardfork::T7;
        let l1 = L1State::new(MockL1Reader::default(), Address::ZERO);
        let preparations = Arc::new(AtomicUsize::new(0));
        let mut inner = TxRegistry::new();
        let observed_l1 = l1.clone();
        let observed_preparations = preparations.clone();
        inner.register(
            0,
            |tx: &TempoTxEnv| Some(tx),
            handler(
                move |request: &mut TxRequest<'_, '_, TempoEvmTypes, TempoTxEnv>| {
                    assert_eq!(observed_l1.initial_anchor(), Some(U256::from(42)));
                    request.host.ext_mut().resolved_fee_token = Some(Address::ZERO);
                    if observed_preparations.fetch_add(1, Ordering::SeqCst) == 1 {
                        return Err(HandlerError::Fatal("injected preparation failure".into()));
                    }
                    Ok(())
                },
                |_, ()| -> HandlerResult<TxResult<TempoEvmTypes>> {
                    panic!("pool validation must not execute user calls")
                },
            ),
        );
        let mut registry = TxRegistry::new();
        registry.register(
            0,
            |tx: &TempoTxEnv| Some(tx),
            ZoneTxHandler {
                inner: inner.get_by_type(0).unwrap(),
                l1: l1.clone(),
            },
        );
        let mut db = InMemoryDB::default();
        db.insert_account_storage(
            &TEMPO_STATE_ADDRESS,
            &TEMPO_BLOCK_NUMBER_SLOT,
            &U256::from(42),
        );
        let mut evm = Evm::new_with_execution_config_and_ext(
            tempo_execution_config(spec, 1),
            spec,
            TempoBlockEnv::default(),
            registry,
            db,
            NoPrecompiles::default(),
            TempoEvmExt::default(),
        );
        for fails in [false, true, false] {
            let result = evm.validate_tx(&transaction(TxKind::Call(Address::ZERO)));
            assert_eq!(result.is_err(), fails);
            assert_eq!(l1.initial_anchor(), None);
            assert_eq!(l1.get_anchor(), None);
        }
        assert_eq!(preparations.load(Ordering::SeqCst), 3);
    }
}
