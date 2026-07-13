//! Shared execution for zone-native and L1-backed Tempo precompiles.
//!
//! Both helpers install an EVM-backed [`StorageCtx`], apply zone-specific [`CallRules`], and
//! forward admitted calls without changing their calldata or caller.
//!
//! # Execution modes
//!
//! - [`create_local_precompile`] executes against ordinary zone-local EVM state.
//! - [`create_l1_backed_precompile`] reads the finalized Tempo block recorded in `TempoState`,
//!   resolves the Tempo hardfork at that same block, and overlays selected policy storage from L1.
//!
//! # Call ordering
//!
//! 1. Reject disallowed delegate calls before constructing storage or reading the L1 anchor.
//! 2. Decode the selector and reject calls that cannot cover a configured fixed gas charge.
//! 3. Install the appropriate storage provider. L1-backed calls read their anchor once and fail
//!    closed if the anchor, hardfork, or overlay cannot be resolved.
//! 4. Apply [`CallRules`]. Admitted calls receive the original calldata and caller; rejected calls
//!    are returned without invoking the supplied implementation.
//! 5. Apply the configured fixed gas charge to successful precompile results.
//!
//! Rule-level rejections include calldata input gas. Calls without a fixed charge retain normal
//! provider metering, while successful fixed-price calls report exactly the configured charge.

use alloc::rc::Rc;
use core::cell::RefCell;

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::Address;
use alloy_sol_types::SolError;
use revm::precompile::{PrecompileHalt, PrecompileId, PrecompileOutput, PrecompileResult};
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_precompiles::{
    DelegateCallNotAllowed, charge_input_cost,
    dispatch::selector_from_calldata,
    storage::{
        PrecompileStorageProvider, StorageCtx, actions::StorageActions,
        evm::EvmPrecompileStorageProvider,
    },
    storage_credits::NonCreditableSlots,
};

use crate::storage::{L1StorageReader, ZonePrecompileStorageProvider, read_l1_anchor};

/// Shared inputs for precompiles executing over finalized Tempo state.
///
/// Each call combines zone EVM configuration and accounting state with an L1 reader. The exact
/// L1 block and active hardfork are resolved from the local `TempoState` anchor during execution.
#[derive(Clone)]
pub(crate) struct L1BackedPrecompileEnv<P> {
    cfg: revm::context::CfgEnv<TempoHardfork>,
    l1_reader: P,
    actions: StorageActions,
    non_creditable_slots: Rc<RefCell<NonCreditableSlots>>,
}

impl<P> L1BackedPrecompileEnv<P> {
    /// Capture the configuration and providers shared by L1-backed calls.
    pub(crate) fn new(
        cfg: &revm::context::CfgEnv<TempoHardfork>,
        l1_reader: P,
        actions: StorageActions,
        non_creditable_slots: Rc<RefCell<NonCreditableSlots>>,
    ) -> Self {
        Self {
            cfg: cfg.clone(),
            l1_reader,
            actions,
            non_creditable_slots,
        }
    }
}

/// Selector- and caller-dependent admission rules applied inside a [`StorageCtx`].
///
/// Returning `Some(result)` from [`Self::check_call`] rejects the call without invoking the
/// forwarded implementation. The helper adds calldata input gas before returning that result.
pub(crate) trait CallRules: 'static {
    /// Whether delegate calls must be rejected before storage setup.
    fn direct_call_only(&self) -> bool {
        false
    }

    /// Return the fixed gas charge for this selector, if one applies.
    fn fixed_gas(&self, _selector: Option<[u8; 4]>) -> Option<u64> {
        None
    }

    /// Return a rejection result, or `None` to forward the original call.
    fn check_call(
        &self,
        _data: &[u8],
        _selector: Option<[u8; 4]>,
        _caller: Address,
        _is_direct: bool,
    ) -> Option<PrecompileResult> {
        None
    }
}

/// Rules for precompiles that require no zone-specific admission or fixed gas handling.
pub(crate) struct NoCallRules;
impl CallRules for NoCallRules {}

/// Rules for precompiles whose semantics require execution at their registered address.
pub(crate) struct DirectCallOnly;
impl CallRules for DirectCallOnly {
    fn direct_call_only(&self) -> bool {
        true
    }
}

