//! Zone-side TIP-403 registry proxy precompile.
//!
//! Deployed at the same address as the L1 [`TIP403Registry`] (`0x403C…0000`), this
//! precompile intercepts external EVM calls to the registry and serves authorization
//! queries from the zone's [`PolicyCheck`] provider (cache-first, L1 RPC fallback).
//!
//! **Read-only calls** (`isAuthorized`, `isAuthorizedSender`, `isAuthorizedRecipient`,
//! `isAuthorizedMintRecipient`, `policyData`, `compoundPolicyData`, `policyExists`)
//! are resolved via the [`PolicyCheck`] trait.
//!
//! **Mutating calls** (`createPolicy`, `modifyPolicyWhitelist`, etc.) are reverted —
//! policy state is managed on L1, not on the zone.

mod dispatch;

use alloy_primitives::Address;
use revm::precompile::PrecompileError;
use tempo_contracts::precompiles::TIP403_REGISTRY_ADDRESS;
use zone_primitives::policy::AuthRole;

use crate::policy::PolicyCheck;

/// The precompile address — same as the L1 TIP403Registry.
pub const ZONE_TIP403_PROXY_ADDRESS: Address = TIP403_REGISTRY_ADDRESS;

/// Fixed gas cost for authorization checks.
pub const AUTH_CHECK_GAS: u64 = 200;

/// Fixed gas cost for policy data lookups.
const POLICY_DATA_GAS: u64 = 200;

alloy_sol_types::sol! {
    /// Returned when a mutating call is attempted on the read-only zone registry.
    error ReadOnlyRegistry();
}

/// Read-only zone-side proxy that mirrors the L1 TIP-403 registry.
///
/// Unlike the L1 [`TIP403Registry`] (which is a storage-backed `#[contract]`
/// precompile), this proxy has **no on-chain storage**. It intercepts EVM calls
/// at the same address (`0x403C…0000`) and resolves authorization queries via
/// the [`PolicyCheck`] trait.
///
/// All mutating calls (`createPolicy`, `modifyPolicyWhitelist`, etc.) are
/// rejected with `ReadOnlyRegistry` — policy state lives exclusively on L1.
///
/// The struct also exposes [`is_authorized`](Self::is_authorized) and
/// [`is_transfer_authorized`](Self::is_transfer_authorized) for use by the
/// [`ZoneTip20Token`](super::ZoneTip20Token) precompile, which needs the same
/// authorization logic during transfer/mint pre-checks.
#[derive(Debug, Clone)]
pub struct ZoneTip403ProxyRegistry<P> {
    provider: P,
}

impl<P: PolicyCheck> ZoneTip403ProxyRegistry<P> {
    /// Create a new proxy registry backed by the given policy provider.
    pub fn new(provider: P) -> Self {
        Self { provider }
    }

    /// Resolve the `transferPolicyId` for a token.
    pub fn resolve_transfer_policy_id(&self, token: Address) -> Result<u64, PrecompileError> {
        self.provider.resolve_transfer_policy_id(token)
    }

    /// Check whether `user` is authorized under `policy_id` for the given `role`.
    pub fn is_authorized(
        &self,
        policy_id: u64,
        user: Address,
        role: AuthRole,
    ) -> Result<bool, PrecompileError> {
        self.provider.is_authorized(policy_id, user, role)
    }

    /// Check sender + recipient authorization for a transfer.
    ///
    /// Short-circuits on sender failure (matching L1 T2 behavior).
    pub fn is_transfer_authorized(
        &self,
        policy_id: u64,
        from: Address,
        to: Address,
    ) -> Result<bool, PrecompileError> {
        if !self.is_authorized(policy_id, from, AuthRole::Sender)? {
            return Ok(false);
        }
        self.is_authorized(policy_id, to, AuthRole::Recipient)
    }
}
