//! Shared execution for Zone-native and upstream Tempo precompiles.
//!
//! Both helpers install an EVM-backed [`StorageCtx`], apply Zone-specific [`CallRules`], and
//! forward admitted calls without changing their calldata or caller. Protocol storage reaches the
//! execution-local anchored database through the ordinary revm journal.
//!
//! # Call ordering
//!
//! 1. Protocol execution rejects delegate calls before storage access.
//! 2. Decode the selector and reject calls that cannot cover a configured fixed gas charge.
//! 3. Apply [`CallRules`], which may inspect local or anchored state through ordinary EVM storage.
//! 4. Forward the original calldata and caller, applying any configured fixed gas charge.
//!
//! Rule-level rejections include calldata input gas. Calls without a fixed charge retain normal
//! provider metering, while successful fixed-price calls report exactly the configured charge.

use alloc::rc::Rc;
use core::cell::RefCell;

use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use alloy_primitives::Address;
use alloy_sol_types::SolError;
use revm::precompile::{PrecompileHalt, PrecompileId, PrecompileOutput, PrecompileResult};
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_precompiles::{
    DelegateCallNotAllowed, charge_input_cost,
    dispatch::selector_from_calldata,
    storage::{StorageCtx, actions::StorageActions, evm::EvmPrecompileStorageProvider},
    storage_credits::NonCreditableSlots,
};

/// Shared inputs for protocol precompiles whose storage is provided by the EVM context.
#[derive(Clone)]
pub struct ZonePrecompileEnv {
    cfg: revm::context::CfgEnv<TempoHardfork>,
    actions: StorageActions,
    non_creditable_slots: Rc<RefCell<NonCreditableSlots>>,
}

impl ZonePrecompileEnv {
    pub fn new(
        cfg: &revm::context::CfgEnv<TempoHardfork>,
        actions: StorageActions,
        non_creditable_slots: Rc<RefCell<NonCreditableSlots>>,
    ) -> Self {
        Self {
            cfg: cfg.clone(),
            actions,
            non_creditable_slots,
        }
    }
}

/// Call metadata, independent of EVM internals, for [`CallRules`] running in a [`StorageCtx`].
/// Provider-free precompiles can inspect `PrecompileInput` directly.
///
/// **MOTIVATION:** Execution helpers move `PrecompileInput::internals` into the
/// [`EvmPrecompileStorageProvider`] before calling [`CallRules`]'s checks. The full input
/// cannot be borrowed after that partial move, so [`ZoneCall`] carries only the metadata
/// needed by [`CallRules`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct ZoneCall<'a> {
    /// Input calldata.
    pub(crate) data: &'a [u8],
    /// EVM caller.
    pub(crate) caller: Address,
    /// Whether target and bytecode addresses match.
    pub(crate) is_direct: bool,
}

impl<'a> ZoneCall<'a> {
    pub(crate) fn new(input: &PrecompileInput<'a>) -> Self {
        Self {
            data: input.data,
            caller: input.caller,
            is_direct: input.is_direct_call(),
        }
    }

    pub(crate) fn selector(&self) -> Option<[u8; 4]> {
        selector_from_calldata(self.data)
    }
}

/// Result of applying zone-specific pre-execution rules.
pub(crate) enum CallCheck {
    /// Allow the call and invoke the supplied precompile implementation.
    ///
    /// Protocol precompiles observe finalized L1 policy state through the EVM database adapter.
    Continue,
    /// Reject the call without invoking the supplied implementation.
    ///
    /// The execution helper charges calldata input gas before returning the result.
    Return(PrecompileResult),
}

/// Selector-, caller-, and call-context-dependent rules evaluated by centralized precompile
/// execution before invoking the implementation.
///
/// Anchored reads are resolved by the EVM database adapter. Rules may enforce admission policy
/// and duplicate cheap business checks as fail-fast preflight, but the
/// precompile implementation remains responsible for its canonical business invariants.
pub(crate) trait CallRules: 'static {
    /// Returns whether this precompile accepts delegate calls.
    fn is_delegate_call_allowed(&self) -> bool {
        true
    }

    /// Return the fixed gas charge for this selector, if one applies.
    fn fixed_gas(&self, _selector: Option<[u8; 4]>) -> Option<u64> {
        None
    }

    /// Applies Zone-specific admission rules before invoking the upstream implementation.
    fn check(&self, _call: ZoneCall<'_>) -> CallCheck {
        CallCheck::Continue
    }
}

/// Rules for precompiles that require no zone-specific admission or fixed gas handling.
pub(crate) struct NoCallRules;
impl CallRules for NoCallRules {}

/// Rules for precompiles whose semantics require execution at their registered address.
pub(crate) struct DirectCallOnly;
impl CallRules for DirectCallOnly {
    fn is_delegate_call_allowed(&self) -> bool {
        false
    }
}

