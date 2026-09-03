//! Zone read-privacy rules for the upstream Tempo AccountKeychain precompile.

use alloy_primitives::Address;
use alloy_sol_types::SolInterface;
use tempo_contracts::precompiles::IAccountKeychain;
use tempo_precompiles::dispatch::abi_decoder_config;

use crate::{
    execution::{CallCheck, CallRules},
    privacy::check_caller,
};

/// Zone-specific rules applied before forwarding to upstream `AccountKeychain`.
#[derive(Clone)]
pub(crate) struct AccountKeychainRules;

impl CallRules for AccountKeychainRules {
    fn admit(&self, data: &[u8], caller: Address) -> CallCheck {
        let Ok(call) = IAccountKeychain::IAccountKeychainCalls::abi_decode_with_config(
            data,
            abi_decoder_config(),
        ) else {
            // Preserve the upstream error and gas behavior for malformed or unknown calldata.
            return CallCheck::Continue;
        };

        // Intentionally exhaustive: an upstream ABI addition must be classified here.
        match call {
            IAccountKeychain::IAccountKeychainCalls::getKey(call) => {
                check_caller(caller, &[call.account])
            }
            IAccountKeychain::IAccountKeychainCalls::getRemainingLimit(call) => {
                check_caller(caller, &[call.account])
            }
            IAccountKeychain::IAccountKeychainCalls::getRemainingLimitWithPeriod(call) => {
                check_caller(caller, &[call.account])
            }
            IAccountKeychain::IAccountKeychainCalls::getAllowedCalls(call) => {
                check_caller(caller, &[call.account])
            }
            IAccountKeychain::IAccountKeychainCalls::isKeyAuthorizationWitnessBurned(call) => {
                check_caller(caller, &[call.account])
            }
            IAccountKeychain::IAccountKeychainCalls::isAdminKey(call) => {
                check_caller(caller, &[call.account])
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
    use alloy_primitives::{Address, B256};
    use alloy_sol_types::{SolCall, SolError};
    use tempo_zone_contracts::Unauthorized;

    use crate::{
        storage::StorageCtx,
        test_utils::{test_context, test_storage_provider},
    };

    fn assert_account_scoped<C: SolCall + Clone>(
        rules: &AccountKeychainRules,
        call: C,
        owner: Address,
        sequencer: Address,
        outsider: Address,
    ) {
        assert!(matches!(
            rules.admit(&call.abi_encode(), owner),
            CallCheck::Continue
        ));
        for caller in [sequencer, outsider] {
            assert!(matches!(
                rules.admit(&call.clone().abi_encode(), caller),
                CallCheck::Revert(data) if data == Unauthorized {}.abi_encode()
            ));
        }
    }

    #[test]
    fn account_indexed_getters_allow_only_owner() {
        let owner = Address::repeat_byte(0x11);
        let key_id = Address::repeat_byte(0x12);
        let token = Address::repeat_byte(0x13);
        let sequencer = Address::repeat_byte(0x22);
        let outsider = Address::repeat_byte(0x33);
        let rules = AccountKeychainRules;
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
    fn aliased_set_allowed_calls_is_bounded() {
        fn word(value: usize) -> [u8; 32] {
            let mut out = [0_u8; 32];
            out[24..].copy_from_slice(&(value as u64).to_be_bytes());
            out
        }

        let width = 500;
        let mut data = Vec::new();
        data.extend(IAccountKeychain::setAllowedCallsCall::SELECTOR);
        data.extend(word(0));
        data.extend(word(64));
        data.extend(word(width));
        for _ in 0..width {
            data.extend(word(width * 32));
        }
        data.extend(word(1));
        data.extend(word(64));
        data.extend(word(width));
        for _ in 0..width {
            data.extend(word(width * 32));
        }
        let mut selector = [0_u8; 32];
        selector[..4].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        data.extend(selector);
        data.extend(word(64));
        data.extend(word(width));
        for i in 0..width {
            data.extend(word(i + 1));
        }

        assert_eq!(data.len(), 48_292);
        assert!(matches!(
            AccountKeychainRules.admit(&data, Address::ZERO),
            CallCheck::Continue
        ));
    }

    #[test]
    fn caller_scoped_and_mutating_methods_remain_upstream_authorized() {
        let caller = Address::repeat_byte(0x11);
        let rules = AccountKeychainRules;

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
