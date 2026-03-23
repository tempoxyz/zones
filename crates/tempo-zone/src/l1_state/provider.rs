//! Cache-first, RPC-fallback provider for reading L1 contract storage slots.
//!
//! [`L1StateProvider`] wraps a [`SharedL1StateCache`] and a [`DynProvider<TempoNetwork>`] backed by an
//! HTTP transport. Reads are served from the in-memory cache when possible. On cache miss the
//! provider falls back to `eth_getStorageAt` via the shared HTTP provider and writes the result
//! back into the cache.
//!
//! Both a synchronous ([`L1StateProvider::get_storage`]) and an asynchronous
//! ([`L1StateProvider::get_storage_async`]) entry point are provided. The synchronous variant is
//! intended for use inside EVM precompiles where async is unavailable — it retries the RPC
//! call indefinitely with exponential backoff to avoid bricking the chain on transient outages.

use alloy_primitives::{Address, B256, U256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_rpc_client::{ConnectionConfig, RpcClient};
use alloy_rpc_types_eth::BlockId;
use alloy_transport::layers::RetryBackoffLayer;
use eyre::Result;
use tempo_alloy::TempoNetwork;
use tracing::{debug, info, warn};
use zone_precompiles::SequencerExt;

use super::cache::SharedL1StateCache;
use crate::abi::PORTAL_SEQUENCER_SLOT;

/// Configuration for the [`L1StateProvider`].
#[derive(Debug, Clone)]
pub struct L1StateProviderConfig {
    /// HTTP RPC endpoint for Tempo L1.
    pub l1_rpc_url: String,
    /// Zone portal address on Tempo L1, used for sequencer lookups.
    pub portal_address: Address,
    /// Maximum number of transport-level retries for failed/rate-limited RPC requests.
    /// Defaults to 10.
    pub max_retries: u32,
    /// Initial backoff in milliseconds for the transport-level retry layer.
    /// Defaults to 20ms.
    pub initial_backoff_ms: u64,
    /// Interval between WebSocket reconnection attempts.
    /// Defaults to 100ms.
    pub retry_connection_interval: std::time::Duration,
}

impl Default for L1StateProviderConfig {
    fn default() -> Self {
        Self {
            l1_rpc_url: String::new(),
            portal_address: Address::ZERO,
            max_retries: 10,
            initial_backoff_ms: 20,
            retry_connection_interval: std::time::Duration::from_millis(100),
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
    /// In-memory cache of L1 contract storage slots, checked before any RPC call.
    cache: SharedL1StateCache,
    /// Zone portal address on Tempo L1 used for sequencer lookups.
    portal_address: Address,
    /// HTTP provider pointed at **Tempo L1**, used as a fallback when the cache misses.
    /// Wraps a [`RetryBackoffLayer`] that handles retries with exponential backoff.
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
    ) -> Result<Self> {
        let retry_layer =
            RetryBackoffLayer::new(config.max_retries, config.initial_backoff_ms, u64::MAX);

        let conn_config = ConnectionConfig::new()
            .with_max_retries(u32::MAX)
            .with_retry_interval(config.retry_connection_interval);

        let client = RpcClient::builder()
            .layer(retry_layer)
            .connect_with_config(&config.l1_rpc_url, conn_config)
            .await
            .map_err(|e| {
                eyre::eyre!(
                    "Failed to connect L1 state provider at {}: {e}",
                    config.l1_rpc_url
                )
            })?;

        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_client(client)
            .erased();

        Ok(Self {
            cache,
            portal_address: config.portal_address,
            provider,
            runtime_handle,
        })
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
            cache,
            portal_address: config.portal_address,
            provider,
            runtime_handle,
        }
    }

    /// Read a storage slot synchronously at a specific L1 block — cache first, RPC fallback.
    ///
    /// This method is designed for use inside EVM precompiles that run on a **blocking thread**.
    /// On cache miss it retries the RPC call indefinitely until the value is fetched. The
    /// transport layer handles backoff internally via [`RetryBackoffLayer`], so retries here
    /// are immediate. This ensures a transient L1 RPC outage stalls block production rather
    /// than bricking the chain with a hard precompile error.
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

        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let start = std::time::Instant::now();
            let result = tokio::task::block_in_place(|| {
                self.runtime_handle
                    .block_on(self.fetch_slot(address, slot, block_number))
            });
            let elapsed = start.elapsed();

            match result {
                Ok(value) => {
                    self.cache.write().set(address, slot, block_number, value);
                    if attempt > 1 {
                        info!(%address, %slot, block_number, %value, ?elapsed, attempt, "L1 storage RPC fetch succeeded after retries");
                    } else {
                        info!(%address, %slot, block_number, %value, ?elapsed, "L1 storage RPC fetch succeeded");
                    }
                    return Ok(value);
                }
                Err(rpc_err) => {
                    warn!(%address, %slot, block_number, %rpc_err, ?elapsed, attempt, "L1 storage RPC fetch failed, retrying");
                }
            }
        }
    }

    /// Read a storage slot at the latest known L1 height.
    ///
    /// Uses the cache anchor when available; otherwise falls back to the
    /// current RPC head before resolving the slot value.
    pub fn get_latest_storage(&self, address: Address, slot: B256) -> Result<B256> {
        let anchor_number = self.cache.read().anchor().number;
        let block_number = if anchor_number != 0 {
            anchor_number
        } else {
            tokio::task::block_in_place(|| {
                self.runtime_handle.block_on(async {
                    self.provider.get_block_number().await.map_err(|e| {
                        eyre::eyre!("eth_blockNumber failed while reading latest storage: {e}")
                    })
                })
            })?
        };

        self.get_storage(address, slot, block_number)
    }

    /// Read the active sequencer address from the configured portal at the latest known L1 height.
    pub fn get_latest_sequencer(&self) -> Result<Address> {
        let value = self.get_latest_storage(self.portal_address, PORTAL_SEQUENCER_SLOT)?;
        Ok(Address::from_slice(&value.as_slice()[12..]))
    }

    /// Read a storage slot asynchronously at a specific L1 block — cache first, RPC fallback.
    ///
    /// Same semantics as [`get_storage`](Self::get_storage) but natively async. The
    /// transport-level [`RetryBackoffLayer`] handles retries with exponential backoff.
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

        let value = self.fetch_slot(address, slot, block_number).await?;
        self.cache.write().set(address, slot, block_number, value);
        Ok(value)
    }

    /// Expose the shared cache handle for external use (e.g. the engine).
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

impl SequencerExt for L1StateProvider {
    fn latest_sequencer(&self) -> Option<Address> {
        self.get_latest_sequencer().ok()
    }
}
