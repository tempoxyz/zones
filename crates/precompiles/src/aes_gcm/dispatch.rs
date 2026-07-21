//! ABI dispatch for the [`AesGcmDecrypt`] precompile.

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolCall;
use revm::precompile::PrecompileResult;
use tempo_precompiles::{
    Precompile as TempoPrecompile, charge_input_cost, dispatch, storage::StorageCtx,
};
use tracing::debug;

use super::{AesGcmDecrypt, IAesGcmDecrypt, decryptCall, decryptReturn};

impl TempoPrecompile for AesGcmDecrypt {
    fn call(&mut self, calldata: &[u8], _msg_sender: Address) -> PrecompileResult {
        let mut storage = StorageCtx::default();
        if let Some(err) = charge_input_cost(&mut storage, calldata) {
            return err;
        }

        dispatch!(
            calldata,
            |call| match call {
                IAesGcmDecrypt::IAesGcmDecryptCalls {
                    decrypt(call) => {
                        debug!(target: "zone::precompile", "AesGcmDecrypt: decrypt");

                        let storage = StorageCtx::default();
                        if let Err(error) =
                            Self::charge_gas(call.ciphertext.len(), call.aad.len())
                        {
                            return storage.error_result(error);
                        }

                        let (plaintext, valid) = Self::decrypt(
                            &call.key.0,
                            &call.nonce.0,
                            &call.ciphertext,
                            &call.aad,
                            &call.tag.0,
                        );

                        let ret = decryptReturn {
                            plaintext: Bytes::from(plaintext),
                            valid,
                        };
                        let encoded = decryptCall::abi_encode_returns(&ret);
                        Ok(storage.success_output(encoded.into()))
                    },
                }
            },
        )
    }
}
