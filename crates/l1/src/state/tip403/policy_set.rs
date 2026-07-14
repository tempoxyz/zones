//! Block-versioned policy sets for TIP-403 policy tracking.

use alloy_primitives::Address;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Block-versioned policy set for TIP-403 policy tracking.
///
/// Models a policy set as a baseline [`HashSet`] plus per-block deltas, matching the L1
/// event model where `WhitelistUpdated` and `BlacklistUpdated` events arrive as
/// `(address, add/remove)` updates per block.
///
/// Users not explicitly tracked are treated as "not in set", matching the L1 storage default
/// for `policy_set[policyId][user]`.
#[derive(Debug, Default)]
pub struct PolicySet {
    /// Addresses in the set at `baseline_height`.
    baseline: HashSet<Address>,
    /// Block height up to which the baseline is valid.
    baseline_height: u64,
    /// Per-block set updates above `baseline_height`.
    pending: BTreeMap<u64, Vec<PolicySetUpdate>>,
    /// Earliest block at which we recorded a set event for each address. Survives `advance()`
    /// so we can distinguish "explicitly absent from the set" from "never observed by the
    /// subscriber", *and* so that known-ness is scoped to the queried block: an event only
    /// tells us the membership at and after the block it was observed on.
    first_seen: HashMap<Address, u64>,
}

impl PolicySet {
    /// Check if `user` is in the set at the given block height.
    ///
    /// Returns `false` for users with no recorded state, matching the L1 storage default.
    pub fn contains(&self, user: Address, block_number: u64) -> bool {
        if block_number <= self.baseline_height {
            return self.baseline.contains(&user);
        }

        // Scan pending blocks in reverse for the latest change affecting this user.
        for (_, updates) in self.pending.range(..=block_number).rev() {
            for update in updates.iter().rev() {
                if update.account == user {
                    return update.in_set;
                }
            }
        }

        self.baseline.contains(&user)
    }

    /// Returns `true` if [`contains`](Self::contains) can be trusted for `user` at
    /// `block_number`.
    ///
    /// A set event only reveals membership at and after the block it was observed on. We can
    /// therefore only answer authoritatively for a query block that is at or after the earliest
    /// event we recorded for the user; below that block (or with no event at all) the user may
    /// have been added before we started observing, so the caller must fall back to RPC.
    ///
    /// This scoping matters because the subscriber runs ahead of the engine: the set can hold a
    /// user's *future* delta (e.g. a block-150 removal) while the engine queries an earlier block
    /// (e.g. block 120). Without the block bound, that future delta would mark the user "known"
    /// and suppress the RPC fallback, so a blacklist membership that only exists before the delta
    /// would be silently treated as absent.
    pub fn is_known(&self, user: &Address, block_number: u64) -> bool {
        self.first_seen
            .get(user)
            .is_some_and(|&first| first <= block_number)
    }

    /// Record a set update at the given block height.
    ///
    /// Updates at or below the baseline height are ignored. The baseline represents finalized
    /// engine-consumed state and is only updated by [`advance`](Self::advance), which prevents
    /// delayed RPC fallback results from overwriting newer event-derived membership.
    pub fn record_status(&mut self, user: Address, block_number: u64, in_set: bool) {
        if block_number <= self.baseline_height {
            return;
        }

        self.first_seen
            .entry(user)
            .and_modify(|first| *first = (*first).min(block_number))
            .or_insert(block_number);
        self.pending
            .entry(block_number)
            .or_default()
            .push(PolicySetUpdate {
                account: user,
                in_set,
            });
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
                    if update.in_set {
                        self.baseline.insert(update.account);
                    } else {
                        self.baseline.remove(&update.account);
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

    /// Returns `true` if no set data has been recorded.
    pub fn is_empty(&self) -> bool {
        self.baseline.is_empty() && self.pending.is_empty()
    }

    /// Clears all set data and resets the baseline height.
    pub fn clear(&mut self) {
        self.baseline.clear();
        self.baseline_height = 0;
        self.pending.clear();
        self.first_seen.clear();
    }
}

/// A single set update within a block.
#[derive(Debug, Clone, Copy)]
pub(super) struct PolicySetUpdate {
    /// The address whose policy-set status changed.
    pub account: Address,
    /// Whether the address is in the policy set after this update.
    pub in_set: bool,
}
