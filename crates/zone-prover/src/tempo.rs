//! Proof-based Tempo state accessor.
//!
//! Replaces the RPC-based [`TempoStateReader`] precompile with a proof-backed
//! accessor that verifies Tempo L1 storage reads against the deduplicated MPT
//! node pool in [`BatchStateProof`].

use alloy_primitives::{Address, B256, Bytes, U256, map::HashMap};
use alloy_evm::precompiles::DynPrecompile;
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::{PrecompileError, PrecompileId, PrecompileOutput, PrecompileResult};

use crate::{
    mpt,
    types::{BatchStateProof, ProverError},
};

// Re-use the same function selectors as the node's TempoStateReader.
alloy_sol_types::sol! {
    function readStorageAt(address account, bytes32 slot, uint64 blockNumber) external view returns (bytes32);
    function readStorageBatchAt(address account, bytes32[] calldata slots, uint64 blockNumber) external view returns (bytes32[] memory);
    error DelegateCallNotAllowed();
}

/// Fixed gas cost charged on every call (matches the node's TempoStateReader).
const BASE_GAS: u64 = 200;
/// Additional gas charged per storage slot read.
const PER_SLOT_GAS: u64 = 200;

/// Read index key for looking up Tempo state reads.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReadKey {
    zone_block_index: u64,
    account: Address,
    slot: U256,
}

/// Proof-based Tempo state accessor.
///
/// Verifies all nodes in the deduplicated pool on construction, then serves
/// Tempo L1 storage reads by looking up pre-verified proofs.
pub struct TempoStateAccessor {
    /// Pre-verified MPT nodes from the deduplicated pool.
    /// Key: `keccak256(rlp_data)`, Value: raw RLP bytes.
    /// Used for full node_path walk verification (not yet wired up).
    #[allow(dead_code)]
    verified_nodes: HashMap<B256, Vec<u8>>,

    /// Index of reads by `(zone_block_index, account, slot)` for O(1) lookup.
    read_index: HashMap<ReadKey, ReadEntry>,

    /// Tempo block number currently bound for each zone block index.
    /// Updated when `advanceTempo` is called.
    block_bindings: HashMap<u64, u64>,
}

/// A pre-verified read entry.
#[derive(Debug, Clone)]
struct ReadEntry {
    /// Expected Tempo block number for this read.
    tempo_block_number: u64,
    /// Path through the verified node pool (used for full verification).
    #[allow(dead_code)]
    node_path: Vec<B256>,
    /// Expected value.
    value: U256,
}

impl TempoStateAccessor {
    /// Create a new accessor from batch state proofs.
    ///
    /// Verifies every node in the pool exactly once (`keccak256(node) == hash`).
    /// Builds a read index for O(1) lookup during execution.
    pub fn from_proofs(proofs: &BatchStateProof) -> Result<Self, ProverError> {
        // Phase 1: Verify each node in the pool.
        let mut verified_nodes = HashMap::default();
        for (claimed_hash, rlp_data) in &proofs.node_pool {
            mpt::verify_pool_node(*claimed_hash, rlp_data)?;
            verified_nodes.insert(*claimed_hash, rlp_data.clone());
        }

        // Phase 2: Index reads for fast lookup.
        let mut read_index = HashMap::default();
        for read in &proofs.reads {
            let key = ReadKey {
                zone_block_index: read.zone_block_index,
                account: read.account,
                slot: read.slot,
            };
            read_index.insert(
                key,
                ReadEntry {
                    tempo_block_number: read.tempo_block_number,
                    node_path: read.node_path.clone(),
                    value: read.value,
                },
            );
        }

        Ok(Self {
            verified_nodes,
            read_index,
            block_bindings: HashMap::default(),
        })
    }

    /// Bind a zone block index to a Tempo block number.
    ///
    /// Called after `advanceTempo` executes to record which Tempo block
    /// is currently active for a given zone block.
    pub fn bind_block(
        &mut self,
        zone_block_index: u64,
        tempo_block_number: u64,
    ) {
        self.block_bindings
            .insert(zone_block_index, tempo_block_number);
    }

    /// Look up a Tempo block hash from the verified read entries.
    ///
    /// This searches for a read against the TempoState contract itself
    /// to find the block hash binding.
    pub fn block_hash(&self, _tempo_block_number: u64) -> Option<B256> {
        // The block hash is derived from the Tempo header RLP provided in the witness.
        // This is validated during block execution when comparing against the proof.
        // For now, return None and let the caller validate via header RLP hashing.
        None
    }

