//! Cache-first, RPC-fallback provider for TIP-403 policy authorization.
//!
//! [`PolicyProvider`] wraps a [`SharedPolicyCache`] and an L1 HTTP provider. Authorization
//! checks are served from the in-memory cache when possible. On cache miss the provider
//! falls back to `isAuthorized(policyId, user)` via the L1 RPC and writes the result back
//! into the cache so subsequent lookups are instant.
//!
//! This mirrors the [`L1StateProvider`](crate::l1_state::L1StateProvider) pattern used for
//! storage slot reads.

use alloy_primitives::Address;
use alloy_provider::DynProvider;
use alloy_rpc_types_eth::BlockId;
use eyre::Result;
use revm::precompile::PrecompileError;
use tempo_alloy::TempoNetwork;
use tempo_contracts::precompiles::{
    ITIP20, ITIP403Registry, ITIP403Registry::PolicyType, TIP403_REGISTRY_ADDRESS,
};
use tracing::{debug, info, warn};
use zone_precompiles::policy::PolicyCheck;

use super::builtin_authorization;

use super::{AuthRole, CompoundData, SharedPolicyCache, metrics::Tip403Metrics};

/// Cache-first, RPC-fallback provider for TIP-403 policy authorization.
///
/// Wraps a [`SharedPolicyCache`] (populated by the [`L1Subscriber`](crate::l1::L1Subscriber))
/// and an L1 HTTP provider. When the cache cannot resolve an authorization query (e.g. the
/// policy existed before the subscriber started), the provider falls back to L1 RPC calls and
/// caches the result for future lookups.
///
/// # Sync dispatch safety
///
/// [`is_authorized`](Self::is_authorized) calls `tokio::task::block_in_place` +
/// `runtime_handle.block_on(...)` to execute async RPC work from a blocking context.
/// This is safe when the caller runs on a blocking thread (e.g. the payload builder).
#[derive(Debug, Clone)]
pub struct PolicyProvider {
    /// Shared in-memory policy cache, populated by the subscriber and RPC fallback.
    cache: SharedPolicyCache,
    /// L1 HTTP provider for RPC fallback on cache miss.
    provider: DynProvider<TempoNetwork>,
    /// Tokio runtime handle for `block_in_place` + `block_on` in sync call sites.
    runtime_handle: tokio::runtime::Handle,
    /// Metrics for cache hit/miss rates and RPC resolution latency.
    metrics: Tip403Metrics,
}

impl PolicyProvider {
    /// Create a new provider from components.
    pub fn new(
        cache: SharedPolicyCache,
        provider: DynProvider<TempoNetwork>,
        runtime_handle: tokio::runtime::Handle,
    ) -> Self {
        Self {
            cache,
            provider,
            runtime_handle,
            metrics: Tip403Metrics::default(),
        }
    }

    /// Returns a reference to the underlying shared policy cache.
    pub fn cache(&self) -> &SharedPolicyCache {
        &self.cache
    }

    /// Returns a reference to the TIP-403 metrics.
    pub fn metrics(&self) -> &Tip403Metrics {
        &self.metrics
    }

    /// Cache-first, RPC-fallback authorization check (sync).
    ///
    /// Intended for use inside the payload builder which runs on a blocking thread.
    /// On cache miss, fetches policy data from L1 via RPC, caches it, and returns
    /// the authorization result.
    ///
    /// # Panics
    ///
    /// Panics if called from within an async context on the same tokio runtime.
    pub fn is_authorized(
        &self,
        token: Address,
        user: Address,
        block_number: u64,
        role: AuthRole,
    ) -> Result<bool> {
        self.metrics.authorization_checks_total.increment(1);

        // 1. Try cache first
        if let Some(result) = self
            .cache
            .read()
            .is_authorized(token, user, block_number, role)
        {
            self.metrics.cache_hits.increment(1);
            return Ok(result);
        }

        // 2. Cache miss — fetch from L1 via RPC
        self.metrics.cache_misses.increment(1);
        debug!(
            %token, %user, block_number, ?role,
            "Policy cache miss, fetching from L1 RPC"
        );
        tokio::task::block_in_place(|| {
            self.runtime_handle
                .block_on(self.fetch_and_cache(token, user, block_number, role))
        })
    }

