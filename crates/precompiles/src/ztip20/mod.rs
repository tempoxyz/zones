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
use alloy_sol_types::SolCall;
use tempo_precompiles::{
    dispatch::selector_from_calldata,
    tip20::{IRolesAuth, ITIP20},
};
use tempo_zone_contracts::Unauthorized;
use zone_primitives::constants::{ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS};

use crate::execution::{CallCheck, CallRules, ZoneCall};

/// Fixed gas charged for TIP20 transfer and approval selectors on the zone.
///
/// A constant charge hides storage-dependent execution costs that could reveal whether a recipient
/// has previously received tokens. This intentionally replaces upstream variable gas pricing.
pub const TIP20_FIXED_TRANSFER_GAS: u64 = 100_000;

pub(crate) const TIP20_FIXED_GAS_SELECTORS: &[[u8; 4]] = &[
    ITIP20::transferCall::SELECTOR,
    ITIP20::transferFromCall::SELECTOR,
    ITIP20::transferWithMemoCall::SELECTOR,
    ITIP20::transferFromWithMemoCall::SELECTOR,
    ITIP20::approveCall::SELECTOR,
];

fn decode_and_check<C: SolCall>(args: &[u8], check: impl FnOnce(C) -> CallCheck) -> CallCheck {
    match C::abi_decode_raw(args) {
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
    fn admit(&self, call: ZoneCall<'_>) -> CallCheck {
        let Some(selector) = selector_from_calldata(call.data) else {
            return CallCheck::Continue;
        };
        let args = &call.data[4..];
        let caller = call.caller;

        match selector {
            ITIP20::mintCall::SELECTOR | ITIP20::mintWithMemoCall::SELECTOR => {
                self.check_auth(caller, &[ZONE_INBOX_ADDRESS])
            }
            ITIP20::burnCall::SELECTOR | ITIP20::burnWithMemoCall::SELECTOR => {
                self.check_auth(caller, &[ZONE_OUTBOX_ADDRESS])
            }
            ITIP20::balanceOfCall::SELECTOR => {
                decode_and_check::<ITIP20::balanceOfCall>(args, |decoded| {
                    self.check_auth_with_sequencer(caller, &[decoded.account])
                })
            }
            ITIP20::allowanceCall::SELECTOR => {
                decode_and_check::<ITIP20::allowanceCall>(args, |decoded| {
                    self.check_auth_with_sequencer(caller, &[decoded.owner, decoded.spender])
                })
            }
            IRolesAuth::hasRoleCall::SELECTOR => {
                decode_and_check::<IRolesAuth::hasRoleCall>(args, |decoded| {
                    self.check_auth_with_sequencer(caller, &[decoded.account])
                })
            }
            _ => CallCheck::Continue,
        }
    }
}

impl TIP20Rules {
    fn check_auth(&self, caller: Address, auths: &[Address]) -> CallCheck {
        if auths.contains(&caller) {
            CallCheck::Continue
        } else {
            CallCheck::revert(Unauthorized {})
        }
    }

    fn check_auth_with_sequencer(&self, caller: Address, auths: &[Address]) -> CallCheck {
        match self.check_auth(caller, auths) {
            CallCheck::Continue => CallCheck::Continue,
            _ if self.is_sequencer(caller) => CallCheck::Continue,
            revert => revert,
        }
    }

    #[inline]
    fn is_sequencer(&self, caller: Address) -> bool {
        self.sequencer
            .latest_sequencer()
            .is_some_and(|s| s == caller)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Address, Bytes, U256, address};
    use alloy_evm::precompiles::DynPrecompile;
    use alloy_sol_types::{SolCall, SolError, SolInterface};
    use revm::precompile::PrecompileResult;
    use tempo_contracts::precompiles::TIP20Error;
    use tempo_precompiles::{
        PATH_USD_ADDRESS,
        storage::StorageCtx,
        test_util::TIP20Setup,
        tip20::{IRolesAuth, ISSUER_ROLE, ITIP20, TIP20Token},
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

    fn rules(sequencer: Address) -> TIP20Rules {
        TIP20Rules::new(Arc::new(MockSequencer {
            address: Some(sequencer),
        }))
    }

    fn assert_allowed(rules: &TIP20Rules, call: impl SolCall, caller: Address) {
        let data = call.abi_encode();
        assert!(matches!(
            rules.admit(ZoneCall {
                data: &data,
                caller,
                is_static: false
            }),
            CallCheck::Continue
        ));
    }

    fn assert_unauthorized(rules: &TIP20Rules, call: impl SolCall, caller: Address) {
        let data = call.abi_encode();
        let CallCheck::Revert(data) = rules.admit(ZoneCall {
            data: &data,
            caller,
            is_static: false,
        }) else {
            panic!("call should be rejected");
        };
        assert_eq!(data, Unauthorized {}.abi_encode());
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
                    TIP20Setup::path_usd(admin)
                        .with_issuer(admin)
                        .with_issuer(issuer)
                        .with_issuer(ZONE_INBOX_ADDRESS)
                        .with_issuer(ZONE_OUTBOX_ADDRESS)
                        .with_mint(alice, U256::from(1_000_000u64))
                        .with_mint(ZONE_OUTBOX_ADDRESS, U256::from(10_000u64))
                        .with_approval(alice, spender, U256::from(300_000u64))
                        .apply()?;
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
    fn read_privacy_rules_allow_owner_spender_and_sequencer() {
        let owner = Address::repeat_byte(0x11);
        let spender = Address::repeat_byte(0x22);
        let sequencer = Address::repeat_byte(0x33);
        let outsider = Address::repeat_byte(0x44);
        let rules = rules(sequencer);

        let balance = ITIP20::balanceOfCall { account: owner };
        assert_allowed(&rules, balance.clone(), owner);
        assert_allowed(&rules, balance.clone(), sequencer);
        assert_unauthorized(&rules, balance, outsider);

        let allowance = ITIP20::allowanceCall { owner, spender };
        for caller in [owner, spender, sequencer] {
            assert_allowed(&rules, allowance.clone(), caller);
        }
        assert_unauthorized(&rules, allowance, outsider);

        let role = IRolesAuth::hasRoleCall {
            account: owner,
            role: *ISSUER_ROLE,
        };
        assert_allowed(&rules, role.clone(), owner);
        assert_allowed(&rules, role.clone(), sequencer);
        assert_unauthorized(&rules, role, outsider);
    }

    #[test]
    fn non_canonical_address_padding_cannot_bypass_read_privacy() -> eyre::Result<()> {
        let mut harness = PrecompileHarness::new()?;
        let mut calldata = ITIP20::balanceOfCall {
            account: harness.alice,
        }
        .abi_encode();

        calldata[4] = 1;
        assert!(ITIP20::balanceOfCall::abi_decode_raw_validate(&calldata[4..]).is_err());
        assert_eq!(
            ITIP20::balanceOfCall::abi_decode_raw(&calldata[4..])?.account,
            harness.alice
        );

        let allowed = harness.call(harness.alice, calldata.clone().into(), 100_000, true)?;
        assert!(allowed.is_success());

        let blocked = harness.call(harness.bob, calldata.into(), 100_000, true)?;
        assert!(blocked.is_revert());
        assert_eq!(blocked.bytes, Bytes::from(Unauthorized {}.abi_encode()));

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
    fn bridge_auth_rules_and_allowed_paths() -> eyre::Result<()> {
        let mut harness = PrecompileHarness::new()?;
        let rules = rules(harness.sequencer);
        assert_unauthorized(
            &rules,
            ITIP20::mintCall {
                to: harness.bob,
                amount: U256::ONE,
            },
            ZONE_OUTBOX_ADDRESS,
        );
        assert_unauthorized(
            &rules,
            ITIP20::burnCall { amount: U256::ONE },
            ZONE_INBOX_ADDRESS,
        );

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

        Ok(())
    }

    #[test]
    fn fixed_gas_selectors_charge_exactly_one_hundred_thousand_gas() -> eyre::Result<()> {
        let mut harness = PrecompileHarness::new()?;
        let calls: Vec<(Address, ITIP20::ITIP20Calls)> = vec![
            (
                harness.alice,
                ITIP20::ITIP20Calls::approve(ITIP20::approveCall {
                    spender: harness.spender,
                    amount: U256::from(111_111u64),
                }),
            ),
            (
                harness.alice,
                ITIP20::ITIP20Calls::approve(ITIP20::approveCall {
                    spender: harness.spender,
                    amount: U256::from(222_222u64),
                }),
            ),
            (
                harness.alice,
                ITIP20::ITIP20Calls::transfer(ITIP20::transferCall {
                    to: harness.bob,
                    amount: U256::from(10_000u64),
                }),
            ),
            (
                harness.alice,
                ITIP20::ITIP20Calls::transfer(ITIP20::transferCall {
                    to: harness.bob,
                    amount: U256::from(10_000u64),
                }),
            ),
            (
                harness.alice,
                ITIP20::ITIP20Calls::transferWithMemo(ITIP20::transferWithMemoCall {
                    to: harness.bob,
                    amount: U256::from(10_000u64),
                    memo: Default::default(),
                }),
            ),
            (
                harness.spender,
                ITIP20::ITIP20Calls::transferFrom(ITIP20::transferFromCall {
                    from: harness.alice,
                    to: harness.bob,
                    amount: U256::from(10_000u64),
                }),
            ),
            (
                harness.spender,
                ITIP20::ITIP20Calls::transferFromWithMemo(ITIP20::transferFromWithMemoCall {
                    from: harness.alice,
                    to: harness.bob,
                    amount: U256::from(10_000u64),
                    memo: Default::default(),
                }),
            ),
        ];

        for (caller, call) in calls {
            let calldata = call.abi_encode().into();
            let output = harness.call(caller, calldata, TIP20_FIXED_TRANSFER_GAS, false)?;
            assert_eq!(output.gas_used, TIP20_FIXED_TRANSFER_GAS);
            assert_eq!(output.state_gas_used, 0);
        }
        Ok(())
    }

    #[test]
    fn fixed_gas_selector_mapping_is_complete() {
        let rules = rules(Address::ZERO);
        for selector in TIP20_FIXED_GAS_SELECTORS {
            assert_eq!(
                rules.fixed_gas(Some(*selector)),
                Some(TIP20_FIXED_TRANSFER_GAS)
            );
        }
        assert_eq!(rules.fixed_gas(Some([0xff; 4])), None);
        assert_eq!(rules.fixed_gas(None), None);
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
}
