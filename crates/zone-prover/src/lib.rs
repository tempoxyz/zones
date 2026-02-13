//! Zone Prover — pure state transition function for zone batch proving.
//!
//! This crate implements the zone prover as described in `prover-design.md`.
//! The core function [`prove_zone_batch`] takes a complete witness of zone blocks
//! and their dependencies, executes the EVM state transitions (including
//! sequencer-only protocol transactions), and outputs commitments for on-chain
//! verification.
//!
//! The function is designed to be `no_std`-compatible for deployment in ZKVMs
//! (SP1) and TEEs (SGX/TDX), though it currently requires `std`.

#![cfg_attr(docsrs, feature(doc_cfg))]
// Suppress unused crate warnings for dependencies needed by full EVM execution
// (tempo-evm, tempo-revm, tempo-chainspec) that aren't fully wired up yet.
#![allow(unused_crate_dependencies)]

pub mod ancestry;
pub mod db;
pub mod execute;
pub mod header;
pub mod mpt;
pub mod tempo;
pub mod types;

use alloy_primitives::{B256, U256, keccak256};
use revm::Database;
use tracing::{debug, info};

use crate::{
    db::WitnessDatabase,
    execute::storage,
    tempo::TempoStateAccessor,
    types::{
        BatchOutput, BatchWitness, BlockTransition, DepositQueueTransition,
        LastBatch, LastBatchCommitment, ProverError, ZoneHeader,
    },
};

/// Default chain ID for the prover.
///
/// This should come from a chain spec, but for now we use a constant.
/// Zone blocks don't typically validate chain ID in the prover context.
const DEFAULT_CHAIN_ID: u64 = 1;