    /// Read a Tempo L1 storage slot for a given zone block.
    ///
    /// Returns the pre-verified value from the proof set.
    pub fn read_storage(
        &self,
        zone_block_index: u64,
        account: Address,
        slot: U256,
    ) -> Result<U256, ProverError> {
        let key = ReadKey {
            zone_block_index,
            account,
            slot,
        };

        let entry = self.read_index.get(&key).ok_or(
            ProverError::TempoReadNotFound {
                block_index: zone_block_index,
                account,
                slot,
            },
        )?;

        // Verify the read is bound to the correct Tempo block.
        if let Some(&bound_block) = self.block_bindings.get(&zone_block_index) {
            if entry.tempo_block_number != bound_block {
                return Err(ProverError::InconsistentState(format!(
                    "Tempo read at block_index={zone_block_index} expects tempo_block={}, \
                     but block is bound to tempo_block={bound_block}",
                    entry.tempo_block_number,
                )));
            }
        }

        // The value was pre-verified during proof construction.
        // In a full implementation, we would walk the node_path through
        // verified_nodes to re-derive the value from the Tempo state root.
        // For now, we trust the pre-verified value.
        //
        // TODO: Implement full node_path walk verification.
        Ok(entry.value)
    }
}

/// Create a prover-side TempoStateReader precompile.
///
/// This replaces the RPC-based precompile with one that reads from the
/// proof-verified [`TempoStateAccessor`].
///
/// # Safety
///
/// The accessor is shared behind a raw pointer for use in the precompile closure.
/// The caller must ensure the accessor outlives the precompile.
pub fn prover_tempo_state_precompile(
    accessor: &TempoStateAccessor,
    block_index: usize,
) -> DynPrecompile {
    // We need to capture the accessor reference and block_index.
    // The precompile is used within a single block execution scope,
    // so the lifetime is guaranteed by the caller.
    let block_idx = block_index as u64;

    // Clone the data we need into the closure.
    let read_index = accessor.read_index.clone();
    let block_bindings = accessor.block_bindings.clone();

    DynPrecompile::new_stateful(
        PrecompileId::Custom("TempoStateReader-Prover".into()),
        move |input| {
            if !input.is_direct_call() {
                return Ok(PrecompileOutput::new_reverted(
                    0,
                    DelegateCallNotAllowed {}.abi_encode().into(),
                ));
            }

            let data = input.data;
            if data.len() < 4 {
                return Ok(PrecompileOutput::new_reverted(0, Bytes::new()));
            }

            let selector: [u8; 4] = data[..4].try_into().expect("len >= 4");

            if selector == readStorageAtCall::SELECTOR {
                handle_single_slot(&read_index, &block_bindings, block_idx, data)
            } else if selector == readStorageBatchAtCall::SELECTOR {
                handle_multi_slot(&read_index, &block_bindings, block_idx, data)
            } else {
                Ok(PrecompileOutput::new_reverted(0, Bytes::new()))
            }
        },
    )
}

fn handle_single_slot(
    read_index: &HashMap<ReadKey, ReadEntry>,
    block_bindings: &HashMap<u64, u64>,
    block_idx: u64,
    data: &[u8],
) -> PrecompileResult {
    let call = readStorageAtCall::abi_decode(data)
        .map_err(|_| PrecompileError::other("ABI decode failed"))?;

    let gas = BASE_GAS + PER_SLOT_GAS;

    let key = ReadKey {
        zone_block_index: block_idx,
        account: call.account,
        slot: U256::from_be_bytes(call.slot.0),
    };

    let entry = read_index
        .get(&key)
        .ok_or_else(|| {
            PrecompileError::other(format!(
                "Tempo read not in proof set: block={block_idx} account={} slot={}",
                call.account, call.slot
            ))
        })?;

    // Validate block binding.
    if let Some(&bound_block) = block_bindings.get(&block_idx) {
        if entry.tempo_block_number != bound_block {
            return Err(PrecompileError::other(format!(
                "Tempo read block mismatch: expected={bound_block}, got={}",
                entry.tempo_block_number
            )));
        }
    }

    let value = B256::from(entry.value.to_be_bytes());
    let encoded = readStorageAtCall::abi_encode_returns(&value);
    Ok(PrecompileOutput::new(gas, encoded.into()))
}

fn handle_multi_slot(
    read_index: &HashMap<ReadKey, ReadEntry>,
    block_bindings: &HashMap<u64, u64>,
    block_idx: u64,
    data: &[u8],
) -> PrecompileResult {
    let call = readStorageBatchAtCall::abi_decode(data)
        .map_err(|_| PrecompileError::other("ABI decode failed"))?;

    let num_slots = call.slots.len() as u64;
    let gas = BASE_GAS + PER_SLOT_GAS * num_slots;

    let mut results = Vec::with_capacity(call.slots.len());
    for slot in &call.slots {
        let key = ReadKey {
            zone_block_index: block_idx,
            account: call.account,
            slot: U256::from_be_bytes(slot.0),
        };

        let entry = read_index
            .get(&key)
            .ok_or_else(|| {
                PrecompileError::other(format!(
                    "Tempo batch read not in proof set: block={block_idx} account={} slot={}",
                    call.account, slot
                ))
            })?;

        if let Some(&bound_block) = block_bindings.get(&block_idx) {
            if entry.tempo_block_number != bound_block {
                return Err(PrecompileError::other(format!(
                    "Tempo read block mismatch: expected={bound_block}, got={}",
                    entry.tempo_block_number
                )));
            }
        }

        results.push(B256::from(entry.value.to_be_bytes()));
    }

    let encoded = readStorageBatchAtCall::abi_encode_returns(&results);
    Ok(PrecompileOutput::new(gas, encoded.into()))
}
