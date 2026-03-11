//! Shared types for zone batch proving.
//!
//! This crate is `no_std` compatible so it can be used inside SP1 (RISC-V) guest
//! programs and TEE enclaves, as well as in the host-side prover.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::vec::Vec;
use alloy_primitives::{Address, B256, U256};
use alloy_rlp::Encodable as _;
use serde::{Deserialize, Serialize};

pub mod constants;
mod sol_types;
pub use sol_types::{BlockTransition, DepositQueueTransition};

/// Public inputs committed by the proof system.
///
/// These values are passed to the verifier contract on L1 and must match
/// the on-chain state for the proof to be accepted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicInputs {
    /// Previous batch's block hash (must equal `portal.blockHash()`).
    pub prev_block_hash: B256,
    /// Tempo block number the batch committed to.
    pub tempo_block_number: u64,
    /// Anchor Tempo block number (same as `tempo_block_number` for direct mode,
    /// or a recent block within the EIP-2935 window for ancestry mode).
    pub anchor_block_number: u64,
    /// Anchor Tempo block hash (verified via EIP-2935 on-chain).
    pub anchor_block_hash: B256,
    /// Expected withdrawal batch index (portal's `withdrawalBatchIndex + 1`).
    pub expected_withdrawal_batch_index: u64,
    /// Registered sequencer address — zone block beneficiary must match.
    pub sequencer: Address,
}

/// Complete witness for proving a zone batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchWitness {
    /// Public inputs committed by the proof system.
    pub public_inputs: PublicInputs,
    /// Previous batch's block header (for state-root binding).
    pub prev_block_header: ZoneHeader,
    /// Zone blocks to execute.
    pub zone_blocks: Vec<ZoneBlock>,
    /// Initial zone state with MPT proofs.
    pub initial_zone_state: ZoneStateWitness,
    /// Tempo state proofs for L1 reads (deduplicated node pool).
    pub tempo_state_proofs: BatchStateProof,
    /// Tempo headers for ancestry verification (only in ancestry mode).
    /// Ordered from `tempo_block_number + 1` to `anchor_block_number`.
    pub tempo_ancestry_headers: Vec<Vec<u8>>,
}

/// Output produced by `prove_zone_batch`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchOutput {
    /// Zone block hash transition (prev -> final).
    pub block_transition: BlockTransition,
    /// Deposit queue processing transition.
    pub deposit_queue_transition: DepositQueueTransition,
    /// Withdrawal queue hash chain for this batch (`B256::ZERO` if no withdrawals).
    pub withdrawal_queue_hash: B256,
    /// Withdrawal batch commitment read from zone state.
    pub last_batch: LastBatchCommitment,
}

/// Withdrawal batch index read from `ZoneOutbox.lastBatch()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastBatchCommitment {
    pub withdrawal_batch_index: u64,
}

/// Simplified zone block header for hash computation.
///
/// The zone block hash is `keccak256(rlp_encode(header))`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneHeader {
    pub parent_hash: B256,
    pub beneficiary: Address,
    pub state_root: B256,
    pub transactions_root: B256,
    pub receipts_root: B256,
    pub number: u64,
    pub timestamp: u64,
}

impl alloy_rlp::Encodable for ZoneHeader {
    fn encode(&self, out: &mut dyn alloy_rlp::BufMut) {
        alloy_rlp::Header {
            list: true,
            payload_length: self.fields_len(),
        }
        .encode(out);
        self.parent_hash.encode(out);
        self.beneficiary.encode(out);
        self.state_root.encode(out);
        self.transactions_root.encode(out);
        self.receipts_root.encode(out);
        self.number.encode(out);
        self.timestamp.encode(out);
    }

    fn length(&self) -> usize {
        alloy_rlp::Header {
            list: true,
            payload_length: self.fields_len(),
        }
        .length()
            + self.fields_len()
    }
}

impl ZoneHeader {
    fn fields_len(&self) -> usize {
        self.parent_hash.length()
            + self.beneficiary.length()
            + self.state_root.length()
            + self.transactions_root.length()
            + self.receipts_root.length()
            + self.number.length()
            + self.timestamp.length()
    }

    /// Compute the block hash: `keccak256(rlp_encode(self))`.
    pub fn hash(&self) -> B256 {
        use alloy_rlp::Encodable;
        let mut buf = Vec::with_capacity(self.length());
        self.encode(&mut buf);
        alloy_primitives::keccak256(&buf)
    }
}

