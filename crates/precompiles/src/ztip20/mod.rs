//! Zone pre-execution rules for the upstream Tempo TIP20 precompile.
//!
//! `TIP20Token` remains the source of truth for token and TIP403 policy behavior.
//! Before forwarding a call to Tempo, `TIP20Rules` applies only zone-specific checks:
//! privacy-gated reads, fixed gas for selected selectors, and bridge mint/burn callers.
//!
//! Accepted calldata and callers are forwarded unchanged to Tempo. Ordinary token state remains
//! Zone-local while the EVM context's database adapter exposes selected policy values from the
//! finalized Tempo L1 state.

use alloc::sync::Arc;

use alloy_primitives::Address;
use alloy_sol_types::{SolCall, SolError};
use tempo_precompiles::{
    dispatch::selector_from_calldata,
    tip20::{IRolesAuth, ITIP20},
};
use tempo_zone_contracts::Unauthorized;
use zone_primitives::constants::{ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS};

use crate::execution::{CallCheck, CallRules};

/// Fixed gas charged for TIP20 transfer and approval selectors on the zone.
pub const TIP20_FIXED_TRANSFER_GAS: u64 = 100_000;

pub(crate) const TIP20_FIXED_GAS_SELECTORS: &[[u8; 4]] = &[
    ITIP20::transferCall::SELECTOR,
    ITIP20::transferFromCall::SELECTOR,
    ITIP20::transferWithMemoCall::SELECTOR,
    ITIP20::transferFromWithMemoCall::SELECTOR,
    ITIP20::approveCall::SELECTOR,
];

fn decode_and_check<C: SolCall>(args: &[u8], check: impl FnOnce(C) -> CallCheck) -> CallCheck {
    match C::abi_decode_raw_validate(args) {
        Ok(decoded) => check(decoded),
        Err(_) => CallCheck::Continue,
    }
}

/// Capability trait for resolving the active zone sequencer.
///
/// The zone runtime implements this for its L1 state provider so rules can authorize
/// sequencer-visible reads without depending on the concrete provider type.
pub trait SequencerExt: Send + Sync {
    /// Return the latest known active sequencer.
    fn latest_sequencer(&self) -> Option<Address>;
}

/// Zone-specific rules applied before forwarding to upstream `TIP20Token`.
#[derive(Clone)]
pub(crate) struct TIP20Rules {
    /// Sequencer-capable backend used to authorize private reads for the active sequencer.
    sequencer: Arc<dyn SequencerExt>,
}

impl TIP20Rules {
    pub(crate) fn new(sequencer: Arc<dyn SequencerExt>) -> Self {
        Self { sequencer }
    }
}

impl CallRules for TIP20Rules {
    fn fixed_gas(&self, selector: Option<[u8; 4]>) -> Option<u64> {
        selector
            .is_some_and(|selector| TIP20_FIXED_GAS_SELECTORS.contains(&selector))
            .then_some(TIP20_FIXED_TRANSFER_GAS)
    }

    /// Apply zone privacy and bridge-path checks before upstream execution.
    fn admit(&self, data: &[u8], caller: Address) -> CallCheck {
        let Some(selector) = selector_from_calldata(data) else {
            return CallCheck::Continue;
        };
        let args = &data[4..];

        match selector {
            ITIP20::mintCall::SELECTOR | ITIP20::mintWithMemoCall::SELECTOR => {
                self.check_mint_auth(caller)
            }
            ITIP20::burnCall::SELECTOR | ITIP20::burnWithMemoCall::SELECTOR => {
                self.check_burn_auth(caller)
            }
            ITIP20::balanceOfCall::SELECTOR => {
                decode_and_check::<ITIP20::balanceOfCall>(args, |decoded| {
                    self.check_balance_read(decoded.account, caller)
                })
            }
            ITIP20::allowanceCall::SELECTOR => {
                decode_and_check::<ITIP20::allowanceCall>(args, |decoded| {
                    self.check_allowance_read(decoded.owner, decoded.spender, caller)
                })
            }
            IRolesAuth::hasRoleCall::SELECTOR => {
                decode_and_check::<IRolesAuth::hasRoleCall>(args, |decoded| {
                    self.check_balance_read(decoded.account, caller)
                })
            }
            _ => CallCheck::Continue,
        }
    }
}

