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
use tempo_chainspec::hardfork::TempoHardfork;

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
/// To enable delegation to upstream Tempo precompiles, the cache also tracks contract mutation
/// barriers and hardfork activations. Barriers prevent values from crossing blocks where logs
/// signal a possible storage change without requiring slot-level decoding, while hardfork metadata
/// selects the Tempo rules active at the same anchor. Reorgs clear all three atomically: slot
/// values, mutation history, and protocol-version metadata.
///
/// The anchor tracks the latest L1 block the cache has received data for, used by the
/// [`L1Subscriber`](crate::l1::L1Subscriber) for reorg detection.
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
    /// First canonical L1 block where each observed Tempo hardfork is active.
    hardfork_schedule: BTreeSet<(u64, TempoHardfork)>,
    /// Highest block covered by the cached activation schedule.
    hardfork_schedule_head: Option<u64>,
    /// Latest L1 block the cache has received data for, used for reorg detection.
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
    /// Returns the most recent value at or before `block_number`, or `None` if no
    /// value has been cached for this slot at or before the requested block.
    pub fn get(&self, address: Address, slot: B256, block_number: u64) -> Option<B256> {
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
        self.invalidations
            .entry(address)
            .or_default()
            .insert(block_number);
    }

    /// Sets a storage slot value in the cache at the given block number.
    pub fn set(&mut self, address: Address, slot: B256, block_number: u64, value: B256) {
        self.slots
            .entry((address, slot))
            .or_default()
            .insert(block_number, value);
    }

    /// Returns the active Tempo hardfork when the cached schedule covers `block_number`.
    pub fn hardfork_at(&self, block_number: u64) -> Option<TempoHardfork> {
        if self.hardfork_schedule_head? < block_number {
            return None;
        }
        self.hardfork_schedule
            .range(..=(block_number, TempoHardfork::T9))
            .next_back()
            .map(|(_, hardfork)| *hardfork)
    }

    /// Extends the known Tempo activation schedule through `block_number`.
    pub fn extend_hardfork_schedule(
        &mut self,
        block_number: u64,
        activations: impl IntoIterator<Item = (u64, TempoHardfork)>,
    ) {
        self.hardfork_schedule.extend(activations);
        self.hardfork_schedule_head = Some(
            self.hardfork_schedule_head
                .unwrap_or_default()
                .max(block_number),
        );
    }

    /// Advance an initialized schedule from a contiguous confirmed L1 block.
    ///
    /// A single observation cannot initialize historical activation boundaries. Until a full
    /// schedule has been resolved, observations are ignored and the provider remains responsible
    /// for initialization through canonical constants/RPC.
    pub fn observe_hardfork(&mut self, block_number: u64, hardfork: TempoHardfork) {
        let Some(head) = self.hardfork_schedule_head else {
            return;
        };
        if block_number != head.saturating_add(1) {
            return;
        }

        let previous = self
            .hardfork_schedule
            .range(..=(head, TempoHardfork::T9))
            .next_back()
            .map(|(_, hardfork)| *hardfork);
        let Some(previous) = previous else {
            return;
        };
        if hardfork < previous {
            // Tempo hardforks never downgrade. Leave the head unchanged so the provider resolves
            // this block instead of extending the cache from stale/incorrect chain metadata.
            return;
        }
        if hardfork > previous {
            self.hardfork_schedule.insert((block_number, hardfork));
        }
        self.hardfork_schedule_head = Some(block_number);
    }

    /// Updates the anchor block that this cache has received data up to.
    pub fn update_anchor(&mut self, anchor: NumHash) {
        self.anchor = anchor;
    }

    /// Returns the current anchor block.
    pub fn anchor(&self) -> NumHash {
        self.anchor
    }

    /// Returns `true` if the given address is one of the tracked contracts.
    pub fn is_tracked(&self, address: &Address) -> bool {
        self.tracked_contracts.contains(address)
    }

    /// Clears all cached slot values but retains the tracked-contract set.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.invalidations.clear();
        self.hardfork_schedule.clear();
        self.hardfork_schedule_head = None;
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

        // Retain the latest pre-boundary invalidation so a pruned stale value cannot become valid.
        for blocks in self.invalidations.values_mut() {
            let baseline = blocks.range(..=min_block).next_back().copied();
            blocks.retain(|block| *block >= min_block || Some(*block) == baseline);
        }
        self.invalidations.retain(|_, blocks| !blocks.is_empty());

        // Keep the latest activation before the pruning boundary as the baseline, plus all newer
        // activations. This preserves hardfork lookup for every retained block.
        let baseline = self
            .hardfork_schedule
            .range(..=(min_block, TempoHardfork::T9))
            .next_back()
            .copied();
        self.hardfork_schedule.retain(|(block, hardfork)| {
            *block >= min_block || Some((*block, *hardfork)) == baseline
        });
    }
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

        cache.set(PORTAL, slot, 10, value);
        assert_eq!(cache.get(PORTAL, slot, 10), Some(value));
    }

    #[test]
    fn get_returns_latest_value_at_or_before_requested_block() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        let slot = B256::with_last_byte(1);

        cache.set(PORTAL, slot, 10, B256::with_last_byte(0x0a));
        cache.set(PORTAL, slot, 20, B256::with_last_byte(0x14));

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
    fn clear_removes_slots_invalidations_and_resets_anchor() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));

        cache.set(PORTAL, B256::ZERO, 100, B256::with_last_byte(1));
        cache.invalidate(PORTAL, 101);
        cache.update_anchor(NumHash {
            number: 100,
            hash: B256::with_last_byte(0xab),
        });

        cache.clear();

        assert_eq!(cache.get(PORTAL, B256::ZERO, 100), None);
        assert!(cache.invalidations.is_empty());
        assert_eq!(cache.anchor(), NumHash::default());
    }

    #[test]
    fn invalidation_blocks_inheritance_until_slot_is_refetched() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        let slot = B256::with_last_byte(1);
        let old = B256::with_last_byte(0x0a);
        let new = B256::with_last_byte(0x14);
        let other = Address::with_last_byte(0x43);

        cache.set(PORTAL, slot, 10, old);
        cache.set(other, slot, 10, old);
        cache.invalidate(PORTAL, 20);
        cache.invalidate(PORTAL, 20); // Multiple logs in one block deduplicate.

        assert_eq!(cache.get(PORTAL, slot, 19), Some(old));
        assert_eq!(cache.get(PORTAL, slot, 20), None);
        assert_eq!(cache.get(PORTAL, slot, 30), None);
        assert_eq!(cache.get(other, slot, 30), Some(old));
        assert_eq!(cache.invalidations[&PORTAL].len(), 1);

        cache.set(PORTAL, slot, 20, new);
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
    fn hardfork_lookup_uses_bounded_activation_schedule() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        cache.extend_hardfork_schedule(20, [(10, TempoHardfork::T2), (20, TempoHardfork::T8)]);

        assert_eq!(cache.hardfork_at(9), None);
        assert_eq!(cache.hardfork_at(10), Some(TempoHardfork::T2));
        assert_eq!(cache.hardfork_at(19), Some(TempoHardfork::T2));
        assert_eq!(cache.hardfork_at(20), Some(TempoHardfork::T8));
        assert_eq!(cache.hardfork_at(21), None);
    }

    #[test]
    fn confirmed_headers_advance_initialized_hardfork_schedule() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        cache.extend_hardfork_schedule(10, [(0, TempoHardfork::T0)]);

        cache.observe_hardfork(11, TempoHardfork::T0);
        cache.observe_hardfork(12, TempoHardfork::T2);

        assert_eq!(cache.hardfork_at(11), Some(TempoHardfork::T0));
        assert_eq!(cache.hardfork_at(12), Some(TempoHardfork::T2));
        assert_eq!(cache.hardfork_at(13), None);
    }

    #[test]
    fn hardfork_observation_does_not_initialize_or_cross_a_gap() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        cache.observe_hardfork(10, TempoHardfork::T2);
        assert_eq!(cache.hardfork_at(10), None);

        cache.extend_hardfork_schedule(10, [(0, TempoHardfork::T0)]);
        cache.observe_hardfork(12, TempoHardfork::T2);
        assert_eq!(cache.hardfork_at(11), None);
        assert_eq!(cache.hardfork_at(12), None);

        cache.observe_hardfork(11, TempoHardfork::Genesis);
        assert_eq!(cache.hardfork_at(11), None);
    }

    #[test]
    fn prune_keeps_hardfork_activation_baseline() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        cache.extend_hardfork_schedule(
            30,
            [
                (0, TempoHardfork::T0),
                (10, TempoHardfork::T2),
                (20, TempoHardfork::T8),
            ],
        );

        cache.prune_before(15);

        assert_eq!(cache.hardfork_at(15), Some(TempoHardfork::T2));
        assert_eq!(cache.hardfork_at(20), Some(TempoHardfork::T8));
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
    fn prune_keeps_baseline_entry_and_invalidation() {
        let mut cache = L1StateCacheInner::new(HashSet::from([PORTAL]));
        let slot = B256::with_last_byte(1);

        cache.set(PORTAL, slot, 5, B256::with_last_byte(0x05));
        cache.set(PORTAL, slot, 10, B256::with_last_byte(0x0a));
        cache.set(PORTAL, slot, 20, B256::with_last_byte(0x14));
        cache.invalidate(PORTAL, 12);
        cache.invalidate(PORTAL, 18);

        cache.prune_before(15);

        assert_eq!(cache.get(PORTAL, slot, 5), None);
        assert_eq!(
            cache.get(PORTAL, slot, 10),
            Some(B256::with_last_byte(0x0a))
        );
        assert_eq!(cache.get(PORTAL, slot, 15), None);
        assert_eq!(cache.get(PORTAL, slot, 19), None);
        assert_eq!(
            cache.get(PORTAL, slot, 20),
            Some(B256::with_last_byte(0x14))
        );
        assert_eq!(
            cache.invalidations[&PORTAL]
                .iter()
                .copied()
                .collect::<Vec<_>>(),
            vec![12, 18]
        );
    }
}
