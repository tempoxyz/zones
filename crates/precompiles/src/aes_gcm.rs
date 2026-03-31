//! AES-256-GCM decryption precompile.
//!
//! Registered at [`AES_GCM_DECRYPT_ADDRESS`] (`0x1C00...0101`).
//!
//! Decrypts ECIES ciphertext and verifies the GCM authentication tag,
//! enabling the [`ZoneInbox`] contract to process encrypted deposits.
//!
//! Uses the NCC-audited [`aes-gcm`] crate (v0.10.3).

use alloc::{borrow::Cow, vec::Vec};

use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use alloy_evm::precompiles::{DynPrecompile, Precompile, PrecompileInput};
use alloy_primitives::{Address, Bytes, address};
use alloy_sol_types::SolCall;
use revm::precompile::{PrecompileError, PrecompileId, PrecompileOutput, PrecompileResult};
use tracing::{debug, warn};

/// AES-256-GCM Decrypt precompile address on Zone L2.
pub const AES_GCM_DECRYPT_ADDRESS: Address = address!("0x1C00000000000000000000000000000000000101");

/// Base gas cost for AES-GCM decryption.
const AES_GCM_BASE_GAS: u64 = 1_000;

/// Additional gas per byte of ciphertext.
const AES_GCM_PER_BYTE_GAS: u64 = 3;

/// Precompile identifier.
static AES_GCM_PRECOMPILE_ID: PrecompileId = PrecompileId::Custom(Cow::Borrowed("AesGcmDecrypt"));

alloy_sol_types::sol! {
    /// Decrypt AES-256-GCM ciphertext and verify authentication tag.
    function decrypt(
        bytes32 key,
        bytes12 nonce,
        bytes ciphertext,
        bytes aad,
        bytes16 tag
    ) external view returns (bytes plaintext, bool valid);
}

/// AES-256-GCM decryption precompile.
///
/// Decrypts ciphertext using the provided key, nonce, and AAD, and verifies
/// the GCM authentication tag. Returns `(plaintext, true)` on success or
/// `(empty, false)` if tag verification fails.
pub struct AesGcmDecrypt;

impl Precompile for AesGcmDecrypt {
    fn precompile_id(&self) -> &PrecompileId {
        &AES_GCM_PRECOMPILE_ID
    }

    fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult {
        let data = input.data;
        if data.len() < 4 {
            return Ok(PrecompileOutput::new_reverted(0, Bytes::new()));
        }

        let selector: [u8; 4] = data[..4].try_into().expect("len >= 4");
        if selector != decryptCall::SELECTOR {
            warn!(target: "zone::precompile", ?selector, "AesGcmDecrypt: unknown selector");
            return Ok(PrecompileOutput::new_reverted(0, Bytes::new()));
        }

        debug!(target: "zone::precompile", "AesGcmDecrypt: decrypt");

        let call = decryptCall::abi_decode(data)
            .map_err(|_| PrecompileError::other("ABI decode failed"))?;

        let gas = AES_GCM_BASE_GAS + AES_GCM_PER_BYTE_GAS * call.ciphertext.len() as u64;

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
        Ok(PrecompileOutput::new(gas, encoded.into()))
    }
}

impl AesGcmDecrypt {
    /// Convert into a [`DynPrecompile`] for registration in a [`PrecompilesMap`].
    pub fn into_dyn(self) -> DynPrecompile {
        DynPrecompile::new(PrecompileId::Custom("AesGcmDecrypt".into()), |input| {
            Self.call(input)
        })
    }
}

impl From<AesGcmDecrypt> for DynPrecompile {
    fn from(value: AesGcmDecrypt) -> Self {
        value.into_dyn()
    }
}

