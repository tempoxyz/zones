//! Deferred proof verification and storage-root-keyed cache reuse for payload construction.

use super::provider::{L1ProofTargets, L1RpcClient};
use alloy_consensus::BlockHeader as _;
use alloy_eips::{BlockId, NumHash};
use alloy_primitives::{Address, B256};
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use eyre::{Result, ensure};
use futures::{StreamExt as _, TryStreamExt as _, stream};
use parking_lot::Mutex;
use reth_primitives_traits::SealedHeader;
use reth_trie_common::AccountProof;
use schnellru::{ByLength, LruMap};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use tempo_primitives::TempoHeader;
use thiserror::Error;
use zone_precompiles::{L1StateError, L1StorageReader};
use zone_primitives::StorageReadKey;

/// Maximum verified slot values retained across payloads.
const DEFAULT_VERIFIED_SLOT_CAPACITY: u32 = 100_000;
/// Maximum finalized blocks whose authenticated account roots are retained.
const DEFAULT_AUTHENTICATED_ROOT_CAPACITY: usize =
    crate::subscriber::MAX_L1_LOOKAHEAD_BLOCKS as usize + 1;
/// Maximum concurrent proof requests when a payload spans multiple Tempo anchors.
const PROOF_RPC_CONCURRENCY: usize = 8;

/// Per-payload journal and memoization cache for provisional values consumed by execution.
///
/// Entries remain unverified until finalization drains the journal through multiproof validation.
type UnverifiedL1Reads = BTreeMap<u64, BTreeMap<StorageReadKey, B256>>;

/// Shared authenticated account roots and proved slot values.
///
/// The slot cache contains only values that passed proof verification. Presence under
/// `(account, storage_root, slot)` is therefore sufficient provenance; no entry state or proof bit
/// is needed.
#[derive(Clone, Debug)]
pub struct VerifiedL1StateCache(Arc<VerifiedL1StateCacheWithMetrics>);

impl Default for VerifiedL1StateCache {
    fn default() -> Self {
        Self::new()
    }
}

impl VerifiedL1StateCache {
    /// Creates an empty bounded verified-state cache.
    pub fn new() -> Self {
        Self::with_limits(
            DEFAULT_VERIFIED_SLOT_CAPACITY,
            DEFAULT_AUTHENTICATED_ROOT_CAPACITY,
        )
    }

    fn with_limits(slot_capacity: u32, root_capacity: usize) -> Self {
        assert!(slot_capacity > 0, "verified slot cache must be non-empty");
        assert!(
            root_capacity > 0,
            "authenticated root cache must be non-empty"
        );
        Self(Arc::new(VerifiedL1StateCacheWithMetrics {
            state: Mutex::new(VerifiedL1StateCacheInner {
                blocks: BTreeMap::new(),
                slots: LruMap::new(ByLength::new(slot_capacity)),
                root_capacity,
            }),
            metrics: Default::default(),
        }))
    }

    /// Records account storage roots authenticated against one sealed finalized header.
    ///
    /// Returns accounts whose root differs from the immediately preceding authenticated block, or
    /// whose parent root is unavailable.
    pub(crate) fn record_verified_roots(
        &self,
        block: NumHash,
        roots: impl IntoIterator<Item = (Address, B256)>,
    ) -> Result<BTreeSet<Address>> {
        let roots = roots.into_iter().collect::<Vec<_>>();
        let changed = {
            let mut state = self.0.state.lock();
            let changed = roots
                .iter()
                .filter_map(|&(account, root)| {
                    state
                        .root_changed_from_parent(block.number, account, root)
                        .then_some(account)
                })
                .collect();
            state.commit_payload(
                roots.iter().map(|&(account, root)| (block, account, root)),
                std::iter::empty::<((StorageReadKey, B256), B256)>(),
            )?;
            changed
        };
        self.0
            .metrics
            .verified_account_roots
            .increment(roots.len() as u64);
        Ok(changed)
    }

