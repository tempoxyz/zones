//! Witness and output types for the zone prover.
//!
//! These types define the complete interface between the zone node (witness generator)
//! and the pure state transition function. All data needed to re-execute a batch of
//! zone blocks without access to the full zone state is captured in [`BatchWitness`].

use alloy_primitives::{Address, B256, Bytes, U256, map::HashMap};
use alloy_sol_types::sol;

/// Public inputs committed by the proof system.
///
/// These values are provided by the portal contract and verified by the on-chain
/// verifier after proof submission.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PublicInputs {
    /// Previous batch's block hash (must equal `portal.blockHash`).
    pub prev_block_hash: B256,

    /// Tempo block number for the batch (must equal portal's `tempoBlockNumber`).
    pub tempo_block_number: u64,

    /// Anchor Tempo block number (`tempo_block_number` or recent block in EIP-2935 window).
    pub anchor_block_number: u64,

    /// Anchor Tempo block hash (must equal portal's EIP-2935 lookup).
    pub anchor_block_hash: B256,

    /// Expected withdrawal batch index (passed by portal as `withdrawalBatchIndex + 1`).
    pub expected_withdrawal_batch_index: u64,

    /// Registered sequencer (passed by portal; zone block beneficiary must match).
    pub sequencer: Address,
}

/// Complete witness for proving a batch of zone blocks.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BatchWitness {
    /// Public inputs committed by the proof system.
    pub public_inputs: PublicInputs,

    /// Zone chain ID for EVM configuration.
    ///
    /// Must match the chain ID used during the original execution (from genesis).
    /// Default is 13371 for Tempo zones.
    pub chain_id: u64,

    /// Previous batch's block header (for state-root binding).
    pub prev_block_header: ZoneHeader,

    /// Zone blocks to execute.
    pub zone_blocks: Vec<ZoneBlock>,

    /// Initial zone state with MPT proofs.
    pub initial_zone_state: ZoneStateWitness,

    /// Tempo state proofs for Tempo reads.
    pub tempo_state_proofs: BatchStateProof,

    /// Tempo headers for ancestry verification (only in ancestry mode).
    /// Ordered from `tempo_block_number + 1` to `anchor_block_number`.
    pub tempo_ancestry_headers: Vec<Vec<u8>>,
}

/// Output commitments produced by the prover.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BatchOutput {
    /// Zone block hash transition (prev -> final).
    pub block_transition: BlockTransition,

    /// Deposit queue processing.
    pub deposit_queue_transition: DepositQueueTransition,

    /// Withdrawal queue hash chain for this batch (`B256::ZERO` if no withdrawals).
    pub withdrawal_queue_hash: B256,

    /// Withdrawal batch parameters read from `ZoneOutbox.lastBatch`.
    pub last_batch: LastBatchCommitment,
}

/// Zone block hash transition.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BlockTransition {
    pub prev_block_hash: B256,
    pub next_block_hash: B256,
}

/// Deposit queue hash chain transition.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DepositQueueTransition {
    pub prev_processed_hash: B256,
    pub next_processed_hash: B256,
}

/// Last batch commitment read from zone state.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LastBatchCommitment {
    pub withdrawal_batch_index: u64,
}

/// Mirrors the Solidity `LastBatch` struct from ZoneOutbox.
/// Used internally when reading from zone state; fields are split across
/// `withdrawal_queue_hash` and `LastBatchCommitment` (index) in output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastBatch {
    pub withdrawal_queue_hash: B256,
    pub withdrawal_batch_index: u64,
}

// ---------------------------------------------------------------------------
//  Zone block types
// ---------------------------------------------------------------------------