/// Pure state transition function for zone batch proving.
///
/// Takes a complete witness of zone blocks and their dependencies, re-executes
/// the EVM state transitions, and outputs commitments for on-chain verification.
///
/// The core commitment is the **zone block hash transition** (not the raw state
/// root), matching the privacy zone spec and Solidity reference implementation.
///
/// # Phases
///
/// 1. Verify Tempo state proofs (deduplicated node pool)
/// 2. Initialize zone state from witness and bind to previous block header
/// 3. Execute zone blocks and compute block hashes
/// 4. Extract output commitments and validate Tempo binding
pub fn prove_zone_batch(witness: BatchWitness) -> Result<BatchOutput, ProverError> {
    info!("Starting zone batch proof generation");

    // ---------------------------------------------------------------
    // Phase 1: Verify Tempo state proofs
    // ---------------------------------------------------------------
    debug!("Phase 1: Verifying Tempo state proofs");

    let mut tempo_state = TempoStateAccessor::from_proofs(&witness.tempo_state_proofs)?;

    debug!(
        node_pool_size = witness.tempo_state_proofs.node_pool.len(),
        reads = witness.tempo_state_proofs.reads.len(),
        "Tempo state proofs verified"
    );

    // ---------------------------------------------------------------
    // Phase 2: Initialize zone state
    // ---------------------------------------------------------------
    debug!("Phase 2: Initializing zone state from witness");

    let witness_db = WitnessDatabase::from_witness(&witness.initial_zone_state)?;

    // Bind initial state root to the previous block header.
    if witness_db.state_root() != witness.prev_block_header.state_root {
        return Err(ProverError::InconsistentState(format!(
            "witness state root {} does not match previous block header state root {}",
            witness_db.state_root(),
            witness.prev_block_header.state_root
        )));
    }

    let prev_header_hash = witness.prev_block_header.block_hash();
    if prev_header_hash != witness.public_inputs.prev_block_hash {
        return Err(ProverError::InvalidProof(format!(
            "previous block header hash {prev_header_hash} does not match \
             public input prev_block_hash {}",
            witness.public_inputs.prev_block_hash
        )));
    }

    // Read the initial deposit queue hash from the witness database.
    let deposit_prev = read_storage_from_db(
        &witness_db,
        execute::ZONE_INBOX_ADDRESS,
        storage::ZONE_INBOX_PROCESSED_HASH_SLOT,
    )?;

    // ---------------------------------------------------------------
    // Phase 3: Execute zone blocks and compute block hashes
    // ---------------------------------------------------------------
    debug!(
        blocks = witness.zone_blocks.len(),
        "Phase 3: Executing zone blocks"
    );

    let mut prev_block_hash = witness.public_inputs.prev_block_hash;
    let mut prev_header = witness.prev_block_header.clone();

    // Track the database across block executions.
    // For now, we pass the WitnessDatabase through each block execution.
    // The WitnessDatabase provides the initial state; the EVM tracks changes
    // via its journaled state.
    let mut current_db = witness_db;

    for (idx, block) in witness.zone_blocks.iter().enumerate() {
        let is_last_block = idx + 1 == witness.zone_blocks.len();

        // Validate block linkage.
        if block.parent_hash != prev_block_hash {
            return Err(ProverError::InconsistentState(format!(
                "block {} parent_hash {} does not match expected {prev_block_hash}",
                block.number, block.parent_hash,
            )));
        }
        if block.number != prev_header.number + 1 {
            return Err(ProverError::InconsistentState(format!(
                "block number {} is not prev + 1 (expected {})",
                block.number,
                prev_header.number + 1,
            )));
        }
        // Timestamps must be non-decreasing.
        if block.timestamp < prev_header.timestamp {
            return Err(ProverError::InconsistentState(format!(
                "block {} timestamp {} < previous timestamp {}",
                block.number, block.timestamp, prev_header.timestamp,
            )));
        }
        if block.beneficiary != witness.public_inputs.sequencer {
            return Err(ProverError::InconsistentState(format!(
                "block {} beneficiary {} does not match sequencer {}",
                block.number, block.beneficiary, witness.public_inputs.sequencer,
            )));
        }

        // Enforce finalizeWithdrawalBatch only in the final block.
        if is_last_block {
            if block.finalize_withdrawal_batch_count.is_none() {
                return Err(ProverError::InconsistentState(
                    "final block must have finalize_withdrawal_batch_count".into(),
                ));
            }
        } else if block.finalize_withdrawal_batch_count.is_some() {
            return Err(ProverError::InconsistentState(format!(
                "block {} (not final) has finalize_withdrawal_batch_count",
                block.number,
            )));
        }

        // Enforce that deposits/decryptions require tempo_header_rlp.
        if block.tempo_header_rlp.is_none()
            && (!block.deposits.is_empty() || !block.decryptions.is_empty())
        {
            return Err(ProverError::InconsistentState(format!(
                "block {} has deposits/decryptions but no tempo_header_rlp",
                block.number,
            )));
        }

        // Execute the block using the real EVM.
        let (exec_result, db) = execute::execute_zone_block(
            current_db,
            block,
            idx,
            &mut tempo_state,
            DEFAULT_CHAIN_ID,
            is_last_block,
        )?;
        current_db = db;

        // Use the expected state root from the witness (Phase 1 approach).
        // Full state root computation from BundleState will be Phase 2.
        let state_root = block.expected_state_root;

        // Build the zone block header and compute the block hash.
        let header = ZoneHeader {
            parent_hash: prev_block_hash,
            beneficiary: block.beneficiary,
            state_root,
            transactions_root: exec_result.transactions_root,
            receipts_root: exec_result.receipts_root,
            number: block.number,
            timestamp: block.timestamp,
        };

        prev_block_hash = header.block_hash();
        prev_header = header;

        debug!(
            block_number = block.number,
            block_hash = %prev_block_hash,
            "Executed zone block"
        );
    }

    // ---------------------------------------------------------------
    // Phase 4: Extract output commitments
    // ---------------------------------------------------------------
    debug!("Phase 4: Extracting output commitments");

    // Read the final deposit queue hash from the post-execution state.
    let deposit_next = read_storage_from_db(
        &current_db,
        execute::ZONE_INBOX_ADDRESS,
        storage::ZONE_INBOX_PROCESSED_HASH_SLOT,
    )?;

    // Read the last batch from the post-execution state.
    let last_batch = read_last_batch_from_db(&current_db)?;

    // Validate TempoState binding.
    // Read TempoState.tempoBlockNumber() from post-execution zone state.
    let packed_slot = read_storage_from_db(
        &current_db,
        execute::TEMPO_STATE_ADDRESS,
        storage::TEMPO_STATE_PACKED_SLOT,
    )?;
    let tempo_block_number_u256 = U256::from_be_bytes(packed_slot.into());
    let tempo_number = storage::extract_tempo_block_number(tempo_block_number_u256);

    if tempo_number != witness.public_inputs.tempo_block_number {
        return Err(ProverError::InconsistentState(format!(
            "TempoState block number {tempo_number} does not match \
             public input tempo_block_number {}",
            witness.public_inputs.tempo_block_number,
        )));
    }

    // Anchor validation.
    if witness.public_inputs.anchor_block_number == tempo_number {
        // Direct mode: anchor == tempo, hashes must match.
        let tempo_hash = compute_tempo_block_hash(&witness)?;
        if tempo_hash != witness.public_inputs.anchor_block_hash {
            return Err(ProverError::InconsistentState(format!(
                "direct mode: tempo hash {tempo_hash} != anchor hash {}",
                witness.public_inputs.anchor_block_hash,
            )));
        }
    } else {
        // Ancestry mode: verify parent-hash chain.
        let tempo_hash = compute_tempo_block_hash(&witness)?;
        ancestry::verify_tempo_ancestry_chain(
            tempo_hash,
            tempo_number,
            witness.public_inputs.anchor_block_number,
            witness.public_inputs.anchor_block_hash,
            &witness.tempo_ancestry_headers,
        )?;
    }

    info!(
        prev_block_hash = %witness.public_inputs.prev_block_hash,
        next_block_hash = %prev_block_hash,
        "Zone batch proof generation complete"
    );

    Ok(BatchOutput {
        block_transition: BlockTransition {
            prev_block_hash: witness.public_inputs.prev_block_hash,
            next_block_hash: prev_block_hash,
        },
        deposit_queue_transition: DepositQueueTransition {
            prev_processed_hash: deposit_prev,
            next_processed_hash: deposit_next,
        },
        withdrawal_queue_hash: last_batch.withdrawal_queue_hash,
        last_batch: LastBatchCommitment {
            withdrawal_batch_index: last_batch.withdrawal_batch_index,
        },
    })
}