    /// Returns a proved slot value when the exact block authenticates its cache key's root.
    fn get(&self, block: NumHash, storage: StorageReadKey) -> Option<B256> {
        let value = {
            let mut inner = self.0.state.lock();
            inner
                .storage_root(block, storage.account)
                .and_then(|storage_root| inner.slots.get(&(storage, storage_root)).copied())
        };
        if value.is_some() {
            self.0.metrics.slot_cache_hits.increment(1);
        } else {
            self.0.metrics.slot_cache_misses.increment(1);
        }
        value
    }

    fn commit_payload(
        &self,
        roots: impl IntoIterator<Item = (NumHash, Address, B256)>,
        slots: impl IntoIterator<Item = ((StorageReadKey, B256), B256)>,
    ) -> Result<()> {
        self.0.state.lock().commit_payload(roots, slots)
    }

    #[cfg(test)]
    fn slot_count(&self) -> usize {
        self.0.state.lock().slots.len()
    }
}

#[derive(Debug)]
struct VerifiedL1StateCacheWithMetrics {
    state: Mutex<VerifiedL1StateCacheInner>,
    metrics: crate::metrics::VerifiedL1StateCacheMetrics,
}

#[derive(Debug)]
struct VerifiedL1StateCacheInner {
    blocks: BTreeMap<u64, AuthenticatedBlockRoots>,
    slots: LruMap<(StorageReadKey, B256), B256>,
    root_capacity: usize,
}

impl VerifiedL1StateCacheInner {
    fn storage_root(&self, block: NumHash, account: Address) -> Option<B256> {
        let cached = self.blocks.get(&block.number)?;
        (cached.hash == block.hash)
            .then(|| cached.roots.get(&account).copied())
            .flatten()
    }

    fn root_changed_from_parent(&self, block_number: u64, account: Address, root: B256) -> bool {
        block_number
            .checked_sub(1)
            .and_then(|parent| self.blocks.get(&parent))
            .and_then(|block| block.roots.get(&account))
            .is_none_or(|parent_root| *parent_root != root)
    }

    fn commit_payload(
        &mut self,
        roots: impl IntoIterator<Item = (NumHash, Address, B256)>,
        slots: impl IntoIterator<Item = ((StorageReadKey, B256), B256)>,
    ) -> Result<()> {
        // Normalize and reject conflicts before touching either cache. This keeps root and slot
        // publication atomic even when one payload contains duplicate proof material.
        let mut roots_by_block = BTreeMap::<u64, AuthenticatedBlockRoots>::new();
        for (block, account, root) in roots {
            let block_roots =
                roots_by_block
                    .entry(block.number)
                    .or_insert_with(|| AuthenticatedBlockRoots {
                        hash: block.hash,
                        roots: BTreeMap::new(),
                    });
            ensure!(
                block_roots.hash == block.hash,
                "conflicting finalized hashes supplied at Tempo block {}: {} and {}",
                block.number,
                block_roots.hash,
                block.hash
            );
            if let Some(previous) = block_roots.roots.insert(account, root) {
                ensure!(
                    previous == root,
                    "conflicting authenticated roots supplied at block {} for account {account}: {previous} and {root}",
                    block.number
                );
            }
        }

        let mut unique_slots = BTreeMap::new();
        for (key, value) in slots {
            if let Some(previous) = unique_slots.insert(key, value) {
                ensure!(
                    previous == value,
                    "conflicting proved values supplied for account {} root {} slot {}: {} and {}",
                    key.0.account,
                    key.1,
                    key.0.slot,
                    previous,
                    value
                );
            }
        }

        for (&number, block_roots) in &roots_by_block {
            if let Some(existing) = self.blocks.get(&number) {
                ensure!(
                    existing.hash == block_roots.hash,
                    "conflicting finalized hash at Tempo block {number}: cached {}, new {}",
                    existing.hash,
                    block_roots.hash
                );
                for (&account, &root) in &block_roots.roots {
                    if let Some(cached) = existing.roots.get(&account) {
                        ensure!(
                            *cached == root,
                            "proof returned storage root {root} for account {account} at block {number}, but authenticated cache contains {cached}"
                        );
                    }
                }
            }
        }
        for (&(storage, storage_root), value) in &unique_slots {
            if let Some(cached) = self.slots.peek(&(storage, storage_root)) {
                ensure!(
                    cached == value,
                    "conflicting proved value for account {} root {} slot {}: cached {}, new {}",
                    storage.account,
                    storage_root,
                    storage.slot,
                    cached,
                    value
                );
            }
        }

        for (number, block_roots) in roots_by_block {
            let entry = self
                .blocks
                .entry(number)
                .or_insert_with(|| AuthenticatedBlockRoots {
                    hash: block_roots.hash,
                    roots: BTreeMap::new(),
                });
            entry.roots.extend(block_roots.roots);
        }
        while self.blocks.len() > self.root_capacity {
            self.blocks.pop_first();
        }
        for (key, value) in unique_slots {
            let inserted = self.slots.insert(key, value);
            debug_assert!(inserted, "non-empty verified slot cache accepts an entry");
        }
        Ok(())
    }
}

