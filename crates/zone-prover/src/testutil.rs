//! Test utilities for building MPT proofs and zone state witnesses.
//!
//! These helpers are used by both unit tests and integration tests to construct
//! valid MPT proofs from known state, enabling end-to-end testing of the prover
//! without a real trie-backed state provider.

use alloy_primitives::{Address, B256, Bytes, U256, keccak256, map::HashMap};
use alloy_trie::{EMPTY_ROOT_HASH, HashBuilder, TrieAccount, proof::ProofRetainer};
use nybbles::Nibbles;

use crate::types::{AccountWitness, ZoneStateWitness};

/// A test account with its storage, used to build a zone state fixture.
#[derive(Debug, Clone)]
pub struct TestAccount {
    pub nonce: u64,
    pub balance: U256,
    pub code_hash: B256,
    pub code: Option<Vec<u8>>,
    pub storage: Vec<(U256, U256)>,
}

impl Default for TestAccount {
    fn default() -> Self {
        Self {
            nonce: 0,
            balance: U256::ZERO,
            code_hash: alloy_primitives::KECCAK256_EMPTY,
            code: None,
            storage: Vec::new(),
        }
    }
}

/// A complete zone state fixture with MPT proofs for all accounts and storage.
#[derive(Debug)]
pub struct ZoneStateFixture {
    /// The computed state root.
    pub state_root: B256,
    /// The assembled witness with valid MPT proofs.
    pub witness: ZoneStateWitness,
    /// Per-account storage roots (for external reference).
    pub storage_roots: std::collections::HashMap<Address, B256>,
}

/// Build a storage trie from slot→value pairs and return the root and per-slot proofs.
///
/// Proofs are in `eth_getProof` format: ordered root-to-leaf RLP-encoded nodes.
pub fn build_storage_trie_with_proofs(
    entries: &[(U256, U256)],
) -> (B256, std::collections::HashMap<U256, Vec<Bytes>>) {
    // Collect proof targets for all slots (including zero-valued ones).
    let mut targets: Vec<Nibbles> = entries
        .iter()
        .map(|(slot, _)| Nibbles::unpack(keccak256(B256::from(slot.to_be_bytes()))))
        .collect();
    targets.sort();
    targets.dedup();

    // Filter out zero values (they're absence proofs in the storage trie).
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

    if targets.is_empty() {
        return (EMPTY_ROOT_HASH, Default::default());
    }
    let retainer = ProofRetainer::new(targets);
    let mut builder = HashBuilder::default().with_proof_retainer(retainer);

    for (key, _, value) in &sorted {
        builder.add_leaf(key.clone(), value);
    }

    let root = builder.root();
    let proof_nodes = builder.take_proof_nodes();

    let mut slot_proofs = std::collections::HashMap::new();
    for (slot, _value) in entries {
        let key = Nibbles::unpack(keccak256(B256::from(slot.to_be_bytes())));
        let nodes: Vec<Bytes> = proof_nodes
            .matching_nodes_sorted(&key)
            .into_iter()
            .map(|(_, b)| b)
            .collect();
        slot_proofs.insert(*slot, nodes);
    }

    (root, slot_proofs)
}

