//! Per-block witness data store.
//!
//! The [`WitnessStore`] bridges the payload builder (which records state accesses
//! during block execution) and the zone monitor (which generates the zone state
//! witness, fetches L1 proofs, assembles the full [`BatchWitness`], and submits
//! proofs to the ZonePortal).
//!
//! ## Flow
//!
//! 1. **Builder** executes a zone block with recording enabled, snapshots the
//!    accessed accounts/storage, and stores a [`BuiltBlockWitness`].
//! 2. **Monitor** processes a block range, calls [`WitnessStore::take_range`],
//!    merges all accessed accounts/storage into a union, generates MPT proofs
//!    against the initial state root S₀ via `eth_getProof`, fetches L1 proofs,
//!    and assembles the [`BatchWitness`] for proof generation.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use alloy_primitives::{Address, B256, U256};
use zone_prover::types::{ZoneBlock, ZoneHeader};

use super::recording_l1::RecordedL1Read;

/// Snapshot of all state accesses recorded during a single block execution.
///
/// Plain-data equivalent of [`RecordedAccesses`](super::RecordedAccesses)
/// without the `Arc<Mutex<>>` interior — suitable for storage and transfer.
#[derive(Debug, Clone, Default)]
pub struct AccessSnapshot {
    /// Accounts whose `basic()` was called.
    pub accounts: BTreeSet<Address>,
    /// Storage slots read per account.
    pub storage: BTreeMap<Address, BTreeSet<U256>>,
}

impl AccessSnapshot {
    /// Merge another snapshot into this one (union of all accesses).
    pub fn merge(&mut self, other: &Self) {
        self.accounts.extend(&other.accounts);
        for (addr, slots) in &other.storage {
            self.storage.entry(*addr).or_default().extend(slots);
        }
    }
}

/// Data captured during a single zone block build.
///
/// Stored by the payload builder after each block and consumed by the
/// zone monitor when assembling a batch for proof generation.
#[derive(Debug, Clone)]
pub struct BuiltBlockWitness {
    /// Zone block in prover format (system txs + pool txs + deposits).
    pub zone_block: ZoneBlock,

    /// Snapshot of all EVM state accesses during this block's execution.
    /// The monitor merges these across all blocks in the batch, then
    /// generates MPT proofs against S₀ for the full union.
    pub access_snapshot: AccessSnapshot,

    /// Previous block header (for block hash verification by the prover).
    pub prev_block_header: ZoneHeader,

    /// Parent block hash — identifies the state root S_n this block was
    /// built against. For the first block in a batch, this is S₀.
    pub parent_block_hash: B256,

    /// L1 storage reads recorded during this block's execution.
    pub l1_reads: Vec<RecordedL1Read>,

    /// Zone chain ID for EVM configuration.
    pub chain_id: u64,

    /// RLP-encoded Tempo header processed by `advanceTempo` in this block.
    /// `None` if the block did not advance Tempo (binding carries over).
    pub tempo_header_rlp: Option<Vec<u8>>,
}

/// Thread-safe store for per-block witness data.
///
/// Keyed by zone block number. The builder inserts after each block, and
/// the monitor takes entries when submitting a batch.
///
/// TODO(production): Add a max entry cap or auto-pruning on insert. Currently,
/// `prune_below` is only called by the proof generator. On nodes that don't
/// generate proofs (validators, full nodes), the store will grow unbounded.
/// Consider either a `max_size` limit that drops the oldest blocks on insert,
/// or a periodic cleanup task independent of proof generation.
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
    /// Returns an error if any block in the range is missing from the store.
    pub fn take_range(&mut self, from: u64, to: u64) -> Result<Vec<(u64, BuiltBlockWitness)>, u64> {
        // Pre-check that all blocks exist before removing any.
        for num in from..=to {
            if !self.blocks.contains_key(&num) {
                return Err(num);
            }
        }
        let mut result = Vec::with_capacity((to - from + 1) as usize);
        for num in from..=to {
            // Safe: pre-check above guarantees existence.
            result.push((num, self.blocks.remove(&num).unwrap()));
        }
        Ok(result)
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

    /// Get witness data for a range of blocks `[from, to]`, cloning them
    /// from the store (non-destructive). Returns entries in block-number order.
    ///
    /// Returns an error if any block in the range is missing from the store.
    pub fn get_range(&self, from: u64, to: u64) -> Result<Vec<(u64, BuiltBlockWitness)>, u64> {
        for num in from..=to {
            if !self.blocks.contains_key(&num) {
                return Err(num);
            }
        }
        let mut result = Vec::with_capacity((to - from + 1) as usize);
        for num in from..=to {
            result.push((num, self.blocks.get(&num).unwrap().clone()));
        }
        Ok(result)
    }

    /// Returns the smallest block number in the store, or `None` if empty.
    pub fn first_block(&self) -> Option<u64> {
        self.blocks.keys().next().copied()
    }

    /// Returns the largest block number in the store, or `None` if empty.
    pub fn last_block(&self) -> Option<u64> {
        self.blocks.keys().next_back().copied()
    }
}

/// Shared, thread-safe witness store handle.
pub type SharedWitnessStore = Arc<Mutex<WitnessStore>>;