/// Create a precompile with zone call rules and ordinary local EVM storage.
///
/// This helper neither reads a Tempo anchor nor installs an L1 overlay. Calls admitted by `rules`
/// are forwarded to `execute` with their original calldata and caller.
pub(crate) fn create_local_precompile<Rules, Execute>(
    id: &'static str,
    cfg: &revm::context::CfgEnv<TempoHardfork>,
    rules: Rules,
    execute: Execute,
) -> DynPrecompile
where
    Rules: CallRules,
    Execute: Fn(&[u8], Address) -> PrecompileResult + 'static,
{
    let spec = cfg.spec;
    let amsterdam_eip8037_enabled = cfg.enable_amsterdam_eip8037;
    let gas_params = cfg.gas_params.clone();

    DynPrecompile::new_stateful(PrecompileId::Custom(id.into()), move |input| {
        if rules.direct_call_only() && !input.is_direct_call() {
            return delegate_call_not_allowed(input.reservoir);
        }

        let selector = selector_from_calldata(input.data);
        let fixed_gas = rules.fixed_gas(selector);
        if fixed_gas.is_some_and(|gas| input.gas < gas) {
            return Ok(PrecompileOutput::halt(
                PrecompileHalt::OutOfGas,
                input.reservoir,
            ));
        }

        let is_direct = input.is_direct_call();
        let mut storage = EvmPrecompileStorageProvider::new(
            input.internals,
            fixed_gas.map_or(input.gas, |_| u64::MAX),
            input.reservoir,
            spec,
            amsterdam_eip8037_enabled,
            input.is_static,
            gas_params.clone(),
        );

        StorageCtx::enter(&mut storage, || {
            let result = match rules.check_call(input.data, selector, input.caller, is_direct) {
                Some(result) => add_input_cost(input.data, result),
                None => execute(input.data, input.caller),
            };
            apply_fixed_gas(result, fixed_gas)
        })
    })
}

/// Create a direct-call-only precompile backed by the finalized Tempo L1 anchor.
///
/// The helper rejects delegate calls before any storage access, reads the `TempoState` anchor once,
/// and constructs [`ZonePrecompileStorageProvider`] with that exact block. Construction is
/// fallible because the provider resolves the active hardfork from the same anchor. Any anchor,
/// hardfork, or L1 storage failure is returned as a precompile error rather than falling back to
/// local or latest state.
///
/// Calls admitted by `rules` are forwarded to `execute` with their original calldata and caller
/// while the L1 overlay is active.
pub(crate) fn create_l1_backed_precompile<P, Rules, Execute>(
    id: &'static str,
    env: L1BackedPrecompileEnv<P>,
    rules: Rules,
    execute: Execute,
) -> DynPrecompile
where
    P: L1StorageReader,
    Rules: CallRules,
    Execute: Fn(&[u8], Address) -> PrecompileResult + 'static,
{
    let zone_spec = env.cfg.spec;
    let amsterdam_eip8037_enabled = env.cfg.enable_amsterdam_eip8037;
    let gas_params = env.cfg.gas_params;
    let actions = env.actions;
    let non_creditable_slots = env.non_creditable_slots;
    let l1_reader = env.l1_reader;

    DynPrecompile::new_stateful(PrecompileId::Custom(id.into()), move |input| {
        if !input.is_direct_call() {
            return delegate_call_not_allowed(input.reservoir);
        }

        let selector = selector_from_calldata(input.data);
        let fixed_gas = rules.fixed_gas(selector);
        if fixed_gas.is_some_and(|gas| input.gas < gas) {
            return Ok(PrecompileOutput::halt(
                PrecompileHalt::OutOfGas,
                input.reservoir,
            ));
        }

        let mut inner = EvmPrecompileStorageProvider::new(
            input.internals,
            fixed_gas.map_or(input.gas, |_| u64::MAX),
            input.reservoir,
            zone_spec,
            amsterdam_eip8037_enabled,
            input.is_static,
            gas_params.clone(),
        )
        .with_actions(actions.clone())
        .with_non_creditable_slots(non_creditable_slots.clone());

        let l1_block_number = match read_l1_anchor(&mut inner) {
            Ok(block_number) => block_number,
            Err(err) => {
                return err.into_precompile_result(inner.gas_used(), inner.reservoir());
            }
        };
        let gas_used = inner.gas_used();
        let reservoir = inner.reservoir();
        let mut storage =
            match ZonePrecompileStorageProvider::new(inner, l1_reader.clone(), l1_block_number) {
                Ok(storage) => storage,
                Err(err) => return err.into_precompile_result(gas_used, reservoir),
            };

        StorageCtx::enter(&mut storage, || {
            let result = match rules.check_call(input.data, selector, input.caller, true) {
                Some(result) => add_input_cost(input.data, result),
                None => execute(input.data, input.caller),
            };
            apply_fixed_gas(result, fixed_gas)
        })
    })
}