/// A zone block to execute inside the prover.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneBlock {
    /// Block number.
    pub number: u64,
    /// Parent block hash.
    pub parent_hash: B256,
    /// Block timestamp.
    pub timestamp: u64,
    /// Beneficiary (must match registered sequencer).
    pub beneficiary: Address,
    /// Tempo header RLP for `ZoneInbox.advanceTempo`.
    /// `None` if this block does not advance Tempo.
    pub tempo_header_rlp: Option<Vec<u8>>,
    /// Deposits processed by the system tx (oldest first).
    pub deposits: Vec<QueuedDeposit>,
    /// Decryption data for encrypted deposits.
    pub decryptions: Vec<DecryptionData>,
    /// If `Some`, finalize a withdrawal batch in the final block.
    pub finalize_withdrawal_batch_count: Option<U256>,
    /// User transactions to execute (serialized).
    pub transactions: Vec<Vec<u8>>,
}

/// Deposit type discriminator for the unified deposit queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DepositType {
    Regular,
    Encrypted,
}

/// A queued deposit entry (regular or encrypted).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueuedDeposit {
    pub deposit_type: DepositType,
    /// ABI-encoded deposit data (`Deposit` or `EncryptedDeposit`).
    pub deposit_data: Vec<u8>,
}

/// Decryption data provided by the sequencer for encrypted deposits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecryptionData {
    /// ECDH shared secret (x-coordinate).
    pub shared_secret: B256,
    /// Y coordinate parity of the shared secret point.
    pub shared_secret_y_parity: u8,
    /// Decrypted recipient.
    pub to: Address,
    /// Decrypted memo.
    pub memo: B256,
    /// Chaum-Pedersen proof of correct shared secret derivation.
    pub cp_proof: ChaumPedersenProof,
}

/// Chaum-Pedersen proof for ECDH shared secret derivation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChaumPedersenProof {
    /// Response: `s = r + c * privSeq (mod n)`.
    pub s: B256,
    /// Challenge: `c = hash(G, ephemeralPub, pubSeq, sharedSecretPoint, R1, R2)`.
    pub c: B256,
}

/// Initial zone state with MPT proofs for accessed accounts/slots.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ZoneStateWitness {
    /// Account data with storage and MPT proofs.
    pub accounts: Vec<(Address, AccountWitness)>,
    /// Zone state root at start of batch.
    pub state_root: B256,
}

/// Witness data for a single account in the zone state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountWitness {
    pub nonce: u64,
    pub balance: U256,
    pub code_hash: B256,
    pub code: Option<Vec<u8>>,
    /// Storage slot values.
    pub storage: Vec<(U256, U256)>,
    /// MPT proof for the account in the state trie.
    pub account_proof: Vec<Vec<u8>>,
    /// MPT proofs for storage slots in the storage trie.
    pub storage_proofs: Vec<(U256, Vec<Vec<u8>>)>,
}

/// Deduplicated Tempo state proofs for cross-domain reads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BatchStateProof {
    /// Deduplicated pool of all MPT nodes, keyed by `keccak256(rlp(node))`.
    pub node_pool: Vec<(B256, Vec<u8>)>,
    /// Tempo state reads with proof paths referencing the node pool.
    pub reads: Vec<L1StateRead>,
}

/// A single Tempo L1 state read with its proof path through the node pool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct L1StateRead {
    /// Which zone block performed this read.
    pub zone_block_index: u64,
    /// Which Tempo block to read from.
    pub tempo_block_number: u64,
    /// Tempo account address.
    pub account: Address,
    /// Storage slot.
    pub slot: U256,
    /// Path through `node_pool` from state root to leaf.
    pub node_path: Vec<B256>,
    /// Expected storage value.
    pub value: U256,
}

/// Errors from the zone batch STF.
#[derive(Debug, Clone)]
pub enum Error {
    /// MPT proof verification failed.
    InvalidProof,
    /// EVM execution error.
    ExecutionError(alloc::string::String),
    /// State or input consistency violation.
    InconsistentState,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidProof => write!(f, "invalid proof"),
            Self::ExecutionError(msg) => write!(f, "execution error: {msg}"),
            Self::InconsistentState => write!(f, "inconsistent state"),
        }
    }
}
