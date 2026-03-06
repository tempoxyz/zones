//! Sparse Merkle Patricia Trie for stateless state root computation.
//!
//! Builds a sparse trie from MPT proofs, applies mutations from EVM execution,
//! and recomputes the trie root. This enables the prover to verify state roots
//! without access to the full state trie.
//!
//! # Algorithm
//!
//! 1. **Build** — Walk each proof (root → leaf), decoding trie nodes. At branch
//!    nodes, non-target children are recorded as *blinded branches* (known only
//!    by their hash). Leaves record the full key and value.
//!
//! 2. **Mutate** — Update, insert, or remove leaf entries. Blinded branches
//!    remain unchanged (by construction, the witness includes proofs for all
//!    keys that will be mutated, so mutations never fall into blinded subtrees).
//!
//! 3. **Root** — Feed all entries (leaves + blinded branches) to [`HashBuilder`]
//!    in sorted order. The HashBuilder reconstructs the trie structure and
//!    computes the Merkle root.

use std::collections::BTreeMap;

use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_rlp::Decodable;
use alloy_trie::{
    EMPTY_ROOT_HASH, HashBuilder,
    nodes::{RlpNode, TrieNode},
};
use nybbles::Nibbles;

use crate::types::{AccountWitness, ProverError, ZoneStateWitness};

// ---------------------------------------------------------------------------
//  SparseTrie
// ---------------------------------------------------------------------------

/// A sparse trie built from MPT proofs.
///
/// Contains only the trie entries (leaves and blinded branch hashes) visible
/// from the provided proofs. Supports mutation and root recomputation.
pub struct SparseTrie {
    /// Entries sorted by nibble path. Leaves have full 64-nibble keys;
    /// blinded branches have shorter keys representing subtree prefixes.
    entries: BTreeMap<Nibbles, TrieEntry>,
}

/// An entry in the sparse trie.
#[derive(Clone, Debug)]
enum TrieEntry {
    /// A leaf node with its RLP-encoded value.
    Leaf(Vec<u8>),
    /// A blinded subtree whose hash is known but contents are not expanded.
    Branch(B256),
}

impl SparseTrie {
    /// Build a sparse trie from a set of MPT proofs.
    ///
    /// Each proof is `(target_key, proof_nodes)` where `target_key` is the
    /// nibble path being proven and `proof_nodes` are the RLP-encoded trie
    /// nodes from root to target.
    ///
    /// Both inclusion proofs (target exists) and exclusion proofs (target
    /// doesn't exist) contribute to the sparse trie structure.
    pub fn from_proofs(proofs: &[(Nibbles, &[Bytes])]) -> Result<Self, ProverError> {
        let mut entries = BTreeMap::new();

        for (target, proof_nodes) in proofs {
            walk_proof(target, proof_nodes, &mut entries)?;
        }

        // Cleanup: remove Branch entries whose keys are proper prefixes of other
        // entries. This happens when one proof expands a subtree that another
        // proof recorded as a blinded branch (because it was a non-target child
        // in the branch node).
        let branch_keys: Vec<Nibbles> = entries
            .iter()
            .filter(|&(_, v)| matches!(v, TrieEntry::Branch(_))).map(|(k, _)| *k)
            .collect();
        for bk in branch_keys {
            let has_descendant = entries
                .range(bk..).nth(1)
                .is_some_and(|(next_key, _)| {
                    next_key.len() > bk.len() && bk.common_prefix_length(next_key) == bk.len()
                });
            if has_descendant {
                entries.remove(&bk);
            }
        }

        Ok(Self { entries })
    }

    /// Create an empty sparse trie (represents `EMPTY_ROOT_HASH`).
    pub fn empty() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Update or insert a leaf at the given full key.
    ///
    /// The `value` is the RLP-encoded leaf payload (e.g., RLP(U256) for storage,
    /// RLP(TrieAccount) for accounts).
    pub fn update_leaf(&mut self, key: Nibbles, value: Vec<u8>) {
        self.entries.insert(key, TrieEntry::Leaf(value));
    }

    /// Remove a leaf at the given full key.
    ///
    /// Used when a storage slot becomes zero or an account is destroyed.
    pub fn remove_leaf(&mut self, key: &Nibbles) {
        self.entries.remove(key);
    }

