//! Shared types for SP1 guest program.
//!
//! These types must match exactly with the prover (tempo-zone-exex) and the
//! Solidity contracts for proper ABI encoding.

use alloy_primitives::{Address, B256, Bytes, U128};
use serde::{Deserialize, Serialize};

/// A withdrawal request from L2 to L1.
///
/// Field order matches Solidity struct in IZone.sol for correct ABI encoding:
/// ```solidity
/// struct Withdrawal {
///     address sender;
///     address to;
///     uint128 amount;
///     bytes32 memo;
///     uint64 gasLimit;
///     address fallbackRecipient;
///     bytes data;
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Withdrawal {
    /// Who initiated the withdrawal on the zone.
    pub sender: Address,
    /// Recipient address on L1.
    pub to: Address,
    /// Amount to withdraw.
    pub amount: U128,
    /// User-provided context.
    pub memo: B256,
    /// Max gas for IExitReceiver callback (0 = no callback).
    pub gas_limit: u64,
    /// Zone address for bounce-back if call fails.
    pub fallback_recipient: Address,
    /// Calldata for IExitReceiver (if gasLimit > 0).
    pub data: Bytes,
}

/// A deposit from L1 to L2.
///
/// Field order matches Solidity struct in IZone.sol for correct ABI encoding:
/// ```solidity
/// struct Deposit {
///     bytes32 l1BlockHash;
///     uint64 l1BlockNumber;
///     uint64 l1Timestamp;
///     address sender;
///     address to;
///     uint128 amount;
///     bytes32 memo;
/// }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deposit {
    /// L1 block hash when deposit was made.
    pub l1_block_hash: B256,
    /// L1 block number.
    pub l1_block_number: u64,
    /// L1 timestamp.
    pub l1_timestamp: u64,
    /// Sender address on L1.
    pub sender: Address,
    /// Recipient address on L2.
    pub to: Address,
    /// Amount deposited.
    pub amount: U128,
    /// User-provided context.
    pub memo: B256,
}

/// Witness data for state transition validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateWitness {
    /// The new state root after applying all transactions.
    pub new_state_root: B256,
    // Future: Add stateless proof data here
    // pub state_proofs: Vec<StateProof>,
    // pub account_proofs: Vec<AccountProof>,
}

/// Input to the SP1 guest program.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchInput {
    /// Hash of the processed deposit queue (before this batch).
    pub processed_deposit_queue_hash: B256,
    /// Hash of the pending deposit queue.
    pub pending_deposit_queue_hash: B256,
    /// Deposits consumed in this batch.
    pub deposits_consumed: Vec<Deposit>,

    /// Previous state root (before this batch).
    pub prev_state_root: B256,

    /// Expected withdrawal queue hash (from L1 state).
    pub expected_withdrawal_queue2: B256,
    /// Withdrawals generated in this batch.
    pub withdrawals: Vec<Withdrawal>,

    /// Witness data for state transition validation.
    pub witness: StateWitness,
}

/// Public values committed by the SP1 program.
///
/// These values are visible on-chain and used for verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicValues {
    /// Hash of the processed deposit queue (before this batch).
    pub processed_deposit_queue_hash: B256,
    /// Hash of the pending deposit queue.
    pub pending_deposit_queue_hash: B256,
    /// New hash of the processed deposit queue (after this batch).
    pub new_processed_deposit_queue_hash: B256,

    /// Previous state root (before this batch).
    pub prev_state_root: B256,
    /// New state root (after this batch).
    pub new_state_root: B256,

    /// Expected withdrawal queue hash (from L1 state).
    pub expected_withdrawal_queue2: B256,
    /// Updated withdrawal queue hash (after appending new withdrawals).
    pub updated_withdrawal_queue2: B256,
    /// Hash of only the new withdrawals in this batch.
    pub new_withdrawal_queue_only: B256,
}
