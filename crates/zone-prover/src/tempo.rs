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

/// Account proof index key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AccountProofKey {
    tempo_block_number: u64,
    account: Address,
}

/// Indexed account proof data for MPT verification.
#[derive(Debug, Clone)]
struct AccountProofEntry {
    nonce: u64,
    balance: U256,
    storage_root: B256,
    code_hash: B256,
    /// Account proof hashes through `verified_nodes`.
    account_path: Vec<B256>,
}

/// Proof-based Tempo state accessor.
///
/// Verifies all nodes in the deduplicated pool on construction, then serves
/// Tempo L1 storage reads by looking up pre-verified proofs and validating
/// them against the `tempoStateRoot`.
pub struct TempoStateAccessor {
    /// Pre-verified MPT nodes from the deduplicated pool.
    /// Key: `keccak256(rlp_data)`, Value: raw RLP bytes.
    verified_nodes: HashMap<B256, Vec<u8>>,

    /// Account proofs indexed by `(tempo_block_number, account)`.
    account_proof_index: HashMap<AccountProofKey, AccountProofEntry>,

    /// Index of reads by `(zone_block_index, account, slot)` for O(1) lookup.
    read_index: HashMap<ReadKey, ReadEntry>,

    /// Tempo block number currently bound for each zone block index.
    /// Updated when `advanceTempo` is called.
    block_bindings: HashMap<u64, u64>,

    /// Tempo L1 state root currently bound for each zone block index.
    /// Used to verify that L1 reads are proven against the correct state root.
    state_root_bindings: HashMap<u64, B256>,
}

/// A pre-verified read entry.
#[derive(Debug, Clone)]
struct ReadEntry {
    /// Expected Tempo block number for this read.
    tempo_block_number: u64,
    /// Storage proof path through `verified_nodes` (storage root -> slot leaf).
    storage_path: Vec<B256>,
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

        // Phase 2: Index account proofs by (tempo_block_number, account).
        let mut account_proof_index = HashMap::default();
        for ap in &proofs.account_proofs {
            let key = AccountProofKey {
                tempo_block_number: ap.tempo_block_number,
                account: ap.account,
            };
            account_proof_index.insert(
                key,
                AccountProofEntry {
                    nonce: ap.nonce,
                    balance: ap.balance,
                    storage_root: ap.storage_root,
                    code_hash: ap.code_hash,
                    account_path: ap.account_path.clone(),
                },
            );
        }

        // Phase 3: Index reads for fast lookup.
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
                    storage_path: read.storage_path.clone(),
                    value: read.value,
                },
            );
        }

        Ok(Self {
            verified_nodes,
            account_proof_index,
            read_index,
            block_bindings: HashMap::default(),
            state_root_bindings: HashMap::default(),
        })
    }

    /// Bind a zone block index to a Tempo block number.
    ///
    /// Called before block execution to record which Tempo block
    /// is currently active for a given zone block.
    pub fn bind_block(
        &mut self,
        zone_block_index: u64,
        tempo_block_number: u64,
    ) {
        self.block_bindings
            .insert(zone_block_index, tempo_block_number);
    }

    /// Bind a zone block index to a Tempo L1 state root.
    ///
    /// Called alongside `bind_block` to record which `tempoStateRoot`
    /// is active for each zone block, enabling L1 proof verification.
    pub fn bind_state_root(
        &mut self,
        zone_block_index: u64,
        tempo_state_root: B256,
    ) {
        self.state_root_bindings
            .insert(zone_block_index, tempo_state_root);
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
        let &bound_block = self.block_bindings.get(&zone_block_index).ok_or(
            ProverError::InconsistentState(format!(
                "no block binding for zone_block_index={zone_block_index}"
            )),
        )?;
        if entry.tempo_block_number != bound_block {
            return Err(ProverError::InconsistentState(format!(
                "Tempo read at block_index={zone_block_index} expects tempo_block={}, \
                 but block is bound to tempo_block={bound_block}",
                entry.tempo_block_number,
            )));
        }

        // Verify the storage read against the L1 state root via MPT proofs.
        let &tempo_state_root = self.state_root_bindings.get(&zone_block_index).ok_or(
            ProverError::InconsistentState(format!(
                "no state root binding for zone_block_index={zone_block_index}"
            )),
        )?;

        let ap_key = AccountProofKey {
            tempo_block_number: entry.tempo_block_number,
            account,
        };
        let ap = self.account_proof_index.get(&ap_key).ok_or(
            ProverError::InvalidProof(format!(
                "no account proof for tempo_block={} account={account}",
                entry.tempo_block_number,
            )),
        )?;

        // Reconstitute account proof nodes from the verified pool.
        let account_proof_nodes = reconstitute_proof(
            &ap.account_path,
            &self.verified_nodes,
        ).map_err(|e| ProverError::InvalidProof(format!(
            "account proof reconstitution: {e}"
        )))?;

        // Verify account exists in L1 state trie.
        mpt::verify_account_proof(
            tempo_state_root,
            account,
            ap.nonce,
            ap.balance,
            ap.storage_root,
            ap.code_hash,
            &account_proof_nodes,
        )?;

        // Reconstitute storage proof nodes.
        let storage_proof_nodes = reconstitute_proof(
            &entry.storage_path,
            &self.verified_nodes,
        ).map_err(|e| ProverError::InvalidProof(format!(
            "storage proof reconstitution: {e}"
        )))?;

        // Verify the storage slot value.
        mpt::verify_storage_proof(
            ap.storage_root,
            slot,
            entry.value,
            &storage_proof_nodes,
        )?;

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
    let state_root_bindings = accessor.state_root_bindings.clone();
    let account_proof_index = accessor.account_proof_index.clone();
    let verified_nodes = accessor.verified_nodes.clone();

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
                handle_single_slot(
                    &read_index, &block_bindings, &state_root_bindings,
                    &account_proof_index, &verified_nodes, block_idx, data,
                )
            } else if selector == readStorageBatchAtCall::SELECTOR {
                handle_multi_slot(
                    &read_index, &block_bindings, &state_root_bindings,
                    &account_proof_index, &verified_nodes, block_idx, data,
                )
            } else {
                Ok(PrecompileOutput::new_reverted(0, Bytes::new()))
            }
        },
    )
}

