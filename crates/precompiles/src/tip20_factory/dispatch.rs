//! ABI dispatch for the [`ZoneTokenFactory`] precompile.

use alloy_primitives::Address;
use revm::precompile::PrecompileResult;
use tempo_precompiles::{
    Precompile as TempoPrecompile, charge_input_cost, dispatch, mutate_void, storage::StorageCtx,
};

use crate::ZonePrecompileError;
use zone_primitives::constants::ZONE_INBOX_ADDRESS;

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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_sol_types::SolInterface;
    use tempo_chainspec::hardfork::TempoHardfork;
    use tempo_precompiles::storage::hashmap::HashMapStorageProvider;

    #[test]
    fn unauthorized_call_output_is_unchanged() {
        let mut storage = HashMapStorageProvider::new_with_spec(1, TempoHardfork::T1);
        StorageCtx::enter(&mut storage, || {
            let actual = ZoneTokenFactory::new().call(&[], Address::ZERO).unwrap();
            let expected = StorageCtx
                .revert_output(ZoneTokenFactoryError::only_zone_inbox().abi_encode().into());

            assert_eq!(actual, expected);
        });
    }
}
