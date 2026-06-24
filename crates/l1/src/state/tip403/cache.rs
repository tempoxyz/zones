//! Block-versioned in-memory cache of TIP-403 transfer policy state from Tempo L1.
//!
//! The zone sequencer needs to know whether addresses are authorized under the TIP-403 policy
//! of each token enabled on the zone. This cache mirrors the L1 `TIP403Registry` storage
//! layout:
//!
//! - **Token → policy ID**: Each token address maps to a `transferPolicyId` via
//!   [`HeightVersioned`](crate::state::versioned::HeightVersioned), tracking the
//!   `TransferPolicyUpdate` event.
//!
//! - **Policy records**: Each policy ID maps to a [`CachedPolicy`] containing:
//!   - The policy type (whitelist, blacklist, or compound).
//!   - Policy set via [`PolicySet`] — a `HashSet` baseline plus per-block deltas
//!     mirroring `WhitelistUpdated` / `BlacklistUpdated` events.
//!   - Compound sub-policy IDs for sender, recipient, and mint recipient roles.
//!
//! ## Special policies
//!
//! Policy ID `0` always rejects, policy ID `1` always allows. These are handled inline by
//! [`PolicyCacheInner::is_authorized`] without any storage lookups.
//!
//! ## Unknown entries
//!
//! Users with no recorded set event are treated as "unknown" — cache lookups return
//! `None` so the caller falls back to RPC. This avoids silent false negatives when the
//! subscriber started after a user was added to a whitelist. Users who were explicitly added
//! or removed are tracked by [`PolicySet`].
//!
//! ## Compound policies (TIP-1015)
//!
//! A compound policy delegates authorization to sub-policies based on the user's role
//! (sender, recipient, or mint recipient). The [`is_authorized`](PolicyCacheInner::is_authorized)
//! method accepts an [`AuthRole`] to resolve the correct sub-policy.
//!
//! ## Resync handling
//!
//! The cache has no per-block rollback. Defensive resync paths should call
//! [`PolicyCacheInner::clear`] and let event replay plus RPC fallback repopulate entries.

use alloy_primitives::Address;
use alloy_provider::DynProvider;
use derive_more::Deref;
use parking_lot::RwLock;
use std::{collections::HashMap, sync::Arc};
use tempo_alloy::TempoNetwork;
use tempo_contracts::precompiles::ITIP403Registry::PolicyType;
use tracing::info;

use super::{builtin_authorization, events::PolicyEvent, policy_set::PolicySet};

use crate::state::versioned::HeightVersioned;

/// Thread-safe TIP-403 policy cache backed by an `Arc<RwLock<PolicyCacheInner>>`.
#[derive(Debug, Clone, Deref, Default)]
pub struct PolicyCache {
    #[deref]
    inner: Arc<RwLock<PolicyCacheInner>>,
}

impl PolicyCache {
    /// Returns the last L1 block number tracked by the cache.
    pub fn last_l1_block(&self) -> u64 {
        self.read().last_l1_block()
    }

    /// Seeds the cache with the initial L1 block height so RPC fallback queries
    /// target the correct block before the subscriber has processed any events.
    pub fn set_last_l1_block(&self, block_number: u64) {
        self.write().set_last_l1_block(block_number);
    }

    /// Collapse versioned entries up to `block_number`.
    ///
    /// Called by the engine after successfully processing an L1 block. Only the
    /// engine should drive this — the subscriber must not advance past blocks the
    /// engine hasn't consumed yet.
    pub fn advance(&self, block_number: u64) {
        self.write().advance(block_number);
    }

    /// Query the current `transferPolicyId` for each tracked token and seed it
    /// into the cache. This ensures the cache knows about tokens that have never
    /// had a `TransferPolicyUpdate` event (i.e. still using the default policy).
    ///
    /// Fails if any token's `transferPolicyId` cannot be resolved — all enabled
    /// tokens must be seeded for the zone to enforce policies correctly.
    pub async fn seed_token_policies(
        &self,
        portal_address: Address,
        tracked_tokens: &[Address],
        provider: &DynProvider<TempoNetwork>,
    ) -> eyre::Result<()> {
        use tempo_contracts::precompiles::ITIP20;

        let block_number = self.last_l1_block();

        let seeded = futures::future::join_all(tracked_tokens.iter().map(|token| {
            let tip20 = ITIP20::new(*token, provider);
            async move {
                let policy_id = tip20
                    .transferPolicyId()
                    .block(alloy_rpc_types_eth::BlockId::number(block_number))
                    .call()
                    .await
                    .map_err(|e| {
                        eyre::eyre!(
                            "failed to seed transferPolicyId for token {token} \
                             (portal {portal_address}): {e}"
                        )
                    })?;
                Ok::<_, eyre::Report>((*token, policy_id))
            }
        }))
        .await
        .into_iter()
        .collect::<eyre::Result<Vec<_>>>()?;

        let mut w = self.write();
        for (token, policy_id) in seeded {
            info!(%token, policy_id, block_number, "Seeded token policy from L1");
            w.set_token_policy(token, block_number, policy_id);
        }

        Ok(())
    }
}