/// A zone block to be executed by the prover.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ZoneBlock {
    /// Block number.
    pub number: u64,

    /// Parent block hash.
    pub parent_hash: B256,

    /// Timestamp.
    pub timestamp: u64,

    /// Beneficiary (must match registered sequencer).
    pub beneficiary: Address,

    /// Block gas limit.
    pub gas_limit: u64,

    /// Base fee per gas used for EVM execution.
    pub base_fee_per_gas: u64,

    /// Expected state root after executing this block.
    /// Provided by the zone node (from `BlockBuilderOutcome`); the prover validates
    /// that EVM execution produces a consistent result.
    pub expected_state_root: B256,

    /// Tempo header RLP used by the call (`ZoneInbox.advanceTempo`).
    /// If `None`, the block does not advance Tempo and the binding carries over.
    pub tempo_header_rlp: Option<Vec<u8>>,

    /// Deposits processed by the system tx (oldest first, unified queue).
    /// Must be empty if `tempo_header_rlp` is `None`.
    pub deposits: Vec<QueuedDeposit>,

    /// Decryption data for encrypted deposits in the system tx.
    /// Must be empty if `tempo_header_rlp` is `None`.
    pub decryptions: Vec<DecryptionData>,

    /// Sequencer-only: finalize a batch (only in final block, must be last).
    /// Required for the final block in a batch; must be absent in intermediate blocks.
    /// Uses `U256` to match Solidity `finalizeWithdrawalBatch(uint256 count)`.
    pub finalize_withdrawal_batch_count: Option<U256>,

    /// Transactions to execute (RLP-encoded `TempoTxEnvelope` bytes).
    pub transactions: Vec<Vec<u8>>,
}

/// Simplified zone block header for block hash computation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ZoneHeader {
    pub parent_hash: B256,
    pub beneficiary: Address,
    pub state_root: B256,
    pub transactions_root: B256,
    pub receipts_root: B256,
    pub number: u64,
    pub timestamp: u64,
}

// ---------------------------------------------------------------------------
//  Deposit / decryption types (mirrors Solidity structs)
// ---------------------------------------------------------------------------

/// Deposit type discriminator.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum DepositType {
    Regular,
    Encrypted,
}

/// A queued deposit from the L1 portal.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct QueuedDeposit {
    pub deposit_type: DepositType,
    /// ABI-encoded deposit data: `abi.encode(Deposit)` or `abi.encode(EncryptedDeposit)`.
    pub deposit_data: Bytes,
}

/// Chaum-Pedersen proof for ECDH shared secret derivation.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ChaumPedersenProof {
    /// Response: `s = r + c * privSeq (mod n)`.
    pub s: B256,
    /// Challenge: `c = hash(G, ephemeralPub, pubSeq, sharedSecretPoint, R1, R2)`.
    pub c: B256,
}

/// Decryption data provided by the sequencer for encrypted deposits.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct DecryptionData {
    /// ECDH shared secret (x-coordinate).
    pub shared_secret: B256,
    /// Y-parity of the shared secret point.
    pub shared_secret_y_parity: u8,
    /// Decrypted recipient.
    pub to: Address,
    /// Decrypted memo.
    pub memo: B256,
    /// Proof of correct shared secret derivation.
    pub cp_proof: ChaumPedersenProof,
}

// ---------------------------------------------------------------------------
//  Zone state witness
// ---------------------------------------------------------------------------

/// Initial zone state with MPT proofs for all accounts and storage slots
/// that will be accessed during batch execution.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ZoneStateWitness {
    /// Account data with storage proofs, keyed by address.
    pub accounts: HashMap<Address, AccountWitness>,

    /// Accounts confirmed absent from the state trie, with exclusion proofs.
    ///
    /// These are addresses accessed during execution that do not exist in the
    /// state (e.g., fresh ETH transfer targets, CALL targets with no code).
    /// The proof verifies that the account path is absent from the state trie.
    pub absent_accounts: HashMap<Address, Vec<Bytes>>,

    /// Zone state root at start of batch.
    pub state_root: B256,
}

/// Witness for a single account in the zone state.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AccountWitness {
    pub nonce: u64,
    pub balance: U256,
    pub code_hash: B256,
    /// The account's storage root (verified by the account MPT proof).
    pub storage_root: B256,
    /// Contract bytecode (if this account is a contract).
    pub code: Option<Bytes>,

    /// Storage slots with values, keyed by slot index.
    pub storage: HashMap<U256, U256>,

    /// MPT proof for the account (against the zone state root).
    pub account_proof: Vec<Bytes>,

    /// MPT proofs for storage slots, keyed by slot index.
    pub storage_proofs: HashMap<U256, Vec<Bytes>>,
}

// ---------------------------------------------------------------------------
//  Tempo state proofs
// ---------------------------------------------------------------------------

/// Batch-level Tempo state proof with deduplicated MPT node pool.
///
/// Instead of including separate MPT proofs for each Tempo storage read,
/// all proofs share a single pool of verified nodes. This provides ~16x
/// compression and prover speedup for large batches.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BatchStateProof {
    /// Deduplicated pool of all MPT nodes, keyed by `keccak256(rlp(node))`.
    pub node_pool: HashMap<B256, Vec<u8>>,

    /// Tempo state reads with storage proof paths.
    pub reads: Vec<L1StateRead>,

    /// Per-account Tempo L1 proof data (account trie proofs).
    ///
    /// Deduplicated by `(tempo_block_number, account)` — multiple storage reads
    /// to the same account at the same block share a single account proof.
    pub account_proofs: Vec<L1AccountProof>,
}

