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
//! `block_number → value`. A lookup for block N may inherit the most recent earlier value only
//! when verified receipt coverage and mutation barriers prove that it remained current.
//!
//! ## Write path
//!
//! - The [`L1Subscriber`](crate::l1::L1Subscriber) writes storage diffs for tracked contracts
//!   as they arrive, tagged with the L1 tip block number.
//! - The [`L1StateProvider`](super::provider::L1StateProvider) writes RPC-fetched values on
//!   cache miss, tagged with the block number that was requested.
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
/// Inherited values are valid only when the subscriber has processed every intervening L1 block
/// and no log from the owning contract indicates a possible mutation. The non-advancing floor
/// excludes historical reads from before the subscriber's contiguous observation range.
///
/// The anchor tracks the latest L1 block whose receipts the
/// [`L1Subscriber`](crate::l1::L1Subscriber) has processed, and is also used for reorg detection.
#[derive(Debug, Default)]
pub struct L1StateCacheInner {
    tracked_contracts: HashSet<Address>,
    /// Per-slot value history: `(address, slot) → { block_number → value }`.
    /// The `BTreeMap` enables efficient range lookups for "latest value at or before block N".
    slots: HashMap<(Address, B256), BTreeMap<u64, B256>>,
    /// Per-address mutation barriers. A value at V cannot serve a read at N when a barrier exists
    /// in `(V, N]`.
    invalidations: HashMap<Address, BTreeSet<u64>>,
    /// Initial canonical Zone L1 anchor. Values below it are never admitted to the forward cache.
    block_floor: u64,
    /// Latest contiguous L1 block whose receipts the subscriber has processed.
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
    /// An exact-height value is always valid. A value inherited from an earlier height is returned
    /// only when the subscriber has processed receipts through `block_number` and no mutation
    /// barrier exists after the value was populated.
    pub fn get(&self, address: Address, slot: B256, block_number: u64) -> Option<B256> {
        let (&cached_block, &value) = self
            .slots
            .get(&(address, slot))?
            .range(..=block_number)
            .next_back()?;
        let invalidated = self
            .invalidations
            .get(&address)
            .and_then(|blocks| blocks.range(..=block_number).next_back())
            .is_some_and(|&invalidated_at| invalidated_at > cached_block);

        let is_exact = cached_block == block_number;
        let can_inherit = block_number <= self.anchor.number && !invalidated;
        let is_cache_valid = cached_block >= self.block_floor && (is_exact || can_inherit);
        is_cache_valid.then_some(value)
    }

    /// Sets a storage slot value in the forward cache at the given block number.
    ///
    /// Values below the initial canonical floor are deliberately not admitted, so historical
    /// reads cannot later be inherited by canonical execution.
    pub fn set(&mut self, address: Address, slot: B256, block_number: u64, value: B256) {
        if block_number < self.block_floor {
            return;
        }
        self.slots
            .entry((address, slot))
            .or_default()
            .insert(block_number, value);
    }

    /// Prevents values populated before the current contiguous receipt-coverage baseline from
    /// entering the forward cache. Initialized at startup and rebased after a coverage reset.
    pub fn initialize_floor(&mut self, block_number: u64) {
        self.block_floor = self.block_floor.max(block_number);
    }

    /// Returns the non-advancing cache floor.
    pub const fn block_floor(&self) -> u64 {
        self.block_floor
    }

    /// Invalidates inherited values for `address` starting at `block_number`.
    pub fn invalidate(&mut self, address: Address, block_number: u64) {
        self.invalidations
            .entry(address)
            .or_default()
            .insert(block_number);
    }

    /// Updates the latest contiguous block whose receipts have been processed.
    pub fn update_anchor(&mut self, anchor: NumHash) {
        self.anchor = anchor;
    }

    /// Returns `true` if the given address is one of the tracked contracts.
    pub fn is_tracked(&self, address: &Address) -> bool {
        self.tracked_contracts.contains(address)
    }