/// Block-versioned cache of TIP-403 policy state from Tempo L1.
///
/// Mirrors the on-chain `TIP403Registry` storage layout with:
/// - Token → `transferPolicyId` mapping (from TIP-20 `TransferPolicyUpdate` events).
/// - Policy ID → policy record (type, policy set, compound data).
///
/// This allows the zone sequencer to evaluate transfer authorization without RPC round-trips.
#[derive(Debug, Default)]
pub struct PolicyCacheInner {
    /// Per-token transfer policy ID.
    tokens: HashMap<Address, HeightVersioned<u64>>,
    /// Per-policy-ID records (type, policy set, compound data).
    ///
    /// Populated from **all** `PolicyCreated`, `CompoundPolicyCreated`,
    /// `WhitelistUpdated`, and `BlacklistUpdated` events on the global
    /// `TIP403Registry` — not filtered to this zone's tokens. This is
    /// intentional: a token can switch to any policy via
    /// `TransferPolicyUpdate`, so pre-caching all policies avoids RPC
    /// round-trips on policy switch. The memory overhead is negligible.
    policies: HashMap<u64, CachedPolicy>,
    /// Highest L1 block number processed by the engine.
    ///
    /// This equals the last block height the engine has processed and
    /// should advance in lockstep with the L1 head tracked by the
    /// `TempoStateReader` contract. The
    /// [`PolicyResolutionTask`](super::PolicyResolutionTask) reads this to
    /// query L1 at the correct block height for cache-miss RPC fallback.
    last_l1_block: u64,
}

impl PolicyCacheInner {
    /// Returns the `transferPolicyId` for a token at the given block, or `None` if not cached.
    pub fn get_token_policy(&self, token: Address, block_number: u64) -> Option<u64> {
        self.tokens.get(&token)?.get(block_number)
    }

    /// Sets the `transferPolicyId` for a token at the given block.
    pub fn set_token_policy(&mut self, token: Address, block_number: u64, policy_id: u64) {
        self.tokens
            .entry(token)
            .or_default()
            .set(block_number, policy_id);
    }

    /// Sets the policy type for a policy ID.
    pub fn set_policy_type(&mut self, policy_id: u64, policy_type: PolicyType) {
        self.get_policy_entry(policy_id).policy_type = Some(policy_type);
    }

    /// Sets whether `user` is in a policy set at the given block.
    pub fn set_policy_status(
        &mut self,
        policy_id: u64,
        user: Address,
        block_number: u64,
        in_set: bool,
    ) {
        self.get_policy_entry(policy_id)
            .policy_set
            .record_status(user, block_number, in_set);
    }

    /// Sets compound policy sub-policy IDs and marks the policy as compound.
    pub fn set_compound(&mut self, policy_id: u64, compound: CompoundData) {
        let entry = self.get_policy_entry(policy_id);
        entry.policy_type = Some(PolicyType::COMPOUND);
        entry.compound = Some(compound);
    }

    /// Returns a reference to the per-policy-ID records for direct inspection.
    pub fn policies(&self) -> &HashMap<u64, CachedPolicy> {
        &self.policies
    }

    /// Returns all token addresses currently tracked by the cache.
    pub fn tracked_tokens(&self) -> Vec<Address> {
        self.tokens.keys().copied().collect()
    }

    /// Returns the number of token-to-policy mappings in the cache.
    pub fn num_token_policies(&self) -> usize {
        self.tokens.len()
    }

    /// Returns the highest L1 block number processed by the cache.
    ///
    /// Returns `0` if no events have been applied yet.
    pub fn last_l1_block(&self) -> u64 {
        self.last_l1_block
    }

    /// Sets the highest L1 block number, unless the cache already tracks a higher block.
    pub fn set_last_l1_block(&mut self, block_number: u64) {
        self.last_l1_block = self.last_l1_block.max(block_number);
    }

    /// Returns a mutable reference to the [`CachedPolicy`] for the given policy ID,
    /// inserting a default entry if absent.
    fn get_policy_entry(&mut self, policy_id: u64) -> &mut CachedPolicy {
        self.policies.entry(policy_id).or_default()
    }

    /// Check if an address is authorized under a token's transfer policy at the given block.
    ///
    /// This mirrors the L1 `TIP403Registry.isAuthorized` / `isAuthorizedSender` /
    /// `isAuthorizedRecipient` / `isAuthorizedMintRecipient` functions. The `role` parameter
    /// selects which sub-policy to check for compound policies; for simple policies it is
    /// ignored.
    ///
    /// Returns `Some(true/false)` if policy data is cached, or `None` when the policy ID,
    /// type, or compound data is unknown (caller should fall back to RPC or fail-open).
    pub fn is_authorized(
        &self,
        token: Address,
        user: Address,
        block_number: u64,
        role: AuthRole,
    ) -> Option<bool> {
        let policy_id = self.tokens.get(&token)?.get(block_number)?;
        self.check_policy(policy_id, user, block_number, role)
    }

