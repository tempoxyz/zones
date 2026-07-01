//! AES-256-GCM decryption precompile.
//!
//! Registered at [`AES_GCM_DECRYPT_ADDRESS`] (`0x1C00...0101`).
//!
//! Decrypts ECIES ciphertext and verifies the GCM authentication tag,
//! enabling the [`ZoneInbox`] contract to process encrypted deposits.
//!
//! Uses the NCC-audited [`aes-gcm`] crate (v0.10.3).

use alloc::vec::Vec;

use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
mod dispatch;

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::{Address, address};
use revm::precompile::PrecompileId;

/// AES-256-GCM Decrypt precompile address on Zone L2.
pub const AES_GCM_DECRYPT_ADDRESS: Address = address!("0x1C00000000000000000000000000000000000101");

/// Base gas cost for AES-GCM decryption.
const AES_GCM_BASE_GAS: u64 = 1_000;

/// Additional gas per byte of authenticated AES-GCM input.
const AES_GCM_PER_BYTE_GAS: u64 = 3;

alloy_sol_types::sol! {
    interface IAesGcmDecrypt {
        /// Decrypt AES-256-GCM ciphertext and verify authentication tag.
        function decrypt(
            bytes32 key,
            bytes12 nonce,
            bytes ciphertext,
            bytes aad,
            bytes16 tag
        ) external view returns (bytes plaintext, bool valid);
    }
}

pub use IAesGcmDecrypt::{decryptCall, decryptReturn};

/// AES-256-GCM decryption precompile.
///
/// Decrypts ciphertext using the provided key, nonce, and AAD, and verifies
/// the GCM authentication tag. Returns `(plaintext, true)` on success or
/// `(empty, false)` if tag verification fails.
pub struct AesGcmDecrypt;

impl AesGcmDecrypt {
    /// Wrap this precompile in a [`DynPrecompile`] with the Tempo storage context
    /// required by the upstream dispatch macro.
    pub fn create(
        cfg: &revm::context::CfgEnv<tempo_chainspec::hardfork::TempoHardfork>,
    ) -> DynPrecompile {
        use tempo_precompiles::{
            Precompile as _,
            storage::{StorageCtx, evm::EvmPrecompileStorageProvider},
        };

        let spec = cfg.spec;
        let amsterdam_eip8037_enabled = cfg.enable_amsterdam_eip8037;
        let gas_params = cfg.gas_params.clone();
        DynPrecompile::new_stateful(PrecompileId::Custom("AesGcmDecrypt".into()), move |input| {
            let mut storage = EvmPrecompileStorageProvider::new(
                input.internals,
                input.gas,
                input.reservoir,
                spec,
                amsterdam_eip8037_enabled,
                input.is_static,
                gas_params.clone(),
            );

            StorageCtx::enter(&mut storage, || {
                let mut precompile = Self;
                precompile.call(input.data, input.caller)
            })
        })
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
    use alloy_evm::{
        EvmInternals,
        precompiles::{Precompile, PrecompileInput},
    };
    use alloy_primitives::{Bytes, U256};
    use alloy_sol_types::SolCall;
    use revm::{
        Context,
        database::{CacheDB, EmptyDB},
        precompile::PrecompileOutput,
    };
    use tempo_chainspec::hardfork::TempoHardfork;
    use tempo_precompiles::{
        charge_input_cost,
        storage::{StorageCtx, evm::EvmPrecompileStorageProvider},
    };

    type TestContext = Context<
        revm::context::BlockEnv,
        revm::context::TxEnv,
        revm::context::CfgEnv<TempoHardfork>,
        CacheDB<EmptyDB>,
    >;

    fn test_context() -> TestContext {
        Context::new(CacheDB::new(EmptyDB::new()), TempoHardfork::default())
    }

    fn encrypt(plaintext: &[u8], aad: &[u8]) -> decryptCall {
        let key = [0x42u8; 32];
        let nonce_bytes = [0x01u8; 12];
        let cipher = Aes256Gcm::new((&key).into());
        let nonce = Nonce::from_slice(&nonce_bytes);
        let encrypted = cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext,
                    aad,
                },
            )
            .expect("encrypt");
        let ct = &encrypted[..encrypted.len() - 16];
        let tag: [u8; 16] = encrypted[encrypted.len() - 16..].try_into().unwrap();

        decryptCall {
            key: key.into(),
            nonce: nonce_bytes.into(),
            ciphertext: Bytes::copy_from_slice(ct),
            aad: Bytes::copy_from_slice(aad),
            tag: tag.into(),
        }
    }

    fn call_precompile(calldata: Bytes) -> PrecompileOutput {
        let mut ctx = test_context();
        let cfg = revm::context::CfgEnv::<TempoHardfork>::default();
        AesGcmDecrypt::create(&cfg)
            .call(PrecompileInput {
                data: &calldata,
                gas: u64::MAX,
                reservoir: 0,
                caller: Address::ZERO,
                value: U256::ZERO,
                target_address: AES_GCM_DECRYPT_ADDRESS,
                is_static: true,
                bytecode_address: AES_GCM_DECRYPT_ADDRESS,
                internals: EvmInternals::from_context(&mut ctx),
            })
            .expect("precompile call succeeds")
    }

    fn charged_input_gas(calldata: &[u8]) -> u64 {
        let mut ctx = test_context();
        let cfg = revm::context::CfgEnv::<TempoHardfork>::default();
        let mut provider = EvmPrecompileStorageProvider::new(
            EvmInternals::from_context(&mut ctx),
            u64::MAX,
            0,
            cfg.spec,
            cfg.enable_amsterdam_eip8037,
            true,
            cfg.gas_params,
        );
        StorageCtx::enter(&mut provider, || {
            let mut storage = StorageCtx::default();
            let gas_before = storage.gas_used();
            assert!(charge_input_cost(&mut storage, calldata).is_none());
            storage.gas_used().saturating_sub(gas_before)
        })
    }

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
    fn precompile_gas_charges_aad_bytes() {
        let plaintext = b"";
        let aad = vec![0xA5; 128];
        let call = encrypt(plaintext, &aad);
        let ciphertext_len = call.ciphertext.len();
        let aad_len = call.aad.len();
        let calldata = call.abi_encode();
        let expected_gas = charged_input_gas(&calldata)
            + AES_GCM_BASE_GAS
            + AES_GCM_PER_BYTE_GAS * (ciphertext_len + aad_len) as u64;

        let output = call_precompile(calldata.into());
        let decoded = decryptCall::abi_decode_returns(&output.bytes).expect("decode return");

        assert!(decoded.valid);
        assert_eq!(decoded.plaintext, Bytes::copy_from_slice(plaintext));
        assert_eq!(output.gas_used, expected_gas);
    }

    #[test]
    fn precompile_decrypts_without_aad_and_reports_ciphertext_gas() {
        let plaintext = b"normal precompile path";
        let call = encrypt(plaintext, &[]);
        let ciphertext_len = call.ciphertext.len();
        let calldata = call.abi_encode();
        let expected_gas = charged_input_gas(&calldata)
            + AES_GCM_BASE_GAS
            + AES_GCM_PER_BYTE_GAS * ciphertext_len as u64;

        let output = call_precompile(calldata.into());
        let decoded = decryptCall::abi_decode_returns(&output.bytes).expect("decode return");

        assert!(decoded.valid);
        assert_eq!(decoded.plaintext, Bytes::copy_from_slice(plaintext));
        assert_eq!(output.gas_used, expected_gas);
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
