//! Zone-side TIP20 factory precompile.
//!
//! Deployed at the same address as the L1 `TIP20Factory` (`0x20FC…0000`), this
//! precompile replaces the standard factory on the zone with a single
//! `enableToken(address, string, string, string)` entrypoint.
//!
//! When the sequencer bridges a new TIP-20 token to the zone, the
//! `ZoneInbox` contract calls `enableToken` during `advanceTempo` to:
//!
//! 1. Initialize the TIP-20 storage at the given address (name, symbol, currency).
//! 2. Grant [`ISSUER_ROLE`] to both [`ZONE_INBOX_ADDRESS`] (for minting on
//!    deposits) and [`ZONE_OUTBOX_ADDRESS`] (for burning on withdrawals).
//!
//! Only [`ZONE_INBOX_ADDRESS`] may call this precompile; all other callers are
//! reverted with `OnlyZoneInbox()`.

mod dispatch;

pub use IZoneTokenFactory::IZoneTokenFactoryErrors as ZoneTokenFactoryError;
use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::Address;
use tempo_precompiles::{
    PATH_USD_ADDRESS, Precompile as _, TIP20_FACTORY_ADDRESS, TIP403_REGISTRY_ADDRESS,
    storage::StorageKey as _,
    tip20::{ISSUER_ROLE, TIP20Token},
    tip403_registry::tip403_registry_slots,
};
use tempo_precompiles_macros::contract;
use zone_primitives::constants::{ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS};

alloy_sol_types::sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IZoneTokenFactory {
        /// Initialize a TIP20 token on the zone and grant issuer roles.
        function enableToken(address token, string name, string symbol, string currency) external;

        error OnlyZoneInbox();
    }
}

pub use IZoneTokenFactory::enableTokenCall;

/// Zone-specific TIP20 factory precompile address (same as the standard factory).
pub const ZONE_TIP20_FACTORY_ADDRESS: Address = TIP20_FACTORY_ADDRESS;

/// Zone-side TIP20 factory precompile.
///
/// Replaces the L1 [`TIP20Factory`] at the same address (`0x20FC…0000`) with a
/// zone-specific implementation that only supports [`enableToken`](enableTokenCall).
/// This is called by [`ZoneInbox`] during `advanceTempo` to create matching
/// TIP-20 tokens for assets bridged from L1.
#[contract(addr = TIP20_FACTORY_ADDRESS)]
pub struct ZoneTokenFactory {}

impl ZoneTokenFactory {
    /// Creates the direct-call-only token factory with zone-local storage and execution.
    pub fn create(env: &crate::ZonePrecompileEnv) -> DynPrecompile {
        crate::execution::create_precompile(
            "ZoneTokenFactory",
            env,
            crate::execution::NoCallRules,
            |data, caller| Self::new().call(data, caller),
        )
    }

    /// Sets the contract bytecode (`0xef`) so the account is non-empty.
    ///
    /// Must be called once during genesis generation before any tokens are
    /// created. Without this, Solidity's `EXTCODESIZE` guard would cause
    /// calls to this address to revert.
    pub fn initialize(&mut self) -> tempo_precompiles::Result<()> {
        self.__initialize()
    }

    /// Initialize a TIP-20 token on the zone for a newly bridged L1 asset.
    ///
    /// Creates the token's storage (name, symbol, currency) at `call.token` and
    /// grants [`ISSUER_ROLE`] to:
    /// - [`ZONE_INBOX_ADDRESS`] — so deposits can mint zone-side tokens.
    /// - [`ZONE_OUTBOX_ADDRESS`] — so withdrawals can burn zone-side tokens.
    ///
    /// The quote token is always set to [`PATH_USD_ADDRESS`].
    pub fn enable_token(&mut self, call: enableTokenCall) -> tempo_precompiles::Result<()> {
        // Upstream initialization writes the default policy ID into the L1-mirrored
        // TIP-403 registry. Cache the anchored value so initialization cannot override it.
        let binding_slot = call
            .token
            .mapping_slot(tip403_registry_slots::TOKEN_TRANSFER_POLICIES);
        let l1_policy = self.storage.sload(TIP403_REGISTRY_ADDRESS, binding_slot)?;

        let mut token = TIP20Token::from_address(call.token)?;
        token.initialize(
            ZONE_INBOX_ADDRESS,
            &call.name,
            &call.symbol,
            &call.currency,
            PATH_USD_ADDRESS,
            ZONE_INBOX_ADDRESS,
        )?;

        // The complete registry word is L1-owned, including both the policy ID and
        // isSet bit. Restore it so the zone never persists a registry transition.
        self.storage
            .sstore(TIP403_REGISTRY_ADDRESS, binding_slot, l1_policy)?;

        token.grant_role_internal(ZONE_INBOX_ADDRESS, *ISSUER_ROLE)?;
        token.grant_role_internal(ZONE_OUTBOX_ADDRESS, *ISSUER_ROLE)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{U256, address};
    use tempo_chainspec::hardfork::TempoHardfork;
    use tempo_precompiles::storage::{StorageCtx, hashmap::HashMapStorageProvider};

    #[test]
    fn initialization_preserves_registry_binding() -> eyre::Result<()> {
        let token_address = address!("20C0000000000000000000000000000000000999");
        let binding_slot =
            token_address.mapping_slot(tip403_registry_slots::TOKEN_TRANSFER_POLICIES);

        for anchored_policy in [U256::ZERO, U256::from(7) | (U256::ONE << 64)] {
            let mut provider = HashMapStorageProvider::new_with_spec(1, TempoHardfork::T9);

            StorageCtx::enter(&mut provider, || -> eyre::Result<()> {
                StorageCtx.sstore(TIP403_REGISTRY_ADDRESS, binding_slot, anchored_policy)?;

                ZoneTokenFactory::new().enable_token(enableTokenCall {
                    token: token_address,
                    name: "Zone Token".to_owned(),
                    symbol: "ZONE".to_owned(),
                    currency: "USD".to_owned(),
                })?;

                assert_eq!(
                    StorageCtx.sload(TIP403_REGISTRY_ADDRESS, binding_slot)?,
                    anchored_policy
                );
                let token = TIP20Token::from_address(token_address)?;
                assert_eq!(token.next_quote_token()?, PATH_USD_ADDRESS);
                Ok(())
            })?;
        }

        Ok(())
    }
}