fn handle_single_slot(
    read_index: &HashMap<ReadKey, ReadEntry>,
    block_bindings: &HashMap<u64, u64>,
    state_root_bindings: &HashMap<u64, B256>,
    account_proof_index: &HashMap<AccountProofKey, AccountProofEntry>,
    verified_nodes: &HashMap<B256, Vec<u8>>,
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

    // Validate block binding (must always be set before execution).
    let &bound_block = block_bindings.get(&block_idx).ok_or_else(|| {
        PrecompileError::other(format!("no block binding for block_idx={block_idx}"))
    })?;
    if entry.tempo_block_number != bound_block {
        return Err(PrecompileError::other(format!(
            "Tempo read block mismatch: expected={bound_block}, got={}",
            entry.tempo_block_number
        )));
    }

    // Verify the storage read against the L1 state root via MPT proofs.
    verify_read_proof(
        state_root_bindings,
        account_proof_index,
        verified_nodes,
        block_idx,
        entry,
        call.account,
        U256::from_be_bytes(call.slot.0),
    )?;

    let value = B256::from(entry.value.to_be_bytes());
    let encoded = readStorageAtCall::abi_encode_returns(&value);
    Ok(PrecompileOutput::new(gas, encoded.into()))
}

fn handle_multi_slot(
    read_index: &HashMap<ReadKey, ReadEntry>,
    block_bindings: &HashMap<u64, u64>,
    state_root_bindings: &HashMap<u64, B256>,
    account_proof_index: &HashMap<AccountProofKey, AccountProofEntry>,
    verified_nodes: &HashMap<B256, Vec<u8>>,
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

        let &bound_block = block_bindings.get(&block_idx).ok_or_else(|| {
            PrecompileError::other(format!("no block binding for block_idx={block_idx}"))
        })?;
        if entry.tempo_block_number != bound_block {
            return Err(PrecompileError::other(format!(
                "Tempo read block mismatch: expected={bound_block}, got={}",
                entry.tempo_block_number
            )));
        }

        verify_read_proof(
            state_root_bindings,
            account_proof_index,
            verified_nodes,
            block_idx,
            entry,
            call.account,
            U256::from_be_bytes(slot.0),
        )?;

        results.push(B256::from(entry.value.to_be_bytes()));
    }

    let encoded = readStorageBatchAtCall::abi_encode_returns(&results);
    Ok(PrecompileOutput::new(gas, encoded.into()))
}

/// Verify a single storage read against the L1 state root via MPT proofs.
///
/// Called from the precompile handlers after the block binding check passes.
fn verify_read_proof(
    state_root_bindings: &HashMap<u64, B256>,
    account_proof_index: &HashMap<AccountProofKey, AccountProofEntry>,
    verified_nodes: &HashMap<B256, Vec<u8>>,
    block_idx: u64,
    entry: &ReadEntry,
    account: Address,
    slot: U256,
) -> PrecompileResult {
    // State root binding must exist — it is set before block execution.
    let &tempo_state_root = state_root_bindings.get(&block_idx).ok_or_else(|| {
        PrecompileError::other(format!(
            "no state root binding for block_idx={block_idx}"
        ))
    })?;

    let ap_key = AccountProofKey {
        tempo_block_number: entry.tempo_block_number,
        account,
    };
    let ap = account_proof_index.get(&ap_key).ok_or_else(|| {
        PrecompileError::other(format!(
            "no account proof for tempo_block={} account={account}",
            entry.tempo_block_number,
        ))
    })?;

    // Reconstitute and verify account proof.
    let account_proof_nodes = reconstitute_proof(&ap.account_path, verified_nodes)
        .map_err(|e| PrecompileError::other(format!("account proof: {e}")))?;
    mpt::verify_account_proof(
        tempo_state_root, account,
        ap.nonce, ap.balance, ap.storage_root, ap.code_hash,
        &account_proof_nodes,
    ).map_err(|e| PrecompileError::other(format!("account proof invalid: {e}")))?;

    // Reconstitute and verify storage proof.
    let storage_proof_nodes = reconstitute_proof(&entry.storage_path, verified_nodes)
        .map_err(|e| PrecompileError::other(format!("storage proof: {e}")))?;
    mpt::verify_storage_proof(ap.storage_root, slot, entry.value, &storage_proof_nodes)
        .map_err(|e| PrecompileError::other(format!("storage proof invalid: {e}")))?;

    Ok(PrecompileOutput::new(0, Bytes::new()))
}

/// Reconstitute proof nodes from pool hashes.
///
/// Looks up each hash in the verified node pool and returns the raw RLP
/// bytes as `Bytes` suitable for `verify_proof`.
fn reconstitute_proof(
    path: &[B256],
    verified_nodes: &HashMap<B256, Vec<u8>>,
) -> Result<Vec<Bytes>, String> {
    path.iter()
        .map(|hash| {
            verified_nodes
                .get(hash)
                .map(|rlp| Bytes::copy_from_slice(rlp))
                .ok_or_else(|| format!("node {hash} not in verified pool"))
        })
        .collect()
}
