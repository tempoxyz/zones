//! Sequencer-side ECIES operations for encrypted deposit decryption.
//!
//! These functions run **off-chain** in the payload builder to produce the
//! [`DecryptionData`](crate::abi::DecryptionData) that the on-chain ZoneInbox
//! contract verifies via the Chaum-Pedersen and AES-GCM precompiles.

use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use alloy_primitives::{Address, B256};
use k256::{
    AffinePoint, ProjectivePoint, Scalar,
    elliptic_curve::{PrimeField, sec1::ToEncodedPoint},
};

use super::{
    aes_gcm::decrypt_aes_gcm,
    chaum_pedersen::{challenge_hash, recover_point},
};

/// Plaintext size for encrypted deposits: 20 bytes (address) + 32 bytes (memo) + 12 bytes (padding).
pub const ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE: usize = 64;

/// Result of sequencer-side ECIES decryption of an encrypted deposit.
#[derive(Debug, Clone)]
pub struct DecryptedDeposit {
    /// ECDH shared secret (x-coordinate of `privSeq * ephemeralPub`).
    pub shared_secret: B256,
    /// Y parity of the shared secret point (0x02 or 0x03).
    pub shared_secret_y_parity: u8,
    /// Decrypted recipient address.
    pub to: Address,
    /// Decrypted memo.
    pub memo: B256,
    /// Chaum-Pedersen proof of correct shared secret derivation.
    pub cp_proof_s: B256,
    pub cp_proof_c: B256,
}

/// Perform ECIES decryption of an encrypted deposit using the sequencer's private key.
///
/// This implements the full ECIES flow:
/// 1. ECDH: compute `sharedSecret = privSeq * ephemeralPub`
/// 2. HKDF-SHA256: derive AES key from shared secret
/// 3. AES-256-GCM: decrypt ciphertext and verify tag
/// 4. Parse plaintext into `(to, memo)`
/// 5. Generate Chaum-Pedersen proof of correct shared secret derivation
///
/// Returns `None` if any step fails (invalid point, decryption failure, etc.).
pub fn decrypt_deposit(
    sequencer_privkey: &k256::SecretKey,
    ephemeral_pub_x: &B256,
    ephemeral_pub_y_parity: u8,
    ciphertext: &[u8],
    nonce: &[u8; 12],
    tag: &[u8; 16],
    portal_address: Address,
    key_index: alloy_primitives::U256,
) -> Option<DecryptedDeposit> {
    // 1. Recover ephemeral public key
    let ephemeral_pub = recover_point(&ephemeral_pub_x.0, ephemeral_pub_y_parity)?;

    // 2. ECDH: sharedSecretPoint = privSeq * ephemeralPub
    let priv_scalar: Scalar = *sequencer_privkey.to_nonzero_scalar();
    let shared_secret_proj = ProjectivePoint::from(ephemeral_pub) * priv_scalar;
    let shared_secret_affine = AffinePoint::from(shared_secret_proj);

    let ss_encoded = shared_secret_affine.to_encoded_point(true);
    let shared_secret_x: [u8; 32] = ss_encoded.x()?.as_slice().try_into().ok()?;
    let shared_secret_y_parity = ss_encoded.as_bytes()[0]; // 0x02 or 0x03

    // 3. HKDF-SHA256: derive AES key
    // Must match the Solidity implementation in ZoneInbox._hkdfSha256
    let info = hkdf_info(&portal_address, &key_index, ephemeral_pub_x);
    let aes_key = hkdf_sha256(&shared_secret_x, b"ecies-aes-key", &info);

    // 4. AES-256-GCM decrypt
    let (plaintext, valid) = decrypt_aes_gcm(&aes_key, nonce, ciphertext, &[], tag);
    if !valid || plaintext.len() != ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE {
        return None;
    }

    // 5. Parse plaintext: [address(20)][memo(32)][padding(12)]
    let to = Address::from_slice(&plaintext[..20]);
    let memo = B256::from_slice(&plaintext[20..52]);

    // 6. Generate Chaum-Pedersen proof
    let sequencer_pub = AffinePoint::from(ProjectivePoint::GENERATOR * priv_scalar);

    let (s, c) = generate_chaum_pedersen_proof(
        &priv_scalar,
        &ephemeral_pub,
        &shared_secret_affine,
        &sequencer_pub,
    );

    Some(DecryptedDeposit {
        shared_secret: B256::from(shared_secret_x),
        shared_secret_y_parity,
        to,
        memo,
        cp_proof_s: B256::from(s.to_repr().as_ref()),
        cp_proof_c: B256::from(c.to_repr().as_ref()),
    })
}