    /// Async version for non-blocking contexts.
    pub async fn is_authorized_async(
        &self,
        token: Address,
        user: Address,
        block_number: u64,
        role: AuthRole,
    ) -> Result<bool> {
        self.metrics.authorization_checks_total.increment(1);

        if let Some(result) = self
            .cache
            .read()
            .is_authorized(token, user, block_number, role)
        {
            self.metrics.cache_hits.increment(1);
            return Ok(result);
        }

        self.metrics.cache_misses.increment(1);
        debug!(
            %token, %user, block_number, ?role,
            "Policy cache miss, fetching from L1 RPC (async)"
        );
        self.fetch_and_cache(token, user, block_number, role).await
    }

    /// Fetch authorization data from L1, cache it, and return the result.
    async fn fetch_and_cache(
        &self,
        token: Address,
        user: Address,
        block_number: u64,
        role: AuthRole,
    ) -> Result<bool> {
        let start = std::time::Instant::now();

        // Resolve the token's transferPolicyId
        let policy_id = self.resolve_policy_id(token, block_number).await?;

        // Builtins — no RPC needed
        if let Some(authorized) = builtin_authorization(policy_id) {
            self.cache
                .write()
                .set_token_policy(token, block_number, policy_id);
            self.metrics
                .rpc_resolution_duration_seconds
                .record(start.elapsed().as_secs_f64());
            return Ok(authorized);
        }

        let result = self
            .resolve_policy_authorization(policy_id, user, block_number, role)
            .await;

        self.metrics
            .rpc_resolution_duration_seconds
            .record(start.elapsed().as_secs_f64());
        result
    }

    /// Fetch authorization for a simple (whitelist/blacklist) policy.
    ///
    /// Calls `isAuthorized(policyId, user)` on L1, derives the raw membership boolean
    /// from the result + policy type, and caches it in the [`MembershipSet`](super::MembershipSet).
    async fn fetch_and_cache_simple(
        &self,
        policy_id: u64,
        user: Address,
        block_number: u64,
        policy_type: PolicyType,
    ) -> Result<bool> {
        let authorized = self
            .rpc_is_authorized(policy_id, user, block_number)
            .await?;

        // Derive raw membership from the authorization result:
        // - Whitelist: authorized == in_set
        // - Blacklist: authorized == !in_set
        let in_set = match policy_type {
            PolicyType::WHITELIST => authorized,
            PolicyType::BLACKLIST => !authorized,
            _ => unreachable!(),
        };

        self.cache
            .write()
            .set_member(policy_id, user, block_number, in_set);

        info!(
            policy_id, %user, block_number, authorized, in_set,
            "Cached policy membership from L1 RPC"
        );

        Ok(authorized)
    }

    /// Fetch authorization for a compound policy.
    ///
    /// Fetches the compound sub-policy structure from L1, resolves the relevant sub-policy
    /// for the requested role, and recursively fetches/caches the sub-policy membership.
    async fn fetch_and_cache_compound(
        &self,
        policy_id: u64,
        user: Address,
        block_number: u64,
        role: AuthRole,
    ) -> Result<bool> {
        let compound = self.resolve_compound_data(policy_id, block_number).await?;

        match role {
            AuthRole::Sender => {
                self.resolve_simple_sub_policy(compound.sender_policy_id, user, block_number)
                    .await
            }
            AuthRole::Recipient => {
                self.resolve_simple_sub_policy(compound.recipient_policy_id, user, block_number)
                    .await
            }
            AuthRole::MintRecipient => {
                self.resolve_simple_sub_policy(
                    compound.mint_recipient_policy_id,
                    user,
                    block_number,
                )
                .await
            }
            AuthRole::Transfer => {
                // Check both sender AND recipient — short-circuit on sender failure.
                let sender_ok = self
                    .resolve_simple_sub_policy(compound.sender_policy_id, user, block_number)
                    .await?;
                if !sender_ok {
                    return Ok(false);
                }
                self.resolve_simple_sub_policy(compound.recipient_policy_id, user, block_number)
                    .await
            }
        }
    }

