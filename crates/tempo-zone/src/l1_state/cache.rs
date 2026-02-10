//! In-memory cache of L1 contract storage slots, anchored by L1 block hash.
//!
//! The cache stores the latest known L1 state for a set of tracked contracts. It is not versioned
//! per block — on reorgs the caller is expected to [`L1StateCache::clear`] and re-populate.

use alloy_primitives::{Address, B256};
use parking_lot::RwLock;
use std::{collections::HashMap, sync::Arc};

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

/// In-memory cache of L1 contract storage slots.
///
/// Slot values are keyed by `(Address, B256)` where the second element is the storage slot key.
/// The cache is anchored to a specific L1 block identified by hash and number.
#[derive(Debug)]
pub struct L1StateCache {
    config: L1StateCacheConfig,
    slots: HashMap<(Address, B256), B256>,
    anchor_block_hash: B256,
    anchor_block_number: u64,
}

impl L1StateCache {
    /// Create a new cache for the given tracked contracts.
    pub fn new(config: L1StateCacheConfig) -> Self {
        Self {
            config,
            slots: HashMap::new(),
            anchor_block_hash: B256::ZERO,
            anchor_block_number: 0,
        }
    }

    /// Returns the cached value for a storage slot, if present.
    pub fn get(&self, address: Address, slot: B256) -> Option<B256> {
        self.slots.get(&(address, slot)).copied()
    }

    /// Sets a storage slot value in the cache.
    pub fn set(&mut self, address: Address, slot: B256, value: B256) {
        self.slots.insert((address, slot), value);
    }

    /// Updates the anchor block that this cache represents.
    pub fn update_anchor(&mut self, block_hash: B256, block_number: u64) {
        self.anchor_block_hash = block_hash;
        self.anchor_block_number = block_number;
    }

    /// Returns the current anchor `(block_hash, block_number)`.
    pub fn anchor(&self) -> (B256, u64) {
        (self.anchor_block_hash, self.anchor_block_number)
    }

    /// Returns `true` if the given address is one of the tracked contracts.
    pub fn is_tracked(&self, address: &Address) -> bool {
        self.config.contracts.iter().any(|c| &c.address == address)
    }

    /// Clears all cached slot values but retains the tracked-contract configuration.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.anchor_block_hash = B256::ZERO;
        self.anchor_block_number = 0;
    }
}

/// Shared handle to the L1 state cache.
pub type SharedL1StateCache = Arc<RwLock<L1StateCache>>;

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
        assert_eq!(cache.get(addr, B256::ZERO), None);
    }

    #[test]
    fn set_and_get_round_trip() {
        let mut cache = L1StateCache::new(test_config());
        let addr = address!("0x0000000000000000000000000000000000004242");
        let slot = B256::with_last_byte(1);
        let value = B256::with_last_byte(0xff);

        cache.set(addr, slot, value);
        assert_eq!(cache.get(addr, slot), Some(value));
    }

    #[test]
    fn set_overwrites_existing_value() {
        let mut cache = L1StateCache::new(test_config());
        let addr = address!("0x0000000000000000000000000000000000004242");
        let slot = B256::with_last_byte(1);

        cache.set(addr, slot, B256::with_last_byte(0x01));
        cache.set(addr, slot, B256::with_last_byte(0x02));
        assert_eq!(cache.get(addr, slot), Some(B256::with_last_byte(0x02)));
    }

    #[test]
    fn clear_removes_slots_and_resets_anchor() {
        let mut cache = L1StateCache::new(test_config());
        let addr = address!("0x0000000000000000000000000000000000004242");

        cache.set(addr, B256::ZERO, B256::with_last_byte(1));
        cache.update_anchor(B256::with_last_byte(0xab), 100);

        cache.clear();

        assert_eq!(cache.get(addr, B256::ZERO), None);
        assert_eq!(cache.anchor(), (B256::ZERO, 0));
    }

    #[test]
    fn anchor_defaults_to_zero() {
        let cache = L1StateCache::new(test_config());
        assert_eq!(cache.anchor(), (B256::ZERO, 0));
    }

    #[test]
    fn update_anchor() {
        let mut cache = L1StateCache::new(test_config());
        let hash = B256::with_last_byte(0xbe);
        cache.update_anchor(hash, 42);
        assert_eq!(cache.anchor(), (hash, 42));
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

        cache.set(addr_a, slot, B256::with_last_byte(0xaa));
        cache.set(addr_b, slot, B256::with_last_byte(0xbb));

        assert_eq!(cache.get(addr_a, slot), Some(B256::with_last_byte(0xaa)));
        assert_eq!(cache.get(addr_b, slot), Some(B256::with_last_byte(0xbb)));
    }
}
