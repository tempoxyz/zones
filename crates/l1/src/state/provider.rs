//! Cache-first, RPC-fallback provider for reading L1 contract storage slots.
//!
//! [`L1StateProvider`] wraps a [`L1StateCache`] and a [`DynProvider<TempoNetwork>`] backed by an
//! HTTP transport. Reads are served from the in-memory cache when possible. On cache miss the
//! provider falls back to `eth_getStorageAt` via the shared HTTP provider and writes the result
//! back into the cache.
//!
//! Both a synchronous ([`L1StateProvider::get_storage`]) and an asynchronous
//! ([`L1StateProvider::get_storage_async`]) entry point are provided. The synchronous variant is
//! intended for use inside EVM precompiles where async is unavailable — it retries with backoff
//! until the read succeeds, so an L1 RPC outage stalls block production rather than deciding
//! block validity from one node's endpoint health.

use alloy_primitives::{Address, B256, U256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::BlockId;
use alloy_transport::{TransportError, layers::RetryBackoffLayer};
use eyre::Result;
use std::{num::NonZeroU32, time::Duration};
use tempo_alloy::TempoNetwork;
use tracing::{debug, info, warn};
use zone_precompiles::{L1StateError, L1StorageReader};

use super::cache::L1StateCache;
use crate::{L1BlockTracker, rpc::rpc_connection_config};

/// Upper bound on the delay between synchronous cache-miss attempts.
///
/// A sustained outage settles into a steady low-rate poll instead of spinning against an endpoint
/// that may already be rate-limiting us.
const MAX_SYNC_RETRY_BACKOFF: Duration = Duration::from_secs(2);

/// Largest exponent applied to the initial backoff, keeping the shift well clear of overflow.
const MAX_SYNC_BACKOFF_SHIFT: u32 = 20;

/// Configuration for the [`L1StateProvider`].
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
    /// Maximum attempts per synchronous wait, applied independently to each phase of a cache
    /// miss: first waiting for the observed L1 head, then fetching over RPC. A caller setting
    /// this to `n` can therefore wait up to `2n` times before failing. `None` waits indefinitely.
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
    /// Known L1 chain ID, if configured.
    chain_id: Option<u64>,
    /// In-memory cache of L1 contract storage slots, checked before any RPC call.
    cache: L1StateCache,
    /// HTTP provider pointed at **Tempo L1**, used as a fallback when the cache misses.
    /// Wraps a [`RetryBackoffLayer`] that handles retries with exponential backoff.
    provider: DynProvider<TempoNetwork>,
    /// Handle to the tokio runtime, used by [`get_storage`](Self::get_storage) to
    /// dispatch async RPC calls from a blocking (non-async) context.
    runtime_handle: tokio::runtime::Handle,
    /// Optional finite attempt limit for synchronous cache-miss fallback.
    max_sync_attempts: Option<NonZeroU32>,
    /// Base delay for the synchronous cache-miss wait, shared with the transport retry layer.
    initial_backoff_ms: u64,
    /// Independently observed L1 head, used to hold back reads above it.
    ///
    /// Unset for callers with no subscriber (tests, the no-L1 CLI fallback), which read unbounded.
    head_bound: Option<L1BlockTracker>,
}

