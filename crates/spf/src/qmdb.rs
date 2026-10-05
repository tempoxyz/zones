//! Experimental Commonware Current QMDB state commitments and Zone replay.
//!
//! The initial prover uses a complete, unpruned mutation history, not a succinct
//! update witness. Roots commit to history and batch boundaries. Rebuilding that
//! history at each block is deliberately a correctness prototype, not a benchmark
//! of a persistent QMDB node. Tempo-side witnesses remain MPT proofs.

use std::collections::{BTreeMap, VecDeque};

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_rlp::Decodable as _;
use commonware_codec::{Decode as _, Encode as _};
use commonware_cryptography::{Sha256, sha256::Digest};
use commonware_parallel::Sequential;
use commonware_runtime::{Runner as _, deterministic};
use commonware_storage::{
    journal::contiguous::variable,
    merkle::full::Config as MerkleConfig,
    mmr,
    qmdb::{
        any::value::VariableEncoding,
        current::{
            VariableConfig,
            ordered::{
                ExclusionProof,
                variable::{Db, KeyValueProof},
            },
        },
    },
    translator::OneCap,
};
use commonware_utils::{NZU16, NZU64, NZUsize, buffer::paged::CacheRef};
use reth_trie_common::{EMPTY_ROOT_HASH, HashedPostState, Nibbles, TrieAccount, TrieNode};

use crate::{BatchOutput, BatchWitness, Error, SpfConfig, ZoneStateBackendWitness};

type Database =
    Db<mmr::Family, deterministic::Context, Digest, Vec<u8>, Sha256, OneCap, 32, Sequential>;
type Inclusion = KeyValueProof<mmr::Family, Digest, Digest, 32>;
type Exclusion = ExclusionProof<mmr::Family, Digest, VariableEncoding<Vec<u8>>, Digest, 32>;

/// A domain-separated flat state key. Both components use Ethereum's hashed keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
    feature = "serde",
    serde(rename_all = "camelCase", deny_unknown_fields)
)]
pub struct QmdbKey {
    /// Keccak-256 of the account address.
    pub account: B256,
    /// Keccak-256 of the 32-byte storage slot, or `None` for account metadata.
    pub slot: Option<B256>,
}

impl QmdbKey {
    /// An account metadata key.
    pub fn account(address: Address) -> Self {
        Self {
            account: keccak256(address),
            slot: None,
        }
    }

    /// A storage key, in a domain disjoint from account metadata.
    pub fn storage(address: Address, slot: U256) -> Self {
        Self {
            account: keccak256(address),
            slot: Some(keccak256(slot.to_be_bytes::<32>())),
        }
    }

    fn digest(self) -> Digest {
        let mut encoded = Vec::from(b"tempo-zone-qmdb-v1".as_slice());
        encoded.push(u8::from(self.slot.is_some()));
        encoded.extend_from_slice(self.account.as_slice());
        if let Some(slot) = self.slot {
            encoded.extend_from_slice(slot.as_slice());
        }
        Digest::from(keccak256(encoded).0)
    }
}

/// One canonical mutation. Account values are RLP `TrieAccount`s with an empty
/// storage root; storage values are nonzero, 32-byte, big-endian words.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct QmdbMutation {
    /// The state key.
    pub key: QmdbKey,
    /// New value, or `None` to remove the key.
    pub value: Option<Bytes>,
}

/// Complete mutation history from an empty database, preserving commit boundaries.
/// Each batch is strictly sorted by `QmdbKey`, without duplicate keys.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct QmdbStateWitness {
    /// Initial snapshot import followed by one mutation batch per Zone block.
    pub batches: Vec<Vec<QmdbMutation>>,
}

/// A succinct read proof against the Current root, including absence proofs.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct QmdbReadProof {
    /// Value committed at the requested key, or absence.
    pub value: Option<Bytes>,
    /// Commonware-encoded inclusion or exclusion proof, selected by `value`.
    pub proof: Bytes,
}

