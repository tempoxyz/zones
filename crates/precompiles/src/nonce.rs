//! Zone read-privacy rules for the upstream Tempo NonceManager precompile.

use alloy_primitives::Address;
use alloy_sol_types::SolInterface;
use tempo_contracts::precompiles::INonce;

use crate::{
    execution::{CallCheck, CallRules},
    privacy::check_caller_or_sequencer,
    storage::{L1State, L1StorageReader},
};

/// Zone-specific rules applied before forwarding to upstream `NonceManager`.
#[derive(Clone)]
pub(crate) struct NonceRules<P> {
    l1: L1State<P>,
}

impl<P> NonceRules<P> {
    pub(crate) fn new(l1: L1State<P>) -> Self {
        Self { l1 }
    }
}

impl<P: L1StorageReader> CallRules for NonceRules<P> {
    fn admit(&self, data: &[u8], caller: Address, _tx_origin: Address) -> CallCheck {
        let Ok(call) = INonce::INonceCalls::abi_decode(data) else {
            // Preserve the upstream error and gas behavior for malformed or unknown calldata.
            return CallCheck::Continue;
        };

        // Intentionally exhaustive: an upstream ABI addition must be classified here.
        match call {
            INonce::INonceCalls::getNonce(call) => {
                check_caller_or_sequencer(&self.l1, caller, &[call.account])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256, address};
    use alloy_sol_types::{SolCall, SolError};
    use tempo_zone_contracts::Unauthorized;

    use crate::{
        storage::StorageCtx,
        test_utils::{MockL1Reader, test_context, test_storage_provider},
    };

    const PORTAL_ADDRESS: Address = address!("0x0000000000000000000000000000000000000b01");

    #[test]
    fn nonce_reads_allow_owner_and_sequencer_but_reject_other_callers() {
        let owner = Address::repeat_byte(0x11);
        let sequencer = Address::repeat_byte(0x22);
        let outsider = Address::repeat_byte(0x33);
        let intermediary = Address::repeat_byte(0x44);
        let reader = MockL1Reader::default();
        reader.seed_active_sequencer(PORTAL_ADDRESS, 0, sequencer);
        let rules = NonceRules::new(L1State::new(reader, PORTAL_ADDRESS));
        let call = INonce::getNonceCall {
            account: owner,
            nonceKey: U256::from(1),
        };
        let mut ctx = test_context();
        let mut storage = test_storage_provider(&mut ctx, u64::MAX, true);

        StorageCtx::enter(&mut storage, || {
            for caller in [owner, sequencer] {
                assert!(matches!(
                    rules.admit(&call.abi_encode(), caller, caller),
                    CallCheck::Continue
                ));
            }
            for caller in [outsider, intermediary] {
                assert!(matches!(
                    rules.admit(&call.abi_encode(), caller, caller),
                    CallCheck::Revert(data) if data == Unauthorized {}.abi_encode()
                ));
            }
        });
    }
}
