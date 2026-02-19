//! Per-block witness data store.
//!
//! The [`WitnessStore`] bridges the payload builder (which records state accesses
//! during block execution and generates MPT proofs) and the zone monitor (which
//! fetches L1 proofs, assembles the full [`BatchWitness`], and submits proofs
//! to the ZonePortal).
//!
//! ## Flow
//!
//! 1. **Builder** executes a zone block with recording enabled, generates the
//!    [`ZoneStateWitness`], and calls [`WitnessStore::insert`].
//! 2. **Monitor** processes a block range, calls [`WitnessStore::take_range`]
//!    to consume the stored data, fetches L1 proofs, and assembles the
//!    [`BatchWitness`] for proof generation.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use zone_prover::types::{ZoneBlock, ZoneHeader, ZoneStateWitness};

use super::recording_l1::RecordedL1Read;

/// Data captured during a single zone block build.
///
/// Stored by the payload builder after each block and consumed by the
/// zone monitor when assembling a batch for proof generation.
#[derive(Debug)]
pub struct BuiltBlockWitness {
    /// Zone block in prover format (system txs + pool txs + deposits).
    pub zone_block: ZoneBlock,

    /// Zone state witness with MPT proofs against the parent state root.
    /// For single-block batches, this is the initial state for the batch.
    /// For multi-block batches, only the first block's witness is used as
    /// the batch's `initial_zone_state`.
    pub zone_state_witness: ZoneStateWitness,

    /// Previous block header (for block hash verification by the prover).
    pub prev_block_header: ZoneHeader,

    /// L1 storage reads recorded during this block's execution.
    pub l1_reads: Vec<RecordedL1Read>,

    /// Zone chain ID for EVM configuration.
    pub chain_id: u64,

    /// RLP-encoded Tempo header for ancestry verification.
    pub tempo_header_rlp: Vec<u8>,
}

/// Thread-safe store for per-block witness data.
///
/// Keyed by zone block number. The builder inserts after each block, and
/// the monitor takes entries when submitting a batch.
#[derive(Debug, Default)]
pub struct WitnessStore {
    blocks: BTreeMap<u64, BuiltBlockWitness>,
}

impl WitnessStore {
    /// Insert witness data for a built zone block.
    ///
    /// Overwrites any existing entry for the same block number (e.g., if
    /// the builder rebuilt a block due to reorg).
    pub fn insert(&mut self, block_number: u64, witness: BuiltBlockWitness) {
        self.blocks.insert(block_number, witness);
    }

    /// Take witness data for a single block, removing it from the store.
    pub fn take(&mut self, block_number: u64) -> Option<BuiltBlockWitness> {
        self.blocks.remove(&block_number)
    }

    /// Take witness data for a range of blocks `[from, to]`, removing them
    /// from the store. Returns entries in block-number order.
    ///
    /// Missing blocks within the range are silently skipped; the caller
    /// should check that the returned count matches the expected range.
    pub fn take_range(&mut self, from: u64, to: u64) -> Vec<(u64, BuiltBlockWitness)> {
        let mut result = Vec::new();
        for num in from..=to {
            if let Some(w) = self.blocks.remove(&num) {
                result.push((num, w));
            }
        }
        result
    }

    /// Number of blocks currently stored.
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Returns true if the store is empty.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Discard entries below a given block number (cleanup after batch submission).
    pub fn prune_below(&mut self, block_number: u64) {
        self.blocks = self.blocks.split_off(&block_number);
    }
}

/// Shared, thread-safe witness store handle.
pub type SharedWitnessStore = Arc<Mutex<WitnessStore>>;
