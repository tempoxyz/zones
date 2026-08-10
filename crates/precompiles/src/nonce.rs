//! Zone privacy rules for the upstream Tempo nonce-manager precompile.

use alloy_primitives::Address;
use alloy_sol_types::SolCall;
use tempo_precompiles::{
    Precompile as _,
    dispatch::selector_from_calldata,
    nonce::{INonce, NonceManager},
};

use crate::{
    account_privacy::AccountPrivacy,
    execution::{CallCheck, CallRules},
};

#[derive(Clone)]
pub(crate) struct NonceRules {
    privacy: AccountPrivacy,
}

impl NonceRules {
    pub(crate) fn new(current_sequencer: Address) -> Self {
        Self {
            privacy: AccountPrivacy::new(current_sequencer),
        }
    }
}

impl CallRules for NonceRules {
    fn admit(&self, data: &[u8], caller: Address) -> CallCheck {
        if selector_from_calldata(data) != Some(INonce::getNonceCall::SELECTOR) {
            return CallCheck::Continue;
        }
        let Ok(call) = INonce::getNonceCall::abi_decode_raw(&data[4..]) else {
            return CallCheck::Continue;
        };
        self.privacy.authorize(caller, &[call.account])
    }
}

pub(crate) fn execute(data: &[u8], caller: Address) -> revm::precompile::PrecompileResult {
    NonceManager::new().call(data, caller)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use alloy_sol_types::SolError;
    use tempo_zone_contracts::Unauthorized;

    #[test]
    fn nonce_getter_allows_only_owner_or_sequencer() {
        let owner = Address::repeat_byte(0x11);
        let outsider = Address::repeat_byte(0x22);
        let sequencer = Address::repeat_byte(0x33);
        let rules = NonceRules::new(sequencer);
        let call = INonce::getNonceCall {
            account: owner,
            nonceKey: U256::from(7),
        };

        for caller in [owner, sequencer] {
            assert!(matches!(
                rules.admit(&call.abi_encode(), caller),
                CallCheck::Continue
            ));
        }
        let CallCheck::Revert(bytes) = rules.admit(&call.abi_encode(), outsider) else {
            panic!("another account's nonce must be private")
        };
        assert_eq!(bytes, Unauthorized {}.abi_encode());
    }
}