#[derive(Debug)]
struct AuthenticatedBlockRoots {
    hash: B256,
    roots: BTreeMap<Address, B256>,
}

/// Payload-attempt-scoped reader that consumes verified hits and journals provisional RPC reads.
///
/// Clones belong to the same payload attempt and share its private [`UnverifiedL1Reads`]. Values
/// returned after an RPC miss are provisional until [`verify_and_commit`](Self::verify_and_commit)
/// succeeds.
#[derive(Clone, Debug)]
pub struct PayloadL1StateProvider {
    rpc_client: L1RpcClient,
    verified: VerifiedL1StateCache,
    anchors: Arc<BTreeMap<u64, TrustedL1Anchor>>,
    unverified_reads: Arc<Mutex<UnverifiedL1Reads>>,
}

impl PayloadL1StateProvider {
    /// Creates a verified reader for one payload attempt.
    pub fn new(
        rpc_client: L1RpcClient,
        verified: VerifiedL1StateCache,
        anchors: impl IntoIterator<Item = SealedHeader<TempoHeader>>,
    ) -> Result<Self> {
        let mut by_number = BTreeMap::new();
        for header in anchors {
            let anchor = TrustedL1Anchor {
                block: header.num_hash(),
                state_root: header.state_root(),
            };
            if let Some(existing) = by_number.insert(anchor.block.number, anchor) {
                ensure!(
                    existing.block.hash == anchor.block.hash
                        && existing.state_root == anchor.state_root,
                    "conflicting Tempo anchors at block {}",
                    anchor.block.number
                );
            }
        }
        Ok(Self {
            rpc_client,
            verified,
            anchors: Arc::new(by_number),
            unverified_reads: Default::default(),
        })
    }

    /// Authenticates all provisional reads, atomically promotes them, and returns their count.
    pub fn verify_and_commit(&self) -> Result<usize, L1ReadValidationError> {
        let unverified_reads = std::mem::take(&mut *self.unverified_reads.lock());
        if unverified_reads.is_empty() {
            return Ok(0);
        }
        let started = std::time::Instant::now();
        let result = self.rpc_client.block_on(verify_payload_reads(
            &self.rpc_client,
            &self.verified,
            &self.anchors,
            unverified_reads,
        ));
        self.verified
            .0
            .metrics
            .proof_duration_seconds
            .record(started.elapsed().as_secs_f64());
        match &result {
            Ok(proved) => self
                .verified
                .0
                .metrics
                .proved_slots
                .increment(*proved as u64),
            Err(_) => self.verified.0.metrics.proof_failures.increment(1),
        }
        result
    }

    fn read_verified(&self, account: Address, slot: B256, block_number: u64) -> Result<B256> {
        let storage = StorageReadKey::new(account, slot);
        if let Some(value) = self
            .unverified_reads
            .lock()
            .get(&block_number)
            .and_then(|reads| reads.get(&storage))
            .copied()
        {
            return Ok(value);
        }

        let anchor = self.anchors.get(&block_number).ok_or_else(|| {
            eyre::eyre!("no trusted Tempo header for L1 read at block {block_number}")
        })?;
        let block = anchor.block;
        if let Some(value) = self.verified.get(block, storage) {
            return Ok(value);
        }

        let at = BlockId::hash(block.hash);
        let value = self
            .rpc_client
            .block_on(self.rpc_client.fetch_storage(account, slot, at))?;
        let mut unverified_reads = self.unverified_reads.lock();
        let reads = unverified_reads.entry(block_number).or_default();
        if let Some(observed) = reads.insert(storage, value) {
            ensure!(
                observed == value,
                "conflicting L1 values observed at block {block_number} for account {account} slot {slot}: first {observed}, second {value}"
            );
        }
        Ok(value)
    }
}

