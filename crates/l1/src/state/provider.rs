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
use std::num::NonZeroU32;
use tempo_alloy::TempoNetwork;
use tracing::{debug, info, warn};
use zone_precompiles::{L1StateError, L1StorageReader};

use super::cache::L1StateCache;
use crate::rpc::rpc_connection_config;

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
                    warn!(%address, %slot, block_number, %rpc_err, ?elapsed, attempt, "L1 storage RPC fetch failed, retrying");
                }
            }
        }
    }

    /// Read and authenticate a storage slot against a trusted L1 state root.
    pub fn get_storage_with_proof(
        &self,
        address: Address,
        slot: B256,
        block_number: u64,
        state_root: B256,
    ) -> Result<B256> {
        {
            let mut cache = self.cache.lock();
            if let Some(value) = cache.get_verified(address, slot, block_number, state_root) {
                return Ok(value);
            }
        }

        warn!(
            %address,
            %slot,
            block_number,
            %state_root,
            "verified L1 storage cache miss, fetching proof from RPC"
        );

        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let start = std::time::Instant::now();
            let result = tokio::task::block_in_place(|| {
                self.runtime_handle.block_on(self.fetch_and_verify_slot(
                    address,
                    slot,
                    block_number,
                    state_root,
                ))
            });
            let elapsed = start.elapsed();

            match result {
                Ok(value) => {
                    self.cache
                        .lock()
                        .set_verified(address, slot, block_number, state_root, value);
                    info!(
                        %address,
                        %slot,
                        block_number,
                        %state_root,
                        %value,
                        ?elapsed,
                        attempt,
                        "authenticated L1 storage fetch succeeded"
                    );
                    return Ok(value);
                }
                Err(proof_err) => {
                    if self
                        .max_sync_attempts
                        .is_some_and(|max_attempts| attempt >= max_attempts.get())
                    {
                        return Err(eyre::eyre!(
                            "authenticated L1 storage fetch failed after {attempt} attempts for address={address} slot={slot} block={block_number} state_root={state_root}: {proof_err}"
                        ));
                    }
                    warn!(
                        %address,
                        %slot,
                        block_number,
                        %state_root,
                        %proof_err,
                        ?elapsed,
                        attempt,
                        "authenticated L1 storage fetch failed, retrying"
                    );
                }
            }
        }
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
    async fn fetch_and_verify_slot(
        &self,
        address: Address,
        slot: B256,
        block_number: u64,
        state_root: B256,
    ) -> Result<B256> {
        let block_id = BlockId::number(block_number);
        let response = self
            .provider
            .get_proof(address, vec![slot])
            .block_id(block_id)
            .await
            .map_err(|e| {
                warn!(%address, %slot, block_number, %e, "eth_getProof RPC call failed");
                eyre::eyre!(
                    "eth_getProof failed for address={address} slot={slot} block={block_number}: {e}"
                )
            })?;

        let result = verify_storage_proof(response, address, slot, state_root).map_err(|e| {
            eyre::eyre!(
                "eth_getProof verification failed for address={address} slot={slot} block={block_number} state_root={state_root}: {e}"
            )
        })?;
        debug!(
            %address,
            %slot,
            block_number,
            %state_root,
            %result,
            "fetched and verified L1 storage slot proof"
        );
        Ok(result)
    }
}

fn verify_storage_proof(
    response: EIP1186AccountProofResponse,
    address: Address,
    slot: B256,
    state_root: B256,
) -> Result<B256> {
    eyre::ensure!(
        response.address == address,
        "returned address {} does not match requested address {address}",
        response.address
    );
    eyre::ensure!(
        response.storage_proof.len() == 1,
        "returned {} storage proofs for one requested slot",
        response.storage_proof.len()
    );
    let storage_proof = &response.storage_proof[0];
    eyre::ensure!(
        storage_proof.key.as_b256() == slot,
        "returned slot {} does not match requested slot {slot}",
        storage_proof.key.as_b256()
    );
    let value = storage_proof.value;

    AccountProof::from_eip1186_proof(response)
        .verify(state_root)
        .map_err(|e| eyre::eyre!("{e}"))?;
    Ok(B256::from(value.to_be_bytes()))
}

impl L1StorageReader for L1StateProvider {
    fn read_l1_storage(
        &self,
        account: Address,
        slot: B256,
        block_number: u64,
        state_root: Option<B256>,
    ) -> std::result::Result<B256, L1StateError> {
        let result = match state_root {
            Some(state_root) => {
                self.get_storage_with_proof(account, slot, block_number, state_root)
            }
            None => self.get_storage(account, slot, block_number),
        };
        result.map_err(|error| L1StateError::StorageUnavailable {
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

        let err =
            tokio::task::spawn_blocking(move || reader.get_storage(Address::ZERO, B256::ZERO, 7))
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
    async fn authenticated_and_unauthenticated_cache_entries_are_isolated() {
        let address = Address::repeat_byte(0x11);
        let slot = B256::with_last_byte(7);
        let asserter = Asserter::new();
        asserter.push_success(&empty_account_response(address, slot));
        asserter.push_success(&U256::from(9));
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

        let proof_reader = reader.clone();
        assert_eq!(
            tokio::task::spawn_blocking(move || {
                proof_reader.get_storage_with_proof(address, slot, 7, EMPTY_ROOT_HASH)
            })
            .await
            .unwrap()
            .unwrap(),
            B256::ZERO
        );
        let unauthenticated_reader = reader.clone();
        assert_eq!(
            tokio::task::spawn_blocking(move || {
                unauthenticated_reader.get_storage(address, slot, 7)
            })
            .await
            .unwrap()
            .unwrap(),
            B256::from(U256::from(9).to_be_bytes())
        );
        assert!(asserter.read_q().is_empty());
    }
}