    /// Resolve authorization for a policy ID, handling builtins, simple, and compound policies.
    ///
    /// - **Builtins** (0 = reject all, 1 = allow all): resolved inline.
    /// - **Simple** (whitelist/blacklist): checks policy set.
    /// - **Compound** (TIP-1015): delegates to the sub-policy selected by `role`.
    ///
    /// Returns `None` when the policy data is not cached (caller should fail-closed or
    /// fall back to RPC depending on context).
    pub fn check_policy(
        &self,
        policy_id: u64,
        user: Address,
        block_number: u64,
        role: AuthRole,
    ) -> Option<bool> {
        if let Some(authorized) = builtin_authorization(policy_id) {
            return Some(authorized);
        }

        let policy = self.policies.get(&policy_id)?;
        let policy_type = policy.policy_type?;

        match policy_type {
            PolicyType::WHITELIST => {
                if !policy.policy_set.is_known(&user) {
                    return None;
                }
                Some(policy.policy_set.contains(user, block_number))
            }
            PolicyType::BLACKLIST => {
                if !policy.policy_set.is_known(&user) {
                    return None;
                }
                Some(!policy.policy_set.contains(user, block_number))
            }
            PolicyType::COMPOUND => {
                let compound = policy.compound.as_ref()?;
                match role {
                    AuthRole::Sender => {
                        self.check_simple(compound.sender_policy_id, user, block_number)
                    }
                    AuthRole::Recipient => {
                        self.check_simple(compound.recipient_policy_id, user, block_number)
                    }
                    AuthRole::MintRecipient => {
                        self.check_simple(compound.mint_recipient_policy_id, user, block_number)
                    }
                    AuthRole::Transfer => {
                        // Check both sender AND recipient — short-circuit on sender failure.
                        let sender_ok =
                            self.check_simple(compound.sender_policy_id, user, block_number)?;
                        if !sender_ok {
                            return Some(false);
                        }
                        self.check_simple(compound.recipient_policy_id, user, block_number)
                    }
                }
            }
            _ => None,
        }
    }

    /// Check authorization against a simple (non-compound) policy.
    ///
    /// Handles builtins and whitelist/blacklist. Returns `None` for compound sub-policies
    /// (compound-of-compound is invalid on L1).
    pub fn check_simple(&self, policy_id: u64, user: Address, block_number: u64) -> Option<bool> {
        if let Some(authorized) = builtin_authorization(policy_id) {
            return Some(authorized);
        }

        let policy = self.policies.get(&policy_id)?;
        let policy_type = policy.policy_type?;

        match policy_type {
            PolicyType::WHITELIST => {
                if !policy.policy_set.is_known(&user) {
                    return None;
                }
                Some(policy.policy_set.contains(user, block_number))
            }
            PolicyType::BLACKLIST => {
                if !policy.policy_set.is_known(&user) {
                    return None;
                }
                Some(!policy.policy_set.contains(user, block_number))
            }
            _ => None,
        }
    }

    /// Apply a batch of decoded policy events for a single block.
    ///
    /// This is the primary ingestion path used by [`L1Subscriber`](crate::l1::L1Subscriber).
    /// Events are decoded outside the write lock, then applied here in one batch.
    ///
    /// **NOTE:** When a `TokenPolicyChanged` event points to a policy ID that was created
    /// before the subscriber started, the cache will have no set data for that policy.
    /// Authorization queries will return `None` (cache miss), causing the
    /// [`PolicyProvider`](super::PolicyProvider) to fall back to per-user L1 RPC. Ideally,
    /// the subscriber should kick off background pre-fetching of the new policy's type and
    /// policy set on `TokenPolicyChanged` to avoid cold-start RPC latency.
    pub fn apply_events(&mut self, block_number: u64, events: &[PolicyEvent]) {
        for event in events {
            match event {
                PolicyEvent::MembershipChanged {
                    policy_id,
                    account,
                    in_set,
                } => {
                    self.set_policy_status(*policy_id, *account, block_number, *in_set);
                }
                PolicyEvent::TokenPolicyChanged { token, policy_id } => {
                    self.set_token_policy(*token, block_number, *policy_id);
                }
                PolicyEvent::PolicyCreated {
                    policy_id,
                    policy_type,
                } => {
                    self.set_policy_type(*policy_id, *policy_type);
                }
                PolicyEvent::CompoundPolicyCreated {
                    policy_id,
                    sender_policy_id,
                    recipient_policy_id,
                    mint_recipient_policy_id,
                } => {
                    self.set_compound(
                        *policy_id,
                        CompoundData {
                            sender_policy_id: *sender_policy_id,
                            recipient_policy_id: *recipient_policy_id,
                            mint_recipient_policy_id: *mint_recipient_policy_id,
                        },
                    );
                }
            }
        }
    }