    /// Clears chain-derived data while retaining the tracked-contract set and initial floor.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.invalidations.clear();
        self.anchor = NumHash::default();
    }

    /// Remove all entries with block numbers strictly less than `min_block`.
    ///
    /// Retains at most one entry per slot below the threshold — the latest one — so that
    /// lookups at `min_block` still have a baseline value.
    pub fn prune_before(&mut self, min_block: u64) {
        for history in self.slots.values_mut() {
            let keep_from = history.range(..min_block).next_back().map(|(k, _)| *k);

            if let Some(keep) = keep_from {
                let to_remove: Vec<u64> = history.range(..keep).map(|(k, _)| *k).collect();
                for k in to_remove {
                    history.remove(&k);
                }
            }
        }

        self.slots.retain(|_, history| !history.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const PORTAL: Address = address!("0x0000000000000000000000000000000000004242");

    fn cover_through(cache: &mut L1StateCacheInner, block_number: u64) {
        cache.update_anchor(NumHash::new(block_number, B256::with_last_byte(1)));
    }

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

        cache.set(PORTAL, slot, 10, value);
        assert_eq!(cache.get(PORTAL, slot, 10), Some(value));
    }

    #[test]
    fn get_returns_latest_value_at_or_before_requested_block() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        let slot = B256::with_last_byte(1);

        cache.set(PORTAL, slot, 10, B256::with_last_byte(0x0a));
        cache.set(PORTAL, slot, 20, B256::with_last_byte(0x14));
        cover_through(&mut cache, 25);

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

        cache.set(PORTAL, slot, 10, B256::with_last_byte(0xff));
        assert_eq!(cache.get(PORTAL, slot, 9), None);
    }

    #[test]
    fn inherited_value_requires_receipt_coverage() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        let slot = B256::with_last_byte(1);
        let value = B256::with_last_byte(0x0a);
        cache.set(PORTAL, slot, 10, value);

        assert_eq!(cache.get(PORTAL, slot, 11), None);
        cover_through(&mut cache, 11);
        assert_eq!(cache.get(PORTAL, slot, 11), Some(value));
    }

    #[test]
    fn invalidation_blocks_inheritance_until_refetched() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        let slot = B256::with_last_byte(1);
        let old = B256::with_last_byte(0x0a);
        let new = B256::with_last_byte(0x0b);
        cache.set(PORTAL, slot, 10, old);
        cache.invalidate(PORTAL, 11);
        cover_through(&mut cache, 12);

        assert_eq!(cache.get(PORTAL, slot, 11), None);
        assert_eq!(cache.get(PORTAL, slot, 12), None);

        cache.set(PORTAL, slot, 11, new);
        assert_eq!(cache.get(PORTAL, slot, 11), Some(new));
        assert_eq!(cache.get(PORTAL, slot, 12), Some(new));
    }

    #[test]
    fn floor_rejects_historical_cache_admission() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        let slot = B256::with_last_byte(1);
        cache.initialize_floor(10);
        cache.set(PORTAL, slot, 9, B256::with_last_byte(9));
        cover_through(&mut cache, 11);

        assert_eq!(cache.get(PORTAL, slot, 9), None);
        assert_eq!(cache.get(PORTAL, slot, 11), None);

        let current = B256::with_last_byte(10);
        cache.set(PORTAL, slot, 10, current);
        assert_eq!(cache.get(PORTAL, slot, 11), Some(current));
    }

    #[test]
    fn clear_removes_chain_data_and_preserves_floor() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));

        cache.initialize_floor(90);
        cache.set(PORTAL, B256::ZERO, 100, B256::with_last_byte(1));
        cache.invalidate(PORTAL, 101);
        cache.update_anchor(NumHash {
            number: 100,
            hash: B256::with_last_byte(0xab),
        });

        cache.clear();

        assert_eq!(cache.get(PORTAL, B256::ZERO, 100), None);
        assert!(cache.invalidations.is_empty());
        assert_eq!(cache.anchor, NumHash::default());
        assert_eq!(cache.block_floor(), 90);
    }

    #[test]
    fn anchor_defaults_to_zero() {
        let cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        assert_eq!(cache.anchor, NumHash::default());
    }

    #[test]
    fn update_anchor() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        let hash = B256::with_last_byte(0xbe);
        cache.update_anchor(NumHash { number: 42, hash });
        assert_eq!(cache.anchor, NumHash { number: 42, hash });
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

        cache.set(PORTAL, slot, 10, B256::with_last_byte(0xaa));
        cache.set(addr_b, slot, 10, B256::with_last_byte(0xbb));

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
    fn prune_keeps_baseline_entry() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        let slot = B256::with_last_byte(1);

        cache.set(PORTAL, slot, 5, B256::with_last_byte(0x05));
        cache.set(PORTAL, slot, 10, B256::with_last_byte(0x0a));
        cache.set(PORTAL, slot, 20, B256::with_last_byte(0x14));
        cover_through(&mut cache, 20);

        cache.prune_before(15);

        assert_eq!(cache.get(PORTAL, slot, 5), None);
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
    }
}
