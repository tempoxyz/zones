//! Shared authorization for account-indexed private reads.
//!
//! The current Zone block beneficiary is a consensus-committed identifier for its sequencer.

use alloy_primitives::Address;
use alloy_sol_types::SolError;
use tempo_zone_contracts::Unauthorized;

use crate::execution::CallCheck;

#[derive(Clone)]
pub(crate) struct AccountPrivacy {
    current_sequencer: Address,
}

impl AccountPrivacy {
    pub(crate) const fn new(current_sequencer: Address) -> Self {
        Self { current_sequencer }
    }

    pub(crate) fn authorize(&self, caller: Address, accounts: &[Address]) -> CallCheck {
        if accounts.contains(&caller)
            || (self.current_sequencer != Address::ZERO && caller == self.current_sequencer)
        {
            CallCheck::Continue
        } else {
            CallCheck::Revert(Unauthorized {}.abi_encode().into())
        }
    }
}
