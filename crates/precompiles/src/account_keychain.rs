//! Zone read-privacy rules for the upstream Tempo AccountKeychain precompile.

use alloy_primitives::Address;
use alloy_sol_types::SolInterface;
use tempo_contracts::precompiles::IAccountKeychain;

use crate::{
    execution::{CallCheck, CallRules},
    privacy::check_caller_or_sequencer,
    storage::{L1State, L1StorageReader},
};

/// Zone-specific rules applied before forwarding to upstream `AccountKeychain`.
#[derive(Clone)]
pub(crate) struct AccountKeychainRules<P> {
    l1: L1State<P>,
}

impl<P> AccountKeychainRules<P> {
    pub(crate) fn new(l1: L1State<P>) -> Self {
        Self { l1 }
    }
}

impl<P: L1StorageReader> CallRules for AccountKeychainRules<P> {
    fn admit(&self, data: &[u8], caller: Address) -> CallCheck {
        let Ok(call) = IAccountKeychain::IAccountKeychainCalls::abi_decode(data) else {
            // Preserve the upstream error and gas behavior for malformed or unknown calldata.
            return CallCheck::Continue;
        };

        // Intentionally exhaustive: an upstream ABI addition must be classified here.
        match call {
            IAccountKeychain::IAccountKeychainCalls::getKey(call) => {
                check_caller_or_sequencer(&self.l1, caller, &[call.account])
            }
            IAccountKeychain::IAccountKeychainCalls::getRemainingLimit(call) => {
                check_caller_or_sequencer(&self.l1, caller, &[call.account])
            }
            IAccountKeychain::IAccountKeychainCalls::getRemainingLimitWithPeriod(call) => {
                check_caller_or_sequencer(&self.l1, caller, &[call.account])
            }
            IAccountKeychain::IAccountKeychainCalls::getAllowedCalls(call) => {
                check_caller_or_sequencer(&self.l1, caller, &[call.account])
            }
            IAccountKeychain::IAccountKeychainCalls::isKeyAuthorizationWitnessBurned(call) => {
                check_caller_or_sequencer(&self.l1, caller, &[call.account])
            }
            IAccountKeychain::IAccountKeychainCalls::isAdminKey(call) => {
                check_caller_or_sequencer(&self.l1, caller, &[call.account])
            }
            IAccountKeychain::IAccountKeychainCalls::authorizeKey_0(_)
            | IAccountKeychain::IAccountKeychainCalls::authorizeKey_1(_)
            | IAccountKeychain::IAccountKeychainCalls::authorizeKey_2(_)
            | IAccountKeychain::IAccountKeychainCalls::authorizeAdminKey(_)
            | IAccountKeychain::IAccountKeychainCalls::burnKeyAuthorizationWitness(_)
            | IAccountKeychain::IAccountKeychainCalls::revokeKey(_)
            | IAccountKeychain::IAccountKeychainCalls::updateSpendingLimit(_)
            | IAccountKeychain::IAccountKeychainCalls::setAllowedCalls(_)
            | IAccountKeychain::IAccountKeychainCalls::removeAllowedCalls(_)
            | IAccountKeychain::IAccountKeychainCalls::getTransactionKey(_) => CallCheck::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, address};
    use alloy_sol_types::{SolCall, SolError};
    use tempo_zone_contracts::Unauthorized;

    use crate::{
        storage::StorageCtx,
        test_utils::{MockL1Reader, test_context, test_storage_provider},
    };

    const PORTAL_ADDRESS: Address = address!("0x0000000000000000000000000000000000000b01");

    fn assert_account_scoped<C: SolCall + Clone>(
        rules: &AccountKeychainRules<MockL1Reader>,
        call: C,
        owner: Address,
        sequencer: Address,
        outsider: Address,
    ) {
        for caller in [owner, sequencer] {
            assert!(matches!(
                rules.admit(&call.clone().abi_encode(), caller),
                CallCheck::Continue
            ));
        }
        assert!(matches!(
            rules.admit(&call.abi_encode(), outsider),
            CallCheck::Revert(data) if data == Unauthorized {}.abi_encode()
        ));
    }

    #[test]
    fn account_indexed_getters_allow_owner_and_sequencer_only() {
        let owner = Address::repeat_byte(0x11);
        let key_id = Address::repeat_byte(0x12);
        let token = Address::repeat_byte(0x13);
        let sequencer = Address::repeat_byte(0x22);
        let outsider = Address::repeat_byte(0x33);
        let reader = MockL1Reader::default();
        reader.seed_active_sequencer(PORTAL_ADDRESS, 0, sequencer);
        let rules = AccountKeychainRules::new(L1State::new(reader, PORTAL_ADDRESS));
        let mut ctx = test_context();
        let mut storage = test_storage_provider(&mut ctx, u64::MAX, true);

        StorageCtx::enter(&mut storage, || {
            assert_account_scoped(
                &rules,
                IAccountKeychain::getKeyCall {
                    account: owner,
                    keyId: key_id,
                },
                owner,
                sequencer,
                outsider,
            );
            assert_account_scoped(
                &rules,
                IAccountKeychain::getRemainingLimitCall {
                    account: owner,
                    keyId: key_id,
                    token,
                },
                owner,
                sequencer,
                outsider,
            );
            assert_account_scoped(
                &rules,
                IAccountKeychain::getRemainingLimitWithPeriodCall {
                    account: owner,
                    keyId: key_id,
                    token,
                },
                owner,
                sequencer,
                outsider,
            );
            assert_account_scoped(
                &rules,
                IAccountKeychain::getAllowedCallsCall {
                    account: owner,
                    keyId: key_id,
                },
                owner,
                sequencer,
                outsider,
            );
            assert_account_scoped(
                &rules,
                IAccountKeychain::isKeyAuthorizationWitnessBurnedCall {
                    account: owner,
                    witness: B256::repeat_byte(0x55),
                },
                owner,
                sequencer,
                outsider,
            );
            assert_account_scoped(
                &rules,
                IAccountKeychain::isAdminKeyCall {
                    account: owner,
                    keyId: key_id,
                },
                owner,
                sequencer,
                outsider,
            );
        });
    }

    #[test]
    fn caller_scoped_and_mutating_methods_remain_upstream_authorized() {
        let caller = Address::repeat_byte(0x11);
        let rules =
            AccountKeychainRules::new(L1State::new(MockL1Reader::default(), PORTAL_ADDRESS));

        assert!(matches!(
            rules.admit(
                &IAccountKeychain::getTransactionKeyCall {}.abi_encode(),
                caller
            ),
            CallCheck::Continue
        ));
        assert!(matches!(
            rules.admit(
                &IAccountKeychain::revokeKeyCall {
                    keyId: Address::repeat_byte(0x22),
                }
                .abi_encode(),
                caller
            ),
            CallCheck::Continue
        ));
    }
}