impl L1StorageReader for PayloadL1StateProvider {
    fn read_l1_storage(
        &self,
        account: Address,
        slot: B256,
        block_number: u64,
    ) -> std::result::Result<B256, L1StateError> {
        self.read_verified(account, slot, block_number)
            .map_err(|error| L1StateError::StorageUnavailable {
                account,
                slot,
                block_number,
                reason: error.to_string(),
            })
    }
}

/// One account and its requested slot values authenticated by an EIP-1186 proof.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VerifiedAccountState {
    /// Authenticated storage-trie root, normalized for an absent account.
    pub(crate) storage_root: B256,
    /// Authenticated values for every requested raw slot.
    pub(crate) slots: BTreeMap<B256, B256>,
}

#[derive(Clone, Copy, Debug)]
struct TrustedL1Anchor {
    block: NumHash,
    state_root: B256,
}

/// Authenticates an `eth_getMultiProof` response against one global state root.
pub(crate) fn verify_multi_proof(
    state_root: B256,
    targets: &L1ProofTargets,
    responses: Vec<EIP1186AccountProofResponse>,
) -> Result<BTreeMap<Address, VerifiedAccountState>> {
    let mut authenticated = BTreeMap::new();

    for response in responses {
        let address = response.address;
        ensure!(
            !authenticated.contains_key(&address),
            "duplicate account proof for {address}"
        );
        let requested = targets
            .get(&address)
            .ok_or_else(|| eyre::eyre!("unexpected account proof for {address}"))?;
        let proof = AccountProof::from_eip1186_proof(response);
        ensure!(proof.address == address, "account proof address changed");
        proof
            .verify(state_root)
            .map_err(|error| eyre::eyre!("invalid proof for account {address}: {error}"))?;

        let mut slots = BTreeMap::new();
        for storage_proof in &proof.storage_proofs {
            ensure!(
                requested.contains(&storage_proof.key),
                "unexpected storage proof for account {address} slot {}",
                storage_proof.key
            );
            let value = B256::from(storage_proof.value.to_be_bytes::<32>());
            ensure!(
                slots.insert(storage_proof.key, value).is_none(),
                "duplicate storage proof for account {address} slot {}",
                storage_proof.key
            );
        }
        ensure!(
            slots.len() == requested.len(),
            "storage proof targets for account {address} were incomplete"
        );
        authenticated.insert(
            address,
            VerifiedAccountState {
                storage_root: proof.storage_root,
                slots,
            },
        );
    }

    ensure!(
        authenticated.len() == targets.len(),
        "proof response omitted {} requested account(s)",
        targets.len() - authenticated.len()
    );
    Ok(authenticated)
}

/// Builds account-only multiproof targets for authenticating storage roots.
///
/// Each account maps to an empty storage-slot set, so no individual slot proofs are requested.
pub(crate) fn root_proof_targets(accounts: impl IntoIterator<Item = Address>) -> L1ProofTargets {
    accounts
        .into_iter()
        .map(|account| (account, BTreeSet::new()))
        .collect()
}