/// QMDB witness errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum QmdbError {
    /// Invalid or noncanonical mutation data.
    #[error("invalid QMDB state value or orphaned storage")]
    InvalidValue,
    /// Mutation order is not canonical.
    #[error("QMDB mutations must be sorted and unique within each batch")]
    MutationOrder,
    /// The history does not bind to the trusted parent root.
    #[error("QMDB root mismatch: expected {expected:?}, got {actual:?}")]
    RootMismatch { expected: B256, actual: B256 },
    /// Commonware could not construct the database or proof.
    #[error("QMDB database or proof operation failed")]
    Database,
    /// QMDB and MPT Zone state must not be mixed.
    #[error("QMDB Zone replay requires an empty MPT node pool")]
    MixedWitness,
    /// A partial MPT proof cannot bootstrap a complete QMDB commitment.
    #[error("MPT snapshot is incomplete: missing node {node:?}")]
    IncompleteMpt { node: B256 },
    /// A snapshot contains malformed or duplicate trie nodes.
    #[error("invalid MPT snapshot")]
    InvalidMpt,
}

/// Execute the shared SPF using QMDB Zone state, with unchanged Tempo MPT proofs.
/// The parent header must already commit to the QMDB root; this does not convert
/// historical MPT block hashes or settlement attestations.
pub fn prove_qmdb_zone_batch(
    config: &SpfConfig,
    witness: BatchWitness,
    history: QmdbStateWitness,
) -> Result<BatchOutput, Error> {
    crate::prove_zone_batch_with_backend(config, witness, ZoneStateBackendWitness::Qmdb(history))
}

/// Compute the Current QMDB root, including operation activity, not only the ops root.
pub fn state_root(witness: &QmdbStateWitness) -> Result<B256, QmdbError> {
    validate_history(witness)?;
    deterministic::Runner::default().start(|context| async move {
        let database = rebuild(context, witness).await?;
        Ok(B256::from_slice(database.root().as_ref()))
    })
}

/// Generate a Current inclusion or exclusion proof from a complete history.
pub fn read_proof(witness: &QmdbStateWitness, key: QmdbKey) -> Result<QmdbReadProof, QmdbError> {
    validate_history(witness)?;
    deterministic::Runner::default().start(|context| async move {
        let database = rebuild(context, witness).await?;
        let key = key.digest();
        let value = database.get(&key).await.map_err(|_| QmdbError::Database)?;
        let proof = if value.is_some() {
            database
                .key_value_proof(key)
                .await
                .map_err(|_| QmdbError::Database)?
                .encode()
        } else {
            database
                .exclusion_proof(&key)
                .await
                .map_err(|_| QmdbError::Database)?
                .encode()
        };
        Ok(QmdbReadProof {
            value: value.map(Bytes::from),
            proof: Bytes::from(proof.to_vec()),
        })
    })
}

/// Verify a read proof against an externally trusted Current QMDB root.
pub fn verify_read_proof(root: B256, key: QmdbKey, proof: &QmdbReadProof) -> bool {
    let root = Digest::from(root.0);
    let key = key.digest();
    match &proof.value {
        Some(value) => {
            Inclusion::decode_cfg(proof.proof.as_ref(), &(256, ())).is_ok_and(|decoded| {
                Database::verify_key_value_proof(key, value.to_vec(), &decoded, &root)
            })
        }
        None => Exclusion::decode_cfg(
            proof.proof.as_ref(),
            &(256, ((), ((..=128).into(), ())), ((..=128).into(), ())),
        )
        .is_ok_and(|decoded| Database::verify_exclusion_proof(&key, &decoded, &root)),
    }
}

