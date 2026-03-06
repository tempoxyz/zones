//! L1 state cache and provider for reading Tempo L1 contract storage from the zone.
//!
//! This module provides:
//!
//! - [`L1StateCache`] — an in-memory cache of L1 contract storage slots, anchored by block hash.
//! - [`L1ChainNotificationListener`] — a service that consumes in-process chain notifications and
//!   updates the cache with full state diffs.
//! - [`L1StateProvider`] — a cache-first, RPC-fallback reader for `eth_getStorageAt`.
//! - [`TempoStateReader`] — a standalone `DynPrecompile` that handles `readStorageAt` calls.
//! - [`tip403`] — TIP-403 policy cache, listener, and provider.

pub mod cache;
pub mod listener;
pub mod precompile;
pub mod provider;
pub mod tip403;
pub mod versioned;

pub use cache::{L1StateCache, SharedL1StateCache};
pub use listener::{L1ChainNotificationListener, spawn_l1_chain_notification_listener};
pub use precompile::TempoStateReader;
pub use provider::{L1StateProvider, L1StateProviderConfig};
pub use tip403::{
    AuthRole, PolicyCache, PolicyEvent, PolicyProvider, PolicyTaskHandle, PolicyTaskMessage,
    SharedPolicyCache, Tip403Metrics, spawn_policy_resolution_task, spawn_pool_prefetch_task,
};
