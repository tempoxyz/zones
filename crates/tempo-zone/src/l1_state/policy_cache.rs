//! Block-versioned in-memory cache of TIP-403 transfer policy state from Tempo L1.
//!
//! The zone sequencer needs to know whether addresses are authorized under the TIP-403 policy
//! of each token enabled on the zone. This cache tracks per-token policy state in a single
//! [`TokenPolicy`] struct:
//!
//! - The `transferPolicyId` — which policy governs a token's transfers.
//! - The policy type — whether the policy is a whitelist or blacklist.
//! - Per-user membership — whether each address is in the policy's set.
//!
//! All entries are block-versioned via [`HeightVersioned`](super::versioned::HeightVersioned) so
//! the zone can query policy state at the `tempoBlockNumber` it committed to.
//!
//! ## Special policies
//!
//! Policy ID `0` always rejects, policy ID `1` always allows. These are handled inline by
//! [`PolicyCache::is_authorized`] without any storage lookups.
//!
//! ## Reorg handling
//!
//! On reorgs the caller is expected to [`PolicyCache::clear`] the entire cache. There is no
//! per-block rollback.

use alloy_primitives::Address;
use derive_more::Deref;
use parking_lot::RwLock;
use std::{collections::HashMap, sync::Arc};
use tempo_contracts::precompiles::ITIP403Registry::PolicyType;

use super::versioned::HeightVersioned;

/// Per-token policy state.
#[derive(Debug, Default)]
pub struct TokenPolicy {
    /// The policy ID governing transfers for this token (from TIP20.transferPolicyId).
    pub policy_id: HeightVersioned<u64>,
    /// The policy type (WHITELIST or BLACKLIST).
    pub policy_type: HeightVersioned<PolicyType>,
    /// Per-user membership in the policy's set.
    pub members: HashMap<Address, HeightVersioned<bool>>,
}

/// Block-versioned cache of TIP-403 policy state from Tempo L1.
///
/// Each token address maps to a [`TokenPolicy`] containing the policy ID, policy type, and
/// per-user membership. This mirrors the on-chain state from the TIP-20 and TIP-403 contracts
/// so the zone sequencer can evaluate transfer authorization without RPC round-trips.
///
/// Each value is tracked as a [`HeightVersioned`] so lookups can target any L1 block height
/// the zone has committed to.
#[derive(Debug, Default)]
pub struct PolicyCache {
    /// Per-token policy state.
    tokens: HashMap<Address, TokenPolicy>,
}

impl PolicyCache {
    /// Returns a mutable reference to the [`TokenPolicy`] for the given token, inserting a
    /// default entry if absent.
    pub fn get_token_policy_entry(&mut self, token: Address) -> &mut TokenPolicy {
        self.tokens.entry(token).or_default()
    }

    /// Returns the `transferPolicyId` for a token at the given block, or `None` if not cached.
    pub fn get_token_policy(&self, token: Address, block_number: u64) -> Option<u64> {
        self.tokens.get(&token)?.policy_id.get(block_number)
    }

    /// Sets the `transferPolicyId` for a token at the given block.
    pub fn set_token_policy(&mut self, token: Address, block_number: u64, policy_id: u64) {
        self.get_token_policy_entry(token)
            .policy_id
            .set(block_number, policy_id);
    }

    /// Sets the policy type for a token at the given block.
    pub fn set_token_policy_type(
        &mut self,
        token: Address,
        block_number: u64,
        policy_type: PolicyType,
    ) {
        self.get_token_policy_entry(token)
            .policy_type
            .set(block_number, policy_type);
    }

    /// Sets whether `user` is a member of the policy set for `token` at the given block.
    pub fn set_member(
        &mut self,
        token: Address,
        user: Address,
        block_number: u64,
        in_set: bool,
    ) {
        self.get_token_policy_entry(token)
            .members
            .entry(user)
            .or_default()
            .set(block_number, in_set);
    }

    /// Returns all token addresses currently using the given `policy_id` at `block_number`.
    pub fn tokens_using_policy(&self, policy_id: u64, block_number: u64) -> Vec<Address> {
        self.tokens
            .iter()
            .filter(|(_, tp)| tp.policy_id.get(block_number) == Some(policy_id))
            .map(|(addr, _)| *addr)
            .collect()
    }

    /// Finds all tokens using `policy_id` at `block_number` and updates their membership for
    /// `user`. Returns the number of tokens updated.
    pub fn update_policy_membership(
        &mut self,
        policy_id: u64,
        user: Address,
        block_number: u64,
        in_set: bool,
    ) -> usize {
        let matching: Vec<Address> = self.tokens_using_policy(policy_id, block_number);
        let count = matching.len();
        for token in matching {
            self.set_member(token, user, block_number, in_set);
        }
        count
    }

    /// Finds all tokens using `policy_id` at `block_number` and updates their policy type.
    pub fn update_policy_type(
        &mut self,
        policy_id: u64,
        block_number: u64,
        policy_type: PolicyType,
    ) {
        let matching: Vec<Address> = self.tokens_using_policy(policy_id, block_number);
        for token in matching {
            self.set_token_policy_type(token, block_number, policy_type);
        }
    }