    /// Clears all cached policy data. `last_l1_block` is preserved — the engine
    /// will advance it when it reprocesses blocks after a reorg.
    pub fn clear(&mut self) {
        self.tokens.clear();
        self.policies.clear();
    }

    /// Collapse all history before `min_block` into single baseline entries.
    pub fn flatten(&mut self, min_block: u64) {
        for v in self.tokens.values_mut() {
            v.flatten(min_block);
        }
        for policy in self.policies.values_mut() {
            policy.policy_set.flatten(min_block);
        }
    }

    /// Advance the baseline to `new_height` for all tracked entries.
    ///
    /// Only the engine should call this after successfully processing a block.
    /// Advancing past unprocessed blocks would fold pending deltas prematurely,
    /// causing incorrect authorization decisions for in-flight blocks. The
    /// subscriber writes events via [`apply_events`](Self::apply_events) but never
    /// advances the cache.
    pub fn advance(&mut self, new_height: u64) {
        self.last_l1_block = self.last_l1_block.max(new_height);
        info!(
            target: "zone::policy",
            new_height,
            tokens = self.tokens.len(),
            policies = self.policies.len(),
            "Advancing policy cache baseline"
        );
        for v in self.tokens.values_mut() {
            v.advance(new_height);
        }
        for policy in self.policies.values_mut() {
            policy.policy_set.advance(new_height);
        }
    }
}

pub(super) use zone_primitives::policy::AuthRole;

/// Per-policy-ID cached record, mirroring `TIP403Registry.policy_records[id]`.
#[derive(Debug, Default)]
pub struct CachedPolicy {
    /// Policy type. `None` if the `PolicyCreated` event hasn't been observed yet.
    pub policy_type: Option<PolicyType>,
    /// Policy set for simple (non-compound) policies.
    pub policy_set: PolicySet,
    /// Compound sub-policy IDs. `None` for simple policies.
    pub compound: Option<CompoundData>,
}

/// Sub-policy IDs for a compound policy (TIP-1015).
///
/// Created once via `createCompoundPolicy` on L1 and never modified.
#[derive(Debug, Clone, Copy)]
pub struct CompoundData {
    pub sender_policy_id: u64,
    pub recipient_policy_id: u64,
    pub mint_recipient_policy_id: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const TOKEN: Address = address!("0x20C0000000000000000000000000000000000000");
    const USER_A: Address = address!("0x0000000000000000000000000000000000000001");
    const USER_B: Address = address!("0x0000000000000000000000000000000000000002");

    // --- PolicySet tests ---

    #[test]
    fn policy_set_default_is_not_in_set() {
        let set = PolicySet::default();
        assert!(!set.contains(USER_A, 100));
    }

    #[test]
    fn policy_set_add_and_remove() {
        let mut set = PolicySet::default();
        set.record_status(USER_A, 10, true);
        assert!(set.contains(USER_A, 10));
        assert!(set.contains(USER_A, 15));
        assert!(!set.contains(USER_A, 5));

        set.record_status(USER_A, 20, false);
        assert!(set.contains(USER_A, 15));
        assert!(!set.contains(USER_A, 25));
    }

    #[test]
    fn policy_set_multiple_users_same_block() {
        let mut set = PolicySet::default();
        set.record_status(USER_A, 10, true);
        set.record_status(USER_B, 10, true);

        assert!(set.contains(USER_A, 10));
        assert!(set.contains(USER_B, 10));
    }

    #[test]
    fn policy_set_advance_folds_deltas() {
        let mut set = PolicySet::default();
        set.record_status(USER_A, 10, true);
        set.record_status(USER_B, 15, true);
        set.record_status(USER_A, 20, false);

        set.advance(15);

        // USER_A added at 10 (folded into baseline), USER_B added at 15 (folded)
        assert!(set.contains(USER_A, 15));
        assert!(set.contains(USER_B, 15));

        // USER_A removed at 20 (still pending)
        assert!(!set.contains(USER_A, 25));
    }

    #[test]
    fn policy_set_at_or_below_baseline_is_ignored() {
        let mut set = PolicySet::default();
        set.record_status(USER_A, 10, true);
        set.advance(20);

        // Delayed writes from finalized heights must not rewrite baseline membership.
        set.record_status(USER_A, 15, false);
        set.record_status(USER_A, 20, false);
        assert!(set.contains(USER_A, 20));

        // Stale writes must not mark unknown users as observed either.
        set.record_status(USER_B, 15, false);
        assert!(!set.is_known(&USER_B));

        set.record_status(USER_A, 21, false);
        assert!(!set.contains(USER_A, 21));
    }

    #[test]
    fn policy_set_initial_baseline_write_is_ignored() {
        let mut set = PolicySet::default();

        set.record_status(USER_A, 0, false);
        assert!(!set.is_known(&USER_A));
        assert!(!set.contains(USER_A, 0));

        set.record_status(USER_B, 0, true);
        assert!(!set.is_known(&USER_B));
        assert!(!set.contains(USER_B, 0));

        set.record_status(USER_B, 1, true);
        assert!(set.is_known(&USER_B));
        assert!(set.contains(USER_B, 1));
    }

