//! Proof-based Tempo state accessor.
//!
//! Replaces the RPC-based [`TempoStateReader`] precompile with a proof-backed
//! accessor that verifies Tempo L1 storage reads against the deduplicated MPT
//! node pool in [`BatchStateProof`].

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, B256, Bytes, U256, map::HashMap};
use alloy_sol_types::{SolCall, SolError};
use alloy_trie::EMPTY_ROOT_HASH;
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
            let candidate = AccountProofEntry {
                nonce: ap.nonce,
                balance: ap.balance,
                storage_root: ap.storage_root,
                code_hash: ap.code_hash,
                account_path: ap.account_path.clone(),
            };
            if let Some(existing) = account_proof_index.get(&key) {
                if existing != &candidate {
                    return Err(ProverError::InvalidProof(format!(
                        "conflicting duplicate account proof for tempo_block={} account={}",
                        ap.tempo_block_number, ap.account
                    )));
                }
            } else {
                account_proof_index.insert(key, candidate);
            }
        }

        // Phase 3: Index reads for fast lookup.
        let mut read_index = HashMap::default();
        for read in &proofs.reads {
            let key = ReadKey {
                zone_block_index: read.zone_block_index,
                account: read.account,
                slot: read.slot,
            };
            let candidate = ReadEntry {
                tempo_block_number: read.tempo_block_number,
                storage_path: read.storage_path.clone(),
                value: read.value,
            };
            if let Some(existing) = read_index.get(&key) {
                if existing != &candidate {
                    return Err(ProverError::InvalidProof(format!(
                        "conflicting duplicate read for zone_block_index={} account={} slot={}",
                        read.zone_block_index, read.account, read.slot
                    )));
                }
            } else {
                read_index.insert(key, candidate);
            }
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
    pub fn bind_block(&mut self, zone_block_index: u64, tempo_block_number: u64) {
        self.block_bindings
            .insert(zone_block_index, tempo_block_number);
    }

    /// Bind a zone block index to a Tempo L1 state root.
    ///
    /// Called alongside `bind_block` to record which `tempoStateRoot`
    /// is active for each zone block, enabling L1 proof verification.
    pub fn bind_state_root(&mut self, zone_block_index: u64, tempo_state_root: B256) {
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

        let entry = self
            .read_index
            .get(&key)
            .ok_or(ProverError::TempoReadNotFound {
                block_index: zone_block_index,
                account,
                slot,
            })?;

        // Verify the read is bound to the correct Tempo block.
        let &bound_block =
            self.block_bindings
                .get(&zone_block_index)
                .ok_or(ProverError::InconsistentState(format!(
                    "no block binding for zone_block_index={zone_block_index}"
                )))?;
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
        let ap = self
            .account_proof_index
            .get(&ap_key)
            .ok_or(ProverError::InvalidProof(format!(
                "no account proof for tempo_block={} account={account}",
                entry.tempo_block_number,
            )))?;

        // Reconstitute account proof nodes from the verified pool.
        let account_proof_nodes = reconstitute_proof(&ap.account_path, &self.verified_nodes)
            .map_err(|e| ProverError::InvalidProof(format!("account proof reconstitution: {e}")))?;

        // Verify account proof: inclusion (normal) or absence (account not present).
        // Absent-account reads are valid in eth_getStorageAt semantics and must
        // return zero.
        let storage_root = match mpt::verify_account_proof(
            tempo_state_root,
            account,
            ap.nonce,
            ap.balance,
            ap.storage_root,
            ap.code_hash,
            &account_proof_nodes,
        ) {
            Ok(()) => ap.storage_root,
            Err(inclusion_err) => {
                mpt::verify_account_absence_proof(tempo_state_root, account, &account_proof_nodes)
                    .map_err(|absence_err| {
                        ProverError::InvalidProof(format!(
                            "account proof invalid for account={account} tempo_block={}: \
                         inclusion failed ({inclusion_err}); absence failed ({absence_err})",
                            entry.tempo_block_number
                        ))
                    })?;

                if !entry.value.is_zero() {
                    return Err(ProverError::InvalidProof(format!(
                        "absent account {account} at tempo_block={} must return zero, got {}",
                        entry.tempo_block_number, entry.value
                    )));
                }

                EMPTY_ROOT_HASH
            }
        };

        // Reconstitute storage proof nodes.
        let storage_proof_nodes = reconstitute_proof(&entry.storage_path, &self.verified_nodes)
            .map_err(|e| ProverError::InvalidProof(format!("storage proof reconstitution: {e}")))?;

        // Verify the storage slot value.
        mpt::verify_storage_proof(storage_root, slot, entry.value, &storage_proof_nodes)?;

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
                    &read_index,
                    &block_bindings,
                    &state_root_bindings,
                    &account_proof_index,
                    &verified_nodes,
                    block_idx,
                    data,
                )
            } else if selector == readStorageBatchAtCall::SELECTOR {
                handle_multi_slot(
                    &read_index,
                    &block_bindings,
                    &state_root_bindings,
                    &account_proof_index,
                    &verified_nodes,
                    block_idx,
                    data,
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

    let entry = read_index.get(&key).ok_or_else(|| {
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

        let entry = read_index.get(&key).ok_or_else(|| {
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
        PrecompileError::other(format!("no state root binding for block_idx={block_idx}"))
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

    // Reconstitute and verify account proof (inclusion or absence).
    let account_proof_nodes = reconstitute_proof(&ap.account_path, verified_nodes)
        .map_err(|e| PrecompileError::other(format!("account proof: {e}")))?;
    let storage_root = match mpt::verify_account_proof(
        tempo_state_root,
        account,
        ap.nonce,
        ap.balance,
        ap.storage_root,
        ap.code_hash,
        &account_proof_nodes,
    ) {
        Ok(()) => ap.storage_root,
        Err(inclusion_err) => {
            mpt::verify_account_absence_proof(tempo_state_root, account, &account_proof_nodes)
                .map_err(|absence_err| {
                    PrecompileError::other(format!(
                        "account proof invalid: inclusion failed ({inclusion_err}); \
                         absence failed ({absence_err})"
                    ))
                })?;
            if !entry.value.is_zero() {
                return Err(PrecompileError::other(format!(
                    "absent account {account} must return zero, got {}",
                    entry.value
                )));
            }
            EMPTY_ROOT_HASH
        }
    };

    // Reconstitute and verify storage proof.
    let storage_proof_nodes = reconstitute_proof(&entry.storage_path, verified_nodes)
        .map_err(|e| PrecompileError::other(format!("storage proof: {e}")))?;
    mpt::verify_storage_proof(storage_root, slot, entry.value, &storage_proof_nodes)
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

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, KECCAK256_EMPTY, U256, keccak256, map::HashMap};
    use alloy_trie::{EMPTY_ROOT_HASH, HashBuilder, TrieAccount, proof::ProofRetainer};
    use nybbles::Nibbles;

    use super::TempoStateAccessor;
    use crate::types::{BatchStateProof, L1AccountProof, L1StateRead, ProverError};

    fn build_absent_account_batch_proof(
        absent_account: Address,
        slot: U256,
        tempo_block_number: u64,
        zone_block_index: u64,
    ) -> (B256, BatchStateProof) {
        let existing = Address::with_last_byte(0x01);
        let existing_key = Nibbles::unpack(keccak256(existing));
        let absent_key = Nibbles::unpack(keccak256(absent_account));

        let existing_account = TrieAccount {
            nonce: 1,
            balance: U256::from(1),
            storage_root: EMPTY_ROOT_HASH,
            code_hash: KECCAK256_EMPTY,
        };
        let mut encoded = Vec::new();
        alloy_rlp::Encodable::encode(&existing_account, &mut encoded);

        let retainer = ProofRetainer::new(vec![existing_key, absent_key]);
        let mut builder = HashBuilder::default().with_proof_retainer(retainer);
        builder.add_leaf(existing_key, &encoded);

        let state_root = builder.root();
        let proof_nodes = builder.take_proof_nodes();

        let account_proof_nodes: Vec<alloy_primitives::Bytes> = proof_nodes
            .matching_nodes_sorted(&absent_key)
            .into_iter()
            .map(|(_, b)| b)
            .collect();

        let mut node_pool = HashMap::default();
        let mut account_path = Vec::new();
        for node in &account_proof_nodes {
            let hash = keccak256(node.as_ref());
            node_pool.insert(hash, node.to_vec());
            account_path.push(hash);
        }

        let batch_proof = BatchStateProof {
            node_pool,
            reads: vec![L1StateRead {
                zone_block_index,
                tempo_block_number,
                account: absent_account,
                slot,
                storage_path: vec![],
                value: U256::ZERO,
            }],
            account_proofs: vec![L1AccountProof {
                tempo_block_number,
                account: absent_account,
                nonce: 0,
                balance: U256::ZERO,
                storage_root: EMPTY_ROOT_HASH,
                code_hash: KECCAK256_EMPTY,
                account_path,
            }],
        };

        (state_root, batch_proof)
    }

    #[test]
    fn test_absent_account_read_returns_zero() {
        let absent = Address::with_last_byte(0xAA);
        let slot = U256::from(4);
        let tempo_block_number = 123u64;
        let zone_block_index = 0u64;

        let (state_root, proof) =
            build_absent_account_batch_proof(absent, slot, tempo_block_number, zone_block_index);

        let mut accessor = TempoStateAccessor::from_proofs(&proof).expect("proofs should index");
        accessor.bind_block(zone_block_index, tempo_block_number);
        accessor.bind_state_root(zone_block_index, state_root);

        let value = accessor
            .read_storage(zone_block_index, absent, slot)
            .expect("absent account read should verify as zero");
        assert_eq!(value, U256::ZERO);
    }

    #[test]
    fn test_absent_account_non_zero_read_is_rejected() {
        let absent = Address::with_last_byte(0xAB);
        let slot = U256::from(4);
        let tempo_block_number = 456u64;
        let zone_block_index = 0u64;

        let (state_root, mut proof) =
            build_absent_account_batch_proof(absent, slot, tempo_block_number, zone_block_index);
        proof.reads[0].value = U256::from(1);

        let mut accessor = TempoStateAccessor::from_proofs(&proof).expect("proofs should index");
        accessor.bind_block(zone_block_index, tempo_block_number);
        accessor.bind_state_root(zone_block_index, state_root);

        let err = accessor
            .read_storage(zone_block_index, absent, slot)
            .expect_err("non-zero read for absent account must fail");
        assert!(matches!(err, ProverError::InvalidProof(_)));
    }

    #[test]
    fn test_conflicting_duplicate_account_proof_rejected() {
        let account = Address::with_last_byte(0x11);
        let proof = BatchStateProof {
            node_pool: HashMap::default(),
            reads: vec![],
            account_proofs: vec![
                L1AccountProof {
                    tempo_block_number: 1,
                    account,
                    nonce: 0,
                    balance: U256::ZERO,
                    storage_root: EMPTY_ROOT_HASH,
                    code_hash: KECCAK256_EMPTY,
                    account_path: vec![],
                },
                L1AccountProof {
                    tempo_block_number: 1,
                    account,
                    nonce: 1,
                    balance: U256::ZERO,
                    storage_root: EMPTY_ROOT_HASH,
                    code_hash: KECCAK256_EMPTY,
                    account_path: vec![],
                },
            ],
        };

        let err = TempoStateAccessor::from_proofs(&proof)
            .err()
            .expect("conflicting duplicate account proofs must fail");
        assert!(matches!(err, ProverError::InvalidProof(_)));
    }

    #[test]
    fn test_conflicting_duplicate_read_rejected() {
        let account = Address::with_last_byte(0x12);
        let slot = U256::from(7);
        let proof = BatchStateProof {
            node_pool: HashMap::default(),
            reads: vec![
                L1StateRead {
                    zone_block_index: 0,
                    tempo_block_number: 1,
                    account,
                    slot,
                    storage_path: vec![],
                    value: U256::ZERO,
                },
                L1StateRead {
                    zone_block_index: 0,
                    tempo_block_number: 2,
                    account,
                    slot,
                    storage_path: vec![],
                    value: U256::from(1),
                },
            ],
            account_proofs: vec![],
        };

        let err = TempoStateAccessor::from_proofs(&proof)
            .err()
            .expect("conflicting duplicate reads must fail");
        assert!(matches!(err, ProverError::InvalidProof(_)));
    }
}