pub(crate) fn create_precompile(
    id: &'static str,
    env: &ZonePrecompileEnv,
    rules: impl CallRules,
    execute: impl Fn(&[u8], Address) -> PrecompileResult + 'static,
) -> DynPrecompile {
    let env = env.clone();
    DynPrecompile::new_stateful(PrecompileId::Custom(id.into()), move |input| {
        let call = ZoneCall::new(&input);
        if !rules.is_delegate_call_allowed() && !call.is_direct {
            return Ok(PrecompileOutput::revert(
                0,
                SolError::abi_encode(&DelegateCallNotAllowed {}).into(),
                input.reservoir,
            ));
        }

        let fixed_gas = rules.fixed_gas(call.selector());
        if fixed_gas.is_some_and(|gas| input.gas < gas) {
            return Ok(PrecompileOutput::halt(
                PrecompileHalt::OutOfGas,
                input.reservoir,
            ));
        }

        let mut storage = EvmPrecompileStorageProvider::new(
            input.internals,
            fixed_gas.map_or(input.gas, |_| u64::MAX),
            input.reservoir,
            env.cfg.spec,
            env.cfg.enable_amsterdam_eip8037,
            input.is_static,
            env.cfg.gas_params.clone(),
        )
        .with_actions(env.actions.clone())
        .with_non_creditable_slots(env.non_creditable_slots.clone());

        match StorageCtx::enter(&mut storage, || rules.check(call)) {
            CallCheck::Continue => {}
            CallCheck::Return(result) => {
                let result = StorageCtx::enter(&mut storage, || add_input_cost(call.data, result));
                return apply_fixed_gas(result, fixed_gas);
            }
        }

        let result = StorageCtx::enter(&mut storage, || execute(call.data, call.caller));
        apply_fixed_gas(result, fixed_gas)
    })
}

fn apply_fixed_gas(mut result: PrecompileResult, fixed_gas: Option<u64>) -> PrecompileResult {
    if let (Ok(output), Some(gas)) = (&mut result, fixed_gas) {
        output.gas_used = gas;
    }
    result
}

fn add_input_cost(calldata: &[u8], mut result: PrecompileResult) -> PrecompileResult {
    let mut storage = StorageCtx::default();
    let gas_before = storage.gas_used();
    if let Some(err) = charge_input_cost(&mut storage, calldata) {
        return err;
    }
    if let Ok(output) = &mut result {
        let input_gas = storage.gas_used().saturating_sub(gas_before);
        output.gas_used = output.gas_used.saturating_add(input_gas);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::{test_context, test_storage_provider};
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

        fn check(&self, call: ZoneCall<'_>) -> CallCheck {
            *self.0.borrow_mut() = Some((
                Bytes::copy_from_slice(call.data),
                call.selector(),
                call.caller,
            ));
            CallCheck::Continue
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
        let env = ZonePrecompileEnv::new(
            &cfg,
            StorageActions::disabled(),
            Rc::new(RefCell::new(NonCreditableSlots::empty())),
        );
        let precompile = create_precompile(
            "ForwardingTest",
            &env,
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
    fn protocol_precompile_applies_admission_and_evm_spec() {
        let observed_spec = Rc::new(Cell::new(None));
        let execute_spec = observed_spec.clone();
        let mut cfg = revm::context::CfgEnv::<TempoHardfork>::default();
        cfg.spec = TempoHardfork::T8;
        let env = ZonePrecompileEnv::new(
            &cfg,
            StorageActions::disabled(),
            Rc::new(RefCell::new(NonCreditableSlots::empty())),
        );
        let checked = Rc::new(Cell::new(false));
        let rejected = create_precompile(
            "L1AdmissionTest",
            &env,
            RejectRules(checked.clone()),
            |_, _| panic!("rejected call must not execute"),
        );
        let mut ctx = test_context();
        assert!(
            rejected
                .call(input(&mut ctx, &[1, 2, 3, 4], Address::ZERO, FIXED_GAS))
                .unwrap()
                .is_revert()
        );
        assert!(checked.get());

        let precompile = create_precompile("ProtocolTest", &env, NoCallRules, move |_, _| {
            execute_spec.set(Some(StorageCtx::default().spec()));
            Ok(StorageCtx::default().success_output(Bytes::new()))
        });

        precompile
            .call(input(&mut ctx, &[], Address::ZERO, u64::MAX))
            .unwrap();

        assert_eq!(observed_spec.get(), Some(TempoHardfork::T8));
    }

    struct RejectRules(Rc<Cell<bool>>);

    impl CallRules for RejectRules {
        fn fixed_gas(&self, _selector: Option<[u8; 4]>) -> Option<u64> {
            Some(FIXED_GAS)
        }

        fn check(&self, _call: ZoneCall<'_>) -> CallCheck {
            self.0.set(true);
            CallCheck::Return(Ok(
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
        let env = ZonePrecompileEnv::new(
            &cfg,
            StorageActions::disabled(),
            Rc::new(RefCell::new(NonCreditableSlots::empty())),
        );
        let precompile = create_precompile(
            "AdmissionTest",
            &env,
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
