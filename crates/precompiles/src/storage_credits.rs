//! Zone read-privacy rules for the upstream Tempo StorageCredits precompile.

use alloy_primitives::Address;
use alloy_sol_types::SolInterface;
use tempo_contracts::precompiles::IStorageCredits;

use crate::{
    execution::{CallCheck, CallRules},
    privacy::check_caller,
};

/// Zone-specific rules applied before forwarding to upstream `StorageCredits`.
#[derive(Clone)]
pub(crate) struct StorageCreditsRules;

impl CallRules for StorageCreditsRules {
    fn admit(&self, data: &[u8], caller: Address) -> CallCheck {
        let Ok(call) = IStorageCredits::IStorageCreditsCalls::abi_decode(data) else {
            return CallCheck::Continue;
        };

        // Intentionally exhaustive: an upstream ABI addition must be classified here.
        match call {
            IStorageCredits::IStorageCreditsCalls::balanceOf(call) => {
                check_caller(caller, &[call.account])
            }
            IStorageCredits::IStorageCreditsCalls::modeOf(call) => {
                check_caller(caller, &[call.account])
            }
            IStorageCredits::IStorageCreditsCalls::budgetOf(call) => {
                check_caller(caller, &[call.account])
            }
            IStorageCredits::IStorageCreditsCalls::setMode(_)
            | IStorageCredits::IStorageCreditsCalls::setBudget(_) => CallCheck::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_sol_types::{SolCall, SolError};
    use tempo_zone_contracts::Unauthorized;

    use crate::{
        storage::StorageCtx,
        test_utils::{test_context, test_storage_provider},
    };

    #[test]
    fn account_indexed_getters_allow_only_owner() {
        let owner = Address::repeat_byte(0x11);
        let sequencer = Address::repeat_byte(0x22);
        let outsider = Address::repeat_byte(0x33);
        let rules = StorageCreditsRules;
        let mut ctx = test_context();
        let mut storage = test_storage_provider(&mut ctx, u64::MAX, true);

        StorageCtx::enter(&mut storage, || {
            for call in [
                IStorageCredits::IStorageCreditsCalls::balanceOf(IStorageCredits::balanceOfCall {
                    account: owner,
                }),
                IStorageCredits::IStorageCreditsCalls::modeOf(IStorageCredits::modeOfCall {
                    account: owner,
                }),
                IStorageCredits::IStorageCreditsCalls::budgetOf(IStorageCredits::budgetOfCall {
                    account: owner,
                }),
            ] {
                assert!(matches!(
                    rules.admit(&call.abi_encode(), owner),
                    CallCheck::Continue
                ));
                for caller in [sequencer, outsider] {
                    assert!(matches!(
                        rules.admit(&call.abi_encode(), caller),
                        CallCheck::Revert(data) if data == Unauthorized {}.abi_encode()
                    ));
                }
            }
        });
    }

    #[test]
    fn mutations_and_malformed_calldata_remain_upstream_authorized() {
        let caller = Address::repeat_byte(0x11);
        let rules = StorageCreditsRules;

        assert!(matches!(
            rules.admit(
                &IStorageCredits::setBudgetCall { credits: 7 }.abi_encode(),
                caller
            ),
            CallCheck::Continue
        ));
        assert!(matches!(
            rules.admit(&IStorageCredits::balanceOfCall::SELECTOR, caller),
            CallCheck::Continue
        ));
    }
}
