//! L1 state cache and provider for reading Tempo L1 contract storage from the zone.
//!
//! This module provides:
//!
//! - [`L1StateCache`] — an in-memory cache of L1 contract storage slots, anchored by block hash.
//! - [`L1StateCacheConfig`] — configuration describing which contracts to track.
//! - [`L1StateListener`] — a service that subscribes to L1 chain notifications and updates the cache.
//! - [`L1StateProvider`] — a cache-first, RPC-fallback reader for `eth_getStorageAt`.
//! - [`TempoStatePrecompile`] — a `DynPrecompile` that handles `readTempoStorageSlot` calls.

pub mod cache;
pub mod listener;
pub mod precompile;
pub mod provider;

pub use cache::{L1StateCache, L1StateCacheConfig, SharedL1StateCache, TrackedContract};
pub use listener::{
    L1ChainNotificationListener, L1StateListener, L1StateListenerConfig,
    spawn_l1_chain_notification_listener, spawn_l1_state_listener,
};
pub use precompile::TempoStatePrecompile;
pub use provider::{L1StateProvider, L1StateProviderConfig};