fn delegate_call_not_allowed(reservoir: u64) -> PrecompileResult {
    Ok(PrecompileOutput::revert(
        0,
        SolError::abi_encode(&DelegateCallNotAllowed {}).into(),
        reservoir,
    ))
}

fn add_input_cost(calldata: &[u8], result: PrecompileResult) -> PrecompileResult {
    let mut storage = StorageCtx::default();
    let gas_before = storage.gas_used();
    if let Some(err) = charge_input_cost(&mut storage, calldata) {
        return err;
    }
    let input_gas = storage.gas_used().saturating_sub(gas_before);
    result.map(|mut output| {
        output.gas_used = output.gas_used.saturating_add(input_gas);
        output
    })
}

fn apply_fixed_gas(result: PrecompileResult, fixed_gas: Option<u64>) -> PrecompileResult {
    match (result, fixed_gas) {
        (Ok(mut output), Some(gas)) => {
            output.gas_used = gas;
            Ok(output)
        }
        (result, _) => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        tempo_state::slots as tempo_state_slots,
        test_utils::{MockL1Reader, test_context, test_storage_provider},
    };
    use alloy_evm::{
        EvmInternals,
        precompiles::{Precompile as _, PrecompileInput},
    };
    use alloy_primitives::{Bytes, U256};
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    const FIXED_GAS: u64 = 123;
    type RuleRecord = Rc<RefCell<Option<(Bytes, Option<[u8; 4]>, Address)>>>;

    struct RecordingRules(RuleRecord);

    impl CallRules for RecordingRules {
        fn fixed_gas(&self, _selector: Option<[u8; 4]>) -> Option<u64> {
            Some(FIXED_GAS)
        }

        fn check_call(
            &self,
            data: &[u8],
            selector: Option<[u8; 4]>,
            caller: Address,
            _is_direct: bool,
        ) -> Option<PrecompileResult> {
            *self.0.borrow_mut() = Some((Bytes::copy_from_slice(data), selector, caller));
            None
        }
    }

    fn input<'a>(
        ctx: &'a mut crate::test_utils::TestContext,
        data: &'a [u8],
        caller: Address,
        gas: u64,
    ) -> PrecompileInput<'a> {
        let target = Address::repeat_byte(0x11);
        PrecompileInput {
            data,
            gas,
            reservoir: 0,
            caller,
            value: U256::ZERO,
            target_address: target,
            is_static: false,
            bytecode_address: target,
            internals: EvmInternals::from_context(ctx),
        }
    }

    #[test]
    fn forwards_original_call_applies_rules_and_restores_storage_context() {
        let recorded_rule = Rc::new(RefCell::new(None));
        let recorded_execute = Rc::new(RefCell::new(None));
        let execute_record = recorded_execute.clone();
        let cfg = revm::context::CfgEnv::<TempoHardfork>::default();
        let precompile = create_local_precompile(
            "ForwardingTest",
            &cfg,
            RecordingRules(recorded_rule.clone()),
            move |data, caller| {
                *execute_record.borrow_mut() = Some((Bytes::copy_from_slice(data), caller));
                Ok(StorageCtx::default().success_output(Bytes::new()))
            },
        );

        let mut outer_ctx = test_context();
        let mut inner_ctx = test_context();
        let mut outer = test_storage_provider(&mut outer_ctx, 777, false);
        let calldata = [0xde, 0xad, 0xbe, 0xef, 0x01];
        let caller = Address::repeat_byte(0x22);
        let output = StorageCtx::enter(&mut outer, || {
            let output = precompile
                .call(input(&mut inner_ctx, &calldata, caller, FIXED_GAS))
                .unwrap();
            assert_eq!(StorageCtx::default().gas_limit(), 777);
            output
        });

        assert_eq!(output.gas_used, FIXED_GAS);
        assert_eq!(
            *recorded_rule.borrow(),
            Some((calldata.into(), Some([0xde, 0xad, 0xbe, 0xef]), caller))
        );
        assert_eq!(*recorded_execute.borrow(), Some((calldata.into(), caller)));
    }

    #[test]
    fn l1_backed_execution_uses_the_anchor_for_its_fallible_provider() {
        let anchor = 42;
        let reader = MockL1Reader::default();
        let observed_spec = Rc::new(Cell::new(None));
        let execute_spec = observed_spec.clone();
        let cfg = revm::context::CfgEnv::<TempoHardfork>::default();
        let env = L1BackedPrecompileEnv::new(
            &cfg,
            reader.clone(),
            StorageActions::disabled(),
            Rc::new(RefCell::new(NonCreditableSlots::empty())),
        );
        let precompile =
            create_l1_backed_precompile("L1BackedTest", env, NoCallRules, move |_, _| {
                execute_spec.set(Some(StorageCtx::default().spec()));
                Ok(StorageCtx::default().success_output(Bytes::new()))
            });
        let mut ctx = test_context();
        test_storage_provider(&mut ctx, u64::MAX, false)
            .sstore(
                tempo_zone_contracts::TEMPO_STATE_ADDRESS,
                tempo_state_slots::TEMPO_BLOCK_NUMBER,
                U256::from(anchor),
            )
            .unwrap();

        precompile
            .call(input(&mut ctx, &[], Address::ZERO, u64::MAX))
            .unwrap();

        assert_eq!(reader.hardfork_requests(), vec![anchor]);
        assert_eq!(observed_spec.get(), Some(TempoHardfork::T8));
    }

    struct RejectRules(Rc<Cell<bool>>);

    impl CallRules for RejectRules {
        fn fixed_gas(&self, _selector: Option<[u8; 4]>) -> Option<u64> {
            Some(FIXED_GAS)
        }

        fn check_call(
            &self,
            _data: &[u8],
            _selector: Option<[u8; 4]>,
            _caller: Address,
            _is_direct: bool,
        ) -> Option<PrecompileResult> {
            self.0.set(true);
            Some(Ok(
                StorageCtx::default().revert_output(Bytes::from_static(b"denied"))
            ))
        }
    }

    #[test]
    fn admission_and_fixed_gas_run_before_forwarded_execution() {
        let checked = Rc::new(Cell::new(false));
        let executed = Rc::new(Cell::new(false));
        let execute_flag = executed.clone();
        let cfg = revm::context::CfgEnv::<TempoHardfork>::default();
        let precompile = create_local_precompile(
            "AdmissionTest",
            &cfg,
            RejectRules(checked.clone()),
            move |_, _| {
                execute_flag.set(true);
                Ok(StorageCtx::default().success_output(Bytes::new()))
            },
        );
        let mut ctx = test_context();
        let calldata = [1, 2, 3, 4];

        let out_of_gas = precompile
            .call(input(&mut ctx, &calldata, Address::ZERO, FIXED_GAS - 1))
            .unwrap();
        assert!(out_of_gas.is_halt());
        assert_eq!(out_of_gas.halt_reason(), Some(&PrecompileHalt::OutOfGas));
        assert!(!checked.get());
        assert!(!executed.get());

        let rejected = precompile
            .call(input(&mut ctx, &calldata, Address::ZERO, FIXED_GAS))
            .unwrap();
        assert!(checked.get());
        assert!(!executed.get());
        assert_eq!(rejected.gas_used, FIXED_GAS);
        assert_eq!(rejected.bytes, Bytes::from_static(b"denied"));
    }
}