async fn verify_payload_reads(
    rpc_client: &L1RpcClient,
    verified: &VerifiedL1StateCache,
    anchors: &BTreeMap<u64, TrustedL1Anchor>,
    unverified_reads: UnverifiedL1Reads,
) -> Result<usize, L1ReadValidationError> {
    let target_count = unverified_reads.values().map(BTreeMap::len).sum();
    let requests = unverified_reads
        .into_iter()
        .map(|(block_number, reads)| {
            let anchor = anchors.get(&block_number).copied().ok_or_else(|| {
                L1ReadValidationError::Integrity(format!(
                    "missing trusted Tempo header for block {block_number}"
                ))
            })?;
            let mut targets = L1ProofTargets::new();
            for key in reads.keys() {
                targets.entry(key.account).or_default().insert(key.slot);
            }
            Ok((anchor, reads, targets))
        })
        .collect::<std::result::Result<Vec<_>, L1ReadValidationError>>()?;

    let batches = stream::iter(requests)
        .map(|(anchor, reads, targets)| async move {
            let block = anchor.block;
            let responses = rpc_client
                .get_multi_proof(BlockId::hash(block.hash), &targets)
                .await
                .map_err(|source| L1ReadValidationError::Availability {
                    block_number: block.number,
                    source,
                })?;
            let authenticated = verify_multi_proof(anchor.state_root, &targets, responses)
                .map_err(|error| {
                    L1ReadValidationError::Integrity(format!(
                        "proof verification failed at Tempo block {}: {error}",
                        block.number
                    ))
                })?;
            Ok::<_, L1ReadValidationError>((block, reads, authenticated))
        })
        .buffer_unordered(PROOF_RPC_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;

    let mut roots = Vec::new();
    let mut slots = Vec::with_capacity(target_count);
    for (block, reads, authenticated) in batches {
        for (account, account_state) in authenticated {
            roots.push((block, account, account_state.storage_root));
            for (slot, proved_value) in account_state.slots {
                let storage = StorageReadKey::new(account, slot);
                let observed = reads.get(&storage).ok_or_else(|| {
                    L1ReadValidationError::Integrity(format!(
                        "proof returned unobserved account {account} slot {slot}"
                    ))
                })?;
                if *observed != proved_value {
                    return Err(L1ReadValidationError::Integrity(format!(
                        "L1 value mismatch at block {} for account {account} slot {slot}: execution used {observed}, proof authenticates {proved_value}",
                        block.number
                    )));
                }
                slots.push(((storage, account_state.storage_root), proved_value));
            }
        }
    }

    verified
        .commit_payload(roots, slots)
        .map_err(|error| L1ReadValidationError::Integrity(error.to_string()))?;
    Ok(target_count)
}

/// Failure while authenticating the L1 values consumed by a payload.
#[derive(Debug, Error)]
pub enum L1ReadValidationError {
    /// Proof material could not be fetched.
    #[error("could not fetch L1 proof at Tempo block {block_number}: {source}")]
    Availability {
        /// Tempo block whose proof request failed.
        block_number: u64,
        /// Underlying provider error.
        #[source]
        source: eyre::Report,
    },
    /// Proof material or the value consumed by execution failed an integrity check.
    #[error("L1 read integrity failure: {0}")]
    Integrity(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::L1StateCache;
    use alloy_consensus::{
        Header,
        constants::{EMPTY_ROOT_HASH, KECCAK_EMPTY},
    };
    use alloy_primitives::U256;
    use alloy_provider::{Provider as _, ProviderBuilder};
    use alloy_rpc_types_eth::EIP1186StorageProof;
    use alloy_transport::mock::Asserter;
    use tempo_alloy::TempoNetwork;

    fn empty_account_response(
        address: Address,
        slots: impl IntoIterator<Item = B256>,
    ) -> EIP1186AccountProofResponse {
        EIP1186AccountProofResponse {
            address,
            code_hash: KECCAK_EMPTY,
            storage_hash: EMPTY_ROOT_HASH,
            storage_proof: slots
                .into_iter()
                .map(|slot| EIP1186StorageProof {
                    key: slot.into(),
                    value: Default::default(),
                    proof: Vec::new(),
                })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn verified_cache_reuses_values_only_under_the_authenticated_root() {
        let state = VerifiedL1StateCache::with_limits(10, 10);
        let account = Address::repeat_byte(0x11);
        let slot = B256::with_last_byte(1);
        let storage = StorageReadKey::new(account, slot);
        let root_a = B256::with_last_byte(0xaa);
        let root_b = B256::with_last_byte(0xbb);
        let block_a = NumHash::new(1, B256::with_last_byte(1));
        let block_b = NumHash::new(2, B256::with_last_byte(2));
        let block_c = NumHash::new(3, B256::with_last_byte(3));
        let value = B256::with_last_byte(0x42);

        state
            .commit_payload([(block_a, account, root_a)], [((storage, root_a), value)])
            .unwrap();
        let changed_b = state.record_verified_roots(block_b, [(account, root_a)]);
        assert!(changed_b.unwrap().is_empty());
        let changed_c = state.record_verified_roots(block_c, [(account, root_b)]);
        assert_eq!(changed_c.unwrap(), BTreeSet::from([account]));

        assert_eq!(state.get(block_a, storage), Some(value));
        assert_eq!(state.get(block_b, storage), Some(value));
        assert_eq!(state.get(block_c, storage), None);
    }

    #[test]
    fn authenticated_root_changes_drive_ordinary_cache_invalidation() {
        let verified = VerifiedL1StateCache::with_limits(10, 10);
        let ordinary = L1StateCache::new();
        let account = Address::repeat_byte(0x11);
        let slot = B256::with_last_byte(1);
        let root_a = B256::with_last_byte(0xaa);
        let root_b = B256::with_last_byte(0xbb);
        let value = B256::with_last_byte(0x42);

        let changed = verified
            .record_verified_roots(
                NumHash::new(1, B256::with_last_byte(1)),
                [(account, root_a)],
            )
            .unwrap();
        ordinary.lock().invalidate_and_set_anchor(1, changed);
        ordinary.lock().set(account, slot, 1, value);

        let changed = verified
            .record_verified_roots(
                NumHash::new(2, B256::with_last_byte(2)),
                [(account, root_a)],
            )
            .unwrap();
        ordinary.lock().invalidate_and_set_anchor(2, changed);
        assert_eq!(ordinary.lock().get(account, slot, 2), Some(value));

        let changed = verified
            .record_verified_roots(
                NumHash::new(3, B256::with_last_byte(3)),
                [(account, root_b)],
            )
            .unwrap();
        ordinary.lock().invalidate_and_set_anchor(3, changed);
        assert_eq!(ordinary.lock().get(account, slot, 3), None);
    }

    #[test]
    fn partial_payload_commit_is_rejected_before_mutation() {
        let state = VerifiedL1StateCache::with_limits(10, 10);
        let account = Address::repeat_byte(0x11);
        let storage = StorageReadKey::new(account, B256::ZERO);
        let root = B256::with_last_byte(1);
        let block = NumHash::new(1, B256::with_last_byte(2));
        let key = (storage, root);
        state
            .commit_payload([(block, account, root)], [(key, B256::ZERO)])
            .unwrap();

        assert!(
            state
                .commit_payload([], [(key, B256::with_last_byte(1))])
                .is_err()
        );
        assert_eq!(state.slot_count(), 1);
        assert_eq!(state.get(block, storage), Some(B256::ZERO));
    }

    #[test]
    fn conflicting_payload_roots_are_rejected_before_any_mutation() {
        let state = VerifiedL1StateCache::with_limits(10, 10);
        let account = Address::repeat_byte(0x11);
        let block = NumHash::new(1, B256::with_last_byte(1));
        let result = state.commit_payload(
            [
                (block, account, B256::with_last_byte(2)),
                (block, account, B256::with_last_byte(3)),
            ],
            [],
        );

        assert!(result.is_err());
        assert!(state.0.state.lock().blocks.is_empty());
    }

    #[test]
    fn authenticates_empty_account_and_requested_zero_slots() {
        let account = Address::repeat_byte(0x11);
        let slots = BTreeSet::from([B256::with_last_byte(1), B256::with_last_byte(2)]);
        let targets = BTreeMap::from([(account, slots.clone())]);
        let verified = verify_multi_proof(
            EMPTY_ROOT_HASH,
            &targets,
            vec![empty_account_response(account, slots)],
        )
        .unwrap();

        let account_state = &verified[&account];
        assert_eq!(account_state.storage_root, EMPTY_ROOT_HASH);
        assert!(account_state.slots.values().all(|value| value.is_zero()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn payload_miss_is_proved_promoted_and_reused_without_another_rpc() {
        let account = Address::repeat_byte(0x11);
        let slot = B256::with_last_byte(1);
        let header = SealedHeader::seal_slow(TempoHeader {
            inner: Header {
                number: 7,
                state_root: EMPTY_ROOT_HASH,
                ..Default::default()
            },
            ..Default::default()
        });
        let asserter = Asserter::new();
        asserter.push_success(&U256::ZERO);
        asserter.push_success(&vec![empty_account_response(account, [slot])]);
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter.clone())
            .erased();
        let rpc_client = L1RpcClient::from_provider(provider, tokio::runtime::Handle::current());
        let verified = VerifiedL1StateCache::new();
        let reader =
            PayloadL1StateProvider::new(rpc_client.clone(), verified.clone(), [header.clone()])
                .unwrap();

        let execution_reader = reader.clone();
        assert_eq!(
            tokio::task::spawn_blocking(move || {
                let first = execution_reader.read_l1_storage(account, slot, 7)?;
                let repeated = execution_reader.read_l1_storage(account, slot, 7)?;
                assert_eq!(repeated, first);
                Ok::<_, L1StateError>(first)
            })
            .await
            .unwrap()
            .unwrap(),
            B256::ZERO
        );
        let verifier = reader.clone();
        assert_eq!(
            tokio::task::spawn_blocking(move || verifier.verify_and_commit())
                .await
                .unwrap()
                .unwrap(),
            1
        );

        let cache_reader = PayloadL1StateProvider::new(rpc_client, verified, [header]).unwrap();
        assert_eq!(
            tokio::task::spawn_blocking(move || { cache_reader.read_l1_storage(account, slot, 7) })
                .await
                .unwrap()
                .unwrap(),
            B256::ZERO
        );
        assert!(asserter.read_q().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proof_value_mismatch_does_not_promote_the_miss() {
        let account = Address::repeat_byte(0x11);
        let slot = B256::with_last_byte(1);
        let header = SealedHeader::seal_slow(TempoHeader {
            inner: Header {
                number: 7,
                state_root: EMPTY_ROOT_HASH,
                ..Default::default()
            },
            ..Default::default()
        });
        let asserter = Asserter::new();
        asserter.push_success(&U256::ONE);
        asserter.push_success(&vec![empty_account_response(account, [slot])]);
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter)
            .erased();
        let rpc_client = L1RpcClient::from_provider(provider, tokio::runtime::Handle::current());
        let verified = VerifiedL1StateCache::new();
        let reader = PayloadL1StateProvider::new(rpc_client, verified.clone(), [header]).unwrap();

        let execution_reader = reader.clone();
        assert_eq!(
            tokio::task::spawn_blocking(move || {
                execution_reader.read_l1_storage(account, slot, 7)
            })
            .await
            .unwrap()
            .unwrap(),
            B256::from(U256::ONE.to_be_bytes::<32>())
        );
        let verifier = reader.clone();
        assert!(
            tokio::task::spawn_blocking(move || verifier.verify_and_commit())
                .await
                .unwrap()
                .is_err()
        );
        assert_eq!(verified.slot_count(), 0);
    }

    #[test]
    fn rejects_missing_duplicate_and_unexpected_proof_targets() {
        let account = Address::repeat_byte(0x11);
        let slot = B256::with_last_byte(1);
        let targets = BTreeMap::from([(account, BTreeSet::from([slot]))]);

        assert!(verify_multi_proof(EMPTY_ROOT_HASH, &targets, vec![]).is_err());
        assert!(
            verify_multi_proof(
                EMPTY_ROOT_HASH,
                &targets,
                vec![
                    empty_account_response(account, [slot]),
                    empty_account_response(account, [slot]),
                ],
            )
            .is_err()
        );
        assert!(
            verify_multi_proof(
                EMPTY_ROOT_HASH,
                &targets,
                vec![empty_account_response(account, [B256::with_last_byte(2)])],
            )
            .is_err()
        );
    }
}
