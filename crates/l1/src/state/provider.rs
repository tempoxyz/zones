//! Trust-neutral Tempo L1 RPC transport and the ordinary cache-backed state provider.
//!
//! [`L1RpcClient`] owns the shared Alloy provider, transport-level retry configuration, exact-block
//! RPC encoding, and the runtime bridge required by blocking EVM consumers. A client method is one
//! logical RPC operation; any bounded exponential backoff happens inside the configured transport.
//!
//! [`L1StateProvider`] layers ordinary read policy on top of that transport. It checks and updates
//! [`L1StateCache`] and owns the separate synchronous outer attempt loop used by EVM precompiles.
//! Keeping those responsibilities separate lets payload-scoped readers share the same transport
//! without inheriting the ordinary cache or its potentially indefinite retry policy.

use alloy_eips::BlockId;
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use alloy_transport::layers::RetryBackoffLayer;
use eyre::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    num::NonZeroU32,
};
use tempo_alloy::TempoNetwork;
use tracing::{debug, info, warn};
use zone_precompiles::{L1StateError, L1StorageReader};

use super::cache::L1StateCache;
use crate::rpc::rpc_connection_config;

/// Storage slots requested from `eth_getMultiProof`, grouped by account.
pub type L1ProofTargets = BTreeMap<Address, BTreeSet<B256>>;

/// Configuration for the Tempo L1 RPC transport and ordinary provider policy.
#[derive(Debug, Clone)]
pub struct L1StateProviderConfig {
    /// Optional known L1 chain ID, avoiding an RPC lookup when configured.
    pub chain_id: Option<u64>,
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
    /// Maximum number of synchronous RPC attempts per cache miss. `None` retries indefinitely.
    pub max_sync_attempts: Option<NonZeroU32>,
}

impl Default for L1StateProviderConfig {
    fn default() -> Self {
        Self {
            chain_id: None,
            l1_rpc_url: String::new(),
            portal_address: Address::ZERO,
            max_retries: 10,
            initial_backoff_ms: 20,
            retry_connection_interval: std::time::Duration::from_millis(100),
            max_sync_attempts: None,
        }
    }
}

/// Cloneable client for Tempo L1 RPC storage-related operations.
///
/// `L1RpcClient` is the core bridge between synchronous EVM execution (precompiles) and the
/// asynchronous L1 RPC layer. It holds:
///
/// - A [`DynProvider<TempoNetwork>`] (alloy HTTP provider) created once and reused across calls.
/// - A [`tokio::runtime::Handle`] used by [`block_on`](Self::block_on) to dispatch async work
///   from a blocking context.
///
/// # Sync dispatch safety
///
/// [`block_on`](Self::block_on) uses `tokio::task::block_in_place`, so it may run outside Tokio or
/// on a multi-thread runtime, but it panics on a current-thread runtime.
#[derive(Clone, Debug)]
pub struct L1RpcClient {
    /// Alloy provider pointed at **Tempo L1** and shared by all clones of this client.
    provider: DynProvider<TempoNetwork>,
    /// Handle to the Tokio runtime, used by this client's consumers to dispatch async RPC calls
    /// from a blocking (non-async) context.
    runtime_handle: tokio::runtime::Handle,
}

impl L1RpcClient {
    /// Connect to the configured L1 RPC endpoint.
    ///
    /// The provider is created eagerly from [`L1StateProviderConfig::l1_rpc_url`] and reused
    /// for the lifetime of this instance. The transport (HTTP or WebSocket) is auto-detected
    /// from the URL scheme. `runtime_handle` is stored for later use by
    /// [`block_on`](Self::block_on).
    pub async fn connect(
        config: &L1StateProviderConfig,
        runtime_handle: tokio::runtime::Handle,
    ) -> Result<Self> {
        let retry_layer =
            RetryBackoffLayer::new(config.max_retries, config.initial_backoff_ms, u64::MAX);
        let conn_config = rpc_connection_config(config.retry_connection_interval);
        let client = RpcClient::builder()
            .layer(retry_layer)
            .connect_with_config(&config.l1_rpc_url, conn_config)
            .await
            .map_err(|error| {
                eyre::eyre!(
                    "failed to connect Tempo L1 RPC client at {}: {error}",
                    config.l1_rpc_url
                )
            })?;
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>().connect_client(client);
        Ok(Self::from_provider(provider, runtime_handle))
    }