    // --- PolicyCacheInner tests: simple policies ---

    #[test]
    fn special_policy_always_reject() {
        let mut cache = PolicyCacheInner::default();
        cache.set_token_policy(TOKEN, 10, 0);
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(false)
        );
    }

    #[test]
    fn token_policy_seed_after_genesis_is_cached() {
        let mut cache = PolicyCacheInner::default();

        cache.set_token_policy(TOKEN, 1, 1);

        assert_eq!(cache.get_token_policy(TOKEN, 1), Some(1));
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 1, AuthRole::Transfer),
            Some(true)
        );
    }

    #[test]
    fn special_policy_always_allow() {
        let mut cache = PolicyCacheInner::default();
        cache.set_token_policy(TOKEN, 10, 1);
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(true)
        );
    }

    #[test]
    fn whitelist_authorized_when_in_set() {
        let mut cache = PolicyCacheInner::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_policy_type(2, PolicyType::WHITELIST);
        cache.set_policy_status(2, USER_A, 10, true);
        cache.set_policy_status(2, USER_B, 10, false);

        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(true)
        );
        assert_eq!(
            cache.is_authorized(TOKEN, USER_B, 10, AuthRole::Transfer),
            Some(false)
        );
    }

    #[test]
    fn blacklist_authorized_when_not_in_set() {
        let mut cache = PolicyCacheInner::default();
        cache.set_token_policy(TOKEN, 10, 3);
        cache.set_policy_type(3, PolicyType::BLACKLIST);
        cache.set_policy_status(3, USER_A, 10, true);
        cache.set_policy_status(3, USER_B, 10, false);

        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(false)
        );
        assert_eq!(
            cache.is_authorized(TOKEN, USER_B, 10, AuthRole::Transfer),
            Some(true)
        );
    }

    #[test]
    fn blacklist_unknown_user_returns_none() {
        let mut cache = PolicyCacheInner::default();
        cache.set_token_policy(TOKEN, 10, 3);
        cache.set_policy_type(3, PolicyType::BLACKLIST);

        // USER_A has no set data — unknown, caller should fall back to RPC
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            None
        );
    }

    #[test]
    fn whitelist_unknown_user_returns_none() {
        let mut cache = PolicyCacheInner::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_policy_type(2, PolicyType::WHITELIST);

        // USER_A has no set data — unknown, caller should fall back to RPC
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            None
        );
    }

    #[test]
    fn returns_none_on_missing_token_policy() {
        let cache = PolicyCacheInner::default();
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            None
        );
    }

    #[test]
    fn returns_none_on_missing_policy_type() {
        let mut cache = PolicyCacheInner::default();
        cache.set_token_policy(TOKEN, 10, 5);
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            None
        );
    }

    #[test]
    fn block_versioned_policy_change() {
        let mut cache = PolicyCacheInner::default();
        cache.set_token_policy(TOKEN, 10, 1);
        cache.set_token_policy(TOKEN, 20, 2);
        cache.set_policy_type(2, PolicyType::WHITELIST);
        cache.set_policy_status(2, USER_A, 20, true);

        // At block 15: policy_id=1 (always allow)
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 15, AuthRole::Transfer),
            Some(true)
        );
        assert_eq!(
            cache.is_authorized(TOKEN, USER_B, 15, AuthRole::Transfer),
            Some(true)
        );

        // At block 25: policy_id=2 (whitelist), USER_A in set
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 25, AuthRole::Transfer),
            Some(true)
        );
        // USER_B never observed → None (fall back to RPC)
        assert_eq!(
            cache.is_authorized(TOKEN, USER_B, 25, AuthRole::Transfer),
            None
        );
    }

    #[test]
    fn block_versioned_set_change() {
        let mut cache = PolicyCacheInner::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_policy_type(2, PolicyType::WHITELIST);
        cache.set_policy_status(2, USER_A, 10, false);
        cache.set_policy_status(2, USER_A, 20, true);

        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 15, AuthRole::Transfer),
            Some(false)
        );
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 25, AuthRole::Transfer),
            Some(true)
        );
    }

    #[test]
    fn clear_removes_all_data() {
        let mut cache = PolicyCacheInner::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_policy_type(2, PolicyType::WHITELIST);
        cache.set_policy_status(2, USER_A, 10, true);

        cache.clear();

        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            None
        );
    }

    #[test]
    fn flatten_keeps_baseline() {
        let mut cache = PolicyCacheInner::default();
        cache.set_token_policy(TOKEN, 5, 1);
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_token_policy(TOKEN, 20, 3);

        cache.flatten(15);

        // Below baseline: returns the baseline value (2, set at block 10)
        assert_eq!(cache.get_token_policy(TOKEN, 5), Some(2));
        assert_eq!(cache.get_token_policy(TOKEN, 10), Some(2));
        assert_eq!(cache.get_token_policy(TOKEN, 15), Some(2));
        assert_eq!(cache.get_token_policy(TOKEN, 20), Some(3));
    }

    #[test]
    fn shared_policy_across_tokens() {
        let mut cache = PolicyCacheInner::default();
        let token2: Address = address!("0x20C0000000000000000000000000000000000001");

        // Two tokens share policy 2
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_token_policy(token2, 10, 2);
        cache.set_policy_type(2, PolicyType::WHITELIST);
        cache.set_policy_status(2, USER_A, 10, true);

        // Both tokens see the same policy set (per-policy, no fan-out needed)
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(true)
        );
        assert_eq!(
            cache.is_authorized(token2, USER_A, 10, AuthRole::Transfer),
            Some(true)
        );
    }

    #[test]
    fn shared_blacklist_across_tokens() {
        let mut cache = PolicyCacheInner::default();
        let token2: Address = address!("0x20C0000000000000000000000000000000000001");

        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_token_policy(token2, 10, 2);
        cache.set_policy_type(2, PolicyType::BLACKLIST);
        cache.set_policy_status(2, USER_A, 10, true);

        // BLACKLIST: authorized when NOT in set
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(false)
        );
        assert_eq!(
            cache.is_authorized(token2, USER_A, 10, AuthRole::Transfer),
            Some(false)
        );
        // USER_B never observed → None (fall back to RPC)
        assert_eq!(
            cache.is_authorized(TOKEN, USER_B, 10, AuthRole::Transfer),
            None
        );
        assert_eq!(
            cache.is_authorized(token2, USER_B, 10, AuthRole::Transfer),
            None
        );
    }

    #[test]
    fn tokens_with_different_policies() {
        let mut cache = PolicyCacheInner::default();
        let token2: Address = address!("0x20C0000000000000000000000000000000000001");

        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_token_policy(token2, 10, 3);
        cache.set_policy_type(2, PolicyType::WHITELIST);
        cache.set_policy_type(3, PolicyType::BLACKLIST);
        cache.set_policy_status(2, USER_A, 10, true);
        cache.set_policy_status(3, USER_A, 10, true);

        // TOKEN uses whitelist policy 2: USER_A whitelisted → authorized
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(true)
        );
        // token2 uses blacklist policy 3: USER_A blacklisted → NOT authorized
        assert_eq!(
            cache.is_authorized(token2, USER_A, 10, AuthRole::Transfer),
            Some(false)
        );
    }

    #[test]
    fn advance_then_lookup() {
        let mut cache = PolicyCacheInner::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_policy_type(2, PolicyType::BLACKLIST);
        cache.set_policy_status(2, USER_A, 10, true);
        cache.set_policy_status(2, USER_A, 20, false);

        cache.advance(15);

        // After advancing to 15, baseline includes block-10 state.
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 12, AuthRole::Transfer),
            Some(false)
        ); // blacklisted
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 25, AuthRole::Transfer),
            Some(true)
        ); // unblacklisted at 20
    }

    #[test]
    fn stale_membership_write_after_advance_cannot_poison_blacklist() {
        let mut cache = PolicyCacheInner::default();
        cache.set_token_policy(TOKEN, 10, 3);
        cache.set_policy_type(3, PolicyType::BLACKLIST);
        cache.set_policy_status(3, USER_A, 10, false);
        cache.advance(10);

        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(true)
        );

        cache.set_policy_status(3, USER_A, 12, true);
        cache.advance(12);

        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 12, AuthRole::Transfer),
            Some(false)
        );

        // Simulates an RPC fallback result captured before the block-12 blacklist event
        // and returning after the engine advanced the cache baseline.
        cache.set_policy_status(3, USER_A, 10, false);
        cache.set_policy_status(3, USER_A, 12, false);

        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 13, AuthRole::Transfer),
            Some(false)
        );
    }

    #[test]
    fn stale_membership_write_after_advance_does_not_mark_unknown_observed() {
        let mut cache = PolicyCacheInner::default();
        cache.set_token_policy(TOKEN, 10, 3);
        cache.set_policy_type(3, PolicyType::BLACKLIST);
        cache.advance(10);

        cache.set_policy_status(3, USER_B, 10, false);

        assert_eq!(
            cache.is_authorized(TOKEN, USER_B, 10, AuthRole::Transfer),
            None
        );
    }

    #[test]
    fn stale_token_policy_write_after_advance_cannot_revert_policy() {
        let mut cache = PolicyCacheInner::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.advance(10);

        cache.set_token_policy(TOKEN, 12, 3);
        cache.advance(12);

        cache.set_token_policy(TOKEN, 10, 1);
        cache.set_token_policy(TOKEN, 12, 1);

        assert_eq!(cache.get_token_policy(TOKEN, 12), Some(3));
        assert_eq!(cache.get_token_policy(TOKEN, 13), Some(3));

        cache.set_token_policy(TOKEN, 13, 1);
        assert_eq!(cache.get_token_policy(TOKEN, 13), Some(1));
    }

    #[test]
    fn flatten_preserves_token_entries() {
        let mut cache = PolicyCacheInner::default();
        let token2: Address = address!("0x20C0000000000000000000000000000000000001");

        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_token_policy(token2, 10, 3);

        cache.flatten(15);

        // Both should survive because their token entries have values
        assert_eq!(cache.get_token_policy(TOKEN, 15), Some(2));
        assert_eq!(cache.get_token_policy(token2, 15), Some(3));
    }

    #[test]
    fn policy_change_mid_block_range() {
        let mut cache = PolicyCacheInner::default();

        // Start with whitelist at block 10, switch to blacklist policy at block 20
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_token_policy(TOKEN, 20, 3);
        cache.set_policy_type(2, PolicyType::WHITELIST);
        cache.set_policy_type(3, PolicyType::BLACKLIST);
        cache.set_policy_status(2, USER_A, 10, true);
        cache.set_policy_status(3, USER_A, 10, true);

        // At block 15 (whitelist policy 2), USER_A is in set → authorized
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 15, AuthRole::Transfer),
            Some(true)
        );
        // At block 25 (blacklist policy 3), USER_A is in set → NOT authorized
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 25, AuthRole::Transfer),
            Some(false)
        );
    }

    // --- Compound policy tests (TIP-1015) ---

    #[test]
    fn compound_policy_sender_check() {
        let mut cache = PolicyCacheInner::default();
        // Simple sub-policies
        cache.set_policy_type(2, PolicyType::BLACKLIST); // sender policy
        cache.set_policy_type(3, PolicyType::WHITELIST); // recipient policy
        cache.set_policy_status(2, USER_A, 10, true); // USER_A blacklisted as sender
        cache.set_policy_status(3, USER_A, 10, true); // USER_A whitelisted as recipient

        // Compound policy referencing sub-policies
        cache.set_compound(
            5,
            CompoundData {
                sender_policy_id: 2,
                recipient_policy_id: 3,
                mint_recipient_policy_id: 1, // builtin allow
            },
        );
        cache.set_token_policy(TOKEN, 10, 5);

        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Sender),
            Some(false)
        );
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Recipient),
            Some(true)
        );
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::MintRecipient),
            Some(true)
        );
    }

    #[test]
    fn compound_policy_transfer_checks_both() {
        let mut cache = PolicyCacheInner::default();
        cache.set_policy_type(2, PolicyType::WHITELIST); // sender
        cache.set_policy_type(3, PolicyType::WHITELIST); // recipient

        cache.set_compound(
            5,
            CompoundData {
                sender_policy_id: 2,
                recipient_policy_id: 3,
                mint_recipient_policy_id: 1,
            },
        );
        cache.set_token_policy(TOKEN, 10, 5);

        // Neither whitelisted → unknown (USER_A never observed)
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            None
        );

        // Only sender whitelisted → fails on recipient (still unknown for recipient sub-policy)
        cache.set_policy_status(2, USER_A, 10, true);
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            None
        );

        // Both whitelisted → authorized
        cache.set_policy_status(3, USER_A, 10, true);
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(true)
        );
    }

    #[test]
    fn compound_policy_with_builtin_sub_policies() {
        let mut cache = PolicyCacheInner::default();
        // Compound: sender=allow(1), recipient=reject(0), mint=allow(1)
        cache.set_compound(
            5,
            CompoundData {
                sender_policy_id: 1,
                recipient_policy_id: 0,
                mint_recipient_policy_id: 1,
            },
        );
        cache.set_token_policy(TOKEN, 10, 5);

        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Sender),
            Some(true)
        );
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Recipient),
            Some(false)
        );
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::MintRecipient),
            Some(true)
        );
        // Transfer: sender=true, recipient=false → false
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(false)
        );
    }

    #[test]
    fn compound_returns_none_when_sub_policy_missing() {
        let mut cache = PolicyCacheInner::default();
        // Compound references sub-policy 99 which doesn't exist
        cache.set_compound(
            5,
            CompoundData {
                sender_policy_id: 99,
                recipient_policy_id: 3,
                mint_recipient_policy_id: 1,
            },
        );
        cache.set_token_policy(TOKEN, 10, 5);

        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Sender),
            None
        );
    }

    #[test]
    fn compound_returns_none_when_compound_data_missing() {
        let mut cache = PolicyCacheInner::default();
        // Policy 5 has COMPOUND type but no compound data set
        cache.set_policy_type(5, PolicyType::COMPOUND);
        cache.set_token_policy(TOKEN, 10, 5);

        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Sender),
            None
        );
    }

    #[test]
    fn apply_events_with_policy_created() {
        let mut cache = PolicyCacheInner::default();
        let events = vec![
            PolicyEvent::PolicyCreated {
                policy_id: 2,
                policy_type: PolicyType::WHITELIST,
            },
            PolicyEvent::MembershipChanged {
                policy_id: 2,
                account: USER_A,
                in_set: true,
            },
            PolicyEvent::TokenPolicyChanged {
                token: TOKEN,
                policy_id: 2,
            },
        ];

        cache.apply_events(10, &events);

        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(true)
        );
        // USER_B never observed → None (fall back to RPC)
        assert_eq!(
            cache.is_authorized(TOKEN, USER_B, 10, AuthRole::Transfer),
            None
        );
    }

    // --- `observed` tracking and `advance` interaction tests ---

    #[test]
    fn observed_survives_advance() {
        let mut cache = PolicyCacheInner::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_policy_type(2, PolicyType::WHITELIST);
        cache.set_policy_status(2, USER_A, 10, true);

        // Before advance: known and authorized
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(true)
        );

        // Advance past the set block — folds pending into baseline
        cache.advance(20);

        // After advance: still known and still authorized (observed persists)
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 25, AuthRole::Transfer),
            Some(true)
        );

        // Unobserved user still returns None after advance
        assert_eq!(
            cache.is_authorized(TOKEN, USER_B, 25, AuthRole::Transfer),
            None
        );
    }

    #[test]
    fn observed_survives_advance_for_removed_set_entry() {
        let mut cache = PolicyCacheInner::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_policy_type(2, PolicyType::WHITELIST);

        // Add then remove USER_A
        cache.set_policy_status(2, USER_A, 10, true);
        cache.set_policy_status(2, USER_A, 20, false);

        // Advance past both events
        cache.advance(25);

        // USER_A was removed but is still "observed" — returns Some(false), not None
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 30, AuthRole::Transfer),
            Some(false)
        );
    }

    #[test]
    fn observed_survives_advance_blacklist() {
        let mut cache = PolicyCacheInner::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_policy_type(2, PolicyType::BLACKLIST);
        cache.set_policy_status(2, USER_A, 10, true); // blacklisted

        cache.advance(20);

        // After advance: observed, blacklisted → not authorized
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 25, AuthRole::Transfer),
            Some(false)
        );

        // Unobserved user → None (not Some(true) which would be a false positive)
        assert_eq!(
            cache.is_authorized(TOKEN, USER_B, 25, AuthRole::Transfer),
            None
        );
    }

    #[test]
    fn clear_resets_observed() {
        let mut cache = PolicyCacheInner::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_policy_type(2, PolicyType::WHITELIST);
        cache.set_policy_status(2, USER_A, 10, true);

        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(true)
        );

        cache.clear();

        // After clear, observed is gone — returns None, not Some(false)
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            None
        );
    }

    #[test]
    fn policy_set_known_direct() {
        let mut set = PolicySet::default();

        // Fresh set: nobody known
        assert!(!set.is_known(&USER_A));
        assert!(!set.is_known(&USER_B));

        // Add USER_A
        set.record_status(USER_A, 10, true);
        assert!(set.is_known(&USER_A));
        assert!(!set.is_known(&USER_B));

        // Advance past the event
        set.advance(20);
        assert!(set.is_known(&USER_A), "observed must survive advance");
        assert!(!set.is_known(&USER_B));

        // Clear resets everything
        set.clear();
        assert!(!set.is_known(&USER_A));
    }

    #[test]
    fn multiple_advances_preserve_observed() {
        let mut cache = PolicyCacheInner::default();
        cache.set_token_policy(TOKEN, 5, 2);
        cache.set_policy_type(2, PolicyType::WHITELIST);

        // Events at different blocks
        cache.set_policy_status(2, USER_A, 10, true);
        cache.set_policy_status(2, USER_B, 20, true);

        // Advance past first event only
        cache.advance(15);
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 15, AuthRole::Transfer),
            Some(true)
        );
        assert_eq!(
            cache.is_authorized(TOKEN, USER_B, 25, AuthRole::Transfer),
            Some(true) // still in pending, but observed
        );

        // Advance past second event
        cache.advance(25);
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 30, AuthRole::Transfer),
            Some(true)
        );
        assert_eq!(
            cache.is_authorized(TOKEN, USER_B, 30, AuthRole::Transfer),
            Some(true)
        );
    }

    #[test]
    fn apply_events_with_compound_policy() {
        let mut cache = PolicyCacheInner::default();
        // Pre-populate simple sub-policies
        cache.set_policy_type(2, PolicyType::BLACKLIST);
        cache.set_policy_type(3, PolicyType::WHITELIST);
        cache.set_policy_status(2, USER_A, 10, false); // explicitly not blacklisted
        cache.set_policy_status(3, USER_A, 10, true);

        let events = vec![
            PolicyEvent::CompoundPolicyCreated {
                policy_id: 5,
                sender_policy_id: 2,
                recipient_policy_id: 3,
                mint_recipient_policy_id: 1,
            },
            PolicyEvent::TokenPolicyChanged {
                token: TOKEN,
                policy_id: 5,
            },
        ];

        cache.apply_events(10, &events);

        // Sender (blacklist, explicitly not in set → authorized), Recipient (whitelist, in set → authorized)
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(true)
        );
        // MintRecipient (builtin allow)
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::MintRecipient),
            Some(true)
        );
    }
}
