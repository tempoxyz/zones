//! Cache-backed policy adapter retained for fee-token admission and settlement.

use alloy_primitives::Address;
use revm::precompile::PrecompileError;
use zone_primitives::policy::AuthRole;

use crate::policy::PolicyCheck;

#[derive(Debug, Clone)]
pub struct ZoneFeePolicy<P> {
    provider: P,
}

impl<P: PolicyCheck> ZoneFeePolicy<P> {
    pub const fn new(provider: P) -> Self {
        Self { provider }
    }

    pub fn resolve_transfer_policy_id(&self, token: Address) -> Result<u64, PrecompileError> {
        self.provider.resolve_transfer_policy_id(token)
    }

    pub fn is_authorized(
        &self,
        policy_id: u64,
        user: Address,
        role: AuthRole,
    ) -> Result<bool, PrecompileError> {
        self.provider.is_authorized(policy_id, user, role)
    }

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