    /// Resolve authorization for a simple sub-policy (builtin or whitelist/blacklist).
    async fn resolve_simple_sub_policy(
        &self,
        policy_id: u64,
        user: Address,
        block_number: u64,
    ) -> Result<bool> {
        // Builtins
        if let Some(authorized) = builtin_authorization(policy_id) {
            return Ok(authorized);
        }

        // Check cache first for this sub-policy.
        // If the policy type is known but the user's membership was never observed by the
        // subscriber, `check_simple` returns `None` and we fall through to RPC.
        {
            let cache = self.cache.read();
            if let Some(result) = cache.check_simple(policy_id, user, block_number) {
                return Ok(result);
            }
        }

        // Cache miss — fetch from L1
        let policy_type = self.resolve_policy_type(policy_id, block_number).await?;
        self.fetch_and_cache_simple(policy_id, user, block_number, policy_type)
            .await
    }

    /// Resolve the `transferPolicyId` for a token — cache first, RPC fallback (sync).
    ///
    /// Intended for the zone TIP-20 precompile which runs on a blocking thread.
    ///
    /// # Panics
    ///
    /// Panics if called from within an async context on the same tokio runtime.
    pub fn resolve_transfer_policy_id(&self, token: Address) -> Result<u64> {
        let block_number = self.cache.read().last_l1_block();
        if let Some(id) = self.cache.read().get_token_policy(token, block_number) {
            return Ok(id);
        }
        tokio::task::block_in_place(|| {
            self.runtime_handle
                .block_on(self.resolve_policy_id(token, block_number))
        })
    }

    /// Resolve the `transferPolicyId` for a token — cache first, RPC fallback.
    async fn resolve_policy_id(&self, token: Address, block_number: u64) -> Result<u64> {
        if let Some(id) = self.cache.read().get_token_policy(token, block_number) {
            return Ok(id);
        }

        let tip20 = ITIP20::new(token, &self.provider);
        let policy_id = tip20
            .transferPolicyId()
            .block(BlockId::number(block_number))
            .call()
            .await
            .map_err(|e| eyre::eyre!("transferPolicyId RPC failed for token {token}: {e}"))?;

        self.cache
            .write()
            .set_token_policy(token, block_number, policy_id);
        info!(%token, policy_id, block_number, "Cached token policy ID from L1 RPC");

        Ok(policy_id)
    }

    /// Resolve the policy type for a policy ID — cache first, RPC fallback.
    async fn resolve_policy_type(&self, policy_id: u64, block_number: u64) -> Result<PolicyType> {
        // Check cache
        if let Some(policy) = self.cache.read().policies().get(&policy_id)
            && let Some(policy_type) = policy.policy_type
        {
            return Ok(policy_type);
        }

        // Fetch from L1
        let registry = ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, &self.provider);
        let result = registry
            .policyData(policy_id)
            .block(BlockId::number(block_number))
            .call()
            .await
            .map_err(|e| eyre::eyre!("policyData RPC failed for policy {policy_id}: {e}"))?;

        self.cache
            .write()
            .set_policy_type(policy_id, result.policyType);
        info!(policy_id, policy_type = ?result.policyType, "Cached policy type from L1 RPC");

