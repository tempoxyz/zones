//! Block-versioned in-memory cache of TIP-403 transfer policy state from Tempo L1.
//!
//! The zone sequencer needs to know whether addresses are authorized under the TIP-403 policy
//! of each token enabled on the zone. This cache tracks:
//!
//! - Per-token `transferPolicyId` — which policy governs a token's transfers.
//! - Per-policy type — whether a policy is a whitelist or blacklist.
//! - Per-policy address membership — whether an address is in the policy's set.
//!
//! All entries are block-versioned via [`VersionedValue`](super::versioned::VersionedValue) so
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
use derive_more::{Deref, DerefMut};
use parking_lot::RwLock;
use std::{collections::HashMap, sync::Arc};
use tempo_contracts::precompiles::ITIP403Registry::PolicyType;

use super::versioned::HeightVersioned;

/// Block-versioned cache of TIP-403 policy state from Tempo L1.
///
/// Mirrors three on-chain mappings from the TIP-20 and TIP-403 contracts so the zone
/// sequencer can evaluate transfer authorization without RPC round-trips:
///
/// | On-chain source | Solidity type | Cache key | Cache value |
/// |---|---|---|---|
/// | [`TIP20.transferPolicyId`] | `uint64` | token address | policy ID (`u64`) |
/// | [`TIP403Registry._policyData[id].policyType`] | `PolicyType` enum | policy ID | `WHITELIST` or `BLACKLIST` |
/// | [`TIP403Registry.policySet[id][addr]`] | `bool` | `(policy ID, address)` | whether the address is in the set |
///
/// Each value is tracked as a [`HeightVersioned`] so lookups can target any L1 block height
/// the zone has committed to.
#[derive(Debug, Default)]
pub struct PolicyCache {
    /// Tracks [`TIP20.transferPolicyId`] — the policy ID governing transfers for each token.
    ///
    /// Key: TIP-20 token contract address.
    /// Value: block-versioned policy ID (`u64`). Policy 0 = always-reject, 1 = always-allow,
    /// ≥ 2 = lookup in [`policy_types`](Self::policy_types) and
    /// [`policy_sets`](Self::policy_sets).
    token_policies: HashMap<Address, HeightVersioned<u64>>,

    /// Tracks [`TIP403Registry._policyData[id].policyType`] — whether a policy is a whitelist
    /// or blacklist.
    ///
    /// Key: policy ID (`u64`, ≥ 2).
    /// Value: block-versioned [`PolicyType`] (`WHITELIST` or `BLACKLIST`).
    policy_types: HashMap<u64, HeightVersioned<PolicyType>>,

    /// Tracks [`TIP403Registry.policySet[id][addr]`] — per-address membership in a policy's
    /// allow/deny set.
    ///
    /// Key: `(policy ID, user address)`.
    /// Value: block-versioned `bool`. For whitelist policies `true` means the address is
    /// whitelisted (authorized). For blacklist policies `true` means the address is
    /// blacklisted (not authorized).
    policy_sets: HashMap<(u64, Address), HeightVersioned<bool>>,
}

impl PolicyCache {
    /// Returns the `transferPolicyId` for a token at the given block, or `None` if not cached.
    pub fn get_token_policy(&self, token: Address, block_number: u64) -> Option<u64> {
        self.token_policies.get(&token)?.get(block_number)
    }

    /// Sets the `transferPolicyId` for a token at the given block.
    pub fn set_token_policy(&mut self, token: Address, block_number: u64, policy_id: u64) {
        self.token_policies
            .entry(token)
            .or_default()
            .set(block_number, policy_id);
    }

    /// Returns the policy type at the given block, or `None` if not cached.
    pub fn get_policy_type(&self, policy_id: u64, block_number: u64) -> Option<PolicyType> {
        self.policy_types.get(&policy_id)?.get(block_number)
    }

    /// Sets the policy type at the given block.
    pub fn set_policy_type(&mut self, policy_id: u64, block_number: u64, policy_type: PolicyType) {
        self.policy_types
            .entry(policy_id)
            .or_default()
            .set(block_number, policy_type);
    }

