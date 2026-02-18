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
pub mod sparse_mpt;
pub mod tempo;
/// Test utilities for building MPT proofs and zone state witnesses.
///
/// Available when the `test-utils` feature is enabled, or during `cargo test`.
#[cfg(any(test, feature = "test-utils"))]
pub mod testutil;
pub mod types;

use alloy_primitives::{Address, B256, U256, keccak256, map::HashMap as PrimHashMap};
use alloy_trie::TrieAccount;
use revm::{Database, database::State};
use tracing::{debug, info};

use crate::{
    db::WitnessDatabase,
    execute::storage,
    sparse_mpt::SparseTrie,
    tempo::TempoStateAccessor,
    types::{
        BatchOutput, BatchWitness, BlockTransition, DepositQueueTransition,
        LastBatch, LastBatchCommitment, ProverError, ZoneHeader,
    },
};

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

    // Wrap WitnessDatabase in State so that EVM state changes are committed
    // and visible to subsequent transactions and post-execution reads.
    // Enable bundle update tracking so we can extract per-block state changes
    // for sparse MPT state root computation.
    let mut current_state: State<WitnessDatabase> =
        State::builder().with_database(witness_db).with_bundle_update().build();

    // Read the initial deposit queue hash from the zone state.
    let deposit_prev = read_storage_from_db(
        &mut current_state,
        execute::ZONE_INBOX_ADDRESS,
        storage::ZONE_INBOX_PROCESSED_HASH_SLOT,
    )?;

    // Read the initial Tempo block number from the zone state to seed the
    // Tempo block number tracker (for blocks that don't advance Tempo).
    let initial_packed = read_storage_from_db(
        &mut current_state,
        execute::TEMPO_STATE_ADDRESS,
        storage::TEMPO_STATE_PACKED_SLOT,
    )?;
    let mut current_tempo_block_number =
        storage::extract_tempo_block_number(U256::from_be_bytes(initial_packed.into()));

    // Build sparse tries from witness proofs for state root computation.
    let mut state_trie = sparse_mpt::build_state_trie(&witness.initial_zone_state)?;
    let mut storage_tries: PrimHashMap<Address, SparseTrie> = PrimHashMap::default();
    for (addr, acct) in &witness.initial_zone_state.accounts {
        let storage_trie = sparse_mpt::build_storage_trie(acct)?;
        storage_tries.insert(*addr, storage_trie);
    }

    // Read the initial tempoStateRoot from TempoState (slot 4).
    // This is the Merkle root of the Tempo L1 state trie, used to verify L1
    // storage proofs. It is updated whenever `advanceTempo` fires.
    let initial_tempo_state_root = read_storage_from_db(
        &mut current_state,
        execute::TEMPO_STATE_ADDRESS,
        storage::TEMPO_STATE_STATE_ROOT_SLOT,
    )?;
    let mut current_tempo_state_root = initial_tempo_state_root;

    // ---------------------------------------------------------------
    // Phase 3: Execute zone blocks and compute block hashes
    // ---------------------------------------------------------------
    debug!(
        blocks = witness.zone_blocks.len(),
        chain_id = witness.chain_id,
        "Phase 3: Executing zone blocks"
    );

    let mut prev_block_hash = witness.public_inputs.prev_block_hash;
    let mut prev_header = witness.prev_block_header.clone();

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

        // Update Tempo block number and state root if this block advances Tempo.
        if let Some(tempo_header_rlp) = &block.tempo_header_rlp {
            current_tempo_block_number =
                ancestry::extract_block_number_from_rlp(tempo_header_rlp).map_err(|e| {
                    ProverError::RlpDecode(format!(
                        "block {} tempo header block number: {e}",
                        block.number,
                    ))
                })?;
            current_tempo_state_root =
                ancestry::extract_state_root_from_rlp(tempo_header_rlp).map_err(|e| {
                    ProverError::RlpDecode(format!(
                        "block {} tempo header state root: {e}",
                        block.number,
                    ))
                })?;
        }

        // Bind the current Tempo block number and state root BEFORE creating
        // the precompile, so that the precompile's cloned bindings are correct.
        tempo_state.bind_block(idx as u64, current_tempo_block_number);
        tempo_state.bind_state_root(idx as u64, current_tempo_state_root);

        // Execute the block using the real EVM.
        let (exec_result, state) = execute::execute_zone_block(
            current_state,
            block,
            idx,
            &tempo_state,
            witness.chain_id,
            is_last_block,
        )?;
        current_state = state;

        // Merge the execution transitions into the bundle and extract the
        // per-block BundleState (account + storage changes).
        current_state.merge_transitions(revm::database::states::bundle_state::BundleRetention::Reverts);
        let bundle = current_state.take_bundle();

        // Compute the new state root from bundle changes + sparse tries.
        let state_root = compute_state_root_from_bundle(
            &bundle,
            &mut state_trie,
            &mut storage_tries,
        )?;

        // Verify the computed state root matches the expected value.
        if state_root != block.expected_state_root {
            return Err(ProverError::InconsistentState(format!(
                "block {} state root mismatch: computed={state_root}, expected={}",
                block.number, block.expected_state_root,
            )));
        }

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
        &mut current_state,
        execute::ZONE_INBOX_ADDRESS,
        storage::ZONE_INBOX_PROCESSED_HASH_SLOT,
    )?;

    // Read the last batch from the post-execution state.
    let last_batch = read_last_batch_from_db(&mut current_state)?;

    // Validate TempoState binding.
    // Read TempoState.tempoBlockNumber() from post-execution zone state.
    let packed_slot = read_storage_from_db(
        &mut current_state,
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

    // Compute the Tempo block hash for anchor validation.
    // First try to find it from the batch's zone blocks (most recent advanceTempo).
    // If no block advances Tempo, read it from the zone state (TempoState slot 0).
    let tempo_hash = compute_tempo_block_hash(&witness, &mut current_state)?;

    // Anchor validation.
    if witness.public_inputs.anchor_block_number == tempo_number {
        // Direct mode: anchor == tempo, hashes must match.
        if tempo_hash != witness.public_inputs.anchor_block_hash {
            return Err(ProverError::InconsistentState(format!(
                "direct mode: tempo hash {tempo_hash} != anchor hash {}",
                witness.public_inputs.anchor_block_hash,
            )));
        }
    } else {
        // Ancestry mode: verify parent-hash chain.
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
//  Helper functions: state root computation
// ---------------------------------------------------------------------------

/// Compute the new state root from a per-block `BundleState` and the
/// persistent sparse tries.
///
/// For each changed account:
/// 1. Compute the new storage root from storage changes
/// 2. Build the updated `TrieAccount` (nonce, balance, storage_root, code_hash)
/// 3. Update the state trie leaf
///
/// For destroyed accounts, remove the state trie leaf.
fn compute_state_root_from_bundle(
    bundle: &revm::database::BundleState,
    state_trie: &mut SparseTrie,
    storage_tries: &mut PrimHashMap<Address, SparseTrie>,
) -> Result<B256, ProverError> {
    for (addr, account) in &bundle.state {
        // Skip accounts that weren't modified.
        if account.status.is_not_modified() {
            continue;
        }

        let acct_key = sparse_mpt::account_key(*addr);

        // Check if the account was destroyed.
        if account.status.was_destroyed() {
            state_trie.remove_leaf(&acct_key);
            storage_tries.remove(addr);
            // If the account was destroyed then re-created (DestroyedChanged),
            // we still need to add it back below.
            if account.info.is_none() {
                continue;
            }
        }

        // Compute the new storage root for this account.
        let storage_trie = storage_tries
            .entry(*addr)
            .or_insert_with(SparseTrie::empty);

        let has_storage_changes = account.storage.iter().any(|(_, slot)| {
            slot.present_value != slot.previous_or_original_value
        });

        if has_storage_changes || account.status.was_destroyed() {
            for (slot_key, slot) in &account.storage {
                if slot.present_value == slot.previous_or_original_value
                    && !account.status.was_destroyed()
                {
                    continue;
                }
                let key = sparse_mpt::storage_key(*slot_key);
                if slot.present_value.is_zero() {
                    storage_trie.remove_leaf(&key);
                } else {
                    let mut encoded = Vec::new();
                    alloy_rlp::Encodable::encode(&slot.present_value, &mut encoded);
                    storage_trie.update_leaf(key, encoded);
                }
            }
        }

        let new_storage_root = storage_trie.root();

        // Build the updated TrieAccount for the state trie leaf.
        if let Some(info) = &account.info {
            let trie_account = TrieAccount {
                nonce: info.nonce,
                balance: info.balance,
                storage_root: new_storage_root,
                code_hash: info.code_hash,
            };
            let mut encoded = Vec::new();
            alloy_rlp::Encodable::encode(&trie_account, &mut encoded);
            state_trie.update_leaf(acct_key, encoded);
        }
    }

    Ok(state_trie.root())
}

// ---------------------------------------------------------------------------
//  Helper functions: state reads
// ---------------------------------------------------------------------------

/// Read a storage slot from a database (e.g., `State<WitnessDatabase>`).
///
/// Converts the U256 value to a B256 for hash-type slots.
fn read_storage_from_db(
    db: &mut impl Database<Error = ProverError>,
    address: alloy_primitives::Address,
    slot: U256,
) -> Result<B256, ProverError> {
    let value = db.storage(address, slot)?;
    Ok(B256::from(value.to_be_bytes()))
}

/// Read ZoneOutbox.lastBatch from the zone state.
fn read_last_batch_from_db(
    db: &mut impl Database<Error = ProverError>,
) -> Result<LastBatch, ProverError> {
    // Read withdrawal_queue_hash from base slot.
    let wqh_value = db.storage(
        execute::ZONE_OUTBOX_ADDRESS,
        storage::ZONE_OUTBOX_LAST_BATCH_BASE_SLOT,
    )?;
    let withdrawal_queue_hash = B256::from(wqh_value.to_be_bytes());

    // Read withdrawal_batch_index from base + 1.
    let wbi_value = db.storage(
        execute::ZONE_OUTBOX_ADDRESS,
        storage::ZONE_OUTBOX_LAST_BATCH_BASE_SLOT + U256::from(1),
    )?;
    let withdrawal_batch_index = wbi_value.to::<u64>();

    Ok(LastBatch {
        withdrawal_queue_hash,
        withdrawal_batch_index,
    })
}

/// Compute the Tempo block hash for anchor validation.
///
/// First tries to find it from the batch's zone blocks (the most recent
/// `advanceTempo` header RLP). If no block advances Tempo, reads the hash
/// from the zone state (TempoState slot 0), which carries over from the
/// previous batch.
fn compute_tempo_block_hash(
    witness: &BatchWitness,
    db: &mut impl Database<Error = ProverError>,
) -> Result<B256, ProverError> {
    // Find the last block with a Tempo header.
    for block in witness.zone_blocks.iter().rev() {
        if let Some(header_rlp) = &block.tempo_header_rlp {
            return Ok(keccak256(header_rlp));
        }
    }

    // No block advances Tempo — read the block hash from TempoState slot 0
    // which carries over from the previous batch.
    let hash = read_storage_from_db(
        db,
        execute::TEMPO_STATE_ADDRESS,
        storage::TEMPO_STATE_BLOCK_HASH_SLOT,
    )?;
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, map::HashMap};

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
            chain_id: 13371,
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
                absent_accounts: HashMap::default(),
                state_root: B256::ZERO,
            },
            tempo_state_proofs: BatchStateProof {
                node_pool: HashMap::default(),
                reads: vec![],
                account_proofs: vec![],
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