    /// Create an L1 RPC client from pre-constructed components.
    pub fn from_provider(
        provider: impl Provider<TempoNetwork> + 'static,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            provider: provider.erased(),
            runtime_handle,
        }
    }

    /// Dispatch an asynchronous RPC workflow from a blocking consumer.
    ///
    /// # Panics
    ///
    /// Panics if called from a current-thread Tokio runtime.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        tokio::task::block_in_place(|| self.runtime_handle.block_on(future))
    }

    /// Read a storage slot asynchronously at a specific L1 block.
    ///
    /// One call performs one logical RPC operation. The transport-level [`RetryBackoffLayer`]
    /// handles bounded retries with exponential backoff. The exact [`BlockId`] is forwarded
    /// unchanged, so callers may anchor the read by number or hash.
    pub async fn fetch_storage(&self, account: Address, slot: B256, at: BlockId) -> Result<B256> {
        let key = U256::from_be_bytes(slot.0);
        let value = self
            .provider
            .get_storage_at(account, key)
            .block_id(at)
            .await
            .map_err(|error| {
                warn!(%account, %slot, block = %at, %error, "eth_getStorageAt RPC call failed");
                eyre::eyre!(
                    "eth_getStorageAt failed for account={account} slot={slot} block={at}: {error}"
                )
            })?;
        let value = B256::from(value.to_be_bytes());
        debug!(%account, %slot, block = %at, %value, "fetched L1 storage slot from RPC");
        Ok(value)
    }

    /// Fetch an `eth_getMultiProof` response asynchronously at a specific L1 block.
    ///
    /// `block` is forwarded unchanged and targets are encoded as account/slot groups using the
    /// existing RPC JSON shape. This method only fetches the typed proof response; trusted-root
    /// verification, completeness checks, and cache promotion remain the caller's responsibility.
    pub async fn get_multi_proof(
        &self,
        block: BlockId,
        targets: &L1ProofTargets,
    ) -> Result<Vec<EIP1186AccountProofResponse>> {
        let targets = targets
            .iter()
            .map(|(&account, slots)| (account, slots.iter().copied().collect::<Vec<_>>()))
            .collect::<Vec<_>>();
        self.provider
            .client()
            .request("eth_getMultiProof", (targets, block))
            .await
            .map_err(Into::into)
    }
}

/// Cache-first, RPC-fallback provider for reading Tempo L1 contract storage.
///
/// `L1StateProvider` supplies the (unverified) L1 view used by EVM precompiles.
///  Storage reads are served from:
///
/// - A [`L1StateCache`] for fast in-memory lookups.
/// - On a cache miss, uses [`L1RpcClient`] to query `eth_getStorageAt` and populate the cache.
#[derive(Debug, Clone)]
pub struct L1StateProvider {
    /// Known L1 chain ID, if configured.
    chain_id: Option<u64>,
    /// In-memory cache of unverified L1 contract storage slots, checked before any RPC call.
    cache: L1StateCache,
    /// **Tempo L1** transport used as a fallback when the cache misses.
    rpc_client: L1RpcClient,
    /// Optional finite attempt limit for synchronous cache-miss fallback.
    max_sync_attempts: Option<NonZeroU32>,
}

impl L1StateProvider {
    /// Returns the chain ID reported by the configured L1 provider.
    pub async fn chain_id(&self) -> Result<u64> {
        match self.chain_id {
            Some(chain_id) => Ok(chain_id),
            None => Ok(self.rpc_client.provider.get_chain_id().await?),
        }
    }

    /// Create a new cache-backed provider.
    ///
    /// See [`L1RpcClient::connect`] for transport construction and runtime-handle behavior.
    pub async fn new(
        config: L1StateProviderConfig,
        cache: L1StateCache,
        runtime_handle: tokio::runtime::Handle,
    ) -> Result<Self> {
        let rpc_client = L1RpcClient::connect(&config, runtime_handle).await?;
        Ok(Self::with_client(config, cache, rpc_client))
    }

    /// Create a provider with an explicitly shared RPC client.
    pub fn with_client(
        config: L1StateProviderConfig,
        cache: L1StateCache,
        rpc_client: L1RpcClient,
    ) -> Self {
        Self {
            chain_id: config.chain_id,
            cache,
            rpc_client,
            max_sync_attempts: config.max_sync_attempts,
        }
    }

