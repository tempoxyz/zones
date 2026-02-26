//! Zone-specific TIP20 factory precompile.
//!
//! Wraps the standard [`TIP20Factory`] precompile and adds an
//! `enableToken(address, string, string, string)` function that initializes a
//! TIP20 token and grants [`ISSUER_ROLE`] to the zone inbox and outbox contracts.
//!
//! Only callable by [`ZONE_INBOX_ADDRESS`].

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::{SolCall, SolError};
use revm::precompile::{PrecompileId, PrecompileOutput};
use tempo_precompiles::{
    PATH_USD_ADDRESS, Precompile as TempoPrecompile, TIP20_FACTORY_ADDRESS, input_cost,
    storage::{StorageCtx, evm::EvmPrecompileStorageProvider},
    tip20::{ISSUER_ROLE, TIP20Token},
    tip20_factory::TIP20Factory,
};

use alloy_evm::precompiles::DynPrecompile;

use crate::abi::{ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS};

alloy_sol_types::sol! {
    /// Initialize a TIP20 token on the zone and grant issuer roles.
    function enableToken(address token, string name, string symbol, string currency) external;

    error OnlyZoneInbox();
}

/// Zone-specific TIP20 factory precompile address (same as the standard factory).
pub const ZONE_TIP20_FACTORY_ADDRESS: Address = TIP20_FACTORY_ADDRESS;

/// Create the zone TIP20 factory [`DynPrecompile`].
pub fn zone_tip20_factory(
    cfg: &revm::context::CfgEnv<tempo_chainspec::hardfork::TempoHardfork>,
) -> DynPrecompile {
    let spec = cfg.spec;
    let gas_params = cfg.gas_params.clone();
    DynPrecompile::new_stateful(
        PrecompileId::Custom("ZoneTIP20Factory".into()),
        move |input| {
            if !input.is_direct_call() {
                return Ok(PrecompileOutput::new_reverted(
                    0,
                    tempo_precompiles::DelegateCallNotAllowed {}
                        .abi_encode()
                        .into(),
                ));
            }

            let mut storage = EvmPrecompileStorageProvider::new(
                input.internals,
                input.gas,
                spec,
                input.is_static,
                gas_params.clone(),
            );

            StorageCtx::enter(&mut storage, || {
                let data = input.data;
                let msg_sender = input.caller;

                if data.len() >= 4 && data[..4] == enableTokenCall::SELECTOR {
                    let call = enableTokenCall::abi_decode(data).map_err(|_| {
                        revm::precompile::PrecompileError::other("ABI decode failed")
                    })?;

                    if msg_sender != ZONE_INBOX_ADDRESS {
                        return Ok(PrecompileOutput::new_reverted(
                            0,
                            OnlyZoneInbox {}.abi_encode().into(),
                        ));
                    }

                    let gas = input_cost(data.len());

                    let mut token = TIP20Token::from_address(call.token)
                        .map_err(|e| revm::precompile::PrecompileError::other(format!("{e}")))?;
                    token
                        .initialize(
                            msg_sender,
                            &call.name,
                            &call.symbol,
                            &call.currency,
                            PATH_USD_ADDRESS,
                            msg_sender,
                        )
                        .map_err(|e| revm::precompile::PrecompileError::other(format!("{e}")))?;
                    token
                        .grant_role_internal(ZONE_INBOX_ADDRESS, *ISSUER_ROLE)
                        .map_err(|e| revm::precompile::PrecompileError::other(format!("{e}")))?;
                    token
                        .grant_role_internal(ZONE_OUTBOX_ADDRESS, *ISSUER_ROLE)
                        .map_err(|e| revm::precompile::PrecompileError::other(format!("{e}")))?;

                    Ok(PrecompileOutput::new(gas, Bytes::new()))
                } else {
                    TIP20Factory::new().call(data, msg_sender)
                }
            })
        },
    )
}
