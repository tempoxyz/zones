//! Policy authorization trait for zone precompiles.
//!
//! Defines [`PolicyCheck`], an abstraction over the concrete `PolicyProvider`
//! so that the policy/token precompiles in this crate don't depend on tokio,
//! alloy providers, or any std-only infrastructure.

use alloy_primitives::Address;
use revm::precompile::PrecompileError;
use zone_primitives::policy::AuthRole;

/// Authorization provider used by the TIP-403 proxy and zone TIP-20 precompiles.
///
/// Implementors resolve policy queries — either from an in-memory cache with
/// RPC fallback (zone node) or from a witness database (SP1 prover guest).
pub trait PolicyCheck {
    /// Check whether `user` is authorized under `policy_id` for the given `role`.
    fn is_authorized(
        &self,
        policy_id: u64,
        user: Address,
        role: AuthRole,
    ) -> Result<bool, PrecompileError>;

    /// Resolve the `transferPolicyId` for a token.
    fn resolve_transfer_policy_id(&self, token: Address) -> Result<u64, PrecompileError>;

    /// Resolve policy type and admin for a policy ID.
    ///
    /// Returns `Ok(Some((policy_type, admin)))` if the policy exists, `Ok(None)` otherwise.
    fn policy_type_sync(
        &self,
        policy_id: u64,
    ) -> Result<tempo_contracts::precompiles::ITIP403Registry::PolicyType, PrecompileError>;

    /// Resolve compound policy sub-IDs.
    ///
    /// Returns `(sender_policy_id, recipient_policy_id, mint_recipient_policy_id)`.
    fn compound_policy_data(&self, policy_id: u64) -> Result<(u64, u64, u64), PrecompileError>;

    /// Check whether a policy exists.
    fn policy_exists(&self, policy_id: u64) -> Result<bool, PrecompileError>;

    /// Return the highest known policy ID counter.
    fn policy_id_counter(&self) -> u64;
}