/// Result of client-side ECIES encryption for a deposit.
///
/// Contains all fields needed to call `ZonePortal.depositEncrypted`.
pub struct EncryptedDepositArgs {
    /// Ephemeral public key x-coordinate.
    pub eph_pub_x: B256,
    /// Ephemeral public key y-parity (0x02 or 0x03).
    pub eph_pub_y_parity: u8,
    /// AES-256-GCM ciphertext.
    pub ciphertext: Vec<u8>,
    /// AES-256-GCM nonce.
    pub nonce: [u8; 12],
    /// AES-256-GCM authentication tag.
    pub tag: [u8; 16],
}

/// Encrypt deposit data for `ZonePortal.depositEncrypted`.
///
/// This is the depositor-side counterpart of [`decrypt_deposit`] — it performs
/// ECIES encryption of `(to, memo)` to the sequencer's public key:
/// 1. Recover sequencer public key from `(seq_pub_x, seq_pub_y_parity)`
/// 2. Generate ephemeral key pair
/// 3. ECDH: `sharedSecret = ephPriv * sequencerPub`
/// 4. HKDF-SHA256: derive AES key
/// 5. AES-256-GCM encrypt `[to(20) | memo(32) | padding(12)]`
pub fn encrypt_deposit(
    seq_pub_x: &B256,
    seq_pub_y_parity: u8,
    to: Address,
    memo: B256,
    portal_address: Address,
    key_index: alloy_primitives::U256,
) -> Option<EncryptedDepositArgs> {
    // 1. Recover sequencer public key
    let seq_pub = recover_point(&seq_pub_x.0, seq_pub_y_parity)?;

    // 2. Generate ephemeral key pair
    let eph_key = k256::SecretKey::random(&mut rand::thread_rng());
    let eph_scalar: Scalar = *eph_key.to_nonzero_scalar();
    let eph_pub = AffinePoint::from(ProjectivePoint::GENERATOR * eph_scalar);
    let (eph_pub_x, eph_pub_y_parity) = compressed_x_and_parity(&eph_pub);

    // 3. ECDH: shared = eph_scalar * sequencer_pub
    let shared_proj = ProjectivePoint::from(seq_pub) * eph_scalar;
    let shared_affine = AffinePoint::from(shared_proj);
    let ss_enc = shared_affine.to_encoded_point(true);
    let shared_secret_x: [u8; 32] = ss_enc.x()?.as_slice().try_into().ok()?;

    // 4. HKDF key derivation
    let info = hkdf_info(&portal_address, &key_index, &eph_pub_x);
    let aes_key = hkdf_sha256(&shared_secret_x, b"ecies-aes-key", &info);

    // 5. Encrypt plaintext with random nonce
    let plaintext = build_plaintext(&to, &memo);
    let cipher = Aes256Gcm::new((&aes_key).into());
    let nonce_bytes: [u8; 12] = rand::random();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let encrypted = cipher.encrypt(nonce, plaintext.as_ref()).ok()?;
    let ciphertext = encrypted[..encrypted.len() - 16].to_vec();
    let tag: [u8; 16] = encrypted[encrypted.len() - 16..].try_into().ok()?;

    Some(EncryptedDepositArgs {
        eph_pub_x,
        eph_pub_y_parity,
        ciphertext,
        nonce: nonce_bytes,
        tag,
    })
}

