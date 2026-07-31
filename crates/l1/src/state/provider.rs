//! Cache-first, RPC-fallback provider for reading L1 contract storage slots.
//!
//! [`L1StateProvider`] wraps a [`L1StateCache`] and a [`DynProvider<TempoNetwork>`] backed by an
//! HTTP transport. Canonical block execution supplies a trusted L1 state root and resolves cache
//! misses with `eth_getProof`; standalone simulations retain the unauthenticated
//! `eth_getStorageAt` path.
//!
//! Synchronous entry points are provided for EVM precompiles where async is unavailable. Canonical
//! Proof fetches and unauthenticated storage fetches use the same retry policy so transient L1
//! RPC outages stall execution rather than producing divergent state.

use alloy_primitives::{Address, B256, U256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::{BlockId, EIP1186AccountProofResponse};
use alloy_transport::layers::RetryBackoffLayer;
use eyre::Result;
use reth_trie_common::AccountProof;
use std::{future::Future, marker::PhantomData, num::NonZeroU32};
use tempo_alloy::TempoNetwork;
use thiserror::Error;
use tracing::{debug, info, warn};
use zone_precompiles::{L1StateError, L1StorageReader, TempoAnchor};

use super::cache::L1StateCache;
use crate::rpc::rpc_connection_config;

#[derive(Debug, Error)]
pub enum FetchError {
    #[error("retryable L1 fetch error: {0}")]
    Retryable(String),
    #[error("fatal L1 fetch error: {0}")]
    Fatal(String),
}

/// Mode policy controlling cache provenance and RPC fallback behavior.
pub trait L1ReadMode: Clone + Send + Sync + 'static {
    const NEEDS_PROOF: bool;

    fn fetch(
        provider: &L1StateProvider<Self>,
        address: Address,
        slot: B256,
        anchor: TempoAnchor,
    ) -> impl Future<Output = Result<B256, FetchError>> + Send;
}

/// Provider mode for ordinary, unverified L1 storage reads.
#[derive(Debug, Clone, Copy)]
pub struct Unverified;

impl L1ReadMode for Unverified {
    const NEEDS_PROOF: bool = false;

    async fn fetch(
        provider: &L1StateProvider<Self>,
        address: Address,
        slot: B256,
        anchor: TempoAnchor,
    ) -> Result<B256, FetchError> {
        provider
            .fetch_slot(address, slot, anchor.block_number())
            .await
            .map_err(|error| FetchError::Retryable(error.to_string()))
    }
}

/// Provider mode for L1 storage reads verified against a trusted state root.
#[derive(Debug, Clone, Copy)]
pub struct ProofVerified;

impl L1ReadMode for ProofVerified {
    const NEEDS_PROOF: bool = true;

    async fn fetch(
        provider: &L1StateProvider<Self>,
        address: Address,
        slot: B256,
        anchor: TempoAnchor,
    ) -> Result<B256, FetchError> {
        provider
            .fetch_slot_with_proof(address, slot, anchor.block_number(), anchor.state_root())
            .await
    }
}

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
pub struct L1StateProvider<M = Unverified> {
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
    /// Compile-time policy selecting ordinary or proof-verified cache and RPC access.
    mode: PhantomData<M>,
}

impl<M: L1ReadMode> L1StateProvider<M> {
    /// Returns the chain ID reported by the configured L1 provider.
    pub async fn chain_id(&self) -> Result<u64> {
        match self.chain_id {
            Some(id) => Ok(id),
            None => Ok(self.provider.get_chain_id().await?),
        }
    }

    /// Expose the shared cache handle for external use (e.g. the engine).
    pub fn cache(&self) -> &L1StateCache {
        &self.cache
    }

