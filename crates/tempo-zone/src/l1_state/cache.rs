//! Block-versioned in-memory cache of Tempo L1 contract storage slots.
//!
//! The zone's [`TempoStateReader`](super::precompile::TempoStateReader) precompile reads
//! Tempo L1 storage at a **specific L1 block height** (the `tempoBlockNumber` the zone committed
//! to via `TempoState.finalizeTempo()` on Zone L2). Because the L1 chain may advance several
//! blocks ahead of the zone's committed height, the cache must be able to serve historical
//! values — not just "latest".
//!
//! ## Storage model
//!
//! Each `(contract_address, slot_key)` pair maps to a [`BTreeMap<u64, B256>`] of
//! `block_number → value`. A lookup for block N returns the most recent entry whose
//! block number is ≤ N, reflecting the value that was current at that height.
//!
//! ## Write path
//!
//! - The [`L1ChainNotificationListener`](super::listener::L1ChainNotificationListener) writes
//!   storage diffs for tracked contracts as they arrive, tagged with the L1 tip block number.
//! - The [`L1StateProvider`](super::provider::L1StateProvider) writes RPC-fetched values on
//!   cache miss, tagged with the block number that was requested.
//!
//! ## Reorg handling
//!
//! On reorgs the caller is expected to [`L1StateCache::clear`] the entire cache and re-populate
//! from the new canonical chain segment. There is no per-block rollback.

use alloy_eips::NumHash;
use alloy_primitives::{Address, B256};
use parking_lot::RwLock;
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

/// Describes an L1 contract whose storage slots should be cached.
#[derive(Debug, Clone)]
pub struct TrackedContract {
    /// Contract address on Tempo L1.
    pub address: Address,
    /// Human-readable name used in log messages.
    pub name: String,
}

/// Configuration for the L1 state cache.
#[derive(Debug, Clone)]
pub struct L1StateCacheConfig {
    /// Contracts whose storage slots are tracked.
    pub contracts: Vec<TrackedContract>,
}

/// Block-versioned cache of Tempo L1 contract storage slots.
///
/// Each `(contract_address, slot_key)` pair maintains a history of values indexed by L1 block
/// number. Lookups for a given block return the most recent value at or before that block,
/// i.e. the value that was current at that height. This allows the zone to read L1 state at
/// the `tempoBlockNumber` it committed to, even if the L1 chain has since advanced.
///
/// The anchor tracks the latest L1 block the cache has received data for, used by the
/// [`L1StateListener`](super::listener::L1StateListener) for reorg detection.
#[derive(Debug)]
pub struct L1StateCache {
    config: L1StateCacheConfig,
    /// Per-slot value history: `(address, slot) → { block_number → value }`.
    /// The `BTreeMap` enables efficient range lookups for "latest value at or before block N".
    slots: HashMap<(Address, B256), BTreeMap<u64, B256>>,
    /// Latest L1 block the cache has received data for, used for reorg detection.
    anchor: NumHash,
}

impl L1StateCache {
    /// Create a new cache for the given tracked contracts.
    pub fn new(config: L1StateCacheConfig) -> Self {
        Self {
            config,
            slots: HashMap::new(),
            anchor: NumHash::default(),
        }
    }

    /// Returns the cached value for a storage slot at the given block number.
    ///
    /// Returns the most recent value at or before `block_number`, or `None` if no
    /// value has been cached for this slot at or before the requested block.
    pub fn get(&self, address: Address, slot: B256, block_number: u64) -> Option<B256> {
        self.slots
            .get(&(address, slot))
            .and_then(|history| history.range(..=block_number).next_back().map(|(_, v)| *v))
    }

    /// Sets a storage slot value in the cache at the given block number.
    pub fn set(&mut self, address: Address, slot: B256, block_number: u64, value: B256) {
        self.slots
            .entry((address, slot))
            .or_default()
            .insert(block_number, value);
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
        self.config.contracts.iter().any(|c| &c.address == address)
    }

    /// Clears all cached slot values but retains the tracked-contract configuration.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.anchor = NumHash::default();
    }

    /// Remove all entries with block numbers strictly less than `min_block`.
    ///
    /// Retains at most one entry per slot below the threshold — the latest one — so that
    /// lookups at `min_block` still have a baseline value.
    pub fn prune_before(&mut self, min_block: u64) {
        for history in self.slots.values_mut() {
            let keep_from = history
                .range(..min_block)
                .next_back()
                .map(|(k, _)| *k);

            if let Some(keep) = keep_from {
                let to_remove: Vec<u64> = history
                    .range(..keep)
                    .map(|(k, _)| *k)
                    .collect();
                for k in to_remove {
                    history.remove(&k);
                }
            }
        }

        self.slots.retain(|_, history| !history.is_empty());
    }
}

/// Shared handle to the L1 state cache.
#[derive(Debug, Clone)]
pub struct SharedL1StateCache(Arc<RwLock<L1StateCache>>);

impl SharedL1StateCache {
    pub fn new(config: L1StateCacheConfig) -> Self {
        Self(Arc::new(RwLock::new(L1StateCache::new(config))))
    }