/// Account-level proof data from a Tempo L1 `eth_getProof` response.
///
/// One entry per unique `(tempo_block_number, account)` pair. Contains the
/// account's trie data and the proof path from `tempoStateRoot` to the
/// account leaf in the L1 state trie.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct L1AccountProof {
    /// The Tempo block number this proof was retrieved at.
    pub tempo_block_number: u64,

    /// Tempo L1 account address.
    pub account: Address,

    /// Account nonce.
    pub nonce: u64,

    /// Account balance.
    pub balance: U256,

    /// Account storage root.
    pub storage_root: B256,

    /// Account code hash.
    pub code_hash: B256,

    /// Account proof path through `node_pool` (state root -> account leaf).
    pub account_path: Vec<B256>,
}

/// A single Tempo L1 state read with a storage proof path through the
/// deduplicated node pool.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct L1StateRead {
    /// Which zone block performed this read.
    pub zone_block_index: u64,

    /// Which Tempo block to read from (must match `TempoState` for this block).
    pub tempo_block_number: u64,

    /// Tempo account address.
    pub account: Address,

    /// Storage slot.
    pub slot: U256,

    /// Storage proof path through `node_pool` (storage root -> slot leaf).
    ///
    /// Only the storage proof portion; the account proof is in `L1AccountProof`.
    pub storage_path: Vec<B256>,

    /// Expected value.
    pub value: U256,
}

// ---------------------------------------------------------------------------
//  Solidity ABI types (for system transaction construction)
// ---------------------------------------------------------------------------

sol! {
    /// Deposit struct matching the Solidity ABI.
    #[derive(Debug)]
    struct SolDeposit {
        address sender;
        address to;
        uint128 amount;
        bytes32 memo;
    }

    /// Queued deposit matching the Solidity ABI.
    #[derive(Debug)]
    struct SolQueuedDeposit {
        uint8 depositType;
        bytes depositData;
    }

    /// Chaum-Pedersen proof matching the Solidity ABI.
    #[derive(Debug)]
    struct SolChaumPedersenProof {
        bytes32 s;
        bytes32 c;
    }

    /// Decryption data matching the Solidity ABI.
    #[derive(Debug)]
    struct SolDecryptionData {
        bytes32 sharedSecret;
        uint8 sharedSecretYParity;
        address to;
        bytes32 memo;
        SolChaumPedersenProof cpProof;
    }

    /// ZoneInbox.advanceTempo ABI.
    #[derive(Debug)]
    function advanceTempo(
        bytes calldata header,
        SolQueuedDeposit[] calldata deposits,
        SolDecryptionData[] calldata decryptions
    );

    /// ZoneOutbox.finalizeWithdrawalBatch ABI.
    #[derive(Debug)]
    function finalizeWithdrawalBatch(uint256 count, uint64 blockNumber);
}

// ---------------------------------------------------------------------------
//  Error types
// ---------------------------------------------------------------------------

/// Errors that can occur during zone batch proving.
///
/// Implements `DBErrorMarker` so it can be used as `revm::Database::Error`.
#[derive(Debug, thiserror::Error)]
pub enum ProverError {
    /// An MPT proof failed verification.
    #[error("invalid proof: {0}")]
    InvalidProof(String),

    /// EVM execution failed.
    #[error("execution error: {0}")]
    ExecutionError(String),

    /// State is inconsistent with the expected values.
    #[error("inconsistent state: {0}")]
    InconsistentState(String),

    /// A required account or storage slot was missing from the witness.
    #[error("missing witness data: {0}")]
    MissingWitness(String),

    /// RLP decoding failed.
    #[error("rlp decode error: {0}")]
    RlpDecode(String),

    /// Tempo state read not found in proof set.
    #[error("tempo read not found: block_index={block_index}, account={account}, slot={slot}")]
    TempoReadNotFound {
        block_index: u64,
        account: Address,
        slot: U256,
    },
}

// Implement revm's DBErrorMarker so ProverError can be used as Database::Error.
impl revm::database_interface::DBErrorMarker for ProverError {}