/// Generate a Chaum-Pedersen proof that `sharedSecret = privSeq * ephemeralPub`.
///
/// Returns `(s, c)` proof scalars.
fn generate_chaum_pedersen_proof(
    priv_seq: &Scalar,
    ephemeral_pub: &AffinePoint,
    shared_secret: &AffinePoint,
    sequencer_pub: &AffinePoint,
) -> (Scalar, Scalar) {
    use k256::elliptic_curve::Field;

    let mut rng = rand::thread_rng();

    // 1. Prover picks random k
    let k = Scalar::random(&mut rng);
    let r1 = AffinePoint::from(ProjectivePoint::GENERATOR * k);
    let r2 = AffinePoint::from(ProjectivePoint::from(*ephemeral_pub) * k);

    // 2. Challenge via shared helper
    let c = challenge_hash(ephemeral_pub, sequencer_pub, shared_secret, &r1, &r2);

    // 3. Response: s = k + c * privSeq
    let s = k + c * priv_seq;

    (s, c)
}

/// HMAC-SHA256 implementation matching ZoneInbox._hmacSha256.
pub(crate) fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    // Pad/hash key to 64 bytes
    let mut key_block = [0u8; 64];
    if key.len() > 64 {
        let hash = Sha256::digest(key);
        key_block[..32].copy_from_slice(&hash);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }

    // Inner hash: SHA256(ipad || message)
    let mut hasher = Sha256::new();
    hasher.update(ipad);
    hasher.update(message);
    let inner_hash = hasher.finalize();

    // Outer hash: SHA256(opad || innerHash)
    let mut hasher = Sha256::new();
    hasher.update(opad);
    hasher.update(inner_hash);
    let result = hasher.finalize();

    result.into()
}

/// HKDF-SHA256 key derivation matching ZoneInbox._hkdfSha256.
pub fn hkdf_sha256(ikm: &[u8; 32], salt: &[u8], info: &[u8]) -> [u8; 32] {
    // Extract: PRK = HMAC-SHA256(salt, IKM)
    let prk = hmac_sha256(salt, ikm);

    // Expand: OKM = HMAC-SHA256(PRK, info || 0x01)
    let mut expand_input = Vec::with_capacity(info.len() + 1);
    expand_input.extend_from_slice(info);
    expand_input.push(0x01);
    hmac_sha256(&prk, &expand_input)
}

/// Extract the compressed x-coordinate and SEC1 parity byte from an affine point.
pub fn compressed_x_and_parity(point: &AffinePoint) -> (B256, u8) {
    let encoded = point.to_encoded_point(true);
    let x = B256::from_slice(encoded.x().unwrap().as_slice());
    let parity = encoded.as_bytes()[0];
    (x, parity)
}

/// Build a 64-byte plaintext from address + memo: `[to(20)|memo(32)|padding(12)]`.
pub fn build_plaintext(to: &Address, memo: &B256) -> [u8; ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE] {
    let mut buf = [0u8; ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE];
    buf[..20].copy_from_slice(to.as_slice());
    buf[20..52].copy_from_slice(memo.as_slice());
    // bytes 52..64 are zero padding
    buf
}

/// Build the 84-byte HKDF info parameter: `[portal(20) | key_index(32) | eph_pub_x(32)]`.
pub(crate) fn hkdf_info(
    portal: &Address,
    key_index: &alloy_primitives::U256,
    eph_pub_x: &B256,
) -> [u8; 84] {
    let mut info = [0u8; 84];
    info[..20].copy_from_slice(portal.as_slice());
    info[20..52].copy_from_slice(&key_index.to_be_bytes::<32>());
    info[52..84].copy_from_slice(&eph_pub_x.0);
    info
}

/// Encrypt plaintext with AES-256-GCM using a zero nonce, returning `(ciphertext, nonce, tag)`.
///
/// Uses a fixed zero nonce for deterministic tests.
pub fn encrypt_plaintext(aes_key: &[u8; 32], plaintext: &[u8]) -> (Vec<u8>, [u8; 12], [u8; 16]) {
    let cipher = Aes256Gcm::new(aes_key.into());
    let nonce_bytes = [0u8; 12];
    let nonce = Nonce::from_slice(&nonce_bytes);
    let encrypted = cipher.encrypt(nonce, plaintext.as_ref()).expect("encrypt");
    let ct = encrypted[..encrypted.len() - 16].to_vec();
    let tag: [u8; 16] = encrypted[encrypted.len() - 16..].try_into().unwrap();
    (ct, nonce_bytes, tag)
}

