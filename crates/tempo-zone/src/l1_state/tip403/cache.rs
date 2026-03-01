//! Block-versioned in-memory cache of TIP-403 transfer policy state from Tempo L1.
//!
//! The zone sequencer needs to know whether addresses are authorized under the TIP-403 policy
//! of each token enabled on the zone. This cache mirrors the L1 `TIP403Registry` storage
//! layout:
//!
//! - **Token → policy ID**: Each token address maps to a `transferPolicyId` via
//!   [`HeightVersioned`](super::versioned::HeightVersioned), tracking the
//!   `TransferPolicyUpdate` event.
//!
//! - **Policy records**: Each policy ID maps to a [`CachedPolicy`] containing:
//!   - The policy type (whitelist, blacklist, or compound).
//!   - Set membership via [`MembershipSet`] — a `HashSet` baseline plus per-block deltas
//!     mirroring `WhitelistUpdated` / `BlacklistUpdated` events.
//!   - Compound sub-policy IDs for sender, recipient, and mint recipient roles.
//!
//! ## Special policies
//!
//! Policy ID `0` always rejects, policy ID `1` always allows. These are handled inline by
//! [`PolicyCache::is_authorized`] without any storage lookups.
//!
//! ## Default membership
//!
//! Users with no recorded membership are treated as "not in set" (`false`), matching the L1
//! storage default for `policy_set[policyId][user]`. This means:
//! - **Blacklist**: unknown users are authorized (not restricted) — correct default.
//! - **Whitelist**: unknown users are not authorized — correct if all additions were observed.
//!
//! ## Compound policies (TIP-1015)
//!
//! A compound policy delegates authorization to sub-policies based on the user's role
//! (sender, recipient, or mint recipient). The [`is_authorized`](PolicyCache::is_authorized)
//! method accepts an [`AuthRole`] to resolve the correct sub-policy.
//!
//! ## Reorg handling
//!
//! On reorgs the caller is expected to [`PolicyCache::clear`] the entire cache. There is no
//! per-block rollback.

use alloy_primitives::Address;
use derive_more::Deref;
use parking_lot::RwLock;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};
use tempo_contracts::precompiles::ITIP403Registry::PolicyType;

use crate::l1_state::versioned::HeightVersioned;

/// Built-in "always reject" policy (policy ID 0). All addresses are unauthorized.
pub(crate) const POLICY_REJECT_ALL: u64 = 0;

/// Built-in "always allow" policy (policy ID 1). All addresses are authorized.
pub(crate) const POLICY_ALLOW_ALL: u64 = 1;

/// First user-created policy ID. IDs below this are reserved builtins.
pub(crate) const FIRST_USER_POLICY: u64 = 2;

/// Block-versioned cache of TIP-403 policy state from Tempo L1.
///
/// Mirrors the on-chain `TIP403Registry` storage layout with:
/// - Token → `transferPolicyId` mapping (from TIP-20 `TransferPolicyUpdate` events).
/// - Policy ID → policy record (type, membership, compound data).
///
/// This allows the zone sequencer to evaluate transfer authorization without RPC round-trips.
#[derive(Debug, Default)]
pub struct PolicyCache {
    /// Per-token transfer policy ID.
    tokens: HashMap<Address, HeightVersioned<u64>>,
    /// Per-policy-ID records (type, membership, compound data).
    policies: HashMap<u64, CachedPolicy>,
}

impl PolicyCache {
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