    /// Read a storage slot synchronously at a specific L1 block — cache first, RPC fallback.
    pub fn get_storage(&self, address: Address, slot: B256, block_number: u64) -> Result<B256> {
        {
            let mut cache = self.cache.lock();
            if let Some(value) = cache.get(address, slot, block_number) {
                return Ok(value);
            }
        }

        warn!(%address, %slot, block_number, "L1 storage cache miss, fetching from RPC");

        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let start = std::time::Instant::now();
            let at = BlockId::number(block_number);
            let result = self
                .rpc_client
                .block_on(self.rpc_client.fetch_storage(address, slot, at));
            let elapsed = start.elapsed();

            match result {
                Ok(value) => {
                    self.cache.lock().set(address, slot, block_number, value);
                    if attempt > 1 {
                        info!(%address, %slot, block_number, %value, ?elapsed, attempt, "L1 storage RPC fetch succeeded after retries");
                    } else {
                        info!(%address, %slot, block_number, %value, ?elapsed, "L1 storage RPC fetch succeeded");
                    }
                    return Ok(value);
                }
                Err(rpc_err) => {
                    if self
                        .max_sync_attempts
                        .is_some_and(|max_attempts| attempt >= max_attempts.get())
                    {
                        return Err(eyre::eyre!(
                            "L1 storage RPC fetch failed after {attempt} attempts for address={address} slot={slot} block={block_number}: {rpc_err}"
                        ));
                    }
                    warn!(%address, %slot, block_number, %rpc_err, ?elapsed, attempt, "L1 storage RPC fetch failed, retrying");
                }
            }
        }
    }

    /// Read a storage slot asynchronously at a specific L1 block — cache first, RPC fallback.
    ///
    /// Same cache semantics as [`get_storage`](Self::get_storage), but performs one asynchronous
    /// transport operation rather than using the synchronous outer attempt loop.
    pub async fn get_storage_async(
        &self,
        address: Address,
        slot: B256,
        block_number: u64,
    ) -> Result<B256> {
        {
            let mut cache = self.cache.lock();
            if let Some(value) = cache.get(address, slot, block_number) {
                return Ok(value);
            }
        }

        warn!(%address, %slot, block_number, "L1 storage cache miss, fetching from RPC");

        let at = BlockId::number(block_number);
        let value = self.rpc_client.fetch_storage(address, slot, at).await?;
        self.cache.lock().set(address, slot, block_number, value);
        Ok(value)
    }

    /// Returns the trust-neutral RPC client used by this provider.
    pub fn rpc_client(&self) -> &L1RpcClient {
        &self.rpc_client
    }

    /// Expose the shared (unverified) cache handle for external use
    pub fn cache(&self) -> &L1StateCache {
        &self.cache
    }
}

impl L1StorageReader for L1StateProvider {
    fn read_l1_storage(
        &self,
        account: Address,
        slot: B256,
        block_number: u64,
    ) -> std::result::Result<B256, L1StateError> {
        self.get_storage(account, slot, block_number)
            .map_err(|error| L1StateError::StorageUnavailable {
                account,
                slot,
                block_number,
                reason: error.to_string(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_provider::ProviderBuilder;

    #[tokio::test(flavor = "multi_thread")]
    async fn finite_sync_attempt_limit_returns_diagnostic_error() {
        let config = L1StateProviderConfig {
            max_sync_attempts: Some(NonZeroU32::MIN),
            ..Default::default()
        };
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect("http://127.0.0.1:1")
            .await
            .expect("HTTP transport construction is lazy")
            .erased();
        let rpc_client = L1RpcClient::from_provider(provider, tokio::runtime::Handle::current());
        let reader = L1StateProvider::with_client(config, L1StateCache::default(), rpc_client);

        let err =
            tokio::task::spawn_blocking(move || reader.get_storage(Address::ZERO, B256::ZERO, 7))
                .await
                .expect("storage task must not panic")
                .expect_err("dead endpoint must fail after one attempt");
        let message = err.to_string();
        assert!(message.contains("after 1 attempts"), "{message}");
        assert!(message.contains("block=7"), "{message}");
    }
}