#[cfg(test)]
mod tests {
    use super::{
        super::test_utils::{EncryptedDepositFixture, assert_cp_proof_valid},
        compressed_x_and_parity, decrypt_deposit, hkdf_sha256, hmac_sha256,
    };
    use alloy_primitives::{Address, B256, U256};

    #[test]
    fn test_ecies_decrypt_roundtrip() {
        let f = EncryptedDepositFixture::new();
        let dec = f.decrypt().expect("decryption should succeed");

        assert_eq!(dec.to, f.to);
        assert_eq!(dec.memo, f.memo);
        assert_cp_proof_valid(&dec, &f.eph_pub, &f.seq_pub);
    }

    #[test]
    fn test_ecies_decrypt_wrong_key() {
        let f = EncryptedDepositFixture::new();
        let wrong_key = {
            use sha2::{Digest, Sha256};
            let bytes: [u8; 32] = Sha256::digest(b"wrong-sequencer-key").into();
            k256::SecretKey::from_slice(&bytes).unwrap()
        };

        let result = decrypt_deposit(
            &wrong_key,
            &f.eph_pub_x,
            f.eph_pub_y_parity,
            &f.ciphertext,
            &f.nonce,
            &f.tag,
            f.portal,
            f.key_index,
        );
        assert!(result.is_none(), "wrong key should fail decryption");
    }

    #[test]
    fn test_ecies_decrypt_tampered_ciphertext() {
        let f = EncryptedDepositFixture::new();
        let mut ct = f.ciphertext.clone();
        ct[0] ^= 0x01;

        let result = decrypt_deposit(
            &f.seq_key,
            &f.eph_pub_x,
            f.eph_pub_y_parity,
            &ct,
            &f.nonce,
            &f.tag,
            f.portal,
            f.key_index,
        );
        assert!(result.is_none(), "tampered ciphertext should fail");
    }

    #[test]
    fn test_ecies_decrypt_tampered_tag() {
        let f = EncryptedDepositFixture::new();
        let mut tag = f.tag;
        tag[0] ^= 0x01;

        let result = decrypt_deposit(
            &f.seq_key,
            &f.eph_pub_x,
            f.eph_pub_y_parity,
            &f.ciphertext,
            &f.nonce,
            &tag,
            f.portal,
            f.key_index,
        );
        assert!(result.is_none(), "tampered tag should fail");
    }

    #[test]
    fn test_ecies_decrypt_wrong_nonce() {
        let f = EncryptedDepositFixture::new();
        let wrong_nonce = [0xFFu8; 12];

        let result = decrypt_deposit(
            &f.seq_key,
            &f.eph_pub_x,
            f.eph_pub_y_parity,
            &f.ciphertext,
            &wrong_nonce,
            &f.tag,
            f.portal,
            f.key_index,
        );
        assert!(result.is_none(), "wrong nonce should fail");
    }

    #[test]
    fn test_ecies_decrypt_wrong_portal_address() {
        let f = EncryptedDepositFixture::new();
        let wrong_portal = Address::repeat_byte(0xFF);

        let result = decrypt_deposit(
            &f.seq_key,
            &f.eph_pub_x,
            f.eph_pub_y_parity,
            &f.ciphertext,
            &f.nonce,
            &f.tag,
            wrong_portal,
            f.key_index,
        );
        assert!(result.is_none(), "wrong portal address should fail");
    }

    #[test]
    fn test_ecies_decrypt_wrong_key_index() {
        let f = EncryptedDepositFixture::new();
        let wrong_index = U256::from(999u64);

        let result = decrypt_deposit(
            &f.seq_key,
            &f.eph_pub_x,
            f.eph_pub_y_parity,
            &f.ciphertext,
            &f.nonce,
            &f.tag,
            f.portal,
            wrong_index,
        );
        assert!(result.is_none(), "wrong key_index should fail");
    }

    #[test]
    fn test_ecies_decrypt_invalid_ephemeral_parity() {
        let f = EncryptedDepositFixture::new();

        let result = decrypt_deposit(
            &f.seq_key,
            &f.eph_pub_x,
            0x00, // invalid SEC1 prefix
            &f.ciphertext,
            &f.nonce,
            &f.tag,
            f.portal,
            f.key_index,
        );
        assert!(result.is_none(), "invalid y parity should fail");
    }