    pub fn read(&self) -> parking_lot::RwLockReadGuard<'_, L1StateCache> {
        self.0.read()
    }

    pub fn write(&self) -> parking_lot::RwLockWriteGuard<'_, L1StateCache> {
        self.0.write()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    fn test_config() -> L1StateCacheConfig {
        L1StateCacheConfig {
            contracts: vec![
                TrackedContract {
                    address: address!("0x0000000000000000000000000000000000004242"),
                    name: "ZonePortal".to_string(),
                },
                TrackedContract {
                    address: address!("0x0000000000000000000000000000000000004343"),
                    name: "TempoState".to_string(),
                },
            ],
        }
    }

    #[test]
    fn get_returns_none_for_missing_slot() {
        let cache = L1StateCache::new(test_config());
        let addr = address!("0x0000000000000000000000000000000000004242");
        assert_eq!(cache.get(addr, B256::ZERO, 100), None);
    }

    #[test]
    fn set_and_get_at_same_block() {
        let mut cache = L1StateCache::new(test_config());
        let addr = address!("0x0000000000000000000000000000000000004242");
        let slot = B256::with_last_byte(1);
        let value = B256::with_last_byte(0xff);

        cache.set(addr, slot, 10, value);
        assert_eq!(cache.get(addr, slot, 10), Some(value));
    }

    #[test]
    fn get_returns_latest_value_at_or_before_requested_block() {
        let mut cache = L1StateCache::new(test_config());
        let addr = address!("0x0000000000000000000000000000000000004242");
        let slot = B256::with_last_byte(1);

        cache.set(addr, slot, 10, B256::with_last_byte(0x0a));
        cache.set(addr, slot, 20, B256::with_last_byte(0x14));

        assert_eq!(cache.get(addr, slot, 10), Some(B256::with_last_byte(0x0a)));
        assert_eq!(cache.get(addr, slot, 15), Some(B256::with_last_byte(0x0a)));
        assert_eq!(cache.get(addr, slot, 20), Some(B256::with_last_byte(0x14)));
        assert_eq!(cache.get(addr, slot, 25), Some(B256::with_last_byte(0x14)));
    }

    #[test]
    fn get_returns_none_before_earliest_entry() {
        let mut cache = L1StateCache::new(test_config());
        let addr = address!("0x0000000000000000000000000000000000004242");
        let slot = B256::with_last_byte(1);

        cache.set(addr, slot, 10, B256::with_last_byte(0xff));
        assert_eq!(cache.get(addr, slot, 9), None);
    }

    #[test]
    fn clear_removes_slots_and_resets_anchor() {
        let mut cache = L1StateCache::new(test_config());
        let addr = address!("0x0000000000000000000000000000000000004242");

        cache.set(addr, B256::ZERO, 100, B256::with_last_byte(1));
        cache.update_anchor(NumHash { number: 100, hash: B256::with_last_byte(0xab) });

        cache.clear();

        assert_eq!(cache.get(addr, B256::ZERO, 100), None);
        assert_eq!(cache.anchor(), NumHash::default());
    }

    #[test]
    fn anchor_defaults_to_zero() {
        let cache = L1StateCache::new(test_config());
        assert_eq!(cache.anchor(), NumHash::default());
    }

    #[test]
    fn update_anchor() {
        let mut cache = L1StateCache::new(test_config());
        let hash = B256::with_last_byte(0xbe);
        cache.update_anchor(NumHash { number: 42, hash });
        assert_eq!(cache.anchor(), NumHash { number: 42, hash });
    }

    #[test]
    fn is_tracked_returns_true_for_configured_contracts() {
        let cache = L1StateCache::new(test_config());
        assert!(cache.is_tracked(&address!("0x0000000000000000000000000000000000004242")));
        assert!(cache.is_tracked(&address!("0x0000000000000000000000000000000000004343")));
    }

    #[test]
    fn is_tracked_returns_false_for_unknown_address() {
        let cache = L1StateCache::new(test_config());
        assert!(!cache.is_tracked(&address!("0x0000000000000000000000000000000000000001")));
    }

    #[test]
    fn different_addresses_same_slot_are_independent() {
        let mut cache = L1StateCache::new(test_config());
        let addr_a = address!("0x0000000000000000000000000000000000004242");
        let addr_b = address!("0x0000000000000000000000000000000000004343");
        let slot = B256::with_last_byte(1);

        cache.set(addr_a, slot, 10, B256::with_last_byte(0xaa));
        cache.set(addr_b, slot, 10, B256::with_last_byte(0xbb));

        assert_eq!(cache.get(addr_a, slot, 10), Some(B256::with_last_byte(0xaa)));
        assert_eq!(cache.get(addr_b, slot, 10), Some(B256::with_last_byte(0xbb)));
    }

    #[test]
    fn prune_keeps_baseline_entry() {
        let mut cache = L1StateCache::new(test_config());
        let addr = address!("0x0000000000000000000000000000000000004242");
        let slot = B256::with_last_byte(1);

        cache.set(addr, slot, 5, B256::with_last_byte(0x05));
        cache.set(addr, slot, 10, B256::with_last_byte(0x0a));
        cache.set(addr, slot, 20, B256::with_last_byte(0x14));

        cache.prune_before(15);

        assert_eq!(cache.get(addr, slot, 5), None);
        assert_eq!(cache.get(addr, slot, 10), Some(B256::with_last_byte(0x0a)));
        assert_eq!(cache.get(addr, slot, 15), Some(B256::with_last_byte(0x0a)));
        assert_eq!(cache.get(addr, slot, 20), Some(B256::with_last_byte(0x14)));
    }
}
