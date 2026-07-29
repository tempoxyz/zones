//! Zone rules for the L1-backed Tempo TIP-403 registry.
//!
//! Other precompiles use the upstream
//! [`tempo_precompiles::tip403_registry::TIP403Registry`] implementation directly over finalized
//! L1 storage. Calls through the canonical EVM address are rejected as external calls.

use crate::execution::{CallCheck, CallRules};
use alloy_primitives::Address;
use alloy_sol_types::SolError;

alloy_sol_types::sol! {
    /// Returned when the zone registry is called through its external EVM interface.
    #[derive(Debug, PartialEq, Eq)]
    error OnlyPrecompiles();
}

/// Rejects all EVM calls to the registry.
///
/// Other precompiles use the TIP-403 implementation directly rather than issuing an EVM call, so
/// every call reaching this adapter is external. External access is disabled because registry
/// reads may require fetching finalized L1 state; exposing them through RPC simulation endpoints
/// could let untrusted callers force repeated L1 reads and consume sequencer resources.
pub(crate) struct Tip403Rules;

impl CallRules for Tip403Rules {
    fn admit(&self, _data: &[u8], _caller: Address) -> CallCheck {
        CallCheck::Revert(OnlyPrecompiles {}.abi_encode().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloy_evm::precompiles::DynPrecompile;
    use alloy_primitives::{Bytes, U256, address};
    use alloy_sol_types::SolCall;
    use revm::precompile::{PrecompileError, PrecompileOutput};
    use tempo_chainspec::hardfork::TempoHardfork;
    use tempo_contracts::precompiles::{ITIP403Registry, TIP403_REGISTRY_ADDRESS};
    use tempo_precompiles::{DelegateCallNotAllowed, storage::PrecompileStorageProvider};

    use crate::{
        create_tip403_precompile,
        tempo_state::slots::TEMPO_BLOCK_NUMBER,
        test_utils::{TestContext, call_precompile, test_context, test_env, test_storage_provider},
    };

    const ANCHOR: u64 = 77;
    const CALLER: Address = address!("0x0000000000000000000000000000000000000aaa");

    struct RegistryHarness {
        ctx: TestContext,
        precompile: DynPrecompile,
    }

    impl RegistryHarness {
        fn new() -> Self {
            let mut ctx = test_context();
            ctx.cfg.spec = TempoHardfork::T8;
            test_storage_provider(&mut ctx, u64::MAX, false)
                .sstore(
                    zone_primitives::constants::TEMPO_STATE_ADDRESS,
                    TEMPO_BLOCK_NUMBER,
                    U256::from(ANCHOR),
                )
                .unwrap();
            let env = test_env(&ctx);
            Self {
                ctx,
                precompile: create_tip403_precompile(&env),
            }
        }

        fn call(&mut self, data: &[u8], gas: u64) -> Result<PrecompileOutput, PrecompileError> {
            self.call_as(data, gas, TIP403_REGISTRY_ADDRESS, TIP403_REGISTRY_ADDRESS)
        }

        fn call_as(
            &mut self,
            data: &[u8],
            gas: u64,
            target: Address,
            bytecode: Address,
        ) -> Result<PrecompileOutput, PrecompileError> {
            call_precompile(
                &mut self.ctx,
                &self.precompile,
                CALLER,
                data,
                gas,
                true,
                target,
                bytecode,
            )
        }
    }

    #[test]
    fn all_evm_callers_are_rejected_by_admission() {
        for caller in [CALLER, Address::repeat_byte(0x20)] {
            assert!(matches!(
                Tip403Rules.admit(
                    &ITIP403Registry::policyIdCounterCall {}.abi_encode(),
                    caller,
                ),
                CallCheck::Revert(data) if data == OnlyPrecompiles {}.abi_encode()
            ));
        }
    }

    #[test]
    fn external_calls_revert_with_only_precompiles() -> eyre::Result<()> {
        let mut harness = RegistryHarness::new();
        let output = harness.call(
            &ITIP403Registry::policyIdCounterCall {}.abi_encode(),
            u64::MAX,
        )?;

        assert!(output.is_revert());
        assert_eq!(output.bytes, Bytes::from(OnlyPrecompiles {}.abi_encode()));
        Ok(())
    }

    #[test]
    fn delegate_calls_revert_before_execution() -> eyre::Result<()> {
        let mut harness = RegistryHarness::new();
        let call = ITIP403Registry::policyIdCounterCall {}.abi_encode();
        let output = harness.call_as(
            &call,
            u64::MAX,
            TIP403_REGISTRY_ADDRESS,
            Address::repeat_byte(0x44),
        )?;

        assert!(output.is_revert());
        assert_eq!(output.gas_used, 0);
        assert_eq!(
            output.bytes,
            Bytes::from(DelegateCallNotAllowed {}.abi_encode())
        );
        Ok(())
    }
}