    /// Sets whether `user` is a member of the policy set at the given block.
    pub fn set_member(&mut self, policy_id: u64, user: Address, block_number: u64, in_set: bool) {
        self.get_policy_entry(policy_id)
            .members
            .set(user, block_number, in_set);
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
    /// - **Simple** (whitelist/blacklist): checks membership set.
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
        if policy_id < FIRST_USER_POLICY {
            // Policy 0 = reject all (empty whitelist), policy 1 = allow all (empty blacklist).
            return Some(policy_id == POLICY_ALLOW_ALL);
        }

        let policy = self.policies.get(&policy_id)?;
        let policy_type = policy.policy_type?;

        match policy_type {
            PolicyType::WHITELIST => Some(policy.members.is_member(user, block_number)),
            PolicyType::BLACKLIST => Some(!policy.members.is_member(user, block_number)),
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
    fn check_simple(&self, policy_id: u64, user: Address, block_number: u64) -> Option<bool> {
        if policy_id < FIRST_USER_POLICY {
            return Some(policy_id == POLICY_ALLOW_ALL);
        }

        let policy = self.policies.get(&policy_id)?;
        let policy_type = policy.policy_type?;

        match policy_type {
            PolicyType::WHITELIST => Some(policy.members.is_member(user, block_number)),
            PolicyType::BLACKLIST => Some(!policy.members.is_member(user, block_number)),
            _ => None,
        }
    }

    /// Apply a batch of decoded policy events for a single block.
    ///
    /// This is the primary ingestion path for the [`PolicyListener`](super::PolicyListener).
    /// Events are decoded outside the write lock, then applied here in one batch.
    pub fn apply_events(&mut self, block_number: u64, events: &[PolicyEvent]) {
        for event in events {
            match event {
                PolicyEvent::MembershipChanged {
                    policy_id,
                    account,
                    in_set,
                } => {
                    self.set_member(*policy_id, *account, block_number, *in_set);
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

    /// Clears all cached policy data.
    pub fn clear(&mut self) {
        self.tokens.clear();
        self.policies.clear();
    }

    /// Collapse all history before `min_block` into single baseline entries.
    pub fn flatten(&mut self, min_block: u64) {
        self.tokens.retain(|_, v| {
            v.flatten(min_block);
            !v.is_empty()
        });
        for policy in self.policies.values_mut() {
            policy.members.flatten(min_block);
        }
    }

    /// Advance the baseline to `new_height` for all tracked entries.
    pub fn advance(&mut self, new_height: u64) {
        for v in self.tokens.values_mut() {
            v.advance(new_height);
        }
        for policy in self.policies.values_mut() {
            policy.members.advance(new_height);
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

/// A decoded L1 policy event ready to be applied to the cache.
///
/// The [`PolicyListener`](super::PolicyListener) decodes raw logs into these events
/// outside the cache write lock, then applies them in batch via
/// [`PolicyCache::apply_events`].
#[derive(Debug, Clone)]
pub enum PolicyEvent {
    /// A user's membership in a policy set changed (`WhitelistUpdated` / `BlacklistUpdated`).
    MembershipChanged {
        policy_id: u64,
        account: Address,
        in_set: bool,
    },
    /// A token's transfer policy ID changed (`TransferPolicyUpdate`).
    TokenPolicyChanged { token: Address, policy_id: u64 },
    /// A new simple policy was created on L1 (`PolicyCreated`).
    PolicyCreated {
        policy_id: u64,
        policy_type: PolicyType,
    },
    /// A new compound policy was created on L1 (`CompoundPolicyCreated`).
    CompoundPolicyCreated {
        policy_id: u64,
        sender_policy_id: u64,
        recipient_policy_id: u64,
        mint_recipient_policy_id: u64,
    },
}

/// Authorization role for policy checks.
///
/// For simple policies (whitelist/blacklist), the role is ignored — the same membership set
/// applies regardless. For compound policies (TIP-1015), the role selects which sub-policy
/// to check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthRole {
    /// Check both sender AND recipient. For compound policies, short-circuits on sender failure.
    Transfer,
    /// Check sender authorization only (compound: uses `senderPolicyId`).
    Sender,
    /// Check recipient authorization only (compound: uses `recipientPolicyId`).
    Recipient,
    /// Check mint recipient authorization only (compound: uses `mintRecipientPolicyId`).
    MintRecipient,
}

/// Per-policy-ID cached record, mirroring `TIP403Registry.policy_records[id]`.
#[derive(Debug, Default)]
pub struct CachedPolicy {
    /// Policy type. `None` if the `PolicyCreated` event hasn't been observed yet.
    pub policy_type: Option<PolicyType>,
    /// Set membership for simple (non-compound) policies.
    pub members: MembershipSet,
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

/// Block-versioned membership set for TIP-403 policy tracking.
///
/// Models set membership as a baseline [`HashSet`] plus per-block [`MembershipUpdate`] deltas,
/// matching the L1 event model where `WhitelistUpdated` and `BlacklistUpdated` events arrive
/// as `(address, add/remove)` updates per block.
///
/// Users not explicitly tracked are treated as "not in set", matching the L1 storage default
/// for `policy_set[policyId][user]`.
#[derive(Debug, Default)]
pub struct MembershipSet {
    /// Addresses in the set at `baseline_height`.
    baseline: HashSet<Address>,
    /// Block height up to which the baseline is valid.
    baseline_height: u64,
    /// Per-block membership changes above `baseline_height`.
    pending: BTreeMap<u64, Vec<MembershipUpdate>>,
}

impl MembershipSet {
    /// Check if `user` is in the set at the given block height.
    ///
    /// Returns `false` for users with no recorded state, matching the L1 storage default.
    pub fn is_member(&self, user: Address, block_number: u64) -> bool {
        if block_number <= self.baseline_height {
            return self.baseline.contains(&user);
        }

        // Scan pending blocks in reverse for the latest change affecting this user.
        for (_, updates) in self.pending.range(..=block_number).rev() {
            for update in updates.iter().rev() {
                if update.account == user {
                    return update.change.is_in_set();
                }
            }
        }

        self.baseline.contains(&user)
    }

    /// Record a membership change at the given block height.
    pub fn set(&mut self, user: Address, block_number: u64, in_set: bool) {
        let change = MembershipChange::from_in_set(in_set);
        if block_number <= self.baseline_height {
            match change {
                MembershipChange::Add => {
                    self.baseline.insert(user);
                }
                MembershipChange::Remove => {
                    self.baseline.remove(&user);
                }
            }
        } else {
            self.pending
                .entry(block_number)
                .or_default()
                .push(MembershipUpdate {
                    account: user,
                    change,
                });
        }
    }

    /// Advance the baseline to `new_height`, folding pending deltas.
    pub fn advance(&mut self, new_height: u64) {
        if new_height <= self.baseline_height {
            return;
        }

        let to_apply: Vec<u64> = self.pending.range(..=new_height).map(|(k, _)| *k).collect();
        for block in to_apply {
            if let Some(updates) = self.pending.remove(&block) {
                for update in updates {
                    match update.change {
                        MembershipChange::Add => {
                            self.baseline.insert(update.account);
                        }
                        MembershipChange::Remove => {
                            self.baseline.remove(&update.account);
                        }
                    }
                }
            }
        }

        self.baseline_height = new_height;
    }

    /// Equivalent to [`advance`](Self::advance).
    pub fn flatten(&mut self, min_block: u64) {
        self.advance(min_block);
    }

    /// Returns `true` if no membership data has been recorded.
    pub fn is_empty(&self) -> bool {
        self.baseline.is_empty() && self.pending.is_empty()
    }

    /// Clears all membership data and resets the baseline height.
    pub fn clear(&mut self) {
        self.baseline.clear();
        self.baseline_height = 0;
        self.pending.clear();
    }
}

/// Whether a policy set member was added or removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MembershipChange {
    /// Address was added to the policy set (whitelisted / blacklisted).
    Add,
    /// Address was removed from the policy set (un-whitelisted / un-blacklisted).
    Remove,
}

impl MembershipChange {
    /// Convert from the L1 event's boolean (`allowed` / `restricted`) to a change.
    pub(super) fn from_in_set(in_set: bool) -> Self {
        if in_set { Self::Add } else { Self::Remove }
    }

    /// Whether this change means the address is in the set.
    pub(super) fn is_in_set(self) -> bool {
        matches!(self, Self::Add)
    }
}

/// A single membership update within a block.
#[derive(Debug, Clone, Copy)]
pub(super) struct MembershipUpdate {
    /// The address whose membership changed.
    pub account: Address,
    /// Whether the address was added to or removed from the set.
    pub change: MembershipChange,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const TOKEN: Address = address!("0x20C0000000000000000000000000000000000000");
    const USER_A: Address = address!("0x0000000000000000000000000000000000000001");
    const USER_B: Address = address!("0x0000000000000000000000000000000000000002");

    // --- MembershipSet tests ---

    #[test]
    fn membership_default_is_not_in_set() {
        let set = MembershipSet::default();
        assert!(!set.is_member(USER_A, 100));
    }

    #[test]
    fn membership_add_and_remove() {
        let mut set = MembershipSet::default();
        set.set(USER_A, 10, true);
        assert!(set.is_member(USER_A, 10));
        assert!(set.is_member(USER_A, 15));
        assert!(!set.is_member(USER_A, 5));

        set.set(USER_A, 20, false);
        assert!(set.is_member(USER_A, 15));
        assert!(!set.is_member(USER_A, 25));
    }

    #[test]
    fn membership_multiple_users_same_block() {
        let mut set = MembershipSet::default();
        set.set(USER_A, 10, true);
        set.set(USER_B, 10, true);

        assert!(set.is_member(USER_A, 10));
        assert!(set.is_member(USER_B, 10));
    }

    #[test]
    fn membership_advance_folds_deltas() {
        let mut set = MembershipSet::default();
        set.set(USER_A, 10, true);
        set.set(USER_B, 15, true);
        set.set(USER_A, 20, false);

        set.advance(15);

        // USER_A added at 10 (folded into baseline), USER_B added at 15 (folded)
        assert!(set.is_member(USER_A, 15));
        assert!(set.is_member(USER_B, 15));

        // USER_A removed at 20 (still pending)
        assert!(!set.is_member(USER_A, 25));
    }

    #[test]
    fn membership_set_below_baseline_updates_directly() {
        let mut set = MembershipSet::default();
        set.set(USER_A, 10, true);
        set.advance(20);

        // Set below baseline updates directly
        set.set(USER_A, 15, false);
        assert!(!set.is_member(USER_A, 20));
    }

    // --- PolicyCache tests: simple policies ---

    #[test]
    fn special_policy_always_reject() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 0);
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(false)
        );
    }

    #[test]
    fn special_policy_always_allow() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 1);
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(true)
        );
    }

    #[test]
    fn whitelist_authorized_when_in_set() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_policy_type(2, PolicyType::WHITELIST);
        cache.set_member(2, USER_A, 10, true);
        cache.set_member(2, USER_B, 10, false);

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
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 3);
        cache.set_policy_type(3, PolicyType::BLACKLIST);
        cache.set_member(3, USER_A, 10, true);
        cache.set_member(3, USER_B, 10, false);

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
    fn blacklist_unknown_user_is_authorized() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 3);
        cache.set_policy_type(3, PolicyType::BLACKLIST);

