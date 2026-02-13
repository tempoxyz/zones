//! L1 state cache and provider for reading Tempo L1 contract storage from the zone.
//!
//! This module provides:
//!
//! - [`L1StateCache`] — an in-memory cache of L1 contract storage slots, anchored by block hash.
//! - [`L1StateListener`] — a service that subscribes to L1 chain notifications and updates the cache.
//! - [`L1StateProvider`] — a cache-first, RPC-fallback reader for `eth_getStorageAt`.
//! - [`TempoStateReader`] — a standalone `DynPrecompile` that handles `readStorageAt` calls.
//! - [`L1StorageReader`] — trait abstracting synchronous L1 storage reads.

use alloy_primitives::{Address, B256};
use eyre::Result;

pub mod cache;
pub mod listener;
pub mod precompile;
pub mod provider;

pub use cache::{L1StateCache, SharedL1StateCache};
pub use listener::{
    L1ChainNotificationListener, L1StateListener, L1StateListenerConfig,
    spawn_l1_chain_notification_listener, spawn_l1_state_listener,
};
pub use precompile::TempoStateReader;
pub use provider::{L1StateProvider, L1StateProviderConfig};

/// Trait abstracting synchronous L1 storage reads.
///
/// Implemented by [`L1StateProvider`] (cache + RPC) and
/// [`RecordingL1StateProvider`](crate::witness::RecordingL1StateProvider) (recording wrapper).
///
/// Used by [`TempoStateReader`] and [`ZoneEvmFactory`](crate::evm::ZoneEvmFactory) to decouple
/// the precompile from the concrete provider implementation.
pub trait L1StorageReader: Send + Sync + 'static {
    /// Read a storage slot from Tempo L1 at a specific block height.
    fn get_storage(&self, address: Address, slot: B256, block_number: u64) -> Result<B256>;
}