/// Import a complete MPT snapshot as a deterministic initial QMDB batch.
/// Every child hash and every nonempty account storage root must be present.
/// Execution witnesses normally cover only accessed paths and are rejected
/// rather than treating unrevealed state as absent. The resulting commitment
/// is a new checkpoint, not the equivalent of an old MPT proof or block hash.
pub fn import_mpt_snapshot(root: B256, nodes: &[Bytes]) -> Result<QmdbStateWitness, QmdbError> {
    let mut pool = BTreeMap::new();
    for node in nodes {
        if pool.insert(keccak256(node), node.as_ref()).is_some() {
            return Err(QmdbError::InvalidMpt);
        }
    }
    if root == EMPTY_ROOT_HASH {
        return Ok(QmdbStateWitness {
            batches: vec![Vec::new()],
        });
    }
    let encoded = pool
        .get(&root)
        .ok_or(QmdbError::IncompleteMpt { node: root })?;
    let mut queue = VecDeque::from([(decode_node(encoded)?, Nibbles::default(), None)]);
    let mut values = BTreeMap::new();
    while let Some((node, path, account)) = queue.pop_front() {
        match node {
            TrieNode::Branch(branch) => {
                if path.len() >= 64 {
                    return Err(QmdbError::InvalidMpt);
                }
                for (nibble, child) in branch.as_ref().children() {
                    if let Some(child) = child {
                        let mut child_path = path;
                        child_path.push_unchecked(nibble);
                        let bytes = if let Some(hash) = child.as_hash() {
                            *pool
                                .get(&hash)
                                .ok_or(QmdbError::IncompleteMpt { node: hash })?
                        } else {
                            child.as_slice()
                        };
                        queue.push_back((decode_node(bytes)?, child_path, account));
                    }
                }
            }
            TrieNode::Extension(extension) => {
                if extension.key.is_empty() || path.len() + extension.key.len() > 64 {
                    return Err(QmdbError::InvalidMpt);
                }
                let mut child_path = path;
                child_path.extend(&extension.key);
                let bytes = if let Some(hash) = extension.child.as_hash() {
                    *pool
                        .get(&hash)
                        .ok_or(QmdbError::IncompleteMpt { node: hash })?
                } else {
                    extension.child.as_slice()
                };
                queue.push_back((decode_node(bytes)?, child_path, account));
            }
            TrieNode::Leaf(leaf) => {
                if path.len() + leaf.key.len() != 64 {
                    return Err(QmdbError::InvalidMpt);
                }
                let mut full_path = path;
                full_path.extend(&leaf.key);
                let hashed_key = B256::from_slice(&full_path.pack());
                let (key, value) = if let Some(account) = account {
                    let mut input = leaf.value.as_slice();
                    let value = U256::decode(&mut input).map_err(|_| QmdbError::InvalidMpt)?;
                    if !input.is_empty() || value == U256::ZERO {
                        return Err(QmdbError::InvalidMpt);
                    }
                    (
                        QmdbKey {
                            account,
                            slot: Some(hashed_key),
                        },
                        Bytes::copy_from_slice(&value.to_be_bytes::<32>()),
                    )
                } else {
                    let mut input = leaf.value.as_slice();
                    let mut metadata =
                        TrieAccount::decode(&mut input).map_err(|_| QmdbError::InvalidMpt)?;
                    if !input.is_empty() {
                        return Err(QmdbError::InvalidMpt);
                    }
                    if metadata.storage_root != EMPTY_ROOT_HASH {
                        let hash = metadata.storage_root;
                        let bytes = pool
                            .get(&hash)
                            .ok_or(QmdbError::IncompleteMpt { node: hash })?;
                        queue.push_back((
                            decode_node(bytes)?,
                            Nibbles::default(),
                            Some(hashed_key),
                        ));
                    }
                    metadata.storage_root = EMPTY_ROOT_HASH;
                    (
                        QmdbKey {
                            account: hashed_key,
                            slot: None,
                        },
                        Bytes::from(alloy_rlp::encode(metadata)),
                    )
                };
                if values.insert(key, value).is_some() {
                    return Err(QmdbError::InvalidMpt);
                }
            }
            _ => return Err(QmdbError::InvalidMpt),
        }
    }
    Ok(QmdbStateWitness {
        batches: vec![
            values
                .into_iter()
                .map(|(key, value)| QmdbMutation {
                    key,
                    value: Some(value),
                })
                .collect(),
        ],
    })
}

fn decode_node(bytes: &[u8]) -> Result<TrieNode, QmdbError> {
    let mut input = bytes;
    let node = TrieNode::decode(&mut input).map_err(|_| QmdbError::InvalidMpt)?;
    if !input.is_empty() {
        return Err(QmdbError::InvalidMpt);
    }
    Ok(node)
}

#[derive(Debug)]
pub(crate) struct QmdbState {
    witness: QmdbStateWitness,
    values: BTreeMap<QmdbKey, Bytes>,
}

impl QmdbState {
    pub(crate) fn new(witness: QmdbStateWitness, expected: B256) -> Result<Self, QmdbError> {
        let actual = state_root(&witness)?;
        if actual != expected {
            return Err(QmdbError::RootMismatch { expected, actual });
        }
        let values = validate_history(&witness)?;
        Ok(Self { witness, values })
    }

    pub(crate) fn account(&self, address: Address) -> Result<Option<TrieAccount>, QmdbError> {
        self.values
            .get(&QmdbKey::account(address))
            .map(|value| decode_account(value))
            .transpose()
    }

