use super::*;
use alloy_primitives::{address, b256};
use k256::ecdsa::SigningKey;
use p256::ecdsa::signature::hazmat::PrehashSigner;
use sha2::{Digest, Sha256};
use tempo_zone_contracts::Withdrawal;

const PORTAL: Address = address!("5ad0000000000000000000000000000000000001");
const TOKEN: Address = address!("20c0000000000000000000000000000000000000");

fn auth() -> ForcedExitAuthorization {
    ForcedExitAuthorization {
        account: address!("7e5f4552091a69125d5dfcb7b8c2659029395bdf"),
        zoneChainId: U256::from(1234),
        token: TOKEN,
        recipient: Address::repeat_byte(0x22),
        nonce: U256::from(42),
        admitBefore: 100,
    }
}
fn sign(a: &ForcedExitAuthorization) -> Vec<u8> {
    let key = SigningKey::from_slice(&U256::from(1).to_be_bytes::<32>()).unwrap();
    let (sig, recovery) = key
        .sign_prehash_recoverable(authorization_digest(a, U256::from(42431), PORTAL).as_slice())
        .unwrap();
    let mut bytes = sig.to_bytes().to_vec();
    bytes.push(recovery.to_byte());
    bytes
}
fn check(a: &ForcedExitAuthorization, sig: &[u8], time: u64) -> Result<(), ForcedExitReason> {
    authorize(
        a,
        sig,
        U256::from(42431),
        PORTAL,
        U256::from(1234),
        TOKEN,
        time,
    )
}

#[test]
fn authorization_binds_every_field_and_domain() {
    let a = auth();
    let sig = sign(&a);
    assert_eq!(check(&a, &sig, 99), Ok(()));
    assert_eq!(
        check(&a, &sig, 100),
        Err(ForcedExitReason::InvalidAuthorization)
    );
    assert!(check(&a, &sig, 101).is_err());
    for field in 0..6 {
        let mut changed = a.clone();
        match field {
            0 => changed.account = Address::repeat_byte(3),
            1 => changed.zoneChainId += U256::ONE,
            2 => changed.token = Address::repeat_byte(4),
            3 => changed.recipient = Address::repeat_byte(5),
            4 => changed.nonce += U256::ONE,
            _ => changed.admitBefore += 1,
        }
        assert!(check(&changed, &sig, 99).is_err());
    }
    assert!(
        authorize(
            &a,
            &sig,
            U256::from(42432),
            PORTAL,
            a.zoneChainId,
            TOKEN,
            99
        )
        .is_err()
    );
    assert!(
        authorize(
            &a,
            &sig,
            U256::from(42431),
            Address::ZERO,
            a.zoneChainId,
            TOKEN,
            99
        )
        .is_err()
    );
    for field in 0..3 {
        let mut invalid = a.clone();
        match field {
            0 => invalid.account = Address::ZERO,
            1 => invalid.recipient = Address::ZERO,
            _ => invalid.admitBefore = 0,
        }
        assert!(check(&invalid, &sign(&invalid), 0).is_err());
    }
}

#[test]
fn payload_is_canonical_and_signature_errors_are_a_separate_category() {
    let a = auth();
    let sig = sign(&a);
    let bytes = encode_payload(&a, &sig);
    assert_eq!(bytes.len(), 384);
    let (decoded, signature) = decode_payload(&bytes).unwrap();
    assert_eq!(decoded, a);
    assert_eq!(signature.as_ref(), sig);
    assert_eq!(check(&decoded, &signature, 99), Ok(()));
    for pos in [0, 31, 32, 7 * 32 + 31, bytes.len() - 1] {
        let mut bad = bytes.clone();
        bad[pos] ^= 1;
        assert_eq!(
            decode_payload(&bad),
            Err(ForcedExitReason::InvalidPayload),
            "byte {pos}"
        );
    }
    let mut extra = bytes.clone();
    extra.extend_from_slice(&[0; 32]);
    assert!(decode_payload(&extra).is_err());
    assert!(decode_payload(&bytes[..bytes.len() - 1]).is_err());
    for bad in [
        vec![0; 65],
        vec![3; 130],
        vec![4; 130],
        vec![1; 2049],
        vec![2; 2050],
    ] {
        let plaintext = encode_payload(&a, &bad);
        let (decoded, signature) = decode_payload(&plaintext).unwrap();
        assert_eq!(
            check(&decoded, &signature, 99),
            Err(ForcedExitReason::InvalidAuthorization)
        );
    }
    for length in [0, 64, 383, 385, 2367, 2369, 2400] {
        assert!(!valid_ciphertext_length(length));
    }
    for length in (384..=2368).step_by(32) {
        assert!(valid_ciphertext_length(length));
    }
    assert_eq!(
        encode_payload(&a, &[0; MAX_SIGNATURE_SIZE]).len(),
        MAX_CIPHERTEXT_SIZE
    );
}

