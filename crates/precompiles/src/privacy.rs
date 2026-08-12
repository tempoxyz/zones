//! Shared account-scoped read authorization for Zone precompiles.

use alloy_primitives::Address;
use alloy_sol_types::SolError;
use tempo_zone_contracts::Unauthorized;

use crate::{
    execution::{CallCheck, CallRuleError},
    has_portal_role,
    storage::{L1State, L1StorageReader},
};

/// Allow a read when the immediate EVM caller owns any account named by the getter, or is an
/// active sequencer at the transaction's anchored Tempo block.
pub(crate) fn check_caller_or_sequencer<P: L1StorageReader>(
    l1: &L1State<P>,
    caller: Address,
    accounts: &[Address],
) -> CallCheck {
    if accounts.contains(&caller) {
        return CallCheck::Continue;
    }

    match l1.read_portal(|portal| &portal.role[caller]) {
        Ok(role) if has_portal_role(role, tempo_zone_contracts::ZonePortal::Role::Sequencer) => {
            CallCheck::Continue
        }
        Ok(_) => CallCheck::Revert(Unauthorized {}.abi_encode().into()),
        Err(error) => CallCheck::Error(CallRuleError::Tempo(error)),
    }
}
