//! Typed read-only access to the configured L1 `ZonePortal` through precompile storage handlers.
//!
//! The Zone EVM database mirrors the configured portal account from L1, so these ordinary
//! handlers resolve reads at the execution-local Tempo anchor without knowing about the L1
//! provider or computing storage slots manually.

use alloy_primitives::Address;
use tempo_precompiles::{
    Result,
    storage::{Handler, Mapping},
    zone_factory::{ZonePortalStorage, zone_portal_slots},
};
use tempo_precompiles_macros::Storable;

/// Packed representation of the portal's token configuration mapping value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Storable)]
struct PortalTokenConfig {
    enabled: bool,
    deposits_active: bool,
}

/// Read-only semantic facade over the canonical `ZonePortal` storage layout.
pub(crate) struct L1Portal {
    storage: ZonePortalStorage,
    token_configs: Mapping<Address, PortalTokenConfig>,
}

impl L1Portal {
    pub(crate) fn new(address: Address) -> Self {
        Self {
            storage: ZonePortalStorage::new(address),
            token_configs: Mapping::new(zone_portal_slots::TOKEN_CONFIGS, address),
        }
    }

    pub(crate) fn is_sequencer(&self, account: Address) -> Result<bool> {
        self.storage.is_sequencer[account].read()
    }

    pub(crate) fn is_token_enabled(&self, token: Address) -> Result<bool> {
        Ok(self.token_configs[token].read()?.enabled)
    }

    pub(crate) fn enforcement_modes(&self) -> Result<(bool, bool)> {
        Ok((
            self.storage.is_access_enforced.read()?,
            self.storage.is_gateway_enforced.read()?,
        ))
    }

    pub(crate) fn role(&self, account: Address) -> Result<u8> {
        self.storage.role[account].read()
    }

    #[cfg(test)]
    pub(crate) fn set_sequencer(&mut self, account: Address, enabled: bool) -> Result<()> {
        self.storage.is_sequencer[account].write(enabled)
    }

    #[cfg(test)]
    pub(crate) fn set_token_config(
        &mut self,
        token: Address,
        enabled: bool,
        deposits_active: bool,
    ) -> Result<()> {
        self.token_configs[token].write(PortalTokenConfig {
            enabled,
            deposits_active,
        })
    }

    #[cfg(test)]
    pub(crate) fn set_enforcement_modes(
        &mut self,
        access_enforced: bool,
        gateway_enforced: bool,
    ) -> Result<()> {
        self.storage.is_access_enforced.write(access_enforced)?;
        self.storage.is_gateway_enforced.write(gateway_enforced)
    }

    #[cfg(test)]
    pub(crate) fn set_role(&mut self, account: Address, role: u8) -> Result<()> {
        self.storage.role[account].write(role)
    }
}
