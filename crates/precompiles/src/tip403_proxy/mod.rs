//! Zone rules for the L1-backed Tempo TIP-403 registry.
//!
//! Calls at the canonical registry address execute the upstream
//! [`tempo_precompiles::tip403_registry::TIP403Registry`] implementation over finalized L1
//! storage. The zone keeps mutating selectors read-only and otherwise follows upstream dispatch,
//! gas, delegate-call, and receive-policy behavior.

use crate::execution::{CallCheck, CallRules};
use alloy_primitives::Address;
use alloy_sol_types::{SolCall, SolError};
use tempo_contracts::precompiles::{ITIP403Registry, TIP403_REGISTRY_ADDRESS};

/// Canonical TIP-403 registry address, shared with Tempo L1.
pub const ZONE_TIP403_PROXY_ADDRESS: Address = TIP403_REGISTRY_ADDRESS;

const TIP403_MUTATING_SELECTORS: &[[u8; 4]] = &[
    ITIP403Registry::createPolicyCall::SELECTOR,
    ITIP403Registry::createPolicyWithAccountsCall::SELECTOR,
    ITIP403Registry::setPolicyAdminCall::SELECTOR,
    ITIP403Registry::modifyPolicyWhitelistCall::SELECTOR,
    ITIP403Registry::modifyPolicyBlacklistCall::SELECTOR,
    ITIP403Registry::createCompoundPolicyCall::SELECTOR,
    ITIP403Registry::setReceivePolicyCall::SELECTOR,
];

alloy_sol_types::sol! {
    /// Returned when a mutating call is attempted on the zone's read-only, L1-backed registry.
    #[derive(Debug, PartialEq, Eq)]
    error ReadOnlyRegistry();
}

/// Rules that keep the zone registry read-only before upstream execution.
pub(crate) struct Tip403Rules;

impl CallRules for Tip403Rules {
    fn admit(&self, data: &[u8], _caller: Address) -> CallCheck {
        if TIP403_MUTATING_SELECTORS
            .iter()
            .any(|selector| data.starts_with(selector))
        {
            return CallCheck::Revert(ReadOnlyRegistry {}.abi_encode().into());
        }

        CallCheck::Continue
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloy_evm::precompiles::DynPrecompile;
    use alloy_primitives::{Bytes, U256, address};
    use revm::precompile::{PrecompileError, PrecompileOutput};
    use tempo_chainspec::hardfork::TempoHardfork;
    use tempo_precompiles::{DelegateCallNotAllowed, storage::PrecompileStorageProvider};

    use crate::{
        create_tip403_precompile,
        tempo_state::slots::TEMPO_BLOCK_NUMBER,
        test_utils::{TestContext, call_precompile, test_context, test_env, test_storage_provider},
    };

    const ANCHOR: u64 = 77;
    const CALLER: Address = address!("0x0000000000000000000000000000000000000aaa");
    const ALICE: Address = address!("0x00000000000000000000000000000000000000a1");
    const BOB: Address = address!("0x00000000000000000000000000000000000000b2");

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
            self.call_as(
                data,
                gas,
                ZONE_TIP403_PROXY_ADDRESS,
                ZONE_TIP403_PROXY_ADDRESS,
            )
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
    fn mutations_revert_while_receive_policy_reads_use_upstream_dispatch() -> eyre::Result<()> {
        let mut harness = RegistryHarness::new();
        let mutation = ITIP403Registry::createPolicyCall {
            admin: CALLER,
            policyType: ITIP403Registry::PolicyType::BLACKLIST,
        }
        .abi_encode();
        let output = harness.call(&mutation, u64::MAX)?;
        assert!(output.is_revert());
        assert_eq!(output.bytes, Bytes::from(ReadOnlyRegistry {}.abi_encode()));

        let receive = harness.call(
            &ITIP403Registry::receivePolicyCall { account: CALLER }.abi_encode(),
            u64::MAX,
        )?;
        assert!(receive.is_success());
        let receive = ITIP403Registry::receivePolicyCall::abi_decode_returns(&receive.bytes)?;
        assert!(!receive.hasReceivePolicy);

        let validation = harness.call(
            &ITIP403Registry::validateReceivePolicyCall {
                token: Address::repeat_byte(0x20),
                sender: ALICE,
                receiver: BOB,
            }
            .abi_encode(),
            u64::MAX,
        )?;
        assert!(validation.is_success());
        let validation =
            ITIP403Registry::validateReceivePolicyCall::abi_decode_returns(&validation.bytes)?;
        assert!(validation.authorized);
        assert_eq!(
            validation.blockedReason,
            ITIP403Registry::BlockedReason::NONE
        );

        let set_receive = ITIP403Registry::setReceivePolicyCall {
            senderPolicyId: 1,
            tokenFilterId: 1,
            recoveryAuthority: Address::ZERO,
        }
        .abi_encode();
        let output = harness.call(&set_receive, u64::MAX)?;
        assert!(output.is_revert());
        assert_eq!(output.bytes, Bytes::from(ReadOnlyRegistry {}.abi_encode()));
        Ok(())
    }

    #[test]
    fn delegate_calls_revert_before_execution() -> eyre::Result<()> {
        let mut harness = RegistryHarness::new();
        let call = ITIP403Registry::policyIdCounterCall {}.abi_encode();
        let output = harness.call_as(
            &call,
            u64::MAX,
            ZONE_TIP403_PROXY_ADDRESS,
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