// ---------------------------------------------------------------------------
//  Helper functions: state reads
// ---------------------------------------------------------------------------

/// Read a storage slot from a WitnessDatabase-backed state.
///
/// Converts the U256 value to a B256 for hash-type slots.
fn read_storage_from_db(
    db: &WitnessDatabase,
    address: alloy_primitives::Address,
    slot: U256,
) -> Result<B256, ProverError> {
    // Clone to get a mutable reference for the Database trait.
    let mut db_clone = db.clone();
    let value = db_clone.storage(address, slot)?;
    Ok(B256::from(value.to_be_bytes()))
}

/// Read ZoneOutbox.lastBatch from the zone state.
fn read_last_batch_from_db(db: &WitnessDatabase) -> Result<LastBatch, ProverError> {
    let mut db_clone = db.clone();

    // Read withdrawal_queue_hash from base slot.
    let wqh_value = db_clone.storage(
        execute::ZONE_OUTBOX_ADDRESS,
        storage::ZONE_OUTBOX_LAST_BATCH_BASE_SLOT,
    )?;
    let withdrawal_queue_hash = B256::from(wqh_value.to_be_bytes());

    // Read withdrawal_batch_index from base + 1.
    let wbi_value = db_clone.storage(
        execute::ZONE_OUTBOX_ADDRESS,
        storage::ZONE_OUTBOX_LAST_BATCH_BASE_SLOT + U256::from(1),
    )?;
    let withdrawal_batch_index = wbi_value.to::<u64>();

    Ok(LastBatch {
        withdrawal_queue_hash,
        withdrawal_batch_index,
    })
}

/// Compute the Tempo block hash from the last advanceTempo call in the batch.
///
/// This is `keccak256(rlp(tempo_header))` for the most recent Tempo header.
fn compute_tempo_block_hash(
    witness: &BatchWitness,
) -> Result<B256, ProverError> {
    // Find the last block with a Tempo header.
    for block in witness.zone_blocks.iter().rev() {
        if let Some(header_rlp) = &block.tempo_header_rlp {
            return Ok(keccak256(header_rlp));
        }
    }

    // If no block advances Tempo, the hash is from the previous batch.
    // This is the binding that carries over.
    Err(ProverError::InconsistentState(
        "no tempo_header_rlp in any zone block — cannot compute Tempo block hash".into(),
    ))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256, map::HashMap};

    use crate::types::*;

    use super::*;

    /// Minimal test to verify the prove function structure.
    #[test]
    fn test_prove_zone_batch_empty_blocks_validation() {
        let witness = BatchWitness {
            public_inputs: PublicInputs {
                prev_block_hash: B256::ZERO,
                tempo_block_number: 100,
                anchor_block_number: 100,
                anchor_block_hash: B256::ZERO,
                expected_withdrawal_batch_index: 1,
                sequencer: Address::ZERO,
            },
            prev_block_header: ZoneHeader {
                parent_hash: B256::ZERO,
                beneficiary: Address::ZERO,
                state_root: B256::ZERO,
                transactions_root: B256::ZERO,
                receipts_root: B256::ZERO,
                number: 0,
                timestamp: 0,
            },
            zone_blocks: vec![],
            initial_zone_state: ZoneStateWitness {
                accounts: HashMap::default(),
                state_root: B256::ZERO,
            },
            tempo_state_proofs: BatchStateProof {
                node_pool: HashMap::default(),
                reads: vec![],
            },
            tempo_ancestry_headers: vec![],
        };

        // With zero blocks, the function should validate structure
        // but the previous block header hash won't match B256::ZERO.
        let result = prove_zone_batch(witness);
        // This will fail because the prev_block_header hash won't match
        // ZERO (it gets RLP-encoded and hashed).
        assert!(result.is_err());
    }
}