    /// Compute the current trie root from all entries.
    pub fn root(&self) -> B256 {
        if self.entries.is_empty() {
            return EMPTY_ROOT_HASH;
        }

        let mut hb = HashBuilder::default();

        for (key, entry) in &self.entries {
            match entry {
                TrieEntry::Leaf(value) => hb.add_leaf(*key, value),
                TrieEntry::Branch(hash) => hb.add_branch(*key, *hash, false),
            }
        }

        hb.root()
    }
}

// ---------------------------------------------------------------------------
//  Proof walking
// ---------------------------------------------------------------------------

/// Walk a single proof and extract entries into the sparse trie.
///
/// For each branch node along the path, non-target children are recorded as
/// blinded branches. The terminal leaf (if any) is recorded directly.
fn walk_proof(
    target: &Nibbles,
    proof_nodes: &[Bytes],
    entries: &mut BTreeMap<Nibbles, TrieEntry>,
) -> Result<(), ProverError> {
    let mut path = Nibbles::default();

    for node_rlp in proof_nodes {
        let node = TrieNode::decode(&mut &node_rlp[..])
            .map_err(|e| ProverError::RlpDecode(format!("proof node decode: {e}")))?;

        match node {
            TrieNode::EmptyRoot => break,

            TrieNode::Branch(branch) => {
                if path.len() >= target.len() {
                    break;
                }
                let target_nibble = target.get_unchecked(path.len());

                // Record non-path children as blinded branches.
                let mut stack_idx = 0;
                for nibble in 0..16u8 {
                    if branch.state_mask.is_bit_set(nibble) {
                        let child = &branch.stack[stack_idx];
                        stack_idx += 1;

                        if nibble != target_nibble {
                            let mut child_path = path;
                            child_path.push(nibble);
                            record_child(child, child_path, entries)?;
                        }
                    }
                }

                // Follow the target child if it exists.
                if branch.state_mask.is_bit_set(target_nibble) {
                    path.push(target_nibble);
                } else {
                    // Exclusion proof: target nibble slot is empty.
                    break;
                }
            }

            TrieNode::Extension(ext) => {
                let ext_len = ext.key.len();
                let remaining = target.len().saturating_sub(path.len());

                // Check if the extension key matches the target path.
                let ext_matches = remaining >= ext_len && {
                    let target_slice = target.slice(path.len()..path.len() + ext_len);
                    target_slice == ext.key
                };

                if ext_matches {
                    // Extension matches — follow it.
                    path.extend(&ext.key);
                } else {
                    // Extension diverges — record child as blinded branch.
                    let mut child_path = path;
                    child_path.extend(&ext.key);
                    record_child(&ext.child, child_path, entries)?;
                    break;
                }
            }

            TrieNode::Leaf(leaf) => {
                let mut full_key = path;
                full_key.extend(&leaf.key);
                entries.insert(full_key, TrieEntry::Leaf(leaf.value));
            }
        }
    }

    Ok(())
}

/// Record a child node — either as a blinded branch hash or by decoding
/// inline node data recursively.
fn record_child(
    child: &RlpNode,
    path: Nibbles,
    entries: &mut BTreeMap<Nibbles, TrieEntry>,
) -> Result<(), ProverError> {
    if let Some(hash) = child.as_hash() {
        entries.insert(path, TrieEntry::Branch(hash));
    } else {
        // Inline node — decode and process recursively.
        process_inline_node(child.as_slice(), path, entries)?;
    }
    Ok(())
}