    pub(crate) fn storage(&self, address: Address, slot: U256) -> Result<U256, QmdbError> {
        Ok(self
            .values
            .get(&QmdbKey::storage(address, slot))
            .map_or(U256::ZERO, |value| U256::from_be_slice(value)))
    }

    pub(crate) fn apply_state(
        &mut self,
        state: HashedPostState,
        wiped: &[B256],
    ) -> Result<B256, QmdbError> {
        let mut changes = BTreeMap::new();
        for account in wiped {
            for key in self
                .values
                .keys()
                .filter(|key| key.account == *account && key.slot.is_some())
            {
                changes.insert(*key, None);
            }
        }
        for (account, storage) in state.storages {
            for (slot, value) in storage.storage {
                changes.insert(
                    QmdbKey {
                        account,
                        slot: Some(slot),
                    },
                    (value != U256::ZERO)
                        .then(|| Bytes::copy_from_slice(&value.to_be_bytes::<32>())),
                );
            }
        }
        for (account, info) in state.accounts {
            if info.is_none() {
                for key in self
                    .values
                    .keys()
                    .filter(|key| key.account == account && key.slot.is_some())
                {
                    changes.insert(*key, None);
                }
                for (key, value) in &mut changes {
                    if key.account == account && key.slot.is_some() {
                        *value = None;
                    }
                }
            }
            changes.insert(
                QmdbKey {
                    account,
                    slot: None,
                },
                info.map(|info| {
                    Bytes::from(alloy_rlp::encode(info.into_trie_account(EMPTY_ROOT_HASH)))
                }),
            );
        }
        let mutations = changes
            .into_iter()
            .map(|(key, value)| QmdbMutation { key, value })
            .collect();
        let mut witness = self.witness.clone();
        witness.batches.push(mutations);
        let root = state_root(&witness)?;
        self.values = validate_history(&witness)?;
        self.witness = witness;
        Ok(root)
    }
}

fn decode_account(value: &[u8]) -> Result<TrieAccount, QmdbError> {
    let mut input = value;
    let account = TrieAccount::decode(&mut input).map_err(|_| QmdbError::InvalidValue)?;
    if !input.is_empty() || account.storage_root != EMPTY_ROOT_HASH {
        return Err(QmdbError::InvalidValue);
    }
    Ok(account)
}

fn validate_history(witness: &QmdbStateWitness) -> Result<BTreeMap<QmdbKey, Bytes>, QmdbError> {
    let mut values = BTreeMap::new();
    for batch in &witness.batches {
        if batch.windows(2).any(|pair| pair[0].key >= pair[1].key) {
            return Err(QmdbError::MutationOrder);
        }
        for mutation in batch {
            if let Some(value) = &mutation.value {
                if mutation.key.slot.is_some() {
                    if value.len() != 32 || U256::from_be_slice(value) == U256::ZERO {
                        return Err(QmdbError::InvalidValue);
                    }
                } else {
                    decode_account(value)?;
                }
                values.insert(mutation.key, value.clone());
            } else {
                values.remove(&mutation.key);
            }
        }
        if values.keys().any(|key| {
            key.slot.is_some()
                && !values.contains_key(&QmdbKey {
                    account: key.account,
                    slot: None,
                })
        }) {
            return Err(QmdbError::InvalidValue);
        }
    }
    Ok(values)
}

