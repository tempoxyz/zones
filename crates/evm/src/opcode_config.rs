//! Zone opcode configuration.

use alloy_consensus::transaction::Recovered;
use alloy_primitives::B256;
use evm2::{
    EvmConfig, ExecutionConfig, OpcodeConfig, SpecId, TxResult, Version,
    interpreter::{
        InstrStop, InterpreterState, Pc, Result, StackMut,
        instructions::create as CreateInstruction, op, private::Instruction,
    },
    registry::{HandlerError, TxRegistry, TxRequest},
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
        let l1 = l1.clone();
        zone.register(
            type_id,
            |tx: &TempoTxEnv| Some(tx),
            move |request: TxRequest<'_, '_, TempoEvmTypes, TempoTxEnv>| {
                l1.reset_transaction_state();
                crate::validate_transaction(request.envelope, CONTRACT_DEPLOYER_ALLOWLIST)
                    .map_err(HandlerError::external)?;

                // The accepted EVM overlay sits above L1OverlayDB. Snapshot its checkpoint
                // here so mirrored reads cannot fall back to the backing database's old value.
                let initial_anchor = request
                    .host
                    .state_mut()
                    .storage_slot_untracked(&TEMPO_STATE_ADDRESS, &TEMPO_BLOCK_NUMBER_SLOT)
                    .map_err(HandlerError::Fatal)?;
                l1.begin_transaction(initial_anchor);

                let tx_hash = match request.envelope.execution_context() {
                    ExecutionContext::Transaction { tx_hash } => tx_hash,
                    ExecutionContext::Simulation => B256::repeat_byte(0xff),
                };
                let fee_payer = request
                    .envelope
                    .fee_payer()
                    .unwrap_or_else(|_| request.envelope.recovered().signer());
                let _guard = tx_context::set_current_transaction(tx_hash, fee_payer);
                let transaction =
                    Recovered::new_unchecked(request.envelope.clone(), request.tx.signer());
                let result = handler.call(&transaction, request.host);
                l1.reset_transaction_state();
                result
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