        Ok(result.policyType)
    }

    /// Resolve compound policy data — cache first, RPC fallback.
    async fn resolve_compound_data(
        &self,
        policy_id: u64,
        block_number: u64,
    ) -> Result<CompoundData> {
        // Check cache
        if let Some(policy) = self.cache.read().policies().get(&policy_id)
            && let Some(ref compound) = policy.compound
        {
            return Ok(*compound);
        }

        // Fetch from L1
        let registry = ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, &self.provider);
        let result = registry
            .compoundPolicyData(policy_id)
            .block(BlockId::number(block_number))
            .call()
            .await
            .map_err(|e| {
                eyre::eyre!("compoundPolicyData RPC failed for policy {policy_id}: {e}")
            })?;

        let compound = CompoundData {
            sender_policy_id: result.senderPolicyId,
            recipient_policy_id: result.recipientPolicyId,
            mint_recipient_policy_id: result.mintRecipientPolicyId,
        };

        self.cache.write().set_compound(policy_id, compound);
        info!(
            policy_id,
            sender = compound.sender_policy_id,
            recipient = compound.recipient_policy_id,
            mint_recipient = compound.mint_recipient_policy_id,
            "Cached compound policy data from L1 RPC"
        );

        Ok(compound)
    }

    /// Cache-first, RPC-fallback authorization check by policy ID (no token resolution).
    ///
    /// Intended for the zone TIP-403 proxy precompile which receives `policyId`
    /// directly. Queries cache at `last_l1_block` to ensure deterministic results
    /// across nodes. On cache miss, falls back to L1 RPC (using `block_in_place`)
    /// and populates the cache so subsequent lookups are instant.
    ///
    /// # Panics
    ///
    /// Panics if called from within an async context on the same tokio runtime.
    pub fn is_authorized_by_policy(
        &self,
        policy_id: u64,
        user: Address,
        role: AuthRole,
    ) -> Result<bool> {
        self.metrics.authorization_checks_total.increment(1);

        // Builtins
        if let Some(authorized) = builtin_authorization(policy_id) {
            self.metrics.cache_hits.increment(1);
            return Ok(authorized);
        }

        // Try cache first — use last_l1_block to avoid leaking subscriber-ahead state
        let block_number = self.cache.read().last_l1_block();
        if let Some(result) = self
            .cache
            .read()
            .check_policy(policy_id, user, block_number, role)
        {
            self.metrics.cache_hits.increment(1);
            return Ok(result);
        }

        // Cache miss — fall back to L1 RPC
        self.metrics.cache_misses.increment(1);
        debug!(
            policy_id, %user, ?role, block_number,
            "Policy proxy cache miss, fetching from L1 RPC"
        );
        tokio::task::block_in_place(|| {
            self.runtime_handle.block_on(self.fetch_and_cache_by_policy(
                policy_id,
                user,
                block_number,
                role,
            ))
        })
    }

    /// Fetch and cache authorization data for a known policy ID (async).
    ///
    /// Like [`fetch_and_cache`](Self::fetch_and_cache) but skips the token →
    /// policy ID resolution step since the caller already has the policy ID.
    async fn fetch_and_cache_by_policy(
        &self,
        policy_id: u64,
        user: Address,
        block_number: u64,
        role: AuthRole,
    ) -> Result<bool> {
        let start = std::time::Instant::now();

        let result = self
            .resolve_policy_authorization(policy_id, user, block_number, role)
            .await;

        self.metrics
            .rpc_resolution_duration_seconds
            .record(start.elapsed().as_secs_f64());
        result
    }

    /// Resolve authorization for a policy ID by fetching policy type and membership
    /// from L1, caching the results.
    ///
    /// Shared implementation used by both [`fetch_and_cache`](Self::fetch_and_cache)
    /// and [`fetch_and_cache_by_policy`](Self::fetch_and_cache_by_policy).
    async fn resolve_policy_authorization(
        &self,
        policy_id: u64,
        user: Address,
        block_number: u64,
        role: AuthRole,
    ) -> Result<bool> {
        let policy_type = self.resolve_policy_type(policy_id, block_number).await?;

        match policy_type {
            PolicyType::WHITELIST | PolicyType::BLACKLIST => {
                self.fetch_and_cache_simple(policy_id, user, block_number, policy_type)
                    .await
            }
            PolicyType::COMPOUND => {
                self.fetch_and_cache_compound(policy_id, user, block_number, role)
                    .await
            }
            _ => eyre::bail!("unknown policy type for policy {policy_id}"),
        }
    }

    /// Cache-first, RPC-fallback policy type resolution (sync).
    ///
    /// Falls back to L1 RPC via `block_in_place` on cache miss.
    ///
    /// # Panics
    ///
    /// Panics if called from within an async context on the same tokio runtime.
    pub fn resolve_policy_type_sync(&self, policy_id: u64) -> Result<PolicyType> {
        if let Some(policy) = self.cache.read().policies().get(&policy_id)
            && let Some(policy_type) = policy.policy_type
        {
            return Ok(policy_type);
        }

        let block_number = self.cache.read().last_l1_block();
        debug!(
            policy_id,
            block_number, "Policy type cache miss, fetching from L1 RPC"
        );
        tokio::task::block_in_place(|| {
            self.runtime_handle
                .block_on(self.resolve_policy_type(policy_id, block_number))
        })
    }

    /// Cache-first, RPC-fallback compound data resolution (sync).
    ///
    /// Falls back to L1 RPC via `block_in_place` on cache miss.
    ///
    /// # Panics
    ///
    /// Panics if called from within an async context on the same tokio runtime.
    pub fn resolve_compound_data_sync(&self, policy_id: u64) -> Result<CompoundData> {
        if let Some(policy) = self.cache.read().policies().get(&policy_id)
            && let Some(ref compound) = policy.compound
        {
            return Ok(*compound);
        }

        let block_number = self.cache.read().last_l1_block();
        debug!(
            policy_id,
            block_number, "Compound data cache miss, fetching from L1 RPC"
        );
        tokio::task::block_in_place(|| {
            self.runtime_handle
                .block_on(self.resolve_compound_data(policy_id, block_number))
        })
    }

    /// Cache-first, RPC-fallback policy existence check (sync).
    ///
    /// Returns `Ok(true)` if the policy exists, `Ok(false)` if it doesn't,
    /// and propagates RPC errors instead of collapsing them into `false`.
    ///
    /// # Panics
    ///
    /// Panics if called from within an async context on the same tokio runtime.
    pub fn policy_exists_sync(&self, policy_id: u64) -> Result<bool> {
        if builtin_authorization(policy_id).is_some() {
            return Ok(true);
        }

        // Cache hit: if we have a policy record with a known type, it exists.
        if let Some(policy) = self.cache.read().policies().get(&policy_id)
            && policy.policy_type.is_some()
        {
            return Ok(true);
        }

        let block_number = self.cache.read().last_l1_block();
        debug!(
            policy_id,
            block_number, "Policy exists cache miss, fetching from L1 RPC"
        );
        let registry = ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, &self.provider);
        tokio::task::block_in_place(|| {
            self.runtime_handle.block_on(async {
                registry
                    .policyExists(policy_id)
                    .block(BlockId::number(block_number))
                    .call()
                    .await
                    .map_err(|e| {
                        self.metrics.rpc_errors.increment(1);
                        warn!(policy_id, block_number, %e, "policyExists RPC failed");
                        eyre::eyre!("policyExists RPC failed for policy {policy_id}: {e}")
                    })
            })
        })
    }

    /// Call `isAuthorized(policyId, user)` on the TIP403Registry via L1 RPC.
    async fn rpc_is_authorized(
        &self,
        policy_id: u64,
        user: Address,
        block_number: u64,
    ) -> Result<bool> {
        let registry = ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, &self.provider);
        let authorized = registry
            .isAuthorized(policy_id, user)
            .block(BlockId::number(block_number))
            .call()
            .await
            .map_err(|e| {
                self.metrics.rpc_errors.increment(1);
                warn!(policy_id, %user, block_number, %e, "isAuthorized RPC failed");
                eyre::eyre!("isAuthorized RPC failed for policy {policy_id} user {user}: {e}")
            })?;

        Ok(authorized)
    }
}

