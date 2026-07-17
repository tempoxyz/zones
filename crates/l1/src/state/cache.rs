//! Block-versioned in-memory cache of Tempo L1 contract storage slots.
//!
//! The zone's `TempoState` precompile reads Tempo L1 storage at a **specific L1 block height**
//! (the `tempoBlockNumber` the zone committed to via `TempoState.finalizeTempo()` on Zone L2).
//! Because the L1 chain may advance several blocks ahead of the zone's committed height, the
//! cache must be able to serve historical values — not just "latest".
//!
//! ## Storage model
//!
//! Each `(contract_address, slot_key)` pair maps to a [`BTreeMap<u64, B256>`] of
//! `block_number → value`. A lookup for block N returns the most recent entry whose
//! block number is ≤ N, reflecting the value that was current at that height.
//!
//! ## Write path
//!
//! - The [`L1Subscriber`](crate::l1::L1Subscriber) writes storage diffs for tracked contracts
//!   as they arrive, tagged with the L1 tip block number.
//! - The [`L1StateProvider`](super::provider::L1StateProvider) writes eligible forward RPC
//!   misses, tagged with the requested block number. Misses below the canonical Zone anchor
//!   floor are returned without being inserted.
//!
//! ## Reorg handling
//!
//! On reorgs the caller is expected to [`L1StateCacheInner::clear`] the entire cache and
//! re-populate from the new canonical chain segment. There is no per-block rollback.

use alloy_eips::NumHash;
use alloy_primitives::{Address, B256};
use derive_more::Deref;
use parking_lot::RwLock;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
};

/// Thread-safe L1 state cache backed by an `Arc<RwLock<L1StateCacheInner>>`.
#[derive(Debug, Clone, Deref, Default)]
pub struct L1StateCache {
    #[deref]
    inner: Arc<RwLock<L1StateCacheInner>>,
}

impl L1StateCache {
    /// Create a new cache tracking the given contract addresses.
    pub fn new(tracked_contracts: HashSet<Address>) -> Self {
        Self {
            inner: Arc::new(RwLock::new(L1StateCacheInner::new(tracked_contracts))),
        }
    }
}

/// Block-versioned cache of Tempo L1 contract storage slots.
///
/// Each `(contract_address, slot_key)` pair maintains a history of values indexed by L1 block
/// number. Lookups for a given block return the most recent value at or before that block,
/// i.e. the value that was current at that height. This allows the zone to read L1 state at
/// the `tempoBlockNumber` it committed to, even if the L1 chain has since advanced.
///
/// Contract mutation barriers prevent values from crossing blocks where logs signal a possible
/// storage change without requiring slot-level decoding. Reorgs clear slot values and mutation
/// history atomically.
///
/// The subscriber anchor and canonical floor track independent progress. The anchor is the latest
/// confirmed L1 block observed by the subscriber and may run ahead while blocks are queued. The
/// floor is the latest L1 height committed by canonical Zone execution. It advances monotonically
/// and drives lazy history compaction without scanning the cache on the import path.
#[derive(Debug, Default)]
pub struct L1StateCacheInner {
    tracked_contracts: HashSet<Address>,
    /// Per-slot value history: `(address, slot) → { block_number → value }`.
    /// The `BTreeMap` enables efficient range lookups for "latest value at or before block N".
    slots: HashMap<(Address, B256), BTreeMap<u64, B256>>,
    /// Per-address mutation barriers: `address → { block_number }`.
    /// A slot value cached at block V may serve block N only when no barrier exists in `(V, N]`.
    /// The subscriber records barriers for contracts whose logs imply possible storage changes.
    invalidations: HashMap<Address, BTreeSet<u64>>,
    /// Latest L1 block height committed by canonical Zone execution.
    ///
    /// New fallback values below this floor are not admitted. Histories are compacted lazily
    /// against it when their slot/address is next mutated, so older entries may remain physically
    /// present until touched. This floor may lag the subscriber [`anchor`](Self::anchor).
    block_floor: u64,
    /// Latest confirmed L1 block observed by the subscriber, used for reorg detection.
    ///
    /// The anchor may run ahead of [`block_floor`](Self::block_floor) while L1 blocks are queued.
    anchor: NumHash,
}