    /// Returns whether an address is in a policy's set at the given block.
    ///
    /// Returns `None` if the membership has not been cached for this `(policyId, address)` pair.
    pub fn get_policy_set_membership(
        &self,
        policy_id: u64,
        address: Address,
        block_number: u64,
    ) -> Option<bool> {
        self.policy_sets
            .get(&(policy_id, address))?
            .get(block_number)
    }

    /// Sets whether an address is in a policy's set at the given block.
    pub fn set_policy_set_membership(
        &mut self,
        policy_id: u64,
        address: Address,
        block_number: u64,
        in_set: bool,
    ) {
        self.policy_sets
            .entry((policy_id, address))
            .or_default()
            .set(block_number, in_set);
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
        let policy_id = self.get_token_policy(token, block_number)?;

        // Special policies — no storage lookup needed.
        if policy_id < 2 {
            return Some(policy_id == 1);
        }

        let policy_type = self.get_policy_type(policy_id, block_number)?;
        let in_set = self.get_policy_set_membership(policy_id, user, block_number)?;

        match policy_type {
            PolicyType::WHITELIST => Some(in_set),
            PolicyType::BLACKLIST => Some(!in_set),
            _ => None,
        }
    }

    /// Clears all cached policy data.
    pub fn clear(&mut self) {
        self.token_policies.clear();
        self.policy_types.clear();
        self.policy_sets.clear();
    }

    /// Collapse all history before `min_block` into single baseline entries.
    pub fn flatten(&mut self, min_block: u64) {
        for v in self.token_policies.values_mut() {
            v.flatten(min_block);
        }
        for v in self.policy_types.values_mut() {
            v.flatten(min_block);
        }
        for v in self.policy_sets.values_mut() {
            v.flatten(min_block);
        }
        self.token_policies.retain(|_, v| !v.is_empty());
        self.policy_types.retain(|_, v| !v.is_empty());
        self.policy_sets.retain(|_, v| !v.is_empty());
    }
}

/// Shared handle to the policy cache.
#[derive(Debug, Clone, Deref, DerefMut)]
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
        cache.set_policy_type(2, 10, PolicyType::WHITELIST);
        cache.set_policy_set_membership(2, USER_A, 10, true);
        cache.set_policy_set_membership(2, USER_B, 10, false);

        assert_eq!(cache.is_authorized(TOKEN, USER_A, 10), Some(true));
        assert_eq!(cache.is_authorized(TOKEN, USER_B, 10), Some(false));
    }

    #[test]
    fn blacklist_authorized_when_not_in_set() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 3);
        cache.set_policy_type(3, 10, PolicyType::BLACKLIST);
        cache.set_policy_set_membership(3, USER_A, 10, true);
        cache.set_policy_set_membership(3, USER_B, 10, false);

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
        cache.set_policy_type(2, 10, PolicyType::WHITELIST);
        assert_eq!(cache.is_authorized(TOKEN, USER_A, 10), None);
    }

    #[test]
    fn block_versioned_policy_change() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 1);
        cache.set_token_policy(TOKEN, 20, 2);
        cache.set_policy_type(2, 20, PolicyType::WHITELIST);
        cache.set_policy_set_membership(2, USER_A, 20, true);

        assert_eq!(cache.is_authorized(TOKEN, USER_A, 15), Some(true));
        assert_eq!(cache.is_authorized(TOKEN, USER_B, 15), Some(true));
        assert_eq!(cache.is_authorized(TOKEN, USER_A, 25), Some(true));
        assert_eq!(cache.is_authorized(TOKEN, USER_B, 25), None);
    }

    #[test]
    fn block_versioned_membership_change() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_policy_type(2, 10, PolicyType::WHITELIST);
        cache.set_policy_set_membership(2, USER_A, 10, false);
        cache.set_policy_set_membership(2, USER_A, 20, true);

        assert_eq!(cache.is_authorized(TOKEN, USER_A, 15), Some(false));
        assert_eq!(cache.is_authorized(TOKEN, USER_A, 25), Some(true));
    }

    #[test]
    fn clear_removes_all_data() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_policy_type(2, 10, PolicyType::WHITELIST);
        cache.set_policy_set_membership(2, USER_A, 10, true);

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
}
