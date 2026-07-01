//! ABI dispatch for the [`ChaumPedersenVerify`] precompile.

use alloy_evm::precompiles::{Precompile, PrecompileInput};
use alloy_sol_types::SolCall;
use revm::precompile::{PrecompileId, PrecompileOutput, PrecompileResult};
use tracing::debug;

use super::{
    CP_PRECOMPILE_ID, CP_VERIFY_GAS, ChaumPedersenVerify, IChaumPedersenVerify,
    verify_chaum_pedersen, verifyProofCall,
};
use crate::dispatch;

impl Precompile for ChaumPedersenVerify {
    fn precompile_id(&self) -> &PrecompileId {
        &CP_PRECOMPILE_ID
    }

    fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult {
        dispatch!(
            input.data,
            input.reservoir,
            |call| match call {
                IChaumPedersenVerify::IChaumPedersenVerifyCalls {
                    verifyProof(call) => {
                        debug!(target: "zone::precompile", "ChaumPedersenVerify: verifyProof");

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
                        Ok(PrecompileOutput::new(
                            CP_VERIFY_GAS,
                            encoded.into(),
                            input.reservoir,
                        ))
                    },
                }
            },
        )
    }
}
