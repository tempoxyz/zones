//! Deposit processing for Privacy Zone.
//!
//! Handles crediting TIP-20 balances and executing deposit calldata.

use crate::{
    state::ZoneState,
    types::{Deposit, ExitIntent},
};
use alloy_evm::{Evm, EvmEnv, EvmFactory};
use alloy_primitives::{Address, TxKind, U256};
use reth_revm::{
    DatabaseCommit, State,
    context::{BlockEnv, TxEnv, result::ExecutionResult},
    db::{BundleState, states::bundle_state::BundleRetention},
};
use reth_tracing::tracing::{debug, warn};
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_evm::{TempoBlockEnv, TempoEvmFactory, TempoHaltReason};
use tempo_precompiles::tip20::TIP20Token;
use tempo_revm::TempoTxEnv;

/// Result of processing a deposit.
#[derive(Debug)]
pub struct DepositResult {
    /// Whether the deposit was fully successful.
    pub success: bool,
    /// If the deposit calldata execution failed, this contains the refund exit.
    pub refund_exit: Option<ExitIntent>,
    /// Gas used by the calldata execution (0 if no calldata).
    pub gas_used: u64,
    /// Bundle state changes from execution (for state root computation).
    pub bundle: Option<BundleState>,
}

/// Process a deposit: credit TIP-20 balance and optionally execute calldata.
///
/// # Flow
/// 1. Credit `deposit.to` with `deposit.amount` of the gas token (TIP-20).
/// 2. If `deposit.gas_limit > 0`: execute call to `deposit.to` with `deposit.data`.
/// 3. If call reverts: create a refund exit back to L1 for `deposit.sender`.
pub fn process_deposit(
    state: &mut ZoneState,
    gas_token: Address,
    deposit: &Deposit,
    zone_block: u64,
    exit_index: u64,
) -> eyre::Result<DepositResult> {
    // Step 1: Credit TIP-20 balance to recipient
    credit_tip20_balance(state, gas_token, deposit.to, deposit.amount)?;

    debug!(
        to = %deposit.to,
        amount = %deposit.amount,
        gas_limit = deposit.gas_limit,
        "Credited deposit balance"
    );

    // Step 2: If no calldata, we're done
    if deposit.gas_limit == 0 || deposit.data.is_empty() {
        return Ok(DepositResult {
            success: true,
            refund_exit: None,
            gas_used: 0,
            bundle: None,
        });
    }

    // Step 3: Execute calldata at `to` address
    let (execution_result, bundle) = execute_deposit_call(state, deposit)?;

    match &execution_result {
        ExecutionResult::Success { gas_used, .. } => {
            debug!(
                to = %deposit.to,
                gas_used = gas_used,
                "Deposit calldata executed successfully"
            );
            Ok(DepositResult {
                success: true,
                refund_exit: None,
                gas_used: *gas_used,
                bundle: Some(bundle),
            })
        }
        ExecutionResult::Revert { gas_used, output } => {
            warn!(
                to = %deposit.to,
                gas_used = gas_used,
                output = %output,
                "Deposit calldata reverted, creating refund exit"
            );

            // Debit the balance we just credited (refund goes back to L1)
            debit_tip20_balance(state, gas_token, deposit.to, deposit.amount)?;

            // Create a refund exit back to L1
            let refund_exit = ExitIntent {
                sender: deposit.to, // The zone recipient who got the failed deposit
                recipient: deposit.sender, // Back to original L1 sender
                amount: deposit.amount,
                zone_block,
                exit_index,
            };

            Ok(DepositResult {
                success: false,
                refund_exit: Some(refund_exit),
                gas_used: *gas_used,
                bundle: Some(bundle),
            })
        }
        ExecutionResult::Halt { gas_used, reason } => {
            warn!(
                to = %deposit.to,
                gas_used = gas_used,
                ?reason,
                "Deposit calldata halted, creating refund exit"
            );

            // Debit the balance we just credited
            debit_tip20_balance(state, gas_token, deposit.to, deposit.amount)?;

            // Create a refund exit back to L1
            let refund_exit = ExitIntent {
                sender: deposit.to,
                recipient: deposit.sender,
                amount: deposit.amount,
                zone_block,
                exit_index,
            };

            Ok(DepositResult {
                success: false,
                refund_exit: Some(refund_exit),
                gas_used: *gas_used,
                bundle: Some(bundle),
            })
        }
    }
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

/// Debit TIP-20 balance (for refunds when deposit calldata fails).
fn debit_tip20_balance(
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
        .checked_sub(amount)
        .ok_or_else(|| eyre::eyre!("Balance underflow"))?;

    // Write new balance
    state.set_storage(gas_token, balance_slot, new_balance);

    Ok(())
}

/// Execute the deposit calldata at the recipient address.
/// Returns the execution result and the bundle state changes.
fn execute_deposit_call(
    state: &mut ZoneState,
    deposit: &Deposit,
) -> eyre::Result<(ExecutionResult<TempoHaltReason>, BundleState)> {
    let env: EvmEnv<TempoHardfork, TempoBlockEnv> = EvmEnv {
        cfg_env: Default::default(),
        block_env: TempoBlockEnv {
            inner: BlockEnv {
                number: U256::from(deposit.l1_block_number),
                timestamp: U256::from(deposit.l1_timestamp),
                gas_limit: deposit.gas_limit,
                ..Default::default()
            },
            timestamp_millis_part: 0,
        },
    };

    // Create a system transaction for the deposit call
    let tx = TempoTxEnv {
        inner: TxEnv {
            caller: deposit.sender, // The L1 sender is the caller
            gas_limit: deposit.gas_limit,
            gas_price: 0, // System tx, no gas price
            kind: TxKind::Call(deposit.to),
            value: U256::ZERO, // Value already credited as TIP-20
            data: deposit.data.clone(),
            nonce: 0, // System tx
            ..Default::default()
        },
        is_system_tx: true,
        ..Default::default()
    };

    let factory = TempoEvmFactory::default();
    let evm_state = State::builder()
        .with_database(state.db_mut())
        .with_bundle_update()
        .build();
    let mut evm = factory.create_evm(evm_state, env);

    let result = evm.transact(tx)?;

    // Commit state changes back to our state
    evm.db_mut().commit(result.state);
    evm.db_mut().merge_transitions(BundleRetention::Reverts);

    // Take the bundle state for state root computation
    let bundle = evm.db_mut().take_bundle();

    Ok((result.result, bundle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn test_deposit_result_no_calldata() {
        let result = DepositResult {
            success: true,
            refund_exit: None,
            gas_used: 0,
            bundle: None,
        };
        assert!(result.success);
        assert!(result.refund_exit.is_none());
    }

    #[test]
    fn test_deposit_with_refund() {
        let refund = ExitIntent {
            sender: address!("1111111111111111111111111111111111111111"),
            recipient: address!("2222222222222222222222222222222222222222"),
            amount: U256::from(1000),
            zone_block: 1,
            exit_index: 0,
        };

        let result = DepositResult {
            success: false,
            refund_exit: Some(refund),
            gas_used: 21000,
            bundle: None,
        };

        assert!(!result.success);
        assert!(result.refund_exit.is_some());
        assert_eq!(result.refund_exit.unwrap().amount, U256::from(1000));
    }
}