    /// Check if an address is authorized under a token's transfer policy at the given block.
    ///
    /// This mirrors [`TIP403Registry.isAuthorized(policyId, user)`] on L1. Callers should
    /// invoke it once per address that needs checking — e.g. both sender **and** recipient for
    /// transfers, or just the recipient for incoming deposits/mints.
    ///
    /// Returns `Some(true/false)` if all required data is cached, or `None` on cache miss
    /// (caller should fall back to RPC).
    pub fn is_authorized(&self, token: Address, user: Address, block_number: u64) -> Option<bool> {
        let tp = self.tokens.get(&token)?;
        let policy_id = tp.policy_id.get(block_number)?;

        // Special policies — no storage lookup needed.
        if policy_id < 2 {
            return Some(policy_id == 1);
        }

        let policy_type = tp.policy_type.get(block_number)?;
        let in_set = tp.members.get(&user)?.get(block_number)?;

        match policy_type {
            PolicyType::WHITELIST => Some(in_set),
            PolicyType::BLACKLIST => Some(!in_set),
            _ => None,
        }
    }

    /// Clears all cached policy data.
    pub fn clear(&mut self) {
        self.tokens.clear();
    }

    /// Collapse all history before `min_block` into single baseline entries.
    pub fn flatten(&mut self, min_block: u64) {
        self.tokens.retain(|_, tp| {
            tp.policy_id.flatten(min_block);
            tp.policy_type.flatten(min_block);
            for v in tp.members.values_mut() {
                v.flatten(min_block);
            }
            tp.members.retain(|_, v| !v.is_empty());
            !tp.policy_id.is_empty()
        });
    }

    /// Advance the baseline to `new_height` for all tracked entries.
    pub fn advance(&mut self, new_height: u64) {
        for tp in self.tokens.values_mut() {
            tp.policy_id.advance(new_height);
            tp.policy_type.advance(new_height);
            for v in tp.members.values_mut() {
                v.advance(new_height);
            }
        }
    }
}

/// Shared handle to the policy cache.
#[derive(Debug, Clone, Deref)]
pub struct SharedPolicyCache(Arc<RwLock<PolicyCache>>);