    /// Read a storage slot synchronously at an L1 anchor — cache first, with a retrying,
    /// mode-specific RPC fallback.
    ///
    /// Proof-verified providers only accept proved cache entries and authenticate cache misses
    /// against the anchor's state root. Unverified providers use `eth_getStorageAt`.
    pub fn get_storage(&self, address: Address, slot: B256, anchor: TempoAnchor) -> Result<B256> {
        if let Some(value) = self
            .cache
            .lock()
            .get::<M>(address, slot, anchor.block_number())
        {
            return Ok(value);
        }
        warn!(%address, %slot, block_number = anchor.block_number(), requires_proof = M::NEEDS_PROOF, "L1 storage cache miss, fetching from RPC");
        let mut attempt = 0;
        loop {
            attempt += 1;
            let start = std::time::Instant::now();
            let result = tokio::task::block_in_place(|| {
                self.runtime_handle
                    .block_on(M::fetch(self, address, slot, anchor))
            });
            let elapsed = start.elapsed();
            match result {
                Ok(value) => {
                    // The mode that selected the fetch also determines the inserted provenance;
                    // proved values can only originate from the proof-verifying policy.
                    let value =
                        self.cache
                            .lock()
                            .set::<M>(address, slot, anchor.block_number(), value)?;
                    info!(%address, %slot, block_number = anchor.block_number(), %value, ?elapsed, attempt, "L1 storage RPC fetch succeeded");
                    return Ok(value);
                }
                Err(FetchError::Fatal(error)) => {
                    return Err(eyre::eyre!(
                        "L1 storage proof verification failed for address={address} slot={slot} block={} state_root={}: {error}",
                        anchor.block_number(),
                        anchor.state_root()
                    ));
                }
                Err(FetchError::Retryable(error)) => {
                    if self
                        .max_sync_attempts
                        .is_some_and(|max| attempt >= max.get())
                    {
                        return Err(eyre::eyre!(
                            "L1 storage RPC fetch failed after {attempt} attempts for address={address} slot={slot} block={}: {error}",
                            anchor.block_number()
                        ));
                    }
                    warn!(%address, %slot, block_number = anchor.block_number(), %error, ?elapsed, attempt, "L1 storage RPC fetch failed, retrying");
                }
            }
        }
    }

    /// Read a storage slot asynchronously at an L1 anchor — cache first, with one mode-specific
    /// logical RPC fetch. Transport retries are handled by [`RetryBackoffLayer`].
    pub async fn get_storage_async(
        &self,
        address: Address,
        slot: B256,
        anchor: TempoAnchor,
    ) -> Result<B256> {
        if let Some(value) = self
            .cache
            .lock()
            .get::<M>(address, slot, anchor.block_number())
        {
            return Ok(value);
        }
        warn!(%address, %slot, block_number = anchor.block_number(), requires_proof = M::NEEDS_PROOF, "L1 storage cache miss, fetching from RPC");

        let value = M::fetch(self, address, slot, anchor).await?;
        self.cache
            .lock()
            .set::<M>(address, slot, anchor.block_number(), value)
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

    /// Fetch and verify a single EIP-1186 storage proof against `state_root`.
    async fn fetch_slot_with_proof(
        &self,
        address: Address,
        slot: B256,
        block_number: u64,
        state_root: B256,
    ) -> std::result::Result<B256, FetchError> {
        let block_id = BlockId::number(block_number);
        let response = self
            .provider
            .get_proof(address, vec![slot])
            .block_id(block_id)
            .await
            .map_err(|e| FetchError::Retryable(e.to_string()))?;

        let result = verify_storage_proof(response, address, slot, state_root)
            .map_err(|e| FetchError::Fatal(e.to_string()))?;
        debug!(block_number, %address, %slot, %state_root, %result, "fetched and verified L1 storage proof");
        Ok(result)
    }
}

impl<M: L1ReadMode> L1StorageReader for L1StateProvider<M> {
    fn read_l1_storage(
        &self,
        anchor: TempoAnchor,
        account: Address,
        slot: B256,
    ) -> std::result::Result<B256, L1StateError> {
        self.get_storage(account, slot, anchor)
            .map_err(|error| L1StateError::StorageUnavailable {
                account,
                slot,
                block_number: anchor.block_number(),
                reason: error.to_string(),
            })
    }
}

impl L1StateProvider<Unverified> {
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
        Ok(Self::new_raw(config, cache, provider, runtime_handle))
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
            mode: PhantomData,
        }
    }

    /// Convert this provider into the proof-verifying mode used for payload construction.
    ///
    /// The returned provider only admits proof-verified cache entries and resolves misses with
    /// `eth_getProof`; it cannot fall back to an ordinary storage fetch.
    pub fn proved(self) -> L1StateProvider<ProofVerified> {
        L1StateProvider {
            chain_id: self.chain_id,
            cache: self.cache,
            provider: self.provider,
            runtime_handle: self.runtime_handle,
            max_sync_attempts: self.max_sync_attempts,
            mode: PhantomData,
        }
    }
}

