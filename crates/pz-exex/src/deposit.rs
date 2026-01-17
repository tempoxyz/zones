//! Deposit processing for Privacy Zone.
//!
//! Handles crediting TIP-20 balances for deposits from L1.

use crate::{state::ZoneState, types::Deposit};
use alloy_primitives::{Address, U256};
use reth_tracing::tracing::debug;
use tempo_precompiles::tip20::TIP20Token;

/// Result of processing a deposit.
#[derive(Debug)]
pub struct DepositResult {
    /// Whether the deposit was successful.
    pub success: bool,
    /// Gas used (always 0 for simple deposits).
    pub gas_used: u64,
}

/// Process a deposit: credit TIP-20 balance to recipient.
pub fn process_deposit(
    state: &mut ZoneState,
    gas_token: Address,
    deposit: &Deposit,
) -> eyre::Result<DepositResult> {
    // Credit TIP-20 balance to recipient
    credit_tip20_balance(state, gas_token, deposit.to, deposit.amount)?;

    debug!(
        to = %deposit.to,
        amount = %deposit.amount,
        "Credited deposit balance"
    );

    Ok(DepositResult {
        success: true,
        gas_used: 0,
    })
}

/// Credit TIP-20 balance by directly writing to the storage slot.
/// This bypasses the normal mint flow (no ISSUER_ROLE check, no supply cap).
fn credit_tip20_balance(
    state: &mut ZoneState,
    gas_token: Address,
    account: Address,
    amount: U256,
) -> eyre::Result<()> {
    let token = TIP20Token::from_address(gas_token)
        .map_err(|e| eyre::eyre!("Invalid TIP-20 token address {}: {:?}", gas_token, e))?;

    let balance_slot = token.balances[account].slot();

    // Read current balance
    let current_balance = state.get_storage(gas_token, balance_slot);
    let new_balance = current_balance
        .checked_add(amount)
        .ok_or_else(|| eyre::eyre!("Balance overflow"))?;

    // Write new balance
    state.set_storage(gas_token, balance_slot, new_balance);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deposit_result() {
        let result = DepositResult {
            success: true,
            gas_used: 0,
        };
        assert!(result.success);
        assert_eq!(result.gas_used, 0);
    }
}
