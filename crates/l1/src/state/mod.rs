//! L1 state cache and provider for reading Tempo L1 contract storage from the zone.
//!
//! This module provides:
//!
//! - [`L1StateCache`] — a shared in-memory cache of L1 contract storage slots.
//! - [`L1StateCacheInner`] — the block-versioned cache storage guarded by [`L1StateCache`].
//! - [`L1StateProvider`] — a cache-first, RPC-fallback reader for `eth_getStorageAt`.
//! - [`tip403`] — TIP-403 policy cache and provider.

pub mod cache;
pub mod provider;
pub mod tip403;
pub mod versioned;

pub use cache::{L1StateCache, L1StateCacheInner};
pub use provider::{L1StateProvider, L1StateProviderConfig};
pub use tip403::{
    AuthRole, PolicyCache, PolicyCacheInner, PolicyEvent, PolicyProvider, PolicyTaskHandle,
    PolicyTaskMessage, Tip403Metrics, spawn_policy_resolution_task, spawn_pool_prefetch_task,
};