async fn rebuild(
    context: deterministic::Context,
    witness: &QmdbStateWitness,
) -> Result<Database, QmdbError> {
    let page_cache = CacheRef::from_pooler(&context, NZU16!(4096), NZUsize!(64));
    let config = VariableConfig {
        merkle_config: MerkleConfig {
            journal_partition: "qmdb-ops-merkle".into(),
            metadata_partition: "qmdb-ops-metadata".into(),
            items_per_blob: NZU64!(65536),
            write_buffer: NZUsize!(65536),
            replay_buffer: NZUsize!(65536),
            strategy: Sequential,
            page_cache: page_cache.clone(),
        },
        journal_config: variable::Config {
            partition: "qmdb-ops".into(),
            items_per_section: NZU64!(65536),
            compression: None,
            codec_config: ((), ((..=128).into(), ())),
            page_cache,
            write_buffer: NZUsize!(65536),
            replay_buffer: NZUsize!(65536),
        },
        grafted_metadata_partition: "qmdb-current".into(),
        translator: OneCap,
        init_cache_size: Some(NZUsize!(1024)),
        init_buffer: NZUsize!(65536),
        init_concurrency: (),
    };
    let mut database = Database::init(context, config)
        .await
        .map_err(|_| QmdbError::Database)?;
    for mutations in &witness.batches {
        let mut batch = database.new_batch();
        for mutation in mutations {
            batch = batch.write(
                mutation.key.digest(),
                mutation.value.as_ref().map(|value| value.to_vec()),
            );
        }
        let batch = batch
            .merkleize(&database, None)
            .await
            .map_err(|_| QmdbError::Database)?;
        (database, _) = database
            .apply_batch(batch)
            .await
            .map_err(|_| QmdbError::Database)?;
    }
    Ok(database)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_trie_common::LeafNode;
    use revm::{
        Database as _, DatabaseCommit as _,
        database::{State, states::bundle_state::BundleRetention},
    };

    use crate::{WitnessDatabase, ZoneStateWitness};

    fn witness() -> QmdbStateWitness {
        let address = Address::repeat_byte(1);
        let account = TrieAccount {
            nonce: 7,
            balance: U256::from(42),
            ..Default::default()
        };
        QmdbStateWitness {
            batches: vec![vec![
                QmdbMutation {
                    key: QmdbKey::account(address),
                    value: Some(Bytes::from(alloy_rlp::encode(account))),
                },
                QmdbMutation {
                    key: QmdbKey::storage(address, U256::from(3)),
                    value: Some(Bytes::copy_from_slice(&U256::from(9).to_be_bytes::<32>())),
                },
            ]],
        }
    }

    #[test]
    fn proves_current_values_and_absence() {
        let witness = witness();
        let root = state_root(&witness).unwrap();
        for key in [
            QmdbKey::account(Address::repeat_byte(1)),
            QmdbKey::storage(Address::repeat_byte(1), U256::from(3)),
            QmdbKey::account(Address::repeat_byte(2)),
        ] {
            let proof = read_proof(&witness, key).unwrap();
            assert!(verify_read_proof(root, key, &proof));
            assert!(!verify_read_proof(B256::ZERO, key, &proof));
            assert!(!verify_read_proof(
                root,
                if proof.value.is_none() {
                    QmdbKey::account(Address::repeat_byte(1))
                } else {
                    QmdbKey::account(Address::repeat_byte(4))
                },
                &proof
            ));
        }
        assert_eq!(state_root(&witness).unwrap(), root);
    }

    #[test]
    fn rejects_tampered_history_and_noncanonical_order() {
        let mut witness = witness();
        let root = state_root(&witness).unwrap();
        witness.batches[0][0].value = Some(Bytes::from(alloy_rlp::encode(TrieAccount::default())));
        assert!(matches!(
            QmdbState::new(witness.clone(), root),
            Err(QmdbError::RootMismatch { .. })
        ));
        witness.batches[0].reverse();
        assert_eq!(state_root(&witness), Err(QmdbError::MutationOrder));
    }

    #[test]
    fn applies_storage_wipes_and_account_deletion() {
        let witness = witness();
        let root = state_root(&witness).unwrap();
        let mut state = QmdbState::new(witness, root).unwrap();
        let address = Address::repeat_byte(1);
        let mut update = HashedPostState::default();
        update.accounts.insert(keccak256(address), None);
        let next = state.apply_state(update, &[]).unwrap();
        assert_ne!(root, next);
        assert_eq!(state.account(address).unwrap(), None);
        assert_eq!(state.storage(address, U256::from(3)).unwrap(), U256::ZERO);
        let key = QmdbKey::storage(address, U256::from(3));
        assert!(verify_read_proof(
            next,
            key,
            &read_proof(&state.witness, key).unwrap()
        ));
    }

    #[test]
    fn rejects_orphaned_storage_and_malformed_values() {
        let mut witness = witness();
        witness.batches[0].remove(0);
        assert_eq!(state_root(&witness), Err(QmdbError::InvalidValue));
        witness.batches[0][0].value = Some(Bytes::from(vec![1]));
        assert_eq!(state_root(&witness), Err(QmdbError::InvalidValue));
    }

    #[test]
    fn imports_complete_mpt_state_but_rejects_missing_storage() {
        let address = Address::repeat_byte(1);
        let slot = U256::from(3);
        let storage = Bytes::from(alloy_rlp::encode(TrieNode::Leaf(LeafNode::new(
            Nibbles::unpack(keccak256(slot.to_be_bytes::<32>())),
            alloy_rlp::encode(U256::from(9)),
        ))));
        let account = TrieAccount {
            nonce: 7,
            balance: U256::from(42),
            storage_root: keccak256(&storage),
            ..Default::default()
        };
        let root_node = Bytes::from(alloy_rlp::encode(TrieNode::Leaf(LeafNode::new(
            Nibbles::unpack(keccak256(address)),
            alloy_rlp::encode(account),
        ))));
        let root = keccak256(&root_node);
        assert_eq!(
            import_mpt_snapshot(root, std::slice::from_ref(&root_node)),
            Err(QmdbError::IncompleteMpt {
                node: keccak256(&storage)
            })
        );
        let imported = import_mpt_snapshot(root, &[root_node, storage]).unwrap();
        assert_eq!(imported, witness());
        let key = QmdbKey::storage(address, slot);
        let qmdb_root = state_root(&imported).unwrap();
        assert_ne!(qmdb_root, root);
        assert!(verify_read_proof(
            qmdb_root,
            key,
            &read_proof(&imported, key).unwrap()
        ));
    }

    #[test]
    fn invalidates_stale_current_proofs_after_updates() {
        let mut witness = witness();
        let key = QmdbKey::storage(Address::repeat_byte(1), U256::from(3));
        let old_root = state_root(&witness).unwrap();
        let old_proof = read_proof(&witness, key).unwrap();
        witness.batches.push(vec![QmdbMutation {
            key,
            value: Some(Bytes::copy_from_slice(&U256::from(10).to_be_bytes::<32>())),
        }]);
        let new_root = state_root(&witness).unwrap();
        assert_ne!(old_root, new_root);
        assert!(!verify_read_proof(new_root, key, &old_proof));
        assert!(verify_read_proof(
            new_root,
            key,
            &read_proof(&witness, key).unwrap()
        ));
    }

    #[test]
    fn wipes_old_storage_before_recreation() {
        let witness = witness();
        let mut state = QmdbState::new(witness.clone(), state_root(&witness).unwrap()).unwrap();
        let address = Address::repeat_byte(1);
        let hashed_address = keccak256(address);
        let mut update = HashedPostState::default();
        update
            .accounts
            .insert(hashed_address, Some(Default::default()));
        update
            .storages
            .entry(hashed_address)
            .or_default()
            .storage
            .insert(keccak256(U256::from(4).to_be_bytes::<32>()), U256::from(19));
        state.apply_state(update, &[hashed_address]).unwrap();
        assert_eq!(state.storage(address, U256::from(3)).unwrap(), U256::ZERO);
        assert_eq!(
            state.storage(address, U256::from(4)).unwrap(),
            U256::from(19)
        );
    }

    #[test]
    fn commits_revm_bundle_updates_to_qmdb() {
        let history = QmdbStateWitness::default();
        let initial_root = state_root(&history).unwrap();
        let database = WitnessDatabase::from_qmdb_state_witness(
            history,
            ZoneStateWitness {
                node_pool: Vec::new(),
                bytecodes: Vec::new(),
            },
            initial_root,
        )
        .unwrap();
        let mut state = State::builder()
            .with_database(database)
            .with_bundle_update()
            .build();
        let address = Address::repeat_byte(1);
        let mut changes = revm::primitives::AddressMap::default();
        let mut account = revm::state::Account::default();
        account.info.balance = U256::from(42);
        account.mark_touch();
        changes.insert(address, account);
        state.commit(changes);
        state.merge_transitions(BundleRetention::PlainState);
        let bundle = state.take_bundle();
        let actual = state.database.state_root(bundle).unwrap();
        let expected = QmdbStateWitness {
            batches: vec![vec![QmdbMutation {
                key: QmdbKey::account(address),
                value: Some(Bytes::from(alloy_rlp::encode(TrieAccount {
                    balance: U256::from(42),
                    ..Default::default()
                }))),
            }]],
        };
        assert_eq!(actual, state_root(&expected).unwrap());
        assert_eq!(
            state.database.basic(address).unwrap().unwrap().balance,
            U256::from(42)
        );
    }
}