    #[test]
    fn test_ecies_decrypt_invalid_ephemeral_x() {
        let f = EncryptedDepositFixture::new();
        let bad_x = B256::repeat_byte(0xFF); // almost certainly not on curve

        let result = decrypt_deposit(
            &f.seq_key,
            &bad_x,
            0x02,
            &f.ciphertext,
            &f.nonce,
            &f.tag,
            f.portal,
            f.key_index,
        );
        assert!(result.is_none(), "invalid ephemeral x should fail");
    }

    #[test]
    fn test_ecies_decrypt_wrong_plaintext_length() {
        let f = EncryptedDepositFixture::new();

        // Encrypt a 63-byte plaintext (wrong length — should be 64)
        let short_plaintext = [0u8; 63];
        let aes_key = {
            use k256::{
                AffinePoint, ProjectivePoint, Scalar, elliptic_curve::sec1::ToEncodedPoint,
            };
            let seq_scalar: Scalar = *f.seq_key.to_nonzero_scalar();
            let shared = AffinePoint::from(ProjectivePoint::from(f.eph_pub) * seq_scalar);
            let ss_enc = shared.to_encoded_point(true);
            let ss_x: [u8; 32] = ss_enc.x().unwrap().as_slice().try_into().unwrap();
            let info = super::hkdf_info(&f.portal, &f.key_index, &f.eph_pub_x);
            hkdf_sha256(&ss_x, b"ecies-aes-key", &info)
        };
        let (ct, nonce, tag) =
            super::super::test_utils::encrypt_plaintext(&aes_key, &short_plaintext);

        let result = decrypt_deposit(
            &f.seq_key,
            &f.eph_pub_x,
            f.eph_pub_y_parity,
            &ct,
            &nonce,
            &tag,
            f.portal,
            f.key_index,
        );
        assert!(result.is_none(), "wrong plaintext length should fail");
    }

    #[test]
    fn test_hmac_sha256_rfc4231_vector() {
        // RFC 4231 Test Case 2
        let key = b"Jefe";
        let data = b"what do ya want for nothing?";
        let expected =
            const_hex::decode("5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843")
                .unwrap();

        let result = hmac_sha256(key, data);
        assert_eq!(result.as_slice(), expected.as_slice());
    }

    #[test]
    fn test_hkdf_sha256_basic() {
        let ikm = [0x0bu8; 32];
        let salt = b"salt-value";
        let info = b"info-value";

        let out1 = hkdf_sha256(&ikm, salt, info);
        assert_ne!(out1, [0u8; 32], "output should not be all zeros");

        // Deterministic: same inputs → same output
        let out2 = hkdf_sha256(&ikm, salt, info);
        assert_eq!(out1, out2, "hkdf must be deterministic");

        // Different salt → different output
        let out3 = hkdf_sha256(&ikm, b"other-salt", info);
        assert_ne!(out1, out3, "different salt should produce different output");
    }

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        use sha2::{Digest, Sha256};

        let seq_bytes: [u8; 32] = Sha256::digest(b"roundtrip-sequencer-key").into();
        let seq_key = k256::SecretKey::from_slice(&seq_bytes).expect("valid key");
        let seq_scalar: k256::Scalar = *seq_key.to_nonzero_scalar();
        let seq_pub = k256::AffinePoint::from(k256::ProjectivePoint::GENERATOR * seq_scalar);
        let (seq_pub_x, seq_pub_y_parity) = compressed_x_and_parity(&seq_pub);

        let to = Address::repeat_byte(0x42);
        let memo = B256::repeat_byte(0xAB);
        let portal = Address::repeat_byte(0x01);
        let key_index = U256::from(7u64);

        let enc = super::encrypt_deposit(&seq_pub_x, seq_pub_y_parity, to, memo, portal, key_index)
            .expect("encryption should succeed");

        let dec = super::decrypt_deposit(
            &seq_key,
            &enc.eph_pub_x,
            enc.eph_pub_y_parity,
            &enc.ciphertext,
            &enc.nonce,
            &enc.tag,
            portal,
            key_index,
        )
        .expect("decryption should succeed");

        assert_eq!(dec.to, to);
        assert_eq!(dec.memo, memo);
    }
}
