//! ABI dispatch for the [`ZoneTokenFactory`] precompile.

use alloy_primitives::Address;
use revm::precompile::PrecompileResult;
use tempo_precompiles::{
    Precompile as TempoPrecompile, charge_input_cost, dispatch, mutate_void, storage::StorageCtx,
};
use zone_primitives::constants::ZONE_INBOX_ADDRESS;

use crate::ZonePrecompileError;

use super::{IZoneTokenFactory, ZoneTokenFactory, ZoneTokenFactoryError};

impl TempoPrecompile for ZoneTokenFactory {
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult {
        if let Some(err) = charge_input_cost(&mut self.storage, calldata) {
            return err;
        }

        if msg_sender != ZONE_INBOX_ADDRESS {
            return StorageCtx.error_result(ZonePrecompileError::from(
                ZoneTokenFactoryError::only_zone_inbox(),
            ));
        }

        dispatch!(
            calldata,
            |call| match call {
                IZoneTokenFactory::IZoneTokenFactoryCalls {
                    enableToken(call) => mutate_void(call, msg_sender, |_sender, call| {
                        self.enable_token(call)
                    }),
                }
            },
        )
    }
}