/// Decrypt AES-256-GCM ciphertext with tag verification.
///
/// The ciphertext, AAD, and tag are passed separately (matching the Solidity interface).
/// Returns `(plaintext, true)` on success, or `(empty, false)` on failure.
pub fn decrypt_aes_gcm(
    key: &[u8; 32],
    nonce: &[u8; 12],
    ciphertext: &[u8],
    aad: &[u8],
    tag: &[u8; 16],
) -> (Vec<u8>, bool) {
    let cipher = Aes256Gcm::new(key.into());
    let gcm_nonce = Nonce::from_slice(nonce);

    // AES-GCM expects ciphertext || tag concatenated
    let mut ct_with_tag = Vec::with_capacity(ciphertext.len() + 16);
    ct_with_tag.extend_from_slice(ciphertext);
    ct_with_tag.extend_from_slice(tag);

    match cipher.decrypt(
        gcm_nonce,
        Payload {
            msg: &ct_with_tag,
            aad,
        },
    ) {
        Ok(plaintext) => (plaintext, true),
        Err(_) => (Vec::new(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_aes_gcm_roundtrip() {
        let key = [0x42u8; 32];
        let nonce_bytes = [0x01u8; 12];
        let plaintext = b"hello world test";

        let cipher = Aes256Gcm::new((&key).into());
        let nonce = Nonce::from_slice(&nonce_bytes);
        let encrypted = cipher.encrypt(nonce, plaintext.as_ref()).expect("encrypt");

        let ct = &encrypted[..encrypted.len() - 16];
        let tag: [u8; 16] = encrypted[encrypted.len() - 16..].try_into().unwrap();

        let (decrypted, valid) = decrypt_aes_gcm(&key, &nonce_bytes, ct, &[], &tag);
        assert!(valid);
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_aes_gcm_bad_tag() {
        let key = [0x42u8; 32];
        let nonce_bytes = [0x01u8; 12];
        let plaintext = b"hello";

        let cipher = Aes256Gcm::new((&key).into());
        let nonce = Nonce::from_slice(&nonce_bytes);
        let encrypted = cipher.encrypt(nonce, plaintext.as_ref()).expect("encrypt");

        let ct = &encrypted[..encrypted.len() - 16];
        let bad_tag = [0xFFu8; 16];

        let (decrypted, valid) = decrypt_aes_gcm(&key, &nonce_bytes, ct, &[], &bad_tag);
        assert!(!valid);
        assert!(decrypted.is_empty());
    }

    #[test]
    fn test_aes_gcm_with_aad() {
        let key = [0x42u8; 32];
        let nonce_bytes = [0x01u8; 12];
        let plaintext = b"hello world test";
        let aad = b"zone-inbox-v1";

        let cipher = Aes256Gcm::new((&key).into());
        let nonce = Nonce::from_slice(&nonce_bytes);
        let encrypted = cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext.as_ref(),
                    aad: aad.as_ref(),
                },
            )
            .expect("encrypt");

        let ct = &encrypted[..encrypted.len() - 16];
        let tag: [u8; 16] = encrypted[encrypted.len() - 16..].try_into().unwrap();

        let (decrypted, valid) = decrypt_aes_gcm(&key, &nonce_bytes, ct, aad, &tag);
        assert!(valid);
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_aes_gcm_wrong_aad() {
        let key = [0x42u8; 32];
        let nonce_bytes = [0x01u8; 12];
        let plaintext = b"secret data";

        let cipher = Aes256Gcm::new((&key).into());
        let nonce = Nonce::from_slice(&nonce_bytes);
        let encrypted = cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext.as_ref(),
                    aad: b"correct",
                },
            )
            .expect("encrypt");

        let ct = &encrypted[..encrypted.len() - 16];
        let tag: [u8; 16] = encrypted[encrypted.len() - 16..].try_into().unwrap();

        let (decrypted, valid) = decrypt_aes_gcm(&key, &nonce_bytes, ct, b"wrong", &tag);
        assert!(!valid);
        assert!(decrypted.is_empty());
    }

    #[test]
    fn test_aes_gcm_missing_aad() {
        let key = [0x42u8; 32];
        let nonce_bytes = [0x01u8; 12];
        let plaintext = b"secret data";
        let aad = b"zone-inbox-v1";

        let cipher = Aes256Gcm::new((&key).into());
        let nonce = Nonce::from_slice(&nonce_bytes);
        let encrypted = cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext.as_ref(),
                    aad: aad.as_ref(),
                },
            )
            .expect("encrypt");

        let ct = &encrypted[..encrypted.len() - 16];
        let tag: [u8; 16] = encrypted[encrypted.len() - 16..].try_into().unwrap();

        let (decrypted, valid) = decrypt_aes_gcm(&key, &nonce_bytes, ct, &[], &tag);
        assert!(!valid);
        assert!(decrypted.is_empty());
    }

    #[test]
    fn test_aes_gcm_flipped_ciphertext_bit() {
        let key = [0x42u8; 32];
        let nonce_bytes = [0x01u8; 12];
        let plaintext = b"hello world test";

        let cipher = Aes256Gcm::new((&key).into());
        let nonce = Nonce::from_slice(&nonce_bytes);
        let encrypted = cipher.encrypt(nonce, plaintext.as_ref()).expect("encrypt");

        let mut ct = encrypted[..encrypted.len() - 16].to_vec();
        let tag: [u8; 16] = encrypted[encrypted.len() - 16..].try_into().unwrap();

        ct[0] ^= 0x01;

        let (decrypted, valid) = decrypt_aes_gcm(&key, &nonce_bytes, &ct, &[], &tag);
        assert!(!valid);
        assert!(decrypted.is_empty());
    }

    #[test]
    fn test_aes_gcm_empty_plaintext() {
        let key = [0x42u8; 32];
        let nonce_bytes = [0x01u8; 12];
        let plaintext = b"";

        let cipher = Aes256Gcm::new((&key).into());
        let nonce = Nonce::from_slice(&nonce_bytes);
        let encrypted = cipher.encrypt(nonce, plaintext.as_ref()).expect("encrypt");

        let ct = &encrypted[..encrypted.len() - 16];
        let tag: [u8; 16] = encrypted[encrypted.len() - 16..].try_into().unwrap();

        let (decrypted, valid) = decrypt_aes_gcm(&key, &nonce_bytes, ct, &[], &tag);
        assert!(valid);
        assert!(decrypted.is_empty());
    }
}
