//! Policy authorization abstraction used by fee-token validation.

use alloy_primitives::Address;
use revm::precompile::PrecompileError;
use zone_primitives::policy::AuthRole;

/// Synchronous policy queries supplied by the zone node.
pub trait PolicyCheck {
    fn is_authorized(
        &self,
        policy_id: u64,
        user: Address,
        role: AuthRole,
    ) -> Result<bool, PrecompileError>;

    fn resolve_transfer_policy_id(&self, token: Address) -> Result<u64, PrecompileError>;

    fn policy_type_sync(
        &self,
        policy_id: u64,
    ) -> Result<tempo_contracts::precompiles::ITIP403Registry::PolicyType, PrecompileError>;

    fn compound_policy_data(&self, policy_id: u64) -> Result<(u64, u64, u64), PrecompileError>;

    fn policy_exists(&self, policy_id: u64) -> Result<bool, PrecompileError>;

    fn policy_id_counter(&self) -> u64;
}
