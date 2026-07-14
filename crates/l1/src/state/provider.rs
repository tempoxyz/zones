//! Cache-first, RPC-fallback provider for reading L1 contract storage slots.
//!
//! [`L1StateProvider`] wraps a [`L1StateCache`] and a [`DynProvider<TempoNetwork>`] backed by an
//! HTTP transport. Reads are served from the in-memory cache when possible. On cache miss the
//! provider falls back to `eth_getStorageAt` via the shared HTTP provider and writes the result
//! back into the cache.
//!
//! Both a synchronous ([`L1StateProvider::get_storage`]) and an asynchronous
//! ([`L1StateProvider::get_storage_async`]) entry point are provided. The synchronous variant is
//! intended for use inside EVM precompiles where async is unavailable — it retries the RPC
//! call indefinitely with exponential backoff to avoid bricking the chain on transient outages.

use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::{BlockId, BlockNumberOrTag};
use alloy_transport::layers::RetryBackoffLayer;
use eyre::Result;
use reth_chainspec::ForkCondition;
use tempo_alloy::TempoNetwork;
use tempo_chainspec::{
    hardfork::TempoHardfork,
    spec::{DEV, TempoHardforks, chainspec_from_chain_id},
};
use tracing::{debug, info, warn};
use zone_precompiles::{L1StorageReader, SequencerExt};

use super::cache::L1StateCache;
use crate::{abi::PORTAL_SEQUENCER_SLOT, rpc::rpc_connection_config};

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
/// - A [`L1StateCache`] for fast in-memory lookups.
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
    cache: L1StateCache,
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
        cache: L1StateCache,
        runtime_handle: tokio::runtime::Handle,
    ) -> Result<Self> {
        let retry_layer =
            RetryBackoffLayer::new(config.max_retries, config.initial_backoff_ms, u64::MAX);

        let conn_config = rpc_connection_config(config.retry_connection_interval);

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
    /// Used by `ZoneEvmConfig::new_without_l1` to build a fallback provider
    /// that won't panic on an empty RPC URL.
    pub fn new_raw(
        config: L1StateProviderConfig,
        cache: L1StateCache,
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

    /// Resolve the Tempo hardfork active at an exact L1 block, using cached metadata first.
    pub fn get_hardfork(&self, block_number: u64) -> Result<TempoHardfork> {
        if let Some(hardfork) = self.cache.read().hardfork_at(block_number) {
            return Ok(hardfork);
        }

        let activations = tokio::task::block_in_place(|| {
            self.runtime_handle
                .block_on(self.fetch_hardfork_schedule(block_number))
        })?;
        let mut cache = self.cache.write();
        cache.extend_hardfork_schedule(block_number, activations);
        cache
            .hardfork_at(block_number)
            .ok_or_else(|| eyre::eyre!("no Tempo hardfork active at L1 block {block_number}"))
    }

    /// Expose the shared cache handle for external use (e.g. the engine).
    pub fn cache(&self) -> &L1StateCache {
        &self.cache
    }

    async fn fetch_hardfork_schedule(
        &self,
        block_number: u64,
    ) -> Result<Vec<(u64, TempoHardfork)>> {
        let (chain_id, block) = tokio::try_join!(
            self.provider.get_chain_id(),
            self.provider
                .get_block_by_number(BlockNumberOrTag::Number(block_number)),
        )?;
        let block = block.ok_or_else(|| eyre::eyre!("L1 block {block_number} not found"))?;
        let chain_spec = chainspec_from_chain_id(chain_id).unwrap_or_else(|| DEV.clone());
        let block_ts = block.header.timestamp();
        let mut activations = Vec::new();

        for &hardfork in TempoHardfork::VARIANTS {
            let ForkCondition::Timestamp(fork_ts) = chain_spec.tempo_fork_activation(hardfork)
            else {
                continue;
            };
            if fork_ts > block_ts {
                continue;
            }
            let known_block = match chain_id {
                4217 => hardfork.mainnet_activation_block(),
                42431 => hardfork.moderato_activation_block(),
                _ => None,
            };
            let activation_block = match known_block {
                Some(block) => block,
                None => self.first_block_at_or_after(fork_ts, block_number).await?,
            };
            activations.push((activation_block, hardfork));
        }

        Ok(activations)
    }

    async fn first_block_at_or_after(&self, timestamp: u64, mut high: u64) -> Result<u64> {
        let mut low = 0u64;
        while low < high {
            let mid = low + (high - low) / 2;
            let block = self
                .provider
                .get_block_by_number(BlockNumberOrTag::Number(mid))
                .await?
                .ok_or_else(|| eyre::eyre!("L1 block {mid} not found"))?;
            if block.header.timestamp() < timestamp {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        Ok(low)
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

impl L1StorageReader for L1StateProvider {
    fn portal_address(&self) -> Address {
        self.portal_address
    }

    fn read_l1_storage(
        &self,
        account: Address,
        slot: B256,
        block_number: u64,
    ) -> std::result::Result<B256, revm::precompile::PrecompileError> {
        self.get_storage(account, slot, block_number).map_err(|e| {
            zone_precompiles::zone_rpc_error(format!(
                "L1 storage unavailable for account={account} slot={slot} block={block_number}: {e}"
            ))
        })
    }

    fn hardfork_at(
        &self,
        block_number: u64,
    ) -> std::result::Result<TempoHardfork, revm::precompile::PrecompileError> {
        self.get_hardfork(block_number).map_err(|e| {
            zone_precompiles::zone_rpc_error(format!(
                "L1 hardfork unavailable for block={block_number}: {e}"
            ))
        })
    }
}

impl SequencerExt for L1StateProvider {
    fn latest_sequencer(&self) -> Option<Address> {
        self.get_latest_sequencer().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_transport::mock::Asserter;

    #[tokio::test(flavor = "multi_thread")]
    async fn get_hardfork_fetches_exact_block_writes_back_and_hits_cache() {
        let asserter = Asserter::new();
        asserter.push_success(&4217u64);
        let consensus = tempo_primitives::TempoHeader {
            inner: alloy_consensus::Header {
                number: 0,
                timestamp: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let header = tempo_alloy::rpc::TempoHeaderResponse {
            inner: alloy_rpc_types_eth::Header::new(consensus),
            timestamp_millis: 0,
        };
        let block: <TempoNetwork as alloy_network::Network>::BlockResponse =
            alloy_rpc_types_eth::Block::empty(header);
        asserter.push_success(&Some(block));

        let cache = L1StateCache::default();
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter)
            .erased();
        let provider = L1StateProvider::new_raw(
            L1StateProviderConfig::default(),
            cache.clone(),
            provider,
            tokio::runtime::Handle::current(),
        );
        let fetched = tokio::task::spawn_blocking({
            let provider = provider.clone();
            move || provider.get_hardfork(0)
        })
        .await
        .unwrap()
        .unwrap();

        assert_eq!(fetched, TempoHardfork::T0);
        assert_eq!(cache.read().hardfork_at(0), Some(TempoHardfork::T0));

        // No further mock response is configured, so this can only succeed from the cache.
        let cached = tokio::task::spawn_blocking(move || provider.get_hardfork(0))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cached, TempoHardfork::T0);
    }
}