impl PolicyCheck for PolicyProvider {
    fn is_authorized(
        &self,
        policy_id: u64,
        user: Address,
        role: AuthRole,
    ) -> Result<bool, PrecompileError> {
        self.is_authorized_by_policy(policy_id, user, role)
            .map_err(|e| {
                zone_precompiles::zone_rpc_error(format!(
                    "auth check failed for policy {policy_id} user {user}: {e}"
                ))
            })
    }

    fn resolve_transfer_policy_id(&self, token: Address) -> Result<u64, PrecompileError> {
        Self::resolve_transfer_policy_id(self, token).map_err(|e| {
            zone_precompiles::zone_rpc_error(format!(
                "failed to resolve transfer_policy_id for {token}: {e}"
            ))
        })
    }

    fn policy_type_sync(&self, policy_id: u64) -> Result<PolicyType, PrecompileError> {
        self.resolve_policy_type_sync(policy_id).map_err(|e| {
            zone_precompiles::zone_rpc_error(format!(
                "policyData failed for policy {policy_id}: {e}"
            ))
        })
    }

    fn compound_policy_data(&self, policy_id: u64) -> Result<(u64, u64, u64), PrecompileError> {
        let compound = self.resolve_compound_data_sync(policy_id).map_err(|e| {
            zone_precompiles::zone_rpc_error(format!(
                "compoundPolicyData failed for policy {policy_id}: {e}"
            ))
        })?;
        Ok((
            compound.sender_policy_id,
            compound.recipient_policy_id,
            compound.mint_recipient_policy_id,
        ))
    }

    fn policy_exists(&self, policy_id: u64) -> Result<bool, PrecompileError> {
        self.policy_exists_sync(policy_id).map_err(|e| {
            PrecompileError::other(format!("policyExists failed for policy {policy_id}: {e}"))
        })
    }

    fn policy_id_counter(&self) -> u64 {
        let cache = self.cache.read();
        cache.policies().keys().max().map_or(2, |max| max + 1)
    }
}