fn verify_storage_proof(
    response: EIP1186AccountProofResponse,
    address: Address,
    slot: B256,
    state_root: B256,
) -> Result<B256> {
    let proof = AccountProof::from_eip1186_proof(response);
    eyre::ensure!(proof.address == address, "unexpected proof address");
    eyre::ensure!(proof.storage_proofs.len() == 1, "expected 1 storage proof");
    let storage_proof = &proof.storage_proofs[0];
    eyre::ensure!(storage_proof.key == slot, "unexpected proof slot",);
    let value = storage_proof.value;

    proof.verify(state_root).map_err(|e| eyre::eyre!("{e}"))?;
    Ok(value.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::constants::{EMPTY_ROOT_HASH, KECCAK_EMPTY};
    use alloy_rpc_types_eth::EIP1186StorageProof;
    use alloy_transport::mock::Asserter;

    fn empty_account_response(address: Address, slot: B256) -> EIP1186AccountProofResponse {
        EIP1186AccountProofResponse {
            address,
            code_hash: KECCAK_EMPTY,
            storage_hash: EMPTY_ROOT_HASH,
            storage_proof: vec![EIP1186StorageProof {
                key: slot.into(),
                value: U256::ZERO,
                proof: Vec::new(),
            }],
            ..Default::default()
        }
    }

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
        let reader = L1StateProvider::new_raw(
            config,
            L1StateCache::default(),
            provider,
            tokio::runtime::Handle::current(),
        );

        let err = tokio::task::spawn_blocking(move || {
            reader.get_storage(Address::ZERO, B256::ZERO, TempoAnchor::dummy(7))
        })
        .await
        .expect("storage task must not panic")
        .expect_err("dead endpoint must fail after one attempt");
        let message = err.to_string();
        assert!(message.contains("after 1 attempts"), "{message}");
        assert!(message.contains("block=7"), "{message}");
    }

    #[test]
    fn verifies_empty_account_storage_exclusion_proof() {
        let address = Address::repeat_byte(0x11);
        let slot = B256::with_last_byte(7);
        assert_eq!(
            verify_storage_proof(
                empty_account_response(address, slot),
                address,
                slot,
                EMPTY_ROOT_HASH,
            )
            .unwrap(),
            B256::ZERO
        );
    }

    #[test]
    fn rejects_proof_bound_to_wrong_root_address_slot_or_value() {
        let address = Address::repeat_byte(0x11);
        let slot = B256::with_last_byte(7);

        assert!(
            verify_storage_proof(
                empty_account_response(address, slot),
                address,
                slot,
                B256::with_last_byte(1),
            )
            .is_err()
        );
        assert!(
            verify_storage_proof(
                empty_account_response(Address::repeat_byte(0x22), slot),
                address,
                slot,
                EMPTY_ROOT_HASH,
            )
            .is_err()
        );
        assert!(
            verify_storage_proof(
                empty_account_response(address, B256::with_last_byte(8)),
                address,
                slot,
                EMPTY_ROOT_HASH,
            )
            .is_err()
        );

        let mut response = empty_account_response(address, slot);
        response.storage_proof[0].value = U256::ONE;
        assert!(verify_storage_proof(response, address, slot, EMPTY_ROOT_HASH).is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn proved_cache_miss_uses_proof_and_satisfies_an_ordinary_read() {
        let address = Address::repeat_byte(0x11);
        let slot = B256::with_last_byte(7);
        let asserter = Asserter::new();
        asserter.push_success(&empty_account_response(address, slot));
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter.clone())
            .erased();
        let reader = L1StateProvider::new_raw(
            L1StateProviderConfig {
                max_sync_attempts: Some(NonZeroU32::MIN),
                ..Default::default()
            },
            L1StateCache::default(),
            provider,
            tokio::runtime::Handle::current(),
        );

        let proof_reader = reader.clone().proved();
        assert_eq!(
            tokio::task::spawn_blocking(move || {
                proof_reader.get_storage(address, slot, TempoAnchor::dummy(7))
            .await
            .unwrap()
            .unwrap(),
            B256::ZERO
        );
        let unauthenticated_reader = reader.clone();
        assert_eq!(
            tokio::task::spawn_blocking(move || {
                unauthenticated_reader.get_storage(address, slot, TempoAnchor::dummy(7))
            })
            .await
            .unwrap()
            .unwrap(),
            B256::ZERO
        );
        assert!(asserter.read_q().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ordinary_cache_miss_uses_get_storage_at() {
        let address = Address::repeat_byte(0x11);
        let slot = B256::with_last_byte(7);
        let expected = U256::from(42);
        let asserter = Asserter::new();
        asserter.push_success(&expected);
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter.clone())
            .erased();
        let reader = L1StateProvider::new_raw(
            L1StateProviderConfig::default(),
            L1StateCache::default(),
            provider,
            tokio::runtime::Handle::current(),
        );

        assert_eq!(
            reader
                .get_storage_async(address, slot, TempoAnchor::dummy(7))
                .await
                .unwrap(),
            B256::from(expected.to_be_bytes())
        );
        assert!(asserter.read_q().is_empty());
    }
}