impl L1StateCacheInner {
    /// Create a new cache tracking the given contract addresses.
    pub fn new(tracked_contracts: HashSet<Address>) -> Self {
        Self {
            tracked_contracts,
            ..Default::default()
        }
    }

    /// Returns the cached value for a storage slot at the given block number.
    ///
    /// Returns `None` below the canonical floor because pruned mutation history cannot safely serve
    /// historical inheritance. Otherwise returns the most recent valid value at or before
    /// `block_number`.
    pub fn get(&self, address: Address, slot: B256, block_number: u64) -> Option<B256> {
        if block_number < self.block_floor {
            return None;
        }

        let (value_block, value) = self
            .slots
            .get(&(address, slot))?
            .range(..=block_number)
            .next_back()?;
        let latest_invalidation = self
            .invalidations
            .get(&address)
            .and_then(|blocks| blocks.range(..=block_number).next_back())
            .copied();
        if latest_invalidation.is_some_and(|invalidated_at| invalidated_at > *value_block) {
            return None;
        }
        Some(*value)
    }

    /// Invalidates inherited slot values for `address` starting at `block_number`.
    ///
    /// Values subsequently inserted at the same block are post-block state and remain valid.
    pub fn invalidate(&mut self, address: Address, block_number: u64) {
        let blocks = self.invalidations.entry(address).or_default();
        blocks.insert(block_number);
        prune_invalidation_history(blocks, self.block_floor);
    }

    /// Sets a storage slot value in the forward cache at the given block number.
    ///
    /// Returns `false` without inserting when `block_number` is below the canonical Zone anchor
    /// floor. Callers that materialize synthetic/test state should check the result so a rejected
    /// seed cannot turn into an unexpected RPC fallback.
    #[must_use = "check whether the cache write was admitted above the block floor"]
    pub fn set(&mut self, address: Address, slot: B256, block_number: u64, value: B256) -> bool {
        if block_number < self.block_floor {
            return false;
        }

        let history = self.slots.entry((address, slot)).or_default();
        history.insert(block_number, value);
        prune_slot_history(history, self.block_floor);
        true
    }

    /// Advances the canonical Zone anchor floor monotonically in O(1).
    pub fn advance_floor(&mut self, block_number: u64) {
        self.block_floor = self.block_floor.max(block_number);
    }

    /// Returns the latest L1 height consumed by canonical Zone execution.
    pub fn block_floor(&self) -> u64 {
        self.block_floor
    }

    /// Updates the latest confirmed L1 block observed by the subscriber.
    pub fn update_anchor(&mut self, anchor: NumHash) {
        self.anchor = anchor;
    }

    /// Returns the latest confirmed L1 block observed by the subscriber.
    pub fn anchor(&self) -> NumHash {
        self.anchor
    }

    /// Adds an address to the conservative mutation-tracking set.
    ///
    /// Returns `true` when the address was newly inserted. Tracking is retained across
    /// [`clear`](Self::clear), so tokens discovered from L1 remain safe across reconnects and reorgs.
    pub fn track(&mut self, address: Address) -> bool {
        self.tracked_contracts.insert(address)
    }

    /// Returns `true` if the given address is one of the tracked contracts.
    pub fn is_tracked(&self, address: &Address) -> bool {
        self.tracked_contracts.contains(address)
    }

    /// Clears subscriber-derived chain data while retaining tracked contracts and the canonical floor.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.invalidations.clear();
        self.anchor = NumHash::default();
    }
}

/// Retains the latest pre-floor entry as a baseline and every newer entry.
fn prune_slot_history<V>(history: &mut BTreeMap<u64, V>, min_block: u64) {
    if history.range(..min_block).nth(1).is_none() {
        return;
    }

    let mut retained = history.split_off(&min_block);
    if let Some(baseline) = history.pop_last() {
        retained.insert(baseline.0, baseline.1);
    }
    *history = retained;
}

