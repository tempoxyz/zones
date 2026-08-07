//! Public witness and commitment types for the Zone SPF.

use std::sync::Arc;

use alloy_primitives::{Address, B256, Bytes, U256};
use tempo_primitives::TempoHeader;
use zone_chainspec::ZoneChainSpec;

pub use tempo_zone_contracts::{
    BlockTransition, ChaumPedersenProof, DecryptionData, DepositQueueTransition, DepositType,
    EnabledToken, QueuedDeposit,
};
/// Trusted network configuration for Zone execution.
///
/// This is deliberately separate from [`BatchWitness`]: it is selected by the
/// verifier for the network it serves, not supplied by the prover. The zone
/// chain specification provides the parent Tempo hard-fork schedule. Block gas
/// limits and other inherited execution fields come from the canonical parent
/// Tempo header carried by the witness.
#[derive(Debug, Clone)]
pub struct SpfConfig {
    pub zone_chain_spec: Arc<ZoneChainSpec>,
}

impl SpfConfig {
    pub fn new(zone_chain_spec: Arc<ZoneChainSpec>) -> Self {
        Self { zone_chain_spec }
    }
}

/// Public values that the verifier binds to a submitted batch proof.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct PublicInputs {
    /// Zone identifier from which the SPF derives the EVM chain ID.
    pub zone_id: u32,
    /// Tempo ZonePortal whose state governs L1-backed Zone execution.
    pub portal: Address,
    /// Tempo block number committed by the submitted batch.
    pub tempo_block_number: u64,
    /// Tempo block number used to anchor this batch.
    pub anchor_block_number: u64,
    /// Block hash for `anchor_block_number`.
    pub anchor_block_hash: B256,
    /// Withdrawal batch index expected by the portal.
    pub expected_withdrawal_batch_index: u64,
}

/// Complete prover input for one Zone batch.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct BatchWitness {
    /// Values committed by the verifier.
    pub public_inputs: PublicInputs,
    /// Canonical Tempo header of the first Zone block's parent. Its hash and
    /// state root anchor the batch and its execution fields seed replay.
    pub parent_header: TempoHeader,
    /// Zone blocks in execution order.
    pub zone_blocks: Vec<ZoneBlock>,
    /// Stateless witness for the Zone state at the start of the batch.
    pub zone_state_witness: ZoneStateWitness,
    /// Stateless witness for Tempo state reads performed during the batch.
    pub tempo_state_witness: TempoStateWitness,
    /// RLP-encoded headers ordered from `tempo_block_number + 1` through the
    /// anchor block when ancestry verification is needed.
    pub tempo_ancestry_headers: Vec<Bytes>,
}

/// Zone block input, including its system-call inputs and raw user transactions.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct ZoneBlock {
    pub number: u64,
    pub parent_hash: B256,
    pub timestamp: u64,
    pub beneficiary: Address,
    /// RLP-encoded Tempo header passed to `ZoneInbox.advanceTempo`.
    pub tempo_header_rlp: Bytes,
    /// Deposits processed by `ZoneInbox.advanceTempo`, in calldata order.
    pub deposits: Vec<QueuedDeposit>,
    /// Encrypted-deposit decryption data, in calldata order.
    pub decryptions: Vec<DecryptionData>,
    /// Tokens enabled by `ZoneInbox.advanceTempo`, in calldata order.
    pub enabled_tokens: Vec<EnabledToken>,
    /// Withdrawal count passed to finalization in this block, if any.
    pub finalize_withdrawal_batch_count: Option<U256>,
    /// Encrypted sender payloads passed to withdrawal finalization.
    pub finalize_withdrawal_batch_encrypted_senders: Vec<Bytes>,
    /// Raw signed user transactions in execution order.
    pub transactions: Vec<Bytes>,
}

/// Stateless Zone state input.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct ZoneStateWitness {
    /// Deduplicated RLP-encoded nodes used for Zone state reads.
    pub node_pool: Vec<Bytes>,
    /// Deduplicated bytecode preimages, indexed by `keccak256(bytecode)`.
    pub bytecodes: Vec<Bytes>,
}

/// Stateless Tempo state input.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct TempoStateWitness {
    /// RLP-encoded header for the Tempo checkpoint bound in the initial Zone
    /// state. Its decoded state root anchors initial Tempo reads.
    pub initial_tempo_header_rlp: Bytes,
    /// Deduplicated RLP-encoded MPT nodes used for Tempo state reads.
    pub node_pool: Vec<Bytes>,
}

/// Commitments returned by a successful Zone batch transition.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct BatchOutput {
    /// Hash transition covering every Zone block in the batch.
    pub block_transition: BlockTransition,
    /// Progress of the ZoneInbox deposit queue during the batch.
    pub deposit_queue_transition: DepositQueueTransition,
    /// Hash chain created by finalizing the batch's withdrawals.
    pub withdrawal_queue_hash: B256,
    /// Batch index committed by `ZoneOutbox.lastBatch`.
    pub last_batch_commitment: LastBatchCommitment,
}

/// The portion of `ZoneOutbox.lastBatch` independently committed by the SPF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct LastBatchCommitment {
    pub withdrawal_batch_index: u64,
}
