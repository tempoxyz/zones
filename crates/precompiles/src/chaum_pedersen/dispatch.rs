//! ABI dispatch for the [`ChaumPedersenVerify`] precompile.

use alloy_primitives::Address;
use alloy_sol_types::SolCall;
use revm::precompile::{PrecompileHalt, PrecompileResult};
use tempo_precompiles::{
    Precompile as TempoPrecompile, charge_input_cost, dispatch, storage::StorageCtx,
};
use tracing::debug;

use super::{
    CP_VERIFY_GAS, ChaumPedersenVerify, IChaumPedersenVerify, verify_chaum_pedersen,
    verifyProofCall,
};

impl TempoPrecompile for ChaumPedersenVerify {
    fn call(&mut self, calldata: &[u8], _msg_sender: Address) -> PrecompileResult {
        let mut storage = StorageCtx::default();
        if let Some(err) = charge_input_cost(&mut storage, calldata) {
            return err;
        }

        dispatch!(
            calldata,
            |call| match call {
                IChaumPedersenVerify::IChaumPedersenVerifyCalls {
                    verifyProof(call) => {
                        debug!(target: "zone::precompile", "ChaumPedersenVerify: verifyProof");

                        let mut storage = StorageCtx::default();
                        if storage.deduct_gas(CP_VERIFY_GAS).is_err() {
                            return Ok(storage.halt_output(PrecompileHalt::OutOfGas));
                        }

                        let valid = verify_chaum_pedersen(
                            &call.ephemeralPubX.0,
                            call.ephemeralPubYParity,
                            &call.sharedSecret.0,
                            call.sharedSecretYParity,
                            &call.sequencerPubX.0,
                            call.sequencerPubYParity,
                            &call.proof.s.0,
                            &call.proof.c.0,
                        );

                        let encoded = verifyProofCall::abi_encode_returns(&valid);
                        Ok(storage.success_output(encoded.into()))
                    },
                }
            },
        )
    }
}
