//! TIP-403 policy types shared between the zone node and the prover.
//!
//! These are extracted here so that `zone-precompiles` (which is `no_std`) can
//! reference `AuthRole` and the builtin policy constants without pulling in the
//! full `tempo-zone` dependency tree.

/// Authorization role for TIP-403 policy checks.
///
/// Determines which sub-policy is evaluated for compound policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// Builtin policy ID that rejects all addresses (whitelist with no members).
pub const POLICY_REJECT_ALL: u64 = 0;

/// Builtin policy ID that allows all addresses (blacklist with no members).
pub const POLICY_ALLOW_ALL: u64 = 1;

/// First user-created policy ID. IDs below this are builtins.
pub const FIRST_USER_POLICY: u64 = 2;