impl L1StateProvider {
    /// Returns the chain ID reported by the configured L1 provider.
    pub async fn chain_id(&self) -> Result<u64> {
        match self.chain_id {
            Some(chain_id) => Ok(chain_id),
            None => Ok(self.provider.get_chain_id().await?),
        }
    }

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
            chain_id: config.chain_id,
            cache,
            provider,
            runtime_handle,
            max_sync_attempts: config.max_sync_attempts,
            initial_backoff_ms: config.initial_backoff_ms,
            head_bound: None,
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
            chain_id: config.chain_id,
            cache,
            provider,
            runtime_handle,
            max_sync_attempts: config.max_sync_attempts,
            initial_backoff_ms: config.initial_backoff_ms,
            head_bound: None,
        }
    }

    /// Hold synchronous reads back until `tracker` has independently observed the L1 block.
    ///
    /// Only production wires this; callers without a subscriber read unbounded.
    #[must_use]
    pub fn with_head_bound(mut self, tracker: L1BlockTracker) -> Self {
        self.head_bound = Some(tracker);
        self
    }

    /// Read a storage slot synchronously at a specific L1 block — cache first, RPC fallback.
    ///
    /// This method is designed for use inside EVM precompiles that run on a **blocking thread**.
    ///
    /// # Why failures stall instead of erroring
    ///
    /// An RPC outcome must never decide block validity. Whether a read succeeds depends on the
    /// health and sync state of one node's endpoint, so converting a failure into a precompile
    /// error — which is fatal, not a revert — lets two honest nodes disagree about the same block.
    /// A node whose gateway returns 502 would reject a block its peers accept. On cache miss this
    /// therefore retries until the value is fetched, stalling block production rather than failing
    /// closed.
    ///
    /// Bounding the request is a separate, sound check, and it happens *before* the request in
    /// [`await_observed_head`](Self::await_observed_head) rather than after it by classifying
    /// whatever the endpoint happened to return.
    ///
    /// [`RetryBackoffLayer`] is the single retry policy: it classifies and backs off per request.
    /// This loop adds no classification of its own, only a capped delay between attempts so a
    /// sustained outage polls at a steady low rate instead of spinning.
    ///
    /// `max_sync_attempts` bounds the wait for callers that must fail finitely — tests and the
    /// no-L1 CLI fallback. Production leaves it unset.
    ///
    /// # Panics
    ///
    /// Panics if called from within an async context on the same tokio runtime (see struct-level
    /// docs).
    pub fn get_storage(&self, address: Address, slot: B256, block_number: u64) -> Result<B256> {
        {
            let mut cache = self.cache.lock();
            if let Some(value) = cache.get(address, slot, block_number) {
                return Ok(value);
            }
        }

        warn!(%address, %slot, block_number, "L1 storage cache miss, fetching from RPC");

        // Bound the request against independently observed L1 state before it reaches the
        // transport, so an attacker-chosen block number cannot steer this node's RPC traffic.
        self.await_observed_head(block_number)?;

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
                    let backoff = self.sync_retry_backoff(attempt);
                    warn!(%address, %slot, block_number, %rpc_err, ?elapsed, attempt, ?backoff, "L1 storage RPC fetch failed, stalling before retry");
                    std::thread::sleep(backoff);
                }
            }
        }
    }

    /// Wait until the requested L1 block is at or below the independently observed head.
    ///
    /// A read above the local head is not evidence that the block is bogus — this node may simply
    /// be behind — so it must never become a validity verdict. Waiting keeps the outcome the same
    /// on every honest node: a lagging node catches up and proceeds, while a block number no
    /// honest head ever reaches stalls without an `eth_getStorageAt` ever being issued. That is
    /// what stops an attacker-chosen number from steering this node's RPC.
    ///
    /// A tracker that has observed nothing yet reads unbounded. `latest` is only populated once
    /// the subscriber records its first block, so blocking before then would stall a node whose
    /// subscriber has not made first contact. This is the weakest point of the bound: the first
    /// Tempo import — the one read with no parent-hash or contiguity constraint — is also the one
    /// most likely to run while the tracker is still cold. Closing that needs a startup ordering
    /// guarantee between the subscriber and block execution, which is tracked separately.
    ///
    /// The wait is driven by the tracker's observation channel rather than a timer, so it wakes
    /// the moment the head advances instead of after a backoff interval. Unlike the RPC retry
    /// loop there is no remote endpoint to pace here — this only reads local process state.
    fn await_observed_head(&self, block_number: u64) -> Result<()> {
        let Some(tracker) = self.head_bound.as_ref() else {
            return Ok(());
        };
        // Subscribe once for the whole wait so an observation recorded between checks still
        // wakes us instead of being missed by a fresh subscribe.
        let mut observations = tracker.subscribe_observations();

        let mut waits = 0u32;
        loop {
            let Some(latest) = tracker.latest() else {
                return Ok(());
            };
            if block_number <= latest.number {
                if waits > 0 {
                    info!(
                        block_number,
                        observed_head = latest.number,
                        waits,
                        "L1 head caught up to the requested block"
                    );
                }
                return Ok(());
            }

            waits += 1;
            if self
                .max_sync_attempts
                .is_some_and(|max_attempts| waits >= max_attempts.get())
            {
                return Err(eyre::eyre!(
                    "L1 block {block_number} is above the observed head {} after {waits} attempts",
                    latest.number
                ));
            }
            warn!(
                block_number,
                observed_head = latest.number,
                waits,
                "L1 read is ahead of the observed head, waiting"
            );
            if tokio::task::block_in_place(|| self.runtime_handle.block_on(observations.changed()))
                .is_err()
            {
                // The subscriber is gone, so nothing will advance the head; fall through and let
                // the RPC layer decide rather than waiting forever.
                return Ok(());
            }
        }
    }

    /// Delay before the next synchronous cache-miss attempt.
    ///
    /// Grows exponentially from the transport's initial backoff and saturates at
    /// [`MAX_SYNC_RETRY_BACKOFF`], so a long outage keeps polling without hammering the endpoint.
    fn sync_retry_backoff(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(MAX_SYNC_BACKOFF_SHIFT);
        Duration::from_millis(self.initial_backoff_ms.saturating_mul(1u64 << shift))
            .min(MAX_SYNC_RETRY_BACKOFF)
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
            let mut cache = self.cache.lock();
            if let Some(value) = cache.get(address, slot, block_number) {
                return Ok(value);
            }
        }

        warn!(%address, %slot, block_number, "L1 storage cache miss, fetching from RPC");

        let value = self.fetch_slot(address, slot, block_number).await?;
        self.cache.lock().set(address, slot, block_number, value);
        Ok(value)
    }

    /// Expose the shared cache handle for external use (e.g. the engine).
    pub fn cache(&self) -> &L1StateCache {
        &self.cache
    }

    /// Fetch a single storage slot from L1 at a specific block via the shared HTTP provider.
    async fn fetch_slot(
        &self,
        address: Address,
        slot: B256,
        block_number: u64,
    ) -> std::result::Result<B256, TransportError> {
        let key = U256::from_be_bytes(slot.0);
        let block_id = BlockId::number(block_number);
        let value: U256 = self
            .provider
            .get_storage_at(address, key)
            .block_id(block_id)
            .await
            .inspect_err(
                |error| warn!(%address, %slot, block_number, %error, "eth_getStorageAt RPC call failed"),
            )?;

        let result = B256::from(value.to_be_bytes());
        debug!(%address, %slot, block_number, %result, "fetched L1 storage slot from RPC");
        Ok(result)
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
    use alloy_eips::NumHash;

    async fn test_reader(config: L1StateProviderConfig) -> L1StateProvider {
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect("http://127.0.0.1:1")
            .await
            .expect("HTTP transport construction is lazy")
            .erased();
        L1StateProvider::new_raw(
            config,
            L1StateCache::default(),
            provider,
            tokio::runtime::Handle::current(),
        )
    }

    /// Tracker whose contiguous observations run through `head`.
    fn tracker_at(head: u64) -> L1BlockTracker {
        let tracker = L1BlockTracker::default();
        tracker.initialize_consumed_through(head.saturating_sub(1));
        tracker
            .record(NumHash::new(head, B256::repeat_byte(0xab)))
            .expect("contiguous observation");
        tracker
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reads_are_unbounded_without_an_observed_head() {
        // No tracker at all: the no-L1 CLI fallback and tests must not block.
        let unbounded = test_reader(L1StateProviderConfig::default()).await;
        unbounded
            .await_observed_head(u64::MAX)
            .expect("an absent tracker imposes no bound");

        // Tracker present but cold: blocking here would deadlock node startup.
        let cold = test_reader(L1StateProviderConfig::default())
            .await
            .with_head_bound(L1BlockTracker::default());
        cold.await_observed_head(u64::MAX)
            .expect("an unobserved tracker imposes no bound");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reads_at_or_below_the_observed_head_proceed() {
        let reader = test_reader(L1StateProviderConfig::default())
            .await
            .with_head_bound(tracker_at(10));

        reader
            .await_observed_head(10)
            .expect("head itself is in range");
        reader
            .await_observed_head(3)
            .expect("below head is in range");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reads_above_the_observed_head_wait_rather_than_reject() {
        let tracker = tracker_at(10);
        let reader = test_reader(L1StateProviderConfig::default())
            .await
            .with_head_bound(tracker.clone());

        // A lagging node must not treat "ahead of my head" as invalid — it waits and proceeds.
        let waiting = tokio::task::spawn_blocking(move || reader.await_observed_head(11));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !waiting.is_finished(),
            "read above the head must not resolve early"
        );

        // The wait is observation-driven, so recording the block releases it immediately rather
        // than after a backoff interval.
        tracker
            .record(NumHash::new(11, B256::repeat_byte(0xcd)))
            .expect("contiguous observation");
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .expect("advancing the head must release the wait")
            .expect("wait task must not panic")
            .expect("block 11 is observed once recorded");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bounded_callers_fail_finitely_above_the_observed_head() {
        let reader = test_reader(L1StateProviderConfig {
            max_sync_attempts: Some(NonZeroU32::MIN),
            ..Default::default()
        })
        .await
        .with_head_bound(tracker_at(10));

        let err = tokio::task::spawn_blocking(move || {
            reader.get_storage(Address::ZERO, B256::ZERO, 5_000)
        })
        .await
        .expect("storage task must not panic")
        .expect_err("a bounded caller must give up above the head");
        let message = err.to_string();
        assert!(message.contains("above the observed head 10"), "{message}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn sync_retry_backoff_grows_then_saturates() {
        let reader = test_reader(L1StateProviderConfig {
            initial_backoff_ms: 20,
            ..Default::default()
        })
        .await;

        assert_eq!(reader.sync_retry_backoff(1), Duration::from_millis(20));
        assert_eq!(reader.sync_retry_backoff(2), Duration::from_millis(40));
        assert_eq!(reader.sync_retry_backoff(3), Duration::from_millis(80));

        // A long outage settles at the cap rather than growing without bound or overflowing.
        assert_eq!(reader.sync_retry_backoff(20), MAX_SYNC_RETRY_BACKOFF);
        assert_eq!(reader.sync_retry_backoff(u32::MAX), MAX_SYNC_RETRY_BACKOFF);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn finite_sync_attempt_limit_returns_diagnostic_error() {
        let reader = test_reader(L1StateProviderConfig {
            max_sync_attempts: Some(NonZeroU32::MIN),
            ..Default::default()
        })
        .await;

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