/// Build a state trie from address→TrieAccount pairs and return the root
/// and per-address account proofs.
pub fn build_state_trie_with_proofs(
    accounts: &[(Address, TrieAccount)],
) -> (B256, std::collections::HashMap<Address, Vec<Bytes>>) {
    let mut sorted: Vec<(Nibbles, Address, Vec<u8>)> = accounts
        .iter()
        .map(|(addr, acct)| {
            let key = Nibbles::unpack(keccak256(*addr));
            let mut enc = Vec::new();
            alloy_rlp::Encodable::encode(acct, &mut enc);
            (key, *addr, enc)
        })
        .collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    if sorted.is_empty() {
        return (EMPTY_ROOT_HASH, Default::default());
    }

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

/// Build a state trie and return proofs for both present and absent addresses.
///
/// This extends [`build_state_trie_with_proofs`] by also collecting proof nodes
/// for addresses that do NOT appear in the trie (absence proofs). The `HashBuilder`'s
/// `ProofRetainer` naturally produces the necessary proof path for absent keys.
pub fn build_state_trie_with_absence_proofs(
    accounts: &[(Address, TrieAccount)],
    absent_addresses: &[Address],
) -> (B256, std::collections::HashMap<Address, Vec<Bytes>>) {
    let mut sorted: Vec<(Nibbles, Address, Vec<u8>)> = accounts
        .iter()
        .map(|(addr, acct)| {
            let key = Nibbles::unpack(keccak256(*addr));
            let mut enc = Vec::new();
            alloy_rlp::Encodable::encode(acct, &mut enc);
            (key, *addr, enc)
        })
        .collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    // Collect proof targets for both present and absent addresses.
    let mut targets: Vec<Nibbles> = sorted.iter().map(|(k, _, _)| k.clone()).collect();
    for addr in absent_addresses {
        targets.push(Nibbles::unpack(keccak256(*addr)));
    }
    targets.sort();
    targets.dedup();

    if sorted.is_empty() && absent_addresses.is_empty() {
        return (EMPTY_ROOT_HASH, Default::default());
    }

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
    for addr in absent_addresses {
        let key = Nibbles::unpack(keccak256(*addr));
        let nodes: Vec<Bytes> = proof_nodes
            .matching_nodes_sorted(&key)
            .into_iter()
            .map(|(_, b)| b)
            .collect();
        addr_proofs.insert(*addr, nodes);
    }

    (root, addr_proofs)
}

/// Build a complete `ZoneStateFixture` from a map of address→TestAccount.
///
/// This builds real MPT proofs for every account and every storage slot,
/// suitable for feeding into `prove_zone_batch`.
pub fn build_zone_state_fixture(
    accounts: &[(Address, TestAccount)],
) -> ZoneStateFixture {
    build_zone_state_fixture_with_absent(accounts, &[])
}

/// Build a complete `ZoneStateFixture` with additional absence proofs.
///
/// Like [`build_zone_state_fixture`], but also generates MPT absence proofs for
/// `absent_addresses` — addresses that the EVM will access but that don't exist
/// in the state (e.g., the TempoStateReader precompile address).
pub fn build_zone_state_fixture_with_absent(
    accounts: &[(Address, TestAccount)],
    absent_addresses: &[Address],
) -> ZoneStateFixture {
    // Step 1: Build storage tries for each account.
    let mut storage_roots = std::collections::HashMap::new();
    let mut storage_proofs_map: std::collections::HashMap<
        Address,
        (B256, std::collections::HashMap<U256, Vec<Bytes>>),
    > = std::collections::HashMap::new();

    for (addr, acct) in accounts {
        let (storage_root, slot_proofs) = build_storage_trie_with_proofs(&acct.storage);
        storage_roots.insert(*addr, storage_root);
        storage_proofs_map.insert(*addr, (storage_root, slot_proofs));
    }

    // Step 2: Build TrieAccounts with the correct storage roots.
    let trie_accounts: Vec<(Address, TrieAccount)> = accounts
        .iter()
        .map(|(addr, acct)| {
            let storage_root = storage_roots[addr];
            let code_hash = acct
                .code
                .as_ref()
                .map(|c| keccak256(c.as_slice()))
                .unwrap_or(acct.code_hash);
            (
                *addr,
                TrieAccount {
                    nonce: acct.nonce,
                    balance: acct.balance,
                    storage_root,
                    code_hash,
                },
            )
        })
        .collect();

    // Step 3: Build the state trie with proof targets for both present and absent accounts.
    let (state_root, addr_proofs) =
        build_state_trie_with_absence_proofs(&trie_accounts, absent_addresses);

    // Step 4: Assemble ZoneStateWitness.
    let mut witness_accounts = HashMap::default();
    for (addr, acct) in accounts {
        let (storage_root, slot_proofs) = storage_proofs_map.get(addr).unwrap();

        let mut storage = HashMap::default();
        for (slot, value) in &acct.storage {
            storage.insert(*slot, *value);
        }

        let mut sp_map = HashMap::default();
        for (slot, proof) in slot_proofs {
            sp_map.insert(*slot, proof.clone());
        }

        let code = acct.code.as_ref().map(|c| Bytes::copy_from_slice(c));

        let code_hash = acct
            .code
            .as_ref()
            .map(|c| keccak256(c.as_slice()))
            .unwrap_or(acct.code_hash);

        witness_accounts.insert(
            *addr,
            AccountWitness {
                nonce: acct.nonce,
                balance: acct.balance,
                code_hash,
                storage_root: *storage_root,
                code,
                storage,
                account_proof: addr_proofs.get(addr).unwrap().clone(),
                storage_proofs: sp_map,
            },
        );
    }

    // Build absence proofs for requested addresses.
    let mut absent_accounts = HashMap::default();
    for addr in absent_addresses {
        if let Some(proof) = addr_proofs.get(addr) {
            absent_accounts.insert(*addr, proof.clone());
        }
    }

    ZoneStateFixture {
        state_root,
        witness: ZoneStateWitness {
            accounts: witness_accounts,
            absent_accounts,
            state_root,
        },
        storage_roots,
    }
}

/// Recompute the state root after modifying some accounts.
///
/// Takes the full set of accounts (initial + modified) and returns the new
/// state root. Useful for computing `expected_state_root` for a zone block.
pub fn compute_state_root(accounts: &[(Address, TestAccount)]) -> B256 {
    let mut storage_roots = std::collections::HashMap::new();
    for (addr, acct) in accounts {
        let (storage_root, _) = build_storage_trie_with_proofs(&acct.storage);
        storage_roots.insert(*addr, storage_root);
    }

    let trie_accounts: Vec<(Address, TrieAccount)> = accounts
        .iter()
        .map(|(addr, acct)| {
            let code_hash = acct
                .code
                .as_ref()
                .map(|c| keccak256(c.as_slice()))
                .unwrap_or(acct.code_hash);
            (
                *addr,
                TrieAccount {
                    nonce: acct.nonce,
                    balance: acct.balance,
                    storage_root: storage_roots[addr],
                    code_hash,
                },
            )
        })
        .collect();

    let (state_root, _) = build_state_trie_with_proofs(&trie_accounts);
    state_root
}
