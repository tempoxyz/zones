use alloy_consensus::crypto::secp256k1::recover_signer_unchecked;
use alloy_primitives::{Address, B256, Signature};

use super::token::AuthError;

/// Recover the signer address from a secp256k1 signature over the given digest.
///
/// The signature must be exactly 65 bytes: `r (32) || s (32) || v (1)`.
pub fn recover_secp256k1(signature: &[u8], digest: &B256) -> Result<Address, AuthError> {
    if signature.len() != 65 {
        return Err(AuthError::InvalidSignature);
    }

    let sig =
        Signature::try_from(&signature[..65]).map_err(|_| AuthError::InvalidSignature)?;

    recover_signer_unchecked(&sig, *digest).map_err(|_| AuthError::InvalidSignature)
}
