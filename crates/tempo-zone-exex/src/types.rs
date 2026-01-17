//! Core types for zone proof generation and submission.
//!
//! These types match the IVerifier interface defined in the Solidity contracts.

use alloy_primitives::{Address, Bytes, B256, U128};
use alloy_sol_types::sol;
use serde::{Deserialize, Serialize};

sol! {
    /// Withdrawal struct matching the Solidity definition in IZone.sol
    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct SolWithdrawal {
        address sender;
        address to;
        uint128 amount;
        bytes32 memo;
        uint64 gasLimit;
        address fallbackRecipient;
        bytes data;
    }

    /// Batch commitment struct matching the Solidity definition
    #[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
    struct SolBatchCommitment {
        bytes32 newProcessedDepositQueueHash;
        bytes32 newStateRoot;
    }

    /// Interface for calling ZonePortal.submitBatch
    interface IZonePortal {
        function submitBatch(
            SolBatchCommitment calldata commitment,
            bytes32 expectedWithdrawalQueue2,
            bytes32 updatedWithdrawalQueue2,
            bytes32 newWithdrawalQueueOnly,
            bytes calldata verifierData,
            bytes calldata proof
        ) external;
    }

    /// WithdrawalRequested event from ZoneOutbox contract
    #[derive(Debug, PartialEq, Eq)]
    event WithdrawalRequested(
        uint64 indexed withdrawalIndex,
        address indexed sender,
        address to,
        uint128 amount,
        bytes32 memo,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes data
    );

    /// DepositMade event from ZonePortal contract
    #[derive(Debug, PartialEq, Eq)]
    event DepositMade(
        uint64 indexed zoneId,
        bytes32 indexed newCurrentDepositQueueHash,
        address indexed sender,
        address to,
        uint128 amount,
        bytes32 memo,
        bytes32 l1BlockHash,
        uint64 l1BlockNumber,
        uint64 l1Timestamp
    );
}

/// Withdrawal struct for Rust code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Withdrawal {
    pub sender: Address,
    pub to: Address,
    pub amount: U128,
    pub memo: B256,
    pub gas_limit: u64,
    pub fallback_recipient: Address,
    pub data: Bytes,
}

impl From<Withdrawal> for SolWithdrawal {
    fn from(w: Withdrawal) -> Self {
        Self {
            sender: w.sender,
            to: w.to,
            amount: u128::from_le_bytes(w.amount.to_le_bytes()),
            memo: w.memo,
            gasLimit: w.gas_limit,
            fallbackRecipient: w.fallback_recipient,
            data: w.data,
        }
    }
}

/// Batch commitment for Rust code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchCommitment {
    pub new_processed_deposit_queue_hash: B256,
    pub new_state_root: B256,
}

impl From<BatchCommitment> for SolBatchCommitment {
    fn from(c: BatchCommitment) -> Self {
        Self {
            newProcessedDepositQueueHash: c.new_processed_deposit_queue_hash,
            newStateRoot: c.new_state_root,
        }
    }
}

/// Inputs to the IVerifier.verify() function.
///
/// These are the 8 bytes32 values plus additional witness data that the prover
/// needs to generate a valid proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchInput {
    /// Where proof starts (from portal state)
    pub processed_deposit_queue_hash: B256,
    /// Stable target ceiling (from portal state)
    pub pending_deposit_queue_hash: B256,
    /// Where zone processed up to (from batch)
    pub new_processed_deposit_queue_hash: B256,
    /// Previous state root
    pub prev_state_root: B256,
    /// New state root after batch execution
    pub new_state_root: B256,
    /// What proof assumed queue2 was
    pub expected_withdrawal_queue2: B256,
    /// Queue2 with new withdrawals added to innermost
    pub updated_withdrawal_queue2: B256,
    /// New withdrawals only (only used if queue2 was empty)
    pub new_withdrawal_queue_only: B256,

    /// Blocks included in this batch
    pub blocks: Vec<BatchBlock>,
    /// Deposits processed in this batch
    pub deposits: Vec<Deposit>,
    /// Withdrawals created in this batch
    pub withdrawals: Vec<Withdrawal>,
    /// State transition witness data
    pub witness: StateTransitionWitness,
}

/// A block included in a batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchBlock {
    /// Block number
    pub number: u64,
    /// Block hash
    pub hash: B256,
    /// Parent hash
    pub parent_hash: B256,
    /// State root after this block
    pub state_root: B256,
    /// Transactions root
    pub transactions_root: B256,
    /// Receipts root
    pub receipts_root: B256,
}

/// A deposit from the portal contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deposit {
    /// L1 block hash when deposit was made
    pub l1_block_hash: B256,
    /// L1 block number
    pub l1_block_number: u64,
    /// L1 timestamp
    pub l1_timestamp: u64,
    /// Sender address
    pub sender: Address,
    /// Recipient address on zone
    pub to: Address,
    /// Amount deposited
    pub amount: U128,
    /// User-provided memo
    pub memo: B256,
}

/// Public values output by the prover.
///
/// These are the 8 bytes32 values that the verifier checks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicValues {
    pub processed_deposit_queue_hash: B256,
    pub pending_deposit_queue_hash: B256,
    pub new_processed_deposit_queue_hash: B256,
    pub prev_state_root: B256,
    pub new_state_root: B256,
    pub expected_withdrawal_queue2: B256,
    pub updated_withdrawal_queue2: B256,
    pub new_withdrawal_queue_only: B256,
}

/// Witness data for state transitions.
///
/// This enum allows switching between real proofs and mock proofs for development.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StateTransitionWitness {
    /// Real witness data for SP1 proof generation
    Real {
        /// Merkle proofs for state reads
        state_proofs: Vec<Bytes>,
        /// Account states accessed
        account_states: Vec<AccountState>,
        /// Storage slots accessed
        storage_proofs: Vec<StorageProof>,
    },
    /// Mock witness for development/testing
    Mock,
}

/// Account state for witness generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountState {
    pub address: Address,
    pub nonce: u64,
    pub balance: U128,
    pub code_hash: B256,
    pub storage_root: B256,
}

/// Storage proof for witness generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageProof {
    pub address: Address,
    pub slot: B256,
    pub value: B256,
    pub proof: Vec<Bytes>,
}

/// Bundle containing proof and public values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofBundle {
    /// The proof bytes
    pub proof: Bytes,
    /// Public values committed by the proof
    pub public_values: PublicValues,
    /// Verifier-specific data (e.g., attestation envelope)
    pub verifier_data: Bytes,
}
