//! ABI dispatch for the [`AesGcmDecrypt`] precompile.

use alloy_primitives::{Address, Bytes};
use alloy_sol_types::SolCall;
use revm::precompile::{PrecompileHalt, PrecompileResult};
use tempo_precompiles::{
    Precompile as TempoPrecompile, charge_input_cost, dispatch, storage::StorageCtx,
};
use tracing::debug;

use super::{
    AES_GCM_BASE_GAS, AES_GCM_PER_BYTE_GAS, AesGcmDecrypt, IAesGcmDecrypt, decrypt_aes_gcm,
    decryptCall, decryptReturn,
};

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

                        let gas = AES_GCM_BASE_GAS
                            + AES_GCM_PER_BYTE_GAS
                                * (call.ciphertext.len() + call.aad.len()) as u64;
                        let mut storage = StorageCtx::default();
                        if storage.deduct_gas(gas).is_err() {
                            return Ok(storage.halt_output(PrecompileHalt::OutOfGas));
                        }

                        let (plaintext, valid) = decrypt_aes_gcm(
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
