//! Shared account-scoped read authorization for Zone precompiles.

use alloy_primitives::Address;
use alloy_sol_types::SolError;
use tempo_zone_contracts::Unauthorized;

use crate::{
    execution::CallCheck,
    storage::{L1State, L1StorageReader},
};

/// Allow a read only when the immediate EVM caller owns an account named by the getter.
pub(crate) fn check_caller<P: L1StorageReader>(
    _l1: &L1State<P>,
    caller: Address,
    accounts: &[Address],
) -> CallCheck {
    if accounts.contains(&caller) {
        CallCheck::Continue
    } else {
        CallCheck::Revert(Unauthorized {}.abi_encode().into())
    }
}
