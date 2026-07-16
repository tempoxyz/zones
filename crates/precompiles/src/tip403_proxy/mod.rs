//! Zone rules for the L1-backed Tempo TIP-403 registry.
//!
//! Calls at the canonical registry address execute the upstream
//! [`tempo_precompiles::tip403_registry::TIP403Registry`] implementation over finalized L1
//! storage. The zone keeps mutating selectors read-only and otherwise follows upstream dispatch,
//! gas, delegate-call, and receive-policy behavior.

use alloy_primitives::Address;
use alloy_sol_types::{SolCall, SolError};
use tempo_contracts::precompiles::{ITIP403Registry, TIP403_REGISTRY_ADDRESS};
use tempo_precompiles::storage::StorageCtx;

use crate::execution::{CallCheck, CallRules, ZoneCall};

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
    fn check_with_local_state(&self, call: ZoneCall<'_>) -> CallCheck {
        if call
            .selector()
            .is_some_and(|selector| TIP403_MUTATING_SELECTORS.contains(&selector))
        {
            return CallCheck::Return(Ok(
                StorageCtx::default().revert_output(ReadOnlyRegistry {}.abi_encode().into())
            ));
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
        test_utils::{
            MockL1Reader, TestContext, call_precompile, test_context, test_l1_env,
            test_storage_provider,
        },
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
        fn new(l1: MockL1Reader) -> Self {
            let mut ctx = test_context();
            ctx.cfg.spec = TempoHardfork::T8;
            test_storage_provider(&mut ctx, u64::MAX, false)
                .sstore(
                    zone_primitives::constants::TEMPO_STATE_ADDRESS,
                    TEMPO_BLOCK_NUMBER,
                    U256::from(ANCHOR),
                )
                .unwrap();
            let env = test_l1_env(&ctx, l1);
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

    fn seeded_reader() -> MockL1Reader {
        let l1 = MockL1Reader::default();
        l1.seed_simple_policy(5, ITIP403Registry::PolicyType::WHITELIST, &[ALICE])
            .unwrap();
        l1.seed_simple_policy(6, ITIP403Registry::PolicyType::BLACKLIST, &[BOB])
            .unwrap();
        l1.seed_compound_policy(10, 5, 6, 1).unwrap();
        l1
    }

    fn bool_result<T: SolCall<Return = bool>>(
        harness: &mut RegistryHarness,
        call: &T,
    ) -> eyre::Result<bool> {
        let output = harness.call(&call.abi_encode(), u64::MAX)?;
        Ok(T::abi_decode_returns(&output.bytes)?)
    }

    #[test]
    fn authorization_and_read_methods_match_upstream_policy_semantics() -> eyre::Result<()> {
        let mut harness = RegistryHarness::new(seeded_reader());

        for (policy_id, user, expected) in [
            (0, ALICE, false),
            (1, ALICE, true),
            (5, ALICE, true),
            (5, BOB, false),
            (6, BOB, false),
        ] {
            let call = ITIP403Registry::isAuthorizedCall {
                policyId: policy_id,
                user,
            };
            assert_eq!(bool_result(&mut harness, &call)?, expected);
        }

        assert!(bool_result(
            &mut harness,
            &ITIP403Registry::isAuthorizedSenderCall {
                policyId: 10,
                user: ALICE,
            },
        )?);
        assert!(!bool_result(
            &mut harness,
            &ITIP403Registry::isAuthorizedRecipientCall {
                policyId: 10,
                user: BOB,
            },
        )?);
        assert!(bool_result(
            &mut harness,
            &ITIP403Registry::isAuthorizedMintRecipientCall {
                policyId: 10,
                user: BOB,
            },
        )?);

        let counter = harness.call(
            &ITIP403Registry::policyIdCounterCall {}.abi_encode(),
            u64::MAX,
        )?;
        assert_eq!(
            ITIP403Registry::policyIdCounterCall::abi_decode_returns(&counter.bytes)?,
            11
        );

        let exists = ITIP403Registry::policyExistsCall { policyId: 10 };
        assert!(bool_result(&mut harness, &exists)?);

        let policy_data = harness.call(
            &ITIP403Registry::policyDataCall { policyId: 5 }.abi_encode(),
            u64::MAX,
        )?;
        let policy_data = ITIP403Registry::policyDataCall::abi_decode_returns(&policy_data.bytes)?;
        assert_eq!(
            policy_data.policyType,
            ITIP403Registry::PolicyType::WHITELIST
        );
        assert_eq!(policy_data.admin, Address::ZERO);

        let compound = harness.call(
            &ITIP403Registry::compoundPolicyDataCall { policyId: 10 }.abi_encode(),
            u64::MAX,
        )?;
        let compound =
            ITIP403Registry::compoundPolicyDataCall::abi_decode_returns(&compound.bytes)?;
        assert_eq!(compound.senderPolicyId, 5);
        assert_eq!(compound.recipientPolicyId, 6);
        assert_eq!(compound.mintRecipientPolicyId, 1);
        Ok(())
    }

    #[test]
    fn mutations_revert_while_receive_policy_reads_use_upstream_dispatch() -> eyre::Result<()> {
        let mut harness = RegistryHarness::new(MockL1Reader::default());
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
    fn delegate_calls_revert_before_anchor_or_l1_access() -> eyre::Result<()> {
        let reader = MockL1Reader::default();
        let mut harness = RegistryHarness::new(reader.clone());
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
        assert!(reader.storage_requests().is_empty());
        Ok(())
    }

    #[test]
    fn registry_reads_every_slot_at_the_exact_tempo_anchor() -> eyre::Result<()> {
        let reader = seeded_reader();
        let mut harness = RegistryHarness::new(reader.clone());
        let call = ITIP403Registry::isAuthorizedCall {
            policyId: 5,
            user: ALICE,
        }
        .abi_encode();
        let output = harness.call(&call, u64::MAX)?;
        assert!(output.is_success());

        let requests = reader.storage_requests();
        assert!(!requests.is_empty());
        assert!(requests.iter().all(|(_, _, block)| *block == ANCHOR));
        Ok(())
    }

    #[test]
    fn anchored_storage_failures_fail_closed() {
        let call = ITIP403Registry::isAuthorizedCall {
            policyId: 5,
            user: ALICE,
        }
        .abi_encode();

        let storage_reader = MockL1Reader::failing_storage();
        let mut harness = RegistryHarness::new(storage_reader.clone());
        assert!(matches!(
            harness.call(&call, u64::MAX),
            Err(PrecompileError::Fatal(message)) if message.contains("RPC unavailable")
        ));
        assert!(
            storage_reader
                .storage_requests()
                .iter()
                .all(|(_, _, block)| *block == ANCHOR)
        );
    }
}
