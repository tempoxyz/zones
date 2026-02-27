//! Zone-side TIP20 factory precompile.
//!
//! Deployed at the same address as the L1 [`TIP20Factory`] (`0x20FC…0000`), this
//! precompile replaces the standard factory on the zone with a single
//! `enableToken(address, string, string, string)` entrypoint.
//!
//! When the sequencer bridges a new TIP-20 token to the zone, the
//! [`ZoneInbox`](crate::abi::ZoneInbox) contract calls `enableToken` during
//! `advanceTempo` to:
//!
//! 1. Initialize the TIP-20 storage at the given address (name, symbol, currency).
//! 2. Grant [`ISSUER_ROLE`] to both [`ZONE_INBOX_ADDRESS`] (for minting on
//!    deposits) and [`ZONE_OUTBOX_ADDRESS`] (for burning on withdrawals).
//!
//! Only [`ZONE_INBOX_ADDRESS`] may call this precompile; all other callers are
//! reverted with `OnlyZoneInbox()`.

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::{PrecompileError, PrecompileOutput, PrecompileResult};
use tempo_precompiles::{
    PATH_USD_ADDRESS, Precompile as TempoPrecompile, TIP20_FACTORY_ADDRESS,
    tip20::{ISSUER_ROLE, TIP20Token},
};
use tempo_precompiles_macros::contract;

use crate::abi::{ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS};

alloy_sol_types::sol! {
    /// Initialize a TIP20 token on the zone and grant issuer roles.
    function enableToken(address token, string name, string symbol, string currency) external;

    error OnlyZoneInbox();
}

/// Zone-specific TIP20 factory precompile address (same as the standard factory).
pub const ZONE_TIP20_FACTORY_ADDRESS: Address = TIP20_FACTORY_ADDRESS;

/// Zone-side TIP20 factory precompile.
///
/// Replaces the L1 [`TIP20Factory`] at the same address (`0x20FC…0000`) with a
/// zone-specific implementation that only supports [`enableToken`](enableTokenCall).
/// This is called by [`ZoneInbox`](crate::abi::ZoneInbox) during `advanceTempo`
/// to create matching TIP-20 tokens for assets bridged from L1.
#[contract(addr = TIP20_FACTORY_ADDRESS)]
pub struct ZoneTokenFactory {}

impl ZoneTokenFactory {
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
    pub fn enable_token(&self, call: enableTokenCall) -> tempo_precompiles::Result<()> {
        let mut token = TIP20Token::from_address(call.token)?;
        token.initialize(
            ZONE_INBOX_ADDRESS,
            &call.name,
            &call.symbol,
            &call.currency,
            PATH_USD_ADDRESS,
            ZONE_INBOX_ADDRESS,
        )?;
        token.grant_role_internal(ZONE_INBOX_ADDRESS, *ISSUER_ROLE)?;
        token.grant_role_internal(ZONE_OUTBOX_ADDRESS, *ISSUER_ROLE)?;

        Ok(())
    }

    /// Wraps this precompile in a [`DynPrecompile`] for registration in the zone EVM.
    ///
    /// The returned precompile handles delegate-call rejection, EVM storage
    /// context setup, and dispatches to [`TempoPrecompile::call`].
    pub fn create(
        cfg: &revm::context::CfgEnv<tempo_chainspec::hardfork::TempoHardfork>,
    ) -> alloy_evm::precompiles::DynPrecompile {
        use revm::precompile::{PrecompileId, PrecompileOutput};
        use tempo_precompiles::{
            DelegateCallNotAllowed, Precompile as _,
            storage::{StorageCtx, evm::EvmPrecompileStorageProvider},
        };

        let spec = cfg.spec;
        let gas_params = cfg.gas_params.clone();
        alloy_evm::precompiles::DynPrecompile::new_stateful(
            PrecompileId::Custom("ZoneTokenFactory".into()),
            move |input| {
                if !input.is_direct_call() {
                    return Ok(PrecompileOutput::new_reverted(
                        0,
                        SolError::abi_encode(&DelegateCallNotAllowed {}).into(),
                    ));
                }

                let mut storage = EvmPrecompileStorageProvider::new(
                    input.internals,
                    input.gas,
                    spec,
                    input.is_static,
                    gas_params.clone(),
                );

                StorageCtx::enter(&mut storage, || Self::new().call(input.data, input.caller))
            },
        )
    }
}

impl TempoPrecompile for ZoneTokenFactory {
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult {
        if msg_sender != ZONE_INBOX_ADDRESS {
            return Ok(PrecompileOutput::new_reverted(
                0,
                OnlyZoneInbox {}.abi_encode().into(),
            ));
        }

        let call = enableTokenCall::abi_decode(calldata)
            .map_err(|_| PrecompileError::other("ABI decode failed"))?;

        self.enable_token(call)
            .map_err(|e| PrecompileError::other(format!("{e}")))?;

        Ok(PrecompileOutput::new(0, Bytes::new()))
    }
}
