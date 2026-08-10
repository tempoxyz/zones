//! Zone privacy rules for the upstream Tempo account-keychain precompile.

use alloy_primitives::Address;
use alloy_sol_types::SolCall;
use tempo_precompiles::{
    Precompile as _,
    account_keychain::{
        AccountKeychain, IAccountKeychain, getAllowedCallsCall, getKeyCall, getRemainingLimitCall,
        getRemainingLimitWithPeriodCall, isKeyAuthorizationWitnessBurnedCall,
    },
    dispatch::selector_from_calldata,
};

use crate::{
    account_privacy::AccountPrivacy,
    execution::{CallCheck, CallRules},
};

#[derive(Clone)]
pub(crate) struct AccountKeychainRules {
    privacy: AccountPrivacy,
}

impl AccountKeychainRules {
    pub(crate) fn new(current_sequencer: Address) -> Self {
        Self {
            privacy: AccountPrivacy::new(current_sequencer),
        }
    }
}

fn account_from<C: SolCall>(args: &[u8], account: impl FnOnce(C) -> Address) -> Option<Address> {
    C::abi_decode_raw(args).ok().map(account)
}

impl CallRules for AccountKeychainRules {
    fn admit(&self, data: &[u8], caller: Address) -> CallCheck {
        let Some(selector) = selector_from_calldata(data) else {
            return CallCheck::Continue;
        };
        let args = &data[4..];
        let account = match selector {
            getKeyCall::SELECTOR => account_from::<getKeyCall>(args, |call| call.account),
            getRemainingLimitCall::SELECTOR => {
                account_from::<getRemainingLimitCall>(args, |call| call.account)
            }
            getRemainingLimitWithPeriodCall::SELECTOR => {
                account_from::<getRemainingLimitWithPeriodCall>(args, |call| call.account)
            }
            getAllowedCallsCall::SELECTOR => {
                account_from::<getAllowedCallsCall>(args, |call| call.account)
            }
            isKeyAuthorizationWitnessBurnedCall::SELECTOR => {
                account_from::<isKeyAuthorizationWitnessBurnedCall>(args, |call| call.account)
            }
            IAccountKeychain::isAdminKeyCall::SELECTOR => {
                account_from::<IAccountKeychain::isAdminKeyCall>(args, |call| call.account)
            }
            _ => return CallCheck::Continue,
        };

        account.map_or(CallCheck::Continue, |account| {
            self.privacy.authorize(caller, &[account])
        })
    }
}

pub(crate) fn execute(data: &[u8], caller: Address) -> revm::precompile::PrecompileResult {
    AccountKeychain::new().call(data, caller)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use alloy_sol_types::SolError;
    use tempo_zone_contracts::Unauthorized;

    fn assert_allowed(rules: &AccountKeychainRules, call: impl SolCall, caller: Address) {
        assert!(matches!(
            rules.admit(&call.abi_encode(), caller),
            CallCheck::Continue
        ));
    }

    fn assert_unauthorized(rules: &AccountKeychainRules, call: impl SolCall, caller: Address) {
        let CallCheck::Revert(bytes) = rules.admit(&call.abi_encode(), caller) else {
            panic!("private account read must revert")
        };
        assert_eq!(bytes, Unauthorized {}.abi_encode());
    }

    #[test]
    fn account_getters_allow_only_owner_or_sequencer() {
        let owner = Address::repeat_byte(0x11);
        let outsider = Address::repeat_byte(0x22);
        let sequencer = Address::repeat_byte(0x33);
        let key = Address::repeat_byte(0x44);
        let rules = AccountKeychainRules::new(sequencer);

        macro_rules! check {
            ($call:expr) => {{
                let call = $call;
                assert_allowed(&rules, call.clone(), owner);
                assert_allowed(&rules, call.clone(), sequencer);
                assert_unauthorized(&rules, call, outsider);
            }};
        }

        check!(getKeyCall {
            account: owner,
            keyId: key
        });
        check!(getRemainingLimitCall {
            account: owner,
            keyId: key,
            token: Address::repeat_byte(0x66),
        });
        check!(getRemainingLimitWithPeriodCall {
            account: owner,
            keyId: key,
            token: Address::repeat_byte(0x66),
        });
        check!(getAllowedCallsCall {
            account: owner,
            keyId: key
        });
        check!(isKeyAuthorizationWitnessBurnedCall {
            account: owner,
            witness: B256::repeat_byte(0x77),
        });
        check!(IAccountKeychain::isAdminKeyCall {
            account: owner,
            keyId: key
        });
    }

    #[test]
    fn non_account_getter_is_unchanged() {
        let rules = AccountKeychainRules::new(Address::ZERO);
        assert!(matches!(
            rules.admit(
                &IAccountKeychain::getTransactionKeyCall {}.abi_encode(),
                Address::repeat_byte(0x11)
            ),
            CallCheck::Continue
        ));
    }

    #[test]
    fn zero_beneficiary_does_not_authorize_zero_caller() {
        let owner = Address::repeat_byte(0x11);
        let rules = AccountKeychainRules::new(Address::ZERO);
        assert_unauthorized(
            &rules,
            getKeyCall {
                account: owner,
                keyId: Address::repeat_byte(0x22),
            },
            Address::ZERO,
        );
    }
}