/// Retains the latest pre-floor barrier and every newer barrier.
fn prune_invalidation_history(history: &mut BTreeSet<u64>, min_block: u64) {
    if history.range(..min_block).nth(1).is_none() {
        return;
    }

    let mut retained = history.split_off(&min_block);
    if let Some(baseline) = history.pop_last() {
        retained.insert(baseline);
    }
    *history = retained;
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const PORTAL: Address = address!("0x0000000000000000000000000000000000004242");

    #[test]
    fn get_returns_none_for_missing_slot() {
        let cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        assert_eq!(cache.get(PORTAL, B256::ZERO, 100), None);
    }

    #[test]
    fn set_and_get_at_same_block() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        let slot = B256::with_last_byte(1);
        let value = B256::with_last_byte(0xff);

        assert!(cache.set(PORTAL, slot, 10, value));
        assert_eq!(cache.get(PORTAL, slot, 10), Some(value));
    }

    #[test]
    fn set_reports_rejection_below_floor() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        let slot = B256::with_last_byte(1);
        cache.advance_floor(10);

        assert!(!cache.set(PORTAL, slot, 9, B256::with_last_byte(0xff)));
        assert_eq!(cache.get(PORTAL, slot, 10), None);
    }

    #[test]
    fn get_returns_latest_value_at_or_before_requested_block() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        let slot = B256::with_last_byte(1);

        assert!(cache.set(PORTAL, slot, 10, B256::with_last_byte(0x0a)));
        assert!(cache.set(PORTAL, slot, 20, B256::with_last_byte(0x14)));

        assert_eq!(
            cache.get(PORTAL, slot, 10),
            Some(B256::with_last_byte(0x0a))
        );
        assert_eq!(
            cache.get(PORTAL, slot, 15),
            Some(B256::with_last_byte(0x0a))
        );
        assert_eq!(
            cache.get(PORTAL, slot, 20),
            Some(B256::with_last_byte(0x14))
        );
        assert_eq!(
            cache.get(PORTAL, slot, 25),
            Some(B256::with_last_byte(0x14))
        );
    }

    #[test]
    fn get_returns_none_before_earliest_entry() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        let slot = B256::with_last_byte(1);

        assert!(cache.set(PORTAL, slot, 10, B256::with_last_byte(0xff)));
        assert_eq!(cache.get(PORTAL, slot, 9), None);
    }

    #[test]
    fn clear_removes_chain_data_but_preserves_canonical_floor() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));

        let discovered_token = Address::with_last_byte(0x44);
        assert!(cache.track(discovered_token));
        assert!(cache.set(PORTAL, B256::ZERO, 100, B256::with_last_byte(1)));
        cache.invalidate(PORTAL, 101);
        cache.advance_floor(100);
        cache.update_anchor(NumHash {
            number: 100,
            hash: B256::with_last_byte(0xab),
        });

        cache.clear();

        assert_eq!(cache.get(PORTAL, B256::ZERO, 100), None);
        assert!(cache.invalidations.is_empty());
        assert_eq!(cache.block_floor, 100);
        assert_eq!(cache.anchor(), NumHash::default());
        assert!(cache.is_tracked(&discovered_token));
    }

    #[test]
    fn invalidation_blocks_inheritance_until_slot_is_refetched() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        let slot = B256::with_last_byte(1);
        let old = B256::with_last_byte(0x0a);
        let new = B256::with_last_byte(0x14);
        let other = Address::with_last_byte(0x43);

        assert!(cache.set(PORTAL, slot, 10, old));
        assert!(cache.set(other, slot, 10, old));
        cache.invalidate(PORTAL, 20);
        cache.invalidate(PORTAL, 20); // Multiple logs in one block deduplicate.

        assert_eq!(cache.get(PORTAL, slot, 19), Some(old));
        assert_eq!(cache.get(PORTAL, slot, 20), None);
        assert_eq!(cache.get(PORTAL, slot, 30), None);
        assert_eq!(cache.get(other, slot, 30), Some(old));
        assert_eq!(cache.invalidations[&PORTAL].len(), 1);

        assert!(cache.set(PORTAL, slot, 20, new));
        assert_eq!(cache.get(PORTAL, slot, 20), Some(new));
        assert_eq!(cache.get(PORTAL, slot, 30), Some(new));

        cache.invalidate(PORTAL, 31);
        assert_eq!(cache.get(PORTAL, slot, 31), None);
    }

    #[test]
    fn anchor_defaults_to_zero() {
        let cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        assert_eq!(cache.anchor(), NumHash::default());
    }

    #[test]
    fn update_anchor() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        let hash = B256::with_last_byte(0xbe);
        cache.update_anchor(NumHash { number: 42, hash });
        assert_eq!(cache.anchor(), NumHash { number: 42, hash });
    }

    #[test]
    fn is_tracked_returns_true_for_portal() {
        let cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        assert!(cache.is_tracked(&PORTAL));
    }

    #[test]
    fn is_tracked_returns_false_for_unknown_address() {
        let cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        assert!(!cache.is_tracked(&address!("0x0000000000000000000000000000000000000001")));
    }

    #[test]
    fn different_addresses_same_slot_are_independent() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        let addr_b = address!("0x0000000000000000000000000000000000004343");
        let slot = B256::with_last_byte(1);

        assert!(cache.set(PORTAL, slot, 10, B256::with_last_byte(0xaa)));
        assert!(cache.set(addr_b, slot, 10, B256::with_last_byte(0xbb)));

        assert_eq!(
            cache.get(PORTAL, slot, 10),
            Some(B256::with_last_byte(0xaa))
        );
        assert_eq!(
            cache.get(addr_b, slot, 10),
            Some(B256::with_last_byte(0xbb))
        );
    }

    #[test]
    fn advance_floor_is_lazy_and_touched_histories_keep_their_baselines() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        let slot = B256::with_last_byte(1);

        assert!(cache.set(PORTAL, slot, 5, B256::with_last_byte(0x05)));
        assert!(cache.set(PORTAL, slot, 10, B256::with_last_byte(0x0a)));
        assert!(cache.set(PORTAL, slot, 20, B256::with_last_byte(0x14)));
        cache.invalidate(PORTAL, 5);
        cache.invalidate(PORTAL, 12);
        cache.invalidate(PORTAL, 18);

        cache.advance_floor(15);

        // Advancing only moves the floor.
        assert_eq!(
            cache.slots[&(PORTAL, slot)]
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            vec![5, 10, 20]
        );
        assert_eq!(cache.get(PORTAL, slot, 10), None);
        assert_eq!(cache.get(PORTAL, slot, 15), None);
        assert_eq!(cache.get(PORTAL, slot, 19), None);
        assert_eq!(
            cache.get(PORTAL, slot, 20),
            Some(B256::with_last_byte(0x14))
        );

        // A historical fallback cannot repopulate the forward cache.
        assert!(!cache.set(PORTAL, slot, 14, B256::with_last_byte(0xee)));
        assert!(!cache.slots[&(PORTAL, slot)].contains_key(&14));

        // Touching one slot/address compacts only that history and preserves its baseline.
        assert!(cache.set(PORTAL, slot, 15, B256::with_last_byte(0x0f)));
        cache.invalidate(PORTAL, 21);
        assert_eq!(cache.get(PORTAL, slot, 5), None);
        assert_eq!(
            cache.slots[&(PORTAL, slot)]
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            vec![10, 15, 20]
        );
        assert_eq!(
            cache.invalidations[&PORTAL]
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![12, 18, 21]
        );
        assert_eq!(
            cache.get(PORTAL, slot, 15),
            Some(B256::with_last_byte(0x0f))
        );
    }

    #[test]
    fn below_floor_lookup_misses_after_invalidation_pruning() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        let slot = B256::with_last_byte(1);

        assert!(cache.set(PORTAL, slot, 5, B256::with_last_byte(0x05)));
        cache.invalidate(PORTAL, 10);
        cache.invalidate(PORTAL, 20);
        cache.advance_floor(30);
        cache.invalidate(PORTAL, 40);

        assert_eq!(
            cache.invalidations[&PORTAL]
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![20, 40]
        );
        assert_eq!(cache.get(PORTAL, slot, 15), None);
    }
}