        // USER_A has no membership data — defaults to "not in set" → authorized for blacklist
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(true)
        );
    }

    #[test]
    fn whitelist_unknown_user_is_not_authorized() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_policy_type(2, PolicyType::WHITELIST);

        // USER_A has no membership data — defaults to "not in set" → not authorized for whitelist
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(false)
        );
    }

    #[test]
    fn returns_none_on_missing_token_policy() {
        let cache = PolicyCache::default();
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            None
        );
    }

    #[test]
    fn returns_none_on_missing_policy_type() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 5);
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            None
        );
    }

    #[test]
    fn block_versioned_policy_change() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 1);
        cache.set_token_policy(TOKEN, 20, 2);
        cache.set_policy_type(2, PolicyType::WHITELIST);
        cache.set_member(2, USER_A, 20, true);

        // At block 15: policy_id=1 (always allow)
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 15, AuthRole::Transfer),
            Some(true)
        );
        assert_eq!(
            cache.is_authorized(TOKEN, USER_B, 15, AuthRole::Transfer),
            Some(true)
        );

        // At block 25: policy_id=2 (whitelist), USER_A in set, USER_B not in set
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 25, AuthRole::Transfer),
            Some(true)
        );
        assert_eq!(
            cache.is_authorized(TOKEN, USER_B, 25, AuthRole::Transfer),
            Some(false)
        );
    }

    #[test]
    fn block_versioned_membership_change() {
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_policy_type(2, PolicyType::WHITELIST);
        cache.set_member(2, USER_A, 10, false);
        cache.set_member(2, USER_A, 20, true);

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
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_policy_type(2, PolicyType::WHITELIST);
        cache.set_member(2, USER_A, 10, true);

        cache.clear();

        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            None
        );
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
    fn shared_policy_across_tokens() {
        let mut cache = PolicyCache::default();
        let token2: Address = address!("0x20C0000000000000000000000000000000000001");

        // Two tokens share policy 2
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_token_policy(token2, 10, 2);
        cache.set_policy_type(2, PolicyType::WHITELIST);
        cache.set_member(2, USER_A, 10, true);

        // Both tokens see the same membership (per-policy, no fan-out needed)
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
        let mut cache = PolicyCache::default();
        let token2: Address = address!("0x20C0000000000000000000000000000000000001");

        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_token_policy(token2, 10, 2);
        cache.set_policy_type(2, PolicyType::BLACKLIST);
        cache.set_member(2, USER_A, 10, true);

        // BLACKLIST: authorized when NOT in set
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(false)
        );
        assert_eq!(
            cache.is_authorized(token2, USER_A, 10, AuthRole::Transfer),
            Some(false)
        );
        // USER_B not in set → authorized
        assert_eq!(
            cache.is_authorized(TOKEN, USER_B, 10, AuthRole::Transfer),
            Some(true)
        );
        assert_eq!(
            cache.is_authorized(token2, USER_B, 10, AuthRole::Transfer),
            Some(true)
        );
    }

    #[test]
    fn tokens_with_different_policies() {
        let mut cache = PolicyCache::default();
        let token2: Address = address!("0x20C0000000000000000000000000000000000001");

        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_token_policy(token2, 10, 3);
        cache.set_policy_type(2, PolicyType::WHITELIST);
        cache.set_policy_type(3, PolicyType::BLACKLIST);
        cache.set_member(2, USER_A, 10, true);
        cache.set_member(3, USER_A, 10, true);

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
        let mut cache = PolicyCache::default();
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_policy_type(2, PolicyType::BLACKLIST);
        cache.set_member(2, USER_A, 10, true);
        cache.set_member(2, USER_A, 20, false);

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
    fn flatten_removes_empty_token_entries() {
        let mut cache = PolicyCache::default();
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
        let mut cache = PolicyCache::default();

        // Start with whitelist at block 10, switch to blacklist policy at block 20
        cache.set_token_policy(TOKEN, 10, 2);
        cache.set_token_policy(TOKEN, 20, 3);
        cache.set_policy_type(2, PolicyType::WHITELIST);
        cache.set_policy_type(3, PolicyType::BLACKLIST);
        cache.set_member(2, USER_A, 10, true);
        cache.set_member(3, USER_A, 10, true);

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
        let mut cache = PolicyCache::default();
        // Simple sub-policies
        cache.set_policy_type(2, PolicyType::BLACKLIST); // sender policy
        cache.set_policy_type(3, PolicyType::WHITELIST); // recipient policy
        cache.set_member(2, USER_A, 10, true); // USER_A blacklisted as sender
        cache.set_member(3, USER_A, 10, true); // USER_A whitelisted as recipient

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
        let mut cache = PolicyCache::default();
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

        // Neither whitelisted → fails on sender
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(false)
        );

        // Only sender whitelisted → fails on recipient
        cache.set_member(2, USER_A, 10, true);
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(false)
        );

        // Both whitelisted → authorized
        cache.set_member(3, USER_A, 10, true);
        assert_eq!(
            cache.is_authorized(TOKEN, USER_A, 10, AuthRole::Transfer),
            Some(true)
        );
    }

    #[test]
    fn compound_policy_with_builtin_sub_policies() {
        let mut cache = PolicyCache::default();
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
        let mut cache = PolicyCache::default();
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
        let mut cache = PolicyCache::default();
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
        let mut cache = PolicyCache::default();
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
        assert_eq!(
            cache.is_authorized(TOKEN, USER_B, 10, AuthRole::Transfer),
            Some(false)
        );
    }

    #[test]
    fn apply_events_with_compound_policy() {
        let mut cache = PolicyCache::default();
        // Pre-populate simple sub-policies
        cache.set_policy_type(2, PolicyType::BLACKLIST);
        cache.set_policy_type(3, PolicyType::WHITELIST);
        cache.set_member(3, USER_A, 10, true);

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

        // Sender (blacklist, not in set → authorized), Recipient (whitelist, in set → authorized)
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