fn p256_key() -> p256::ecdsa::SigningKey {
    p256::ecdsa::SigningKey::from_slice(&[1u8; 32]).unwrap()
}
fn p256_account(key: &p256::ecdsa::SigningKey) -> Address {
    let point = key.verifying_key().to_encoded_point(false);
    Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
}
fn append_p256(bytes: &mut Vec<u8>, key: &p256::ecdsa::SigningKey, hash: &[u8]) {
    let sig: p256::ecdsa::Signature = key.sign_prehash(hash).unwrap();
    let sig = sig.normalize_s().unwrap_or(sig);
    bytes.extend_from_slice(&sig.to_bytes());
    bytes.extend_from_slice(&key.verifying_key().to_encoded_point(false).as_bytes()[1..]);
}
#[test]
fn primitive_p256_and_webauthn_root_signatures() {
    let key = p256_key();
    let mut a = auth();
    a.account = p256_account(&key);
    let digest = authorization_digest(&a, U256::from(42431), PORTAL);
    for pre_hash in [false, true] {
        let mut sig = vec![1];
        let hash: [u8; 32] = if pre_hash {
            Sha256::digest(digest).into()
        } else {
            digest.0
        };
        append_p256(&mut sig, &key, &hash);
        sig.push(u8::from(pre_hash));
        assert_eq!(check(&a, &sig, 99), Ok(()));
        let last = sig.len() - 1;
        sig[last] = 2;
        assert_eq!(
            check(&a, &sig, 99),
            Err(ForcedExitReason::InvalidAuthorization)
        );
        sig.push(0);
        assert!(check(&a, &sig, 99).is_err());
    }
    // Base64url without padding, encoded independently for the WebAuthn challenge.
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut challenge = String::new();
    for chunk in digest.as_slice().chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for shift in (0..4).take(chunk.len() + 1) {
            challenge.push(ALPHABET[((value >> (18 - 6 * shift)) & 63) as usize] as char);
        }
    }
    let json = format!(
        r#"{{"type":"webauthn.get","challenge":"{challenge}","origin":"https://example.com"}}"#
    );
    let mut auth_data = vec![0u8; 37];
    auth_data[32] = 1;
    let mut signed = auth_data.clone();
    signed.extend_from_slice(&Sha256::digest(json.as_bytes()));
    let mut signature = vec![2];
    signature.extend_from_slice(&auth_data);
    signature.extend_from_slice(json.as_bytes());
    append_p256(&mut signature, &key, &Sha256::digest(signed));
    assert_eq!(check(&a, &signature, 99), Ok(()));
    signature[33] = 0;
    assert!(check(&a, &signature, 99).is_err());
}

#[test]
fn keychain_envelopes_cannot_authorize() {
    let a = auth();
    let sig = sign(&a);
    for prefix in [3u8, 4] {
        let mut envelope = vec![prefix];
        envelope.extend_from_slice(a.account.as_slice());
        envelope.extend_from_slice(&sig);
        assert_eq!(
            check(&a, &envelope, 99),
            Err(ForcedExitReason::InvalidAuthorization)
        );
    }
}

#[test]
fn sender_tag_uses_private_signed_payload_and_unique_fallback_nonce() {
    let a = auth();
    let sig = sign(&a);
    let hash = private_request_hash(&a, &sig);
    assert_eq!(hash, keccak256(encode_payload(&a, &sig)));
    assert_ne!(hash, authorization_digest(&a, U256::from(42431), PORTAL));
    let mut changed = sig;
    changed[0] ^= 1;
    assert_ne!(hash, private_request_hash(&a, &changed));
    let tag = Withdrawal::sender_tag(a.account, hash, 9);
    assert_ne!(tag, Withdrawal::sender_tag(a.account, hash, 10));
    assert_ne!(tag, Withdrawal::sender_tag(a.account, B256::ZERO, 9));
}

#[test]
fn solidity_interop_vectors() {
    use tempo_zone_contracts::DepositPayload;
    let a = auth();
    let payload = encode_payload(&a, &[0x33; 65]);
    let entry = ForcedExit {
        requestId: 1,
        token: TOKEN,
        keyIndex: U256::from(3),
        encrypted: DepositPayload {
            ephemeralPubkeyX: b256!(
                "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
            ),
            ephemeralPubkeyYParity: 2,
            ciphertext: vec![0x44; 384].into(),
            nonce: [0x55; 12].into(),
            tag: [0x66; 16].into(),
        },
        feePayer: Address::repeat_byte(0x77),
        requestedAtBlock: 80,
        requestedAtTime: 90,
    };
    let withdrawal = Withdrawal {
        token: TOKEN,
        senderTag: Withdrawal::sender_tag(a.account, private_request_hash(&a, &sign(&a)), 9),
        to: a.recipient,
        amount: 123456,
        memo: B256::ZERO,
        gasLimit: 0,
        fallbackNonce: 9,
        callbackData: Bytes::new(),
        encryptedSender: Bytes::new(),
    };
    let values = serde_json::json!({
        "typeHash": a.eip712_type_hash(),
        "digest": authorization_digest(&a, U256::from(42431), PORTAL),
        "signature": Bytes::from(sign(&a)),
        "payload": Bytes::from(payload),
        "queueHash": request_queue_hash(&entry, B256::repeat_byte(0x88)),
        "senderTag": withdrawal.senderTag,
        "privateRequestHash": private_request_hash(&a, &sign(&a)),
        "withdrawalHash": keccak256(withdrawal.abi_encode()),
        "withdrawalQueueHash": keccak256((withdrawal, B256::repeat_byte(0x99)).abi_encode_params()),
    });
    let expected: serde_json::Value = serde_json::from_str(include_str!(
        "../../contracts/test/fixtures/forcedExit.json"
    ))
    .unwrap();
    assert_eq!(values, expected);
}
