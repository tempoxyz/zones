//! Cache-first, RPC-fallback provider for reading L1 contract storage slots.
//!
//! [`L1StateProvider`] wraps a [`SharedL1StateCache`] and a [`DynProvider<TempoNetwork>`] backed by an
//! HTTP transport. Reads are served from the in-memory cache when possible. On cache miss the
//! provider falls back to `eth_getStorageAt` via the shared HTTP provider and writes the result
//! back into the cache.
//!
//! Both a synchronous ([`L1StateProvider::get_storage`]) and an asynchronous
//! ([`L1StateProvider::get_storage_async`]) entry point are provided. The synchronous variant is
//! intended for use inside EVM precompiles where async is unavailable — it dispatches the RPC
//! call through a [`tokio::runtime::Handle`] with a configurable timeout.

use std::time::Duration;

use alloy_primitives::{Address, B256, U256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_rpc_types_eth::BlockId;
use eyre::Result;
use tempo_alloy::TempoNetwork;
use tracing::{debug, info, warn};

use super::cache::SharedL1StateCache;

/// Configuration for the [`L1StateProvider`].
#[derive(Debug, Clone)]
pub struct L1StateProviderConfig {
    /// HTTP RPC endpoint for Tempo L1.
    pub l1_rpc_url: String,
    /// Timeout applied to each individual RPC request. Defaults to 2 seconds.
    pub request_timeout: Duration,
}

impl Default for L1StateProviderConfig {
    fn default() -> Self {
        Self {
            l1_rpc_url: String::new(),
            request_timeout: Duration::from_secs(2),
        }
    }
}

/// Cache-first, RPC-fallback provider for reading Tempo L1 contract storage.
///
/// `L1StateProvider` is the core bridge between synchronous EVM execution (precompiles) and the
/// asynchronous L1 RPC layer. It holds:
///
/// - A [`SharedL1StateCache`] for fast in-memory lookups.
/// - A [`DynProvider<TempoNetwork>`] (alloy HTTP provider) created once and reused across calls.
/// - A [`tokio::runtime::Handle`] used by the synchronous [`get_storage`](Self::get_storage)
///   method to dispatch async work from a blocking context.
///
/// # Sync dispatch safety
///
/// [`get_storage`](Self::get_storage) calls `runtime_handle.block_on(...)` to execute the async
/// RPC fetch. This is safe **only** when the caller is running on a blocking / OS thread that is
/// *not* part of the tokio async runtime (e.g. the EVM execution thread spawned via
/// `spawn_blocking`). Calling it from within an async task on the same runtime will panic.
#[derive(Debug, Clone)]
pub struct L1StateProvider {
    config: L1StateProviderConfig,
    /// In-memory cache of L1 contract storage slots, checked before any RPC call.
    cache: SharedL1StateCache,
    /// HTTP provider pointed at **Tempo L1**, used as a fallback when the cache misses.
    provider: DynProvider<TempoNetwork>,
    /// Handle to the tokio runtime, used by [`get_storage`](Self::get_storage) to
    /// dispatch async RPC calls from a blocking (non-async) context.
    runtime_handle: tokio::runtime::Handle,
}

impl L1StateProvider {
    /// Create a new provider.
    ///
    /// The provider is created eagerly from [`L1StateProviderConfig::l1_rpc_url`] and reused
    /// for the lifetime of this instance. The transport (HTTP or WebSocket) is auto-detected
    /// from the URL scheme. `runtime_handle` is stored for later use by the synchronous
    /// [`get_storage`](Self::get_storage) method.
    pub async fn new(
        config: L1StateProviderConfig,
        cache: SharedL1StateCache,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&config.l1_rpc_url)
            .await
            .expect("valid L1 RPC URL")
            .erased();

        Self {
            config,
            cache,
            provider,
            runtime_handle,
        }
    }

    /// Create a provider from pre-constructed components.
    ///
    /// Used by [`ZoneEvmConfig::new_without_l1`](crate::evm::ZoneEvmConfig::new_without_l1)
    /// to build a fallback provider that won't panic on an empty RPC URL.
    pub fn new_raw(
        config: L1StateProviderConfig,
        cache: SharedL1StateCache,
        provider: DynProvider<TempoNetwork>,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            config,
            cache,
            provider,
            runtime_handle,
        }
    }

    /// Read a storage slot synchronously at a specific L1 block — cache first, RPC fallback.
    ///
    /// This method is designed for use inside EVM precompiles that run on a **blocking thread**.
    /// On cache miss it dispatches an async RPC call via `runtime_handle.block_on()` with the
    /// configured [`request_timeout`](L1StateProviderConfig::request_timeout).
    ///
    /// # Panics
    ///
    /// Panics if called from within an async context on the same tokio runtime (see struct-level
    /// docs).
    pub fn get_storage(&self, address: Address, slot: B256, block_number: u64) -> Result<B256> {
        {
            let cache = self.cache.read();
            if let Some(value) = cache.get(address, slot, block_number) {
                debug!(%address, %slot, block_number, %value, "L1 storage cache hit");
                return Ok(value);
            }
        }

        warn!(%address, %slot, block_number, "L1 storage cache miss, fetching from RPC");

        let start = std::time::Instant::now();
        let result = tokio::task::block_in_place(|| {
            self.runtime_handle.block_on(tokio::time::timeout(
                self.config.request_timeout,
                self.fetch_slot(address, slot, block_number),
            ))
        });
        let elapsed = start.elapsed();

        match result {
            Ok(inner) => {
                let value = inner?;
                self.cache.write().set(address, slot, block_number, value);
                info!(%address, %slot, block_number, %value, ?elapsed, "L1 storage RPC fetch succeeded");
                Ok(value)
            }
            Err(_elapsed) => {
                warn!(%address, %slot, block_number, ?elapsed, timeout = ?self.config.request_timeout, "L1 storage RPC fetch timed out");
                Err(eyre::eyre!(
                    "L1 RPC request timed out after {:?} for address={address} slot={slot} block={block_number}",
                    self.config.request_timeout,
                ))
            }
        }
    }

    /// Read a storage slot asynchronously at a specific L1 block — cache first, RPC fallback.
    ///
    /// Same semantics as [`get_storage`](Self::get_storage) but natively async, using
    /// `tokio::time::timeout` directly.
    pub async fn get_storage_async(
        &self,
        address: Address,
        slot: B256,
        block_number: u64,
    ) -> Result<B256> {
        {
            let cache = self.cache.read();
            if let Some(value) = cache.get(address, slot, block_number) {
                debug!(%address, %slot, block_number, %value, "L1 storage cache hit");
                return Ok(value);
            }
        }

        warn!(%address, %slot, block_number, "L1 storage cache miss, fetching from RPC");

        let result = tokio::time::timeout(
            self.config.request_timeout,
            self.fetch_slot(address, slot, block_number),
        )
        .await;

        match result {
            Ok(inner) => {
                let value = inner?;
                self.cache.write().set(address, slot, block_number, value);
                Ok(value)
            }
            Err(_elapsed) => Err(eyre::eyre!(
                "L1 RPC request timed out after {:?} for address={address} slot={slot} block={block_number}",
                self.config.request_timeout,
            )),
        }
    }

    /// Expose the shared cache handle for external use (e.g. the listener).
    pub fn cache(&self) -> &SharedL1StateCache {
        &self.cache
    }

    /// Fetch a single storage slot from L1 at a specific block via the shared HTTP provider.
    async fn fetch_slot(&self, address: Address, slot: B256, block_number: u64) -> Result<B256> {
        let key = U256::from_be_bytes(slot.0);
        let block_id = BlockId::number(block_number);
        let value: U256 = self.provider.get_storage_at(address, key).block_id(block_id).await.map_err(|e| {
            warn!(%address, %slot, block_number, %e, "eth_getStorageAt RPC call failed");
            eyre::eyre!("eth_getStorageAt failed for address={address} slot={slot} block={block_number}: {e}")
        })?;

        let result = B256::from(value.to_be_bytes());
        debug!(%address, %slot, block_number, %result, "fetched L1 storage slot from RPC");
        Ok(result)
    }
}
