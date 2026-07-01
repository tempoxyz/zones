//! ABI dispatch for the [`AesGcmDecrypt`] precompile.

use alloy_evm::precompiles::{Precompile, PrecompileInput};
use alloy_primitives::Bytes;
use alloy_sol_types::{SolCall, SolInterface};
use revm::precompile::{PrecompileId, PrecompileOutput, PrecompileResult};
use tracing::debug;

use super::{
    AES_GCM_BASE_GAS, AES_GCM_PER_BYTE_GAS, AES_GCM_PRECOMPILE_ID, AesGcmDecrypt, IAesGcmDecrypt,
    decrypt_aes_gcm, decryptCall, decryptReturn,
};

impl Precompile for AesGcmDecrypt {
    fn precompile_id(&self) -> &PrecompileId {
        &AES_GCM_PRECOMPILE_ID
    }

    fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult {
        let call = match IAesGcmDecrypt::IAesGcmDecryptCalls::abi_decode(input.data) {
            Ok(IAesGcmDecrypt::IAesGcmDecryptCalls::decrypt(call)) => call,
            Err(_) => return Ok(PrecompileOutput::revert(0, Bytes::new(), input.reservoir)),
        };

        debug!(target: "zone::precompile", "AesGcmDecrypt: decrypt");

        let gas = AES_GCM_BASE_GAS
            + AES_GCM_PER_BYTE_GAS * (call.ciphertext.len() + call.aad.len()) as u64;

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
        Ok(PrecompileOutput::new(gas, encoded.into(), input.reservoir))
    }
}