fn unauthorized() -> CallCheck {
    CallCheck::Revert(Unauthorized {}.abi_encode().into())
}

impl TIP20Rules {
    fn check_balance_read(&self, owner: Address, caller: Address) -> CallCheck {
        if caller == owner {
            return CallCheck::Continue;
        }
        self.check_sequencer(caller)
    }

    fn check_allowance_read(&self, owner: Address, spender: Address, caller: Address) -> CallCheck {
        if caller == spender {
            return CallCheck::Continue;
        }
        self.check_balance_read(owner, caller)
    }

    fn check_mint_auth(&self, caller: Address) -> CallCheck {
        if caller == ZONE_INBOX_ADDRESS {
            CallCheck::Continue
        } else {
            unauthorized()
        }
    }

    fn check_burn_auth(&self, caller: Address) -> CallCheck {
        if caller == ZONE_OUTBOX_ADDRESS {
            CallCheck::Continue
        } else {
            unauthorized()
        }
    }

    fn check_sequencer(&self, caller: Address) -> CallCheck {
        if self
            .sequencer
            .latest_sequencer()
            .is_some_and(|sequencer| caller == sequencer)
        {
            CallCheck::Continue
        } else {
            unauthorized()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, Bytes, U256, address};
    use alloy_evm::precompiles::DynPrecompile;
    use alloy_sol_types::{SolCall, SolError, SolInterface};
    use revm::precompile::{PrecompileHalt, PrecompileResult};
    use tempo_contracts::precompiles::TIP20Error;
    use tempo_precompiles::{
        PATH_USD_ADDRESS,
        storage::StorageCtx,
        tip20::{IRolesAuth, ISSUER_ROLE, ITIP20, RolesAuthError, TIP20Token},
    };
    use tempo_zone_contracts::Unauthorized;

    use crate::test_utils::{
        TestContext, call_precompile, test_context, test_env, test_storage_provider,
    };

    #[derive(Clone, Copy)]
    struct MockSequencer {
        address: Option<Address>,
    }

    impl SequencerExt for MockSequencer {
        fn latest_sequencer(&self) -> Option<Address> {
            self.address
        }
    }

    struct PrecompileHarness {
        ctx: TestContext,
        token: Address,
        alice: Address,
        bob: Address,
        spender: Address,
        sequencer: Address,
        precompile: DynPrecompile,
    }

    impl PrecompileHarness {
        fn new() -> eyre::Result<Self> {
            let token = PATH_USD_ADDRESS;
            let admin = address!("0x00000000000000000000000000000000000000a1");
            let alice = address!("0x00000000000000000000000000000000000000a2");
            let bob = address!("0x00000000000000000000000000000000000000a3");
            let spender = address!("0x00000000000000000000000000000000000000a4");
            let issuer = address!("0x00000000000000000000000000000000000000a5");
            let sequencer = address!("0x00000000000000000000000000000000000000a6");
            let mut ctx = test_context();

            {
                let mut storage = test_storage_provider(&mut ctx, u64::MAX, false);
                StorageCtx::enter(&mut storage, || -> eyre::Result<()> {
                    StorageCtx::default().sstore(
                        zone_primitives::constants::TEMPO_STATE_ADDRESS,
                        crate::tempo_state::slots::TEMPO_BLOCK_NUMBER,
                        U256::from(7u64),
                    )?;
                    let mut token_contract =
                        TIP20Token::from_address(token).expect("PATH_USD must be valid");
                    token_contract.initialize(
                        admin,
                        "Zone USD",
                        "zUSD",
                        "USD",
                        Address::ZERO,
                        admin,
                    )?;
                    token_contract.grant_role_internal(admin, *ISSUER_ROLE)?;
                    token_contract.grant_role_internal(issuer, *ISSUER_ROLE)?;
                    token_contract.grant_role_internal(ZONE_INBOX_ADDRESS, *ISSUER_ROLE)?;
                    token_contract.grant_role_internal(ZONE_OUTBOX_ADDRESS, *ISSUER_ROLE)?;
                    token_contract.mint(
                        admin,
                        ITIP20::mintCall {
                            to: alice,
                            amount: U256::from(1_000_000u64),
                        },
                    )?;
                    token_contract.mint(
                        admin,
                        ITIP20::mintCall {
                            to: ZONE_OUTBOX_ADDRESS,
                            amount: U256::from(10_000u64),
                        },
                    )?;
                    token_contract.approve(
                        alice,
                        ITIP20::approveCall {
                            spender,
                            amount: U256::from(300_000u64),
                        },
                    )?;
                    Ok(())
                })?;
            }

            let env = test_env(&ctx);
            let precompile = crate::create_tip20_precompile(
                token,
                &env,
                Arc::new(MockSequencer {
                    address: Some(sequencer),
                }),
            );

            Ok(Self {
                ctx,
                token,
                alice,
                bob,
                spender,
                sequencer,
                precompile,
            })
        }

        fn call(
            &mut self,
            caller: Address,
            calldata: Bytes,
            gas: u64,
            is_static: bool,
        ) -> PrecompileResult {
            call_precompile(
                &mut self.ctx,
                &self.precompile,
                caller,
                &calldata,
                gas,
                is_static,
                self.token,
                self.token,
            )
        }

        fn balance_of(&mut self, account: Address) -> eyre::Result<U256> {
            let mut storage = test_storage_provider(&mut self.ctx, u64::MAX, false);
            StorageCtx::enter(&mut storage, || {
                let token = TIP20Token::from_address(self.token).expect("token must exist");
                Ok(token.balance_of(ITIP20::balanceOfCall { account })?)
            })
        }

        fn allowance(&mut self, owner: Address, spender: Address) -> eyre::Result<U256> {
            let mut storage = test_storage_provider(&mut self.ctx, u64::MAX, false);
            StorageCtx::enter(&mut storage, || {
                let token = TIP20Token::from_address(self.token).expect("token must exist");
                Ok(token.allowance(ITIP20::allowanceCall { owner, spender })?)
            })
        }
    }

    #[test]
    fn balance_of_enforces_account_or_sequencer_access() -> eyre::Result<()> {
        let mut harness = PrecompileHarness::new()?;
        let calldata: Bytes = ITIP20::balanceOfCall {
            account: harness.alice,
        }
        .abi_encode()
        .into();

        let owner = harness.call(harness.alice, calldata.clone(), 100_000, true)?;
        assert_eq!(
            ITIP20::balanceOfCall::abi_decode_returns(&owner.bytes)?,
            U256::from(1_000_000u64)
        );

        let sequencer = harness.call(harness.sequencer, calldata.clone(), 100_000, true)?;
        assert_eq!(
            ITIP20::balanceOfCall::abi_decode_returns(&sequencer.bytes)?,
            U256::from(1_000_000u64)
        );

        let outsider = harness.call(harness.bob, calldata, 100_000, true)?;
        assert!(outsider.is_revert());
        assert_eq!(outsider.bytes, Bytes::from(Unauthorized {}.abi_encode()));

        Ok(())
    }

    #[test]
    fn allowance_enforces_owner_spender_or_sequencer_access() -> eyre::Result<()> {
        let mut harness = PrecompileHarness::new()?;
        let calldata: Bytes = ITIP20::allowanceCall {
            owner: harness.alice,
            spender: harness.spender,
        }
        .abi_encode()
        .into();

        let owner = harness.call(harness.alice, calldata.clone(), 100_000, true)?;
        assert_eq!(
            ITIP20::allowanceCall::abi_decode_returns(&owner.bytes)?,
            U256::from(300_000u64)
        );

        let spender = harness.call(harness.spender, calldata.clone(), 100_000, true)?;
        assert_eq!(
            ITIP20::allowanceCall::abi_decode_returns(&spender.bytes)?,
            U256::from(300_000u64)
        );

        let sequencer = harness.call(harness.sequencer, calldata.clone(), 100_000, true)?;
        assert_eq!(
            ITIP20::allowanceCall::abi_decode_returns(&sequencer.bytes)?,
            U256::from(300_000u64)
        );

        let outsider = harness.call(harness.bob, calldata, 100_000, true)?;
        assert!(outsider.is_revert());
        assert_eq!(outsider.bytes, Bytes::from(Unauthorized {}.abi_encode()));

        Ok(())
    }

    #[test]
    fn wrapper_still_enforces_privacy_and_fixed_gas() -> eyre::Result<()> {
        let mut harness = PrecompileHarness::new()?;

        let private_balance = harness.call(
            harness.bob,
            ITIP20::balanceOfCall {
                account: harness.alice,
            }
            .abi_encode()
            .into(),
            TIP20_FIXED_TRANSFER_GAS,
            true,
        )?;
        assert!(private_balance.is_revert());
        assert_eq!(
            private_balance.bytes,
            Bytes::from(Unauthorized {}.abi_encode())
        );

        let transfer = harness.call(
            harness.alice,
            ITIP20::transferCall {
                to: harness.bob,
                amount: U256::from(12_345u64),
            }
            .abi_encode()
            .into(),
            TIP20_FIXED_TRANSFER_GAS,
            false,
        )?;
        assert!(transfer.is_success());
        assert_eq!(transfer.gas_used, TIP20_FIXED_TRANSFER_GAS);
        assert_eq!(harness.balance_of(harness.bob)?, U256::from(12_345u64));

        Ok(())
    }

    #[test]
    fn uninitialized_token_rejects_before_policy_read() -> eyre::Result<()> {
        let token = address!("20C0000000000000000000000000000000000999");
        let caller = address!("0x00000000000000000000000000000000000000a2");
        let to = address!("0x00000000000000000000000000000000000000a3");
        let mut ctx = test_context();
        let env = test_env(&ctx);
        let precompile =
            crate::create_tip20_precompile(token, &env, Arc::new(MockSequencer { address: None }));
        let calldata: Bytes = ITIP20::transferCall {
            to,
            amount: U256::from(1u64),
        }
        .abi_encode()
        .into();

        let result = call_precompile(
            &mut ctx,
            &precompile,
            caller,
            &calldata,
            TIP20_FIXED_TRANSFER_GAS,
            false,
            token,
            token,
        )?;

        assert!(result.is_revert());
        assert_eq!(
            result.bytes,
            Bytes::from(TIP20Error::uninitialized().selector().to_vec())
        );

        Ok(())
    }

    #[test]
    fn malformed_calldata_uses_upstream_dispatch() -> eyre::Result<()> {
        let mut harness = PrecompileHarness::new()?;

        let balance_of = harness.call(
            harness.alice,
            Bytes::from(ITIP20::balanceOfCall::SELECTOR.to_vec()),
            100_000,
            true,
        )?;
        assert!(balance_of.is_revert());
        assert_eq!(balance_of.bytes, Bytes::new());

        let transfer = harness.call(
            harness.alice,
            Bytes::from(ITIP20::transferCall::SELECTOR.to_vec()),
            TIP20_FIXED_TRANSFER_GAS,
            false,
        )?;
        assert!(transfer.is_revert());
        assert_eq!(transfer.bytes, Bytes::new());
        assert_eq!(transfer.gas_used, TIP20_FIXED_TRANSFER_GAS);

        Ok(())
    }

    #[test]
    fn bridge_auth_rejects_crossed_system_calls_and_keeps_allowed_paths() -> eyre::Result<()> {
        let mut harness = PrecompileHarness::new()?;

        let inbox_mint = harness.call(
            ZONE_INBOX_ADDRESS,
            ITIP20::mintCall {
                to: harness.bob,
                amount: U256::from(50_000u64),
            }
            .abi_encode()
            .into(),
            100_000,
            false,
        )?;
        assert!(inbox_mint.is_success());
        assert_eq!(harness.balance_of(harness.bob)?, U256::from(50_000u64));

        let outbox_burn = harness.call(
            ZONE_OUTBOX_ADDRESS,
            ITIP20::burnCall {
                amount: U256::from(10_000u64),
            }
            .abi_encode()
            .into(),
            100_000,
            false,
        )?;
        assert!(outbox_burn.is_success());
        assert_eq!(harness.balance_of(ZONE_OUTBOX_ADDRESS)?, U256::ZERO);

        let crossed_mint = harness.call(
            ZONE_OUTBOX_ADDRESS,
            ITIP20::mintCall {
                to: harness.bob,
                amount: U256::from(1u64),
            }
            .abi_encode()
            .into(),
            100_000,
            false,
        )?;
        assert!(crossed_mint.is_revert());
        assert_eq!(
            crossed_mint.bytes,
            Bytes::from(RolesAuthError::unauthorized().selector().to_vec())
        );

        let crossed_burn = harness.call(
            ZONE_INBOX_ADDRESS,
            ITIP20::burnCall {
                amount: U256::from(1u64),
            }
            .abi_encode()
            .into(),
            100_000,
            false,
        )?;
        assert!(crossed_burn.is_revert());
        assert_eq!(
            crossed_burn.bytes,
            Bytes::from(RolesAuthError::unauthorized().selector().to_vec())
        );

        Ok(())
    }

    #[test]
    fn fixed_gas_selectors_charge_exactly_one_hundred_thousand_gas() -> eyre::Result<()> {
        let mut harness = PrecompileHarness::new()?;

        let approve = harness.call(
            harness.alice,
            ITIP20::approveCall {
                spender: harness.spender,
                amount: U256::from(111_111u64),
            }
            .abi_encode()
            .into(),
            TIP20_FIXED_TRANSFER_GAS,
            false,
        )?;
        assert_eq!(approve.gas_used, TIP20_FIXED_TRANSFER_GAS);
        assert_eq!(approve.state_gas_used, 0);

        let approve_update = harness.call(
            harness.alice,
            ITIP20::approveCall {
                spender: harness.spender,
                amount: U256::from(222_222u64),
            }
            .abi_encode()
            .into(),
            TIP20_FIXED_TRANSFER_GAS,
            false,
        )?;
        assert_eq!(approve_update.gas_used, TIP20_FIXED_TRANSFER_GAS);
        assert_eq!(approve_update.state_gas_used, 0);

        let transfer_new = harness.call(
            harness.alice,
            ITIP20::transferCall {
                to: harness.bob,
                amount: U256::from(10_000u64),
            }
            .abi_encode()
            .into(),
            TIP20_FIXED_TRANSFER_GAS,
            false,
        )?;
        assert_eq!(transfer_new.gas_used, TIP20_FIXED_TRANSFER_GAS);
        assert_eq!(transfer_new.state_gas_used, 0);

        let transfer_existing = harness.call(
            harness.alice,
            ITIP20::transferCall {
                to: harness.bob,
                amount: U256::from(10_000u64),
            }
            .abi_encode()
            .into(),
            TIP20_FIXED_TRANSFER_GAS,
            false,
        )?;
        assert_eq!(transfer_existing.gas_used, TIP20_FIXED_TRANSFER_GAS);
        assert_eq!(transfer_existing.state_gas_used, 0);

        let transfer_with_memo = harness.call(
            harness.alice,
            ITIP20::transferWithMemoCall {
                to: harness.bob,
                amount: U256::from(10_000u64),
                memo: Default::default(),
            }
            .abi_encode()
            .into(),
            TIP20_FIXED_TRANSFER_GAS,
            false,
        )?;
        assert_eq!(transfer_with_memo.gas_used, TIP20_FIXED_TRANSFER_GAS);
        assert_eq!(transfer_with_memo.state_gas_used, 0);

        let transfer_from = harness.call(
            harness.spender,
            ITIP20::transferFromCall {
                from: harness.alice,
                to: harness.bob,
                amount: U256::from(10_000u64),
            }
            .abi_encode()
            .into(),
            TIP20_FIXED_TRANSFER_GAS,
            false,
        )?;
        assert_eq!(transfer_from.gas_used, TIP20_FIXED_TRANSFER_GAS);
        assert_eq!(transfer_from.state_gas_used, 0);

        let transfer_from_with_memo = harness.call(
            harness.spender,
            ITIP20::transferFromWithMemoCall {
                from: harness.alice,
                to: harness.bob,
                amount: U256::from(10_000u64),
                memo: Default::default(),
            }
            .abi_encode()
            .into(),
            TIP20_FIXED_TRANSFER_GAS,
            false,
        )?;
        assert_eq!(transfer_from_with_memo.gas_used, TIP20_FIXED_TRANSFER_GAS);
        assert_eq!(transfer_from_with_memo.state_gas_used, 0);

        Ok(())
    }

    #[test]
    fn fixed_gas_selectors_fail_out_of_gas_below_threshold() -> eyre::Result<()> {
        let mut harness = PrecompileHarness::new()?;

        for calldata in [
            ITIP20::transferCall {
                to: harness.bob,
                amount: U256::from(1u64),
            }
            .abi_encode()
            .into(),
            ITIP20::transferFromCall {
                from: harness.alice,
                to: harness.bob,
                amount: U256::from(1u64),
            }
            .abi_encode()
            .into(),
            ITIP20::transferWithMemoCall {
                to: harness.bob,
                amount: U256::from(1u64),
                memo: Default::default(),
            }
            .abi_encode()
            .into(),
            ITIP20::transferFromWithMemoCall {
                from: harness.alice,
                to: harness.bob,
                amount: U256::from(1u64),
                memo: Default::default(),
            }
            .abi_encode()
            .into(),
            ITIP20::approveCall {
                spender: harness.spender,
                amount: U256::from(1u64),
            }
            .abi_encode()
            .into(),
        ] {
            let output = harness
                .call(harness.alice, calldata, TIP20_FIXED_TRANSFER_GAS - 1, false)
                .expect("out of gas is returned as a halted precompile output");
            assert!(output.is_halt());
            assert_eq!(output.halt_reason(), Some(&PrecompileHalt::OutOfGas));
        }

        Ok(())
    }

    #[test]
    fn fixed_gas_keeps_allowance_and_balance_state_changes_intact() -> eyre::Result<()> {
        let mut harness = PrecompileHarness::new()?;

        let approve = harness.call(
            harness.alice,
            ITIP20::approveCall {
                spender: harness.spender,
                amount: U256::from(123_456u64),
            }
            .abi_encode()
            .into(),
            TIP20_FIXED_TRANSFER_GAS,
            false,
        )?;
        assert!(approve.is_success());
        assert_eq!(
            harness.allowance(harness.alice, harness.spender)?,
            U256::from(123_456u64)
        );

        let transfer = harness.call(
            harness.alice,
            ITIP20::transferCall {
                to: harness.bob,
                amount: U256::from(7_654u64),
            }
            .abi_encode()
            .into(),
            TIP20_FIXED_TRANSFER_GAS,
            false,
        )?;
        assert!(transfer.is_success());
        assert_eq!(harness.balance_of(harness.bob)?, U256::from(7_654u64));

        Ok(())
    }

    #[test]
    fn has_role_enforces_account_or_sequencer_access() -> eyre::Result<()> {
        let mut harness = PrecompileHarness::new()?;
        let calldata: Bytes = IRolesAuth::hasRoleCall {
            account: harness.alice,
            role: *ISSUER_ROLE,
        }
        .abi_encode()
        .into();

        // Owner can query their own roles
        let owner = harness.call(harness.alice, calldata.clone(), 100_000, true)?;
        assert!(owner.is_success());

        // Sequencer can query anyone's roles
        let sequencer = harness.call(harness.sequencer, calldata.clone(), 100_000, true)?;
        assert!(sequencer.is_success());

        // Outsider is rejected
        let outsider = harness.call(harness.bob, calldata, 100_000, true)?;
        assert!(outsider.is_revert());
        assert_eq!(outsider.bytes, Bytes::from(Unauthorized {}.abi_encode()));

        Ok(())
    }
}