/// Recursively process an inline trie node, extracting entries.
///
/// Inline nodes appear when a child's RLP is < 32 bytes (too short to be
/// stored by hash). They are decoded and their contents extracted.
fn process_inline_node(
    rlp: &[u8],
    path: Nibbles,
    entries: &mut BTreeMap<Nibbles, TrieEntry>,
) -> Result<(), ProverError> {
    let node = TrieNode::decode(&mut &rlp[..])
        .map_err(|e| ProverError::RlpDecode(format!("inline node decode: {e}")))?;

    match node {
        TrieNode::EmptyRoot => {}
        TrieNode::Leaf(leaf) => {
            let mut full_key = path;
            full_key.extend(&leaf.key);
            entries.insert(full_key, TrieEntry::Leaf(leaf.value));
        }
        TrieNode::Extension(ext) => {
            let mut child_path = path;
            child_path.extend(&ext.key);
            record_child(&ext.child, child_path, entries)?;
        }
        TrieNode::Branch(branch) => {
            let mut stack_idx = 0;
            for nibble in 0..16u8 {
                if branch.state_mask.is_bit_set(nibble) {
                    let child = &branch.stack[stack_idx];
                    stack_idx += 1;
                    let mut child_path = path;
                    child_path.push(nibble);
                    record_child(child, child_path, entries)?;
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
//  Builder functions
// ---------------------------------------------------------------------------

/// Build the initial account state trie from all proofs in the witness.
///
/// Includes both existing account proofs and absence proofs for non-existent
/// accounts. The resulting sparse trie covers all paths that may be mutated
/// during batch execution.
pub fn build_state_trie(witness: &ZoneStateWitness) -> Result<SparseTrie, ProverError> {
    let mut proofs: Vec<(Nibbles, &[Bytes])> = Vec::new();

    // Existing account proofs.
    for (addr, acct) in &witness.accounts {
        let key = Nibbles::unpack(keccak256(*addr));
        proofs.push((key, acct.account_proof.as_slice()));
    }

    // Absence proofs for confirmed-absent accounts.
    for (addr, proof) in &witness.absent_accounts {
        let key = Nibbles::unpack(keccak256(*addr));
        proofs.push((key, proof.as_slice()));
    }

    SparseTrie::from_proofs(&proofs)
}

/// Build the initial storage trie for a single account from its storage proofs.
pub fn build_storage_trie(account: &AccountWitness) -> Result<SparseTrie, ProverError> {
    let mut proofs: Vec<(Nibbles, &[Bytes])> = Vec::new();

    for (slot, proof) in &account.storage_proofs {
        let key = Nibbles::unpack(keccak256(B256::from(slot.to_be_bytes())));
        proofs.push((key, proof.as_slice()));
    }

    SparseTrie::from_proofs(&proofs)
}

/// Compute the storage trie nibble key for a slot.
pub fn storage_key(slot: alloy_primitives::U256) -> Nibbles {
    Nibbles::unpack(keccak256(B256::from(slot.to_be_bytes())))
}

/// Compute the state trie nibble key for an address.
pub fn account_key(address: Address) -> Nibbles {
    Nibbles::unpack(keccak256(address))
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, Bytes, U256, address};
    use alloy_trie::{EMPTY_ROOT_HASH, HashBuilder, TrieAccount, proof::ProofRetainer};

    use super::*;
    use crate::types::{AccountWitness, ZoneStateWitness};

    // -------------------------------------------------------------------
    //  Test helpers
    // -------------------------------------------------------------------

    /// Build a storage trie from slot→value pairs and return the root,
    /// per-slot proofs, and the full map of RLP-encoded values.
    ///
    /// Proofs are in the `eth_getProof` format: ordered root-to-leaf RLP nodes.
    fn build_storage_trie_with_proofs(
        entries: &[(U256, U256)],
    ) -> (B256, std::collections::HashMap<U256, Vec<Bytes>>) {
        if entries.is_empty() {
            return (EMPTY_ROOT_HASH, Default::default());
        }

        // Sort entries by their trie key (keccak of slot as B256).
        let mut sorted: Vec<(Nibbles, U256, Vec<u8>)> = entries
            .iter()
            .filter(|(_, v)| !v.is_zero())
            .map(|(slot, value)| {
                let key = Nibbles::unpack(keccak256(B256::from(slot.to_be_bytes())));
                let mut enc = Vec::new();
                alloy_rlp::Encodable::encode(value, &mut enc);
                (key, *slot, enc)
            })
            .collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));

        if sorted.is_empty() {
            return (EMPTY_ROOT_HASH, Default::default());
        }

        // Collect target nibble keys for the ProofRetainer.
        let targets: Vec<Nibbles> = sorted.iter().map(|(k, _, _)| k.clone()).collect();
        let retainer = ProofRetainer::new(targets.clone());
        let mut builder = HashBuilder::default().with_proof_retainer(retainer);

        for (key, _, value) in &sorted {
            builder.add_leaf(key.clone(), value);
        }

        let root = builder.root();
        let proof_nodes = builder.take_proof_nodes();

        // Extract per-slot proofs.
        let mut slot_proofs = std::collections::HashMap::new();
        for (key, slot, _) in &sorted {
            let nodes: Vec<Bytes> = proof_nodes
                .matching_nodes_sorted(key)
                .into_iter()
                .map(|(_, b)| b)
                .collect();
            slot_proofs.insert(*slot, nodes);
        }

        (root, slot_proofs)
    }

    /// Build a state trie from address→(TrieAccount) pairs and return the root
    /// and per-address proofs.
    fn build_state_trie_with_proofs(
        accounts: &[(alloy_primitives::Address, TrieAccount)],
    ) -> (
        B256,
        std::collections::HashMap<alloy_primitives::Address, Vec<Bytes>>,
    ) {
        // Sort entries by trie key.
        let mut sorted: Vec<(Nibbles, alloy_primitives::Address, Vec<u8>)> = accounts
            .iter()
            .map(|(addr, acct)| {
                let key = Nibbles::unpack(keccak256(*addr));
                let mut enc = Vec::new();
                alloy_rlp::Encodable::encode(acct, &mut enc);
                (key, *addr, enc)
            })
            .collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));

        let targets: Vec<Nibbles> = sorted.iter().map(|(k, _, _)| k.clone()).collect();
        let retainer = ProofRetainer::new(targets);
        let mut builder = HashBuilder::default().with_proof_retainer(retainer);

        for (key, _, value) in &sorted {
            builder.add_leaf(key.clone(), value);
        }

        let root = builder.root();
        let proof_nodes = builder.take_proof_nodes();

        let mut addr_proofs = std::collections::HashMap::new();
        for (key, addr, _) in &sorted {
            let nodes: Vec<Bytes> = proof_nodes
                .matching_nodes_sorted(key)
                .into_iter()
                .map(|(_, b)| b)
                .collect();
            addr_proofs.insert(*addr, nodes);
        }

        (root, addr_proofs)
    }

    /// Generate an absence proof for an address against a state trie.
    ///
    /// Builds the trie from existing accounts and uses ProofRetainer to capture
    /// the proof path for the absent address.
    fn build_state_trie_with_absence_proof(
        accounts: &[(alloy_primitives::Address, TrieAccount)],
        absent_addr: alloy_primitives::Address,
    ) -> (
        B256,
        Vec<Bytes>,
        std::collections::HashMap<alloy_primitives::Address, Vec<Bytes>>,
    ) {
        let mut sorted: Vec<(Nibbles, alloy_primitives::Address, Vec<u8>)> = accounts
            .iter()
            .map(|(addr, acct)| {
                let key = Nibbles::unpack(keccak256(*addr));
                let mut enc = Vec::new();
                alloy_rlp::Encodable::encode(acct, &mut enc);
                (key, *addr, enc)
            })
            .collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));

        // Targets include both existing accounts and the absent one.
        let absent_key = Nibbles::unpack(keccak256(absent_addr));
        let mut targets: Vec<Nibbles> = sorted.iter().map(|(k, _, _)| k.clone()).collect();
        targets.push(absent_key.clone());

        let retainer = ProofRetainer::new(targets);
        let mut builder = HashBuilder::default().with_proof_retainer(retainer);

        for (key, _, value) in &sorted {
            builder.add_leaf(key.clone(), value);
        }

        let root = builder.root();
        let proof_nodes = builder.take_proof_nodes();

        // Extract existing account proofs.
        let mut addr_proofs = std::collections::HashMap::new();
        for (key, addr, _) in &sorted {
            let nodes: Vec<Bytes> = proof_nodes
                .matching_nodes_sorted(key)
                .into_iter()
                .map(|(_, b)| b)
                .collect();
            addr_proofs.insert(*addr, nodes);
        }

        // Extract absence proof for the absent address.
        let absence_proof: Vec<Bytes> = proof_nodes
            .matching_nodes_sorted(&absent_key)
            .into_iter()
            .map(|(_, b)| b)
            .collect();

        (root, absence_proof, addr_proofs)
    }

    // -------------------------------------------------------------------
    //  Existing unit tests
    // -------------------------------------------------------------------

    #[test]
    fn test_empty_trie_root() {
        let trie = SparseTrie::empty();
        assert_eq!(trie.root(), EMPTY_ROOT_HASH);
    }

    #[test]
    fn test_single_leaf_from_empty() {
        let mut trie = SparseTrie::empty();

        let slot = U256::from(1);
        let key = storage_key(slot);
        let mut encoded = Vec::new();
        alloy_rlp::Encodable::encode(&slot, &mut encoded);
        trie.update_leaf(key.clone(), encoded);

        let root = trie.root();
        assert_ne!(root, EMPTY_ROOT_HASH);
        assert_ne!(root, B256::ZERO);

        trie.remove_leaf(&key);
        assert_eq!(trie.root(), EMPTY_ROOT_HASH);
    }

    #[test]
    fn test_multiple_leaves_consistent_root() {
        let mut trie1 = SparseTrie::empty();
        let mut trie2 = SparseTrie::empty();

        for i in 0u64..5 {
            let slot = U256::from(i);
            let key = storage_key(slot);
            let mut encoded = Vec::new();
            alloy_rlp::Encodable::encode(&U256::from(i * 100), &mut encoded);
            trie1.update_leaf(key.clone(), encoded.clone());
            trie2.update_leaf(key, encoded);
        }

        assert_eq!(trie1.root(), trie2.root());
    }

    // -------------------------------------------------------------------
    //  Proof round-trip integration tests
    // -------------------------------------------------------------------

    /// Verify that a storage trie built from proofs produces the same root as
    /// the original trie (and that the proofs pass mpt::verify_storage_proof).
    #[test]
    fn test_storage_trie_proof_roundtrip() {
        let entries = vec![
            (U256::from(0), U256::from(100)),
            (U256::from(1), U256::from(200)),
            (U256::from(5), U256::from(500)),
            (U256::from(42), U256::from(9999)),
        ];

        let (storage_root, slot_proofs) = build_storage_trie_with_proofs(&entries);
        assert_ne!(storage_root, EMPTY_ROOT_HASH);

        // Verify each proof against the storage root.
        for (slot, value) in &entries {
            let proof = slot_proofs.get(slot).expect("proof exists for slot");
            crate::mpt::verify_storage_proof(storage_root, *slot, *value, proof)
                .expect("storage proof should verify");
        }

        // Build sparse trie from proofs and verify root matches.
        let mut proofs: Vec<(Nibbles, &[Bytes])> = Vec::new();
        for (slot, proof) in &slot_proofs {
            let key = Nibbles::unpack(keccak256(B256::from(slot.to_be_bytes())));
            proofs.push((key, proof.as_slice()));
        }
        let sparse = SparseTrie::from_proofs(&proofs).expect("from_proofs should succeed");
        assert_eq!(
            sparse.root(),
            storage_root,
            "sparse trie root must match original"
        );
    }

    /// Verify that a state trie built from proofs produces the same root.
    #[test]
    fn test_state_trie_proof_roundtrip() {
        let addr_a = address!("0x1111111111111111111111111111111111111111");
        let addr_b = address!("0x2222222222222222222222222222222222222222");
        let addr_c = address!("0x3333333333333333333333333333333333333333");

        let (storage_root_a, _) = build_storage_trie_with_proofs(&[
            (U256::from(0), U256::from(100)),
            (U256::from(1), U256::from(200)),
        ]);
        let (storage_root_b, _) =
            build_storage_trie_with_proofs(&[(U256::from(5), U256::from(500))]);

        let accounts = vec![
            (
                addr_a,
                TrieAccount {
                    nonce: 1,
                    balance: U256::from(1000),
                    storage_root: storage_root_a,
                    code_hash: alloy_primitives::KECCAK256_EMPTY,
                },
            ),
            (
                addr_b,
                TrieAccount {
                    nonce: 0,
                    balance: U256::from(2000),
                    storage_root: storage_root_b,
                    code_hash: alloy_primitives::KECCAK256_EMPTY,
                },
            ),
            (
                addr_c,
                TrieAccount {
                    nonce: 5,
                    balance: U256::ZERO,
                    storage_root: EMPTY_ROOT_HASH,
                    code_hash: alloy_primitives::KECCAK256_EMPTY,
                },
            ),
        ];

        let (state_root, addr_proofs) = build_state_trie_with_proofs(&accounts);
        assert_ne!(state_root, EMPTY_ROOT_HASH);

        // Verify each account proof.
        for (addr, acct) in &accounts {
            let proof = addr_proofs.get(addr).expect("proof exists");
            crate::mpt::verify_account_proof(
                state_root,
                *addr,
                acct.nonce,
                acct.balance,
                acct.storage_root,
                acct.code_hash,
                proof,
            )
            .expect("account proof should verify");
        }

        // Build sparse trie from proofs and verify root matches.
        let mut proofs: Vec<(Nibbles, &[Bytes])> = Vec::new();
        for (addr, proof) in &addr_proofs {
            let key = Nibbles::unpack(keccak256(*addr));
            proofs.push((key, proof.as_slice()));
        }
        let sparse = SparseTrie::from_proofs(&proofs).expect("from_proofs should succeed");
        assert_eq!(
            sparse.root(),
            state_root,
            "sparse state trie root must match"
        );
    }

    /// Verify that mutating the sparse trie (update balance, remove account)
    /// produces the same root as a freshly-built trie with those changes.
    #[test]
    fn test_sparse_trie_mutation_matches_fresh_build() {
        let addr_a = address!("0x1111111111111111111111111111111111111111");
        let addr_b = address!("0x2222222222222222222222222222222222222222");
        let addr_c = address!("0x3333333333333333333333333333333333333333");

        let acct_a = TrieAccount {
            nonce: 1,
            balance: U256::from(1000),
            storage_root: EMPTY_ROOT_HASH,
            code_hash: alloy_primitives::KECCAK256_EMPTY,
        };
        let acct_b = TrieAccount {
            nonce: 0,
            balance: U256::from(2000),
            storage_root: EMPTY_ROOT_HASH,
            code_hash: alloy_primitives::KECCAK256_EMPTY,
        };
        let acct_c = TrieAccount {
            nonce: 5,
            balance: U256::ZERO,
            storage_root: EMPTY_ROOT_HASH,
            code_hash: alloy_primitives::KECCAK256_EMPTY,
        };

        let initial = vec![(addr_a, acct_a), (addr_b, acct_b), (addr_c, acct_c)];

        let (_initial_root, addr_proofs) = build_state_trie_with_proofs(&initial);

        // Build sparse trie from proofs.
        let mut proofs: Vec<(Nibbles, &[Bytes])> = Vec::new();
        for (addr, proof) in &addr_proofs {
            let key = Nibbles::unpack(keccak256(*addr));
            proofs.push((key, proof.as_slice()));
        }
        let mut sparse = SparseTrie::from_proofs(&proofs).expect("from_proofs should succeed");

        // Mutation 1: update addr_a's balance (1000 → 5000).
        let modified_a = TrieAccount {
            nonce: 1,
            balance: U256::from(5000),
            storage_root: EMPTY_ROOT_HASH,
            code_hash: alloy_primitives::KECCAK256_EMPTY,
        };
        let key_a = account_key(addr_a);
        let mut enc = Vec::new();
        alloy_rlp::Encodable::encode(&modified_a, &mut enc);
        sparse.update_leaf(key_a, enc);

        // Mutation 2: remove addr_c.
        let key_c = account_key(addr_c);
        sparse.remove_leaf(&key_c);

        // Build a fresh trie with the expected state: modified addr_a + addr_b.
        let expected = vec![(addr_a, modified_a), (addr_b, acct_b)];
        let (expected_root, _) = build_state_trie_with_proofs(&expected);

        assert_eq!(
            sparse.root(),
            expected_root,
            "mutated sparse root must match fresh build"
        );
    }

    /// Verify that storage trie mutations (add slot, remove slot, update slot)
    /// produce the correct new root.
    #[test]
    fn test_storage_trie_mutation_roundtrip() {
        let initial_entries = vec![
            (U256::from(0), U256::from(100)),
            (U256::from(1), U256::from(200)),
            (U256::from(5), U256::from(500)),
        ];

        let (initial_root, slot_proofs) = build_storage_trie_with_proofs(&initial_entries);

        // Build sparse trie from proofs.
        let mut proofs: Vec<(Nibbles, &[Bytes])> = Vec::new();
        for (slot, proof) in &slot_proofs {
            let key = Nibbles::unpack(keccak256(B256::from(slot.to_be_bytes())));
            proofs.push((key, proof.as_slice()));
        }
        let mut sparse = SparseTrie::from_proofs(&proofs).expect("from_proofs should succeed");
        assert_eq!(sparse.root(), initial_root, "initial root must match");

        // Mutation: update slot 1 (200 → 999), remove slot 5 (→ zero).
        let key_1 = storage_key(U256::from(1));
        let mut enc = Vec::new();
        alloy_rlp::Encodable::encode(&U256::from(999), &mut enc);
        sparse.update_leaf(key_1, enc);

        let key_5 = storage_key(U256::from(5));
        sparse.remove_leaf(&key_5);

        // Build fresh trie with expected state.
        let expected_entries = vec![
            (U256::from(0), U256::from(100)),
            (U256::from(1), U256::from(999)),
        ];
        let (expected_root, _) = build_storage_trie_with_proofs(&expected_entries);
        assert_eq!(
            sparse.root(),
            expected_root,
            "mutated storage root must match"
        );
    }

    /// Integration: build ZoneStateWitness with real proofs, verify
    /// build_state_trie + build_storage_trie return correct roots.
    #[test]
    fn test_build_trie_from_witness() {
        let addr_a = address!("0x1111111111111111111111111111111111111111");
        let addr_b = address!("0x2222222222222222222222222222222222222222");

        // Build storage for addr_a.
        let storage_a_entries = vec![
            (U256::from(0), U256::from(100)),
            (U256::from(7), U256::from(777)),
        ];
        let (storage_root_a, storage_proofs_a) = build_storage_trie_with_proofs(&storage_a_entries);

        // Build storage for addr_b.
        let storage_b_entries = vec![(U256::from(3), U256::from(333))];
        let (storage_root_b, storage_proofs_b) = build_storage_trie_with_proofs(&storage_b_entries);

        // Build state trie.
        let accounts = vec![
            (
                addr_a,
                TrieAccount {
                    nonce: 1,
                    balance: U256::from(1000),
                    storage_root: storage_root_a,
                    code_hash: alloy_primitives::KECCAK256_EMPTY,
                },
            ),
            (
                addr_b,
                TrieAccount {
                    nonce: 0,
                    balance: U256::from(2000),
                    storage_root: storage_root_b,
                    code_hash: alloy_primitives::KECCAK256_EMPTY,
                },
            ),
        ];
        let (state_root, addr_proofs) = build_state_trie_with_proofs(&accounts);

        // Construct ZoneStateWitness.
        let mut witness_accounts = alloy_primitives::map::HashMap::default();
        for ((addr, acct), (storage_entries, storage_proofs)) in accounts.iter().zip([
            (&storage_a_entries, &storage_proofs_a),
            (&storage_b_entries, &storage_proofs_b),
        ]) {
            let mut storage = alloy_primitives::map::HashMap::default();
            for (slot, value) in storage_entries.iter() {
                storage.insert(*slot, *value);
            }
            let mut sp_map = alloy_primitives::map::HashMap::default();
            for (slot, proof) in storage_proofs {
                sp_map.insert(*slot, proof.clone());
            }
            witness_accounts.insert(
                *addr,
                AccountWitness {
                    nonce: acct.nonce,
                    balance: acct.balance,
                    code_hash: acct.code_hash,
                    storage_root: acct.storage_root,
                    code: None,
                    storage,
                    account_proof: addr_proofs.get(addr).unwrap().clone(),
                    storage_proofs: sp_map,
                },
            );
        }

        let witness = ZoneStateWitness {
            accounts: witness_accounts,
            absent_accounts: alloy_primitives::map::HashMap::default(),
            state_root,
        };

        // Test build_state_trie returns the correct root.
        let state_trie = build_state_trie(&witness).expect("build_state_trie should succeed");
        assert_eq!(state_trie.root(), state_root, "state trie root must match");

        // Test build_storage_trie returns correct roots for each account.
        for (addr, acct) in &witness.accounts {
            let st = build_storage_trie(acct).expect("build_storage_trie should succeed");
            let expected = if *addr == addr_a {
                storage_root_a
            } else {
                storage_root_b
            };
            assert_eq!(
                st.root(),
                expected,
                "storage trie root for {addr} must match"
            );
        }
    }

    /// Integration: verify WitnessDatabase::from_witness accepts valid proofs
    /// and returns correct values for account and storage reads.
    #[test]
    fn test_witness_database_with_real_proofs() {
        use crate::db::WitnessDatabase;
        use revm::Database;

        let addr = address!("0xABCDABCDABCDABCDABCDABCDABCDABCDABCDABCD");

        // Build storage trie.
        let storage_entries = vec![
            (U256::from(0), U256::from(42)),
            (U256::from(1), U256::from(99)),
        ];
        let (storage_root, storage_proofs) = build_storage_trie_with_proofs(&storage_entries);

        // Build state trie with one account.
        let acct = TrieAccount {
            nonce: 10,
            balance: U256::from(5000),
            storage_root,
            code_hash: alloy_primitives::KECCAK256_EMPTY,
        };
        let (state_root, addr_proofs) = build_state_trie_with_proofs(&[(addr, acct)]);

        // Build the witness.
        let mut storage = alloy_primitives::map::HashMap::default();
        for (slot, value) in &storage_entries {
            storage.insert(*slot, *value);
        }
        let mut sp_map = alloy_primitives::map::HashMap::default();
        for (slot, proof) in &storage_proofs {
            sp_map.insert(*slot, proof.clone());
        }
        let mut accts = alloy_primitives::map::HashMap::default();
        accts.insert(
            addr,
            AccountWitness {
                nonce: 10,
                balance: U256::from(5000),
                code_hash: alloy_primitives::KECCAK256_EMPTY,
                storage_root,
                code: None,
                storage,
                account_proof: addr_proofs.get(&addr).unwrap().clone(),
                storage_proofs: sp_map,
            },
        );

        let witness = ZoneStateWitness {
            accounts: accts,
            absent_accounts: alloy_primitives::map::HashMap::default(),
            state_root,
        };

        // from_witness should verify all proofs successfully.
        let mut db = WitnessDatabase::from_witness(&witness)
            .expect("from_witness should succeed with valid proofs");

        // Check basic account info.
        let info = db
            .basic(addr)
            .expect("basic should succeed")
            .expect("account exists");
        assert_eq!(info.nonce, 10);
        assert_eq!(info.balance, U256::from(5000));

        // Check storage reads.
        assert_eq!(db.storage(addr, U256::from(0)).unwrap(), U256::from(42));
        assert_eq!(db.storage(addr, U256::from(1)).unwrap(), U256::from(99));

        // Missing slot should error (not zero).
        assert!(db.storage(addr, U256::from(2)).is_err());

        // Missing account should error.
        let other = address!("0x0000000000000000000000000000000000000099");
        assert!(db.basic(other).is_err());
    }

    /// Integration: verify that absence proofs work end-to-end through
    /// WitnessDatabase and SparseTrie.
    #[test]
    fn test_absence_proof_roundtrip() {
        use crate::db::WitnessDatabase;
        use revm::Database;

        let addr_existing = address!("0x1111111111111111111111111111111111111111");
        let addr_absent = address!("0x9999999999999999999999999999999999999999");

        let acct = TrieAccount {
            nonce: 1,
            balance: U256::from(100),
            storage_root: EMPTY_ROOT_HASH,
            code_hash: alloy_primitives::KECCAK256_EMPTY,
        };

        let (state_root, absence_proof, addr_proofs) =
            build_state_trie_with_absence_proof(&[(addr_existing, acct)], addr_absent);

        // Verify the absence proof.
        crate::mpt::verify_account_absence_proof(state_root, addr_absent, &absence_proof)
            .expect("absence proof should verify");

        // Build ZoneStateWitness with the absent account.
        let mut accts = alloy_primitives::map::HashMap::default();
        accts.insert(
            addr_existing,
            AccountWitness {
                nonce: 1,
                balance: U256::from(100),
                code_hash: alloy_primitives::KECCAK256_EMPTY,
                storage_root: EMPTY_ROOT_HASH,
                code: None,
                storage: alloy_primitives::map::HashMap::default(),
                account_proof: addr_proofs.get(&addr_existing).unwrap().clone(),
                storage_proofs: alloy_primitives::map::HashMap::default(),
            },
        );

        let mut absent = alloy_primitives::map::HashMap::default();
        absent.insert(addr_absent, absence_proof);

        let witness = ZoneStateWitness {
            accounts: accts,
            absent_accounts: absent,
            state_root,
        };

        // WitnessDatabase should accept both existing and absent accounts.
        let mut db = WitnessDatabase::from_witness(&witness).expect("from_witness should succeed");

        // Existing account returns info.
        let info = db.basic(addr_existing).unwrap().unwrap();
        assert_eq!(info.nonce, 1);

        // Absent account returns None (not error).
        assert_eq!(db.basic(addr_absent).unwrap(), None);

        // Absent account storage returns zero.
        assert_eq!(db.storage(addr_absent, U256::from(0)).unwrap(), U256::ZERO);

        // build_state_trie should also work with the absence proof.
        let state_trie = build_state_trie(&witness).expect("build_state_trie should succeed");
        assert_eq!(state_trie.root(), state_root);
    }
}