impl Default for SharedPolicyCache {
    fn default() -> Self {
        Self(Arc::new(RwLock::new(PolicyCache::default())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const TOKEN: Address = address!("0x20C0000000000000000000000000000000000000");
    const USER_A: Address = address!("0x0000000000000000000000000000000000000001");
    const USER_B: Address = address!("0x0000000000000000000000000000000000000002");

    #[test]
    fn special_policy_always_reject() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 0);
        assert_eq!(cache.is_authorized(TOKEN, USER_A, 10), Some(false));
    }

    #[test]
    fn special_policy_always_allow() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 1);
        assert_eq!(cache.is_authorized(TOKEN, USER_A, 10), Some(true));
    }

    #[test]
    fn whitelist_authorized_when_in_set() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_token_policy_type(TOKEN, 10, PolicyType::WHITELIST);
        cache.set_member(TOKEN, USER_A, 10, true);
        cache.set_member(TOKEN, USER_B, 10, false);

        assert_eq!(cache.is_authorized(TOKEN, USER_A, 10), Some(true));
        assert_eq!(cache.is_authorized(TOKEN, USER_B, 10), Some(false));
    }

    #[test]
    fn blacklist_authorized_when_not_in_set() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 3);
        cache.set_token_policy_type(TOKEN, 10, PolicyType::BLACKLIST);
        cache.set_member(TOKEN, USER_A, 10, true);
        cache.set_member(TOKEN, USER_B, 10, false);

        assert_eq!(cache.is_authorized(TOKEN, USER_A, 10), Some(false));
        assert_eq!(cache.is_authorized(TOKEN, USER_B, 10), Some(true));
    }

    #[test]
    fn returns_none_on_missing_token_policy() {
        let cache = PolicyCache::default();
        assert_eq!(cache.is_authorized(TOKEN, USER_A, 10), None);
    }

    #[test]
    fn returns_none_on_missing_policy_type() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 5);
        assert_eq!(cache.is_authorized(TOKEN, USER_A, 10), None);
    }

    #[test]
    fn returns_none_on_missing_membership() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_token_policy_type(TOKEN, 10, PolicyType::WHITELIST);
        assert_eq!(cache.is_authorized(TOKEN, USER_A, 10), None);
    }

    #[test]
    fn block_versioned_policy_change() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 1);
        cache.set_token_policy(TOKEN, 20, 2);
        cache.set_token_policy_type(TOKEN, 20, PolicyType::WHITELIST);
        cache.set_member(TOKEN, USER_A, 20, true);

        assert_eq!(cache.is_authorized(TOKEN, USER_A, 15), Some(true));
        assert_eq!(cache.is_authorized(TOKEN, USER_B, 15), Some(true));
        assert_eq!(cache.is_authorized(TOKEN, USER_A, 25), Some(true));
        assert_eq!(cache.is_authorized(TOKEN, USER_B, 25), None);
    }

    #[test]
    fn block_versioned_membership_change() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_token_policy_type(TOKEN, 10, PolicyType::WHITELIST);
        cache.set_member(TOKEN, USER_A, 10, false);
        cache.set_member(TOKEN, USER_A, 20, true);

        assert_eq!(cache.is_authorized(TOKEN, USER_A, 15), Some(false));
        assert_eq!(cache.is_authorized(TOKEN, USER_A, 25), Some(true));
    }

    #[test]
    fn clear_removes_all_data() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_token_policy_type(TOKEN, 10, PolicyType::WHITELIST);
        cache.set_member(TOKEN, USER_A, 10, true);

        cache.clear();

        assert_eq!(cache.is_authorized(TOKEN, USER_A, 10), None);
    }

    #[test]
    fn flatten_keeps_baseline() {
        let mut cache = PolicyCache::default();
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
    fn update_policy_membership_fans_out() {
        let mut cache = PolicyCache::default();
        let token2: Address = address!("0x20C0000000000000000000000000000000000001");

        // Two tokens share policy 2
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_token_policy_type(TOKEN, 10, PolicyType::WHITELIST);
        cache.set_token_policy(token2, 10, 2);
        cache.set_token_policy_type(token2, 10, PolicyType::WHITELIST);

        let count = cache.update_policy_membership(2, USER_A, 10, true);
        assert_eq!(count, 2);

        assert_eq!(cache.is_authorized(TOKEN, USER_A, 10), Some(true));
        assert_eq!(cache.is_authorized(token2, USER_A, 10), Some(true));
    }

    #[test]
    fn update_policy_type_fans_out() {
        let mut cache = PolicyCache::default();
        let token2: Address = address!("0x20C0000000000000000000000000000000000001");

        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_token_policy(token2, 10, 2);

        cache.update_policy_type(2, 10, PolicyType::BLACKLIST);

        cache.set_member(TOKEN, USER_A, 10, true);
        cache.set_member(token2, USER_A, 10, false);

        // BLACKLIST: authorized when NOT in set
        assert_eq!(cache.is_authorized(TOKEN, USER_A, 10), Some(false));
        assert_eq!(cache.is_authorized(token2, USER_A, 10), Some(true));
    }

    #[test]
    fn tokens_using_policy_returns_correct_set() {
        let mut cache = PolicyCache::default();
        let token2: Address = address!("0x20C0000000000000000000000000000000000001");

        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_token_policy(token2, 10, 3);

        let mut result = cache.tokens_using_policy(2, 10);
        result.sort();
        assert_eq!(result, vec![TOKEN]);

        assert!(cache.tokens_using_policy(99, 10).is_empty());
    }

    #[test]
    fn get_token_policy_entry_creates_default() {
        let mut cache = PolicyCache::default();
        let entry = cache.get_token_policy_entry(TOKEN);
        assert!(entry.policy_id.is_empty());
        assert!(entry.members.is_empty());
    }

    #[test]
    fn advance_then_lookup() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_token_policy_type(TOKEN, 10, PolicyType::BLACKLIST);
        cache.set_member(TOKEN, USER_A, 10, true);
        cache.set_member(TOKEN, USER_A, 20, false);

        cache.advance(15);

        // After advancing to 15, baseline includes block-10 state.
        assert_eq!(cache.is_authorized(TOKEN, USER_A, 12), Some(false)); // blacklisted
        assert_eq!(cache.is_authorized(TOKEN, USER_A, 25), Some(true)); // unblacklisted at 20
    }

    #[test]
    fn flatten_removes_empty_entries() {
        let mut cache = PolicyCache::default();
        let token2: Address = address!("0x20C0000000000000000000000000000000000001");

        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_token_policy(token2, 10, 3);

        // Flatten everything but don't add baseline values for token2's policy_id —
        // flatten only keeps entries that aren't empty after compaction.
        cache.flatten(15);

        // Both should survive because their policy_id has values
        assert_eq!(cache.get_token_policy(TOKEN, 15), Some(2));
        assert_eq!(cache.get_token_policy(token2, 15), Some(3));
    }

    #[test]
    fn policy_change_mid_block_range() {
        let mut cache = PolicyCache::default();

        // Start with whitelist at block 10, switch to blacklist at block 20
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_token_policy_type(TOKEN, 10, PolicyType::WHITELIST);
        cache.set_token_policy_type(TOKEN, 20, PolicyType::BLACKLIST);
        cache.set_member(TOKEN, USER_A, 10, true);

        // At block 15 (whitelist), USER_A is in set → authorized
        assert_eq!(cache.is_authorized(TOKEN, USER_A, 15), Some(true));
        // At block 25 (blacklist), USER_A is in set → NOT authorized
        assert_eq!(cache.is_authorized(TOKEN, USER_A, 25), Some(false));
    }
}
