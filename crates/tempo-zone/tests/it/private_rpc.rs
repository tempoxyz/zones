//! Tests for the private zone RPC module.

use alloy_primitives::{Address, keccak256};
use base64::Engine as _;
use p256::{
    EncodedPoint,
    ecdsa::{SigningKey as P256SigningKey, signature::hazmat::PrehashSigner},
};
use rand::thread_rng;
use sha2::{Digest, Sha256};
use tempo_primitives::transaction::tt_signature::{
    KeychainSignature, PrimitiveSignature, TempoSignature, WebAuthnSignature, normalize_p256_s,
};
use zone::rpc::{
    auth::{AuthorizationToken, SignatureType, build_token_fields},
    types::{MethodTier, classify_method},
};

// ============ Helpers ============

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Build a raw token blob from the given fields with a fake 65-byte secp256k1 signature.
///
/// Uses [`build_token_fields`] for the canonical encoding and patches the
/// version byte when a non-zero version is requested (for negative tests).
fn build_token_blob(
    version: u8,
    zone_id: u64,
    chain_id: u64,
    portal: Address,
    issued_at: u64,
    expires_at: u64,
) -> Vec<u8> {
    let (mut fields, _digest) =
        build_token_fields(zone_id, chain_id, portal, issued_at, expires_at);
    // build_token_fields always sets version=0; override for tests that need a different version.
    fields[0] = version;
    let mut blob = vec![0u8; 65]; // fake secp256k1 sig
    blob.extend_from_slice(&fields);
    blob
}

fn make_test_token(
    version: u8,
    zone_id: u64,
    chain_id: u64,
    portal: Address,
    issued_at: u64,
    expires_at: u64,
) -> AuthorizationToken {
    let blob = build_token_blob(version, zone_id, chain_id, portal, issued_at, expires_at);
    AuthorizationToken::parse(&blob).unwrap()
}

fn build_signed_token_blob(signature: TempoSignature, fields: &[u8]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(signature.encoded_length() + fields.len());
    blob.extend_from_slice(signature.to_bytes().as_ref());
    blob.extend_from_slice(fields);
    blob
}

fn p256_public_key(
    signing_key: &P256SigningKey,
) -> (alloy_primitives::B256, alloy_primitives::B256) {
    let encoded = EncodedPoint::from(signing_key.verifying_key());
    (
        alloy_primitives::B256::from_slice(encoded.x().expect("x coordinate present")),
        alloy_primitives::B256::from_slice(encoded.y().expect("y coordinate present")),
    )
}

fn sign_p256_signature(
    digest: alloy_primitives::B256,
    signing_key: &P256SigningKey,
) -> TempoSignature {
    let pre_hashed = Sha256::digest(digest);
    let signature: p256::ecdsa::Signature = signing_key
        .sign_prehash(&pre_hashed)
        .expect("p256 signing should succeed");
    let sig_bytes = signature.to_bytes();
    let (pub_key_x, pub_key_y) = p256_public_key(signing_key);

    TempoSignature::Primitive(PrimitiveSignature::P256(
        tempo_primitives::transaction::tt_signature::P256SignatureWithPreHash {
            r: alloy_primitives::B256::from_slice(&sig_bytes[0..32]),
            s: normalize_p256_s(&sig_bytes[32..64]),
            pub_key_x,
            pub_key_y,
            pre_hash: true,
        },
    ))
}

fn sign_webauthn_signature(
    digest: alloy_primitives::B256,
    signing_key: &P256SigningKey,
) -> TempoSignature {
    let mut authenticator_data = vec![0u8; 37];
    authenticator_data[0..32].copy_from_slice(&[0xAA; 32]);
    authenticator_data[32] = 0x01;
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    let client_data_json = format!(
        r#"{{"type":"webauthn.get","challenge":"{challenge}","origin":"https://example.com","crossOrigin":false}}"#
    );
    let client_data_hash = Sha256::digest(client_data_json.as_bytes());
    let mut final_hasher = Sha256::new();
    final_hasher.update(&authenticator_data);
    final_hasher.update(client_data_hash);
    let message_hash = final_hasher.finalize();
    let signature: p256::ecdsa::Signature = signing_key
        .sign_prehash(&message_hash)
        .expect("webauthn signing should succeed");
    let sig_bytes = signature.to_bytes();
    let (pub_key_x, pub_key_y) = p256_public_key(signing_key);
    let mut webauthn_data = authenticator_data;
    webauthn_data.extend_from_slice(client_data_json.as_bytes());

    TempoSignature::Primitive(PrimitiveSignature::WebAuthn(WebAuthnSignature {
        webauthn_data: alloy_primitives::Bytes::from(webauthn_data),
        r: alloy_primitives::B256::from_slice(&sig_bytes[0..32]),
        s: normalize_p256_s(&sig_bytes[32..64]),
        pub_key_x,
        pub_key_y,
    }))
}

fn sign_keychain_signature(
    digest: alloy_primitives::B256,
    signing_key: &P256SigningKey,
    root_account: Address,
    version: u8,
) -> TempoSignature {
    let keychain_digest = match version {
        0x03 => digest,
        0x04 => KeychainSignature::signing_hash(digest, root_account),
        _ => panic!("unsupported keychain version"),
    };
    let primitive = match sign_p256_signature(keychain_digest, signing_key) {
        TempoSignature::Primitive(primitive) => primitive,
        TempoSignature::Keychain(_) => unreachable!("primitive signature expected"),
    };

    if version == 0x03 {
        TempoSignature::Keychain(KeychainSignature::new_v1(root_account, primitive))
    } else {
        TempoSignature::Keychain(KeychainSignature::new(root_account, primitive))
    }
}

// ============ Auth Token Parsing ============

#[test]
fn parse_token_fields() {
    let now = now_secs();
    let portal = Address::repeat_byte(0xAA);

    let token = make_test_token(0, 42, 1337, portal, now, now + 600);

    assert_eq!(token.version, 0);
    assert_eq!(token.zone_id, 42);
    assert_eq!(token.chain_id, 1337);
    assert_eq!(token.zone_portal, portal);
    assert_eq!(token.issued_at, now);
    assert_eq!(token.expires_at, now + 600);
    assert_eq!(token.signature.len(), 65);
    assert_eq!(token.signature_type().unwrap(), SignatureType::Secp256k1);
}

#[test]
fn parse_secp256k1_signature_type_with_reserved_prefix_bytes() {
    let now = now_secs();

    for prefix in [0x02, 0x03] {
        let mut blob = vec![prefix];
        blob.extend_from_slice(&[0u8; 64]);
        blob.push(0);
        blob.extend_from_slice(&1u64.to_be_bytes());
        blob.extend_from_slice(&1u64.to_be_bytes());
        blob.extend_from_slice(&[0u8; 20]);
        blob.extend_from_slice(&now.to_be_bytes());
        blob.extend_from_slice(&(now + 600).to_be_bytes());

        let token = AuthorizationToken::parse(&blob).unwrap();
        assert_eq!(
            token.signature_type().unwrap(),
            SignatureType::Secp256k1,
            "65-byte signatures must remain secp256k1 even when starting with {prefix:#04x}",
        );
    }
}

#[test]
fn parse_token_too_short() {
    // 53 bytes = exactly the message length, no room for a signature
    let blob = vec![0u8; 53];
    assert!(AuthorizationToken::parse(&blob).is_err());

    // Even shorter
    assert!(AuthorizationToken::parse(&[0u8; 10]).is_err());
}

#[test]
fn parse_p256_signature_type() {
    let now = now_secs();

    // P256 sig: 0x01 prefix + 129 zero bytes = 130 total
    let mut blob = vec![0x01];
    blob.extend_from_slice(&[0u8; 129]);
    // Append 53 bytes of message fields
    blob.push(0); // version
    blob.extend_from_slice(&1u64.to_be_bytes());
    blob.extend_from_slice(&1u64.to_be_bytes());
    blob.extend_from_slice(&[0u8; 20]); // portal
    blob.extend_from_slice(&now.to_be_bytes());
    blob.extend_from_slice(&(now + 600).to_be_bytes());

    let token = AuthorizationToken::parse(&blob).unwrap();
    assert_eq!(token.signature.len(), 130);
    assert_eq!(token.signature_type().unwrap(), SignatureType::P256);
}

#[test]
fn parse_keychain_v2_signature_type() {
    let now = now_secs();

    let mut blob = vec![0x04];
    blob.extend_from_slice(Address::repeat_byte(0x11).as_slice());
    blob.push(0x01);
    blob.extend_from_slice(&[0u8; 129]);
    blob.push(0);
    blob.extend_from_slice(&1u64.to_be_bytes());
    blob.extend_from_slice(&1u64.to_be_bytes());
    blob.extend_from_slice(&[0u8; 20]);
    blob.extend_from_slice(&now.to_be_bytes());
    blob.extend_from_slice(&(now + 600).to_be_bytes());

    let token = AuthorizationToken::parse(&blob).unwrap();
    assert_eq!(token.signature_type().unwrap(), SignatureType::Keychain);
}

#[test]
fn parse_unknown_signature_length() {
    // 50-byte signature (not 65 or 130, and first byte is not 0x02/0x03) → should error
    let mut blob = vec![0u8; 50];
    // 53 bytes of fields
    blob.push(0);
    blob.extend_from_slice(&1u64.to_be_bytes());
    blob.extend_from_slice(&1u64.to_be_bytes());
    blob.extend_from_slice(&[0u8; 20]);
    let now = now_secs();
    blob.extend_from_slice(&now.to_be_bytes());
    blob.extend_from_slice(&(now + 600).to_be_bytes());

    let token = AuthorizationToken::parse(&blob).unwrap();
    assert!(token.signature_type().is_err());
}

#[test]
fn digest_is_deterministic() {
    let portal = Address::repeat_byte(0xCC);
    let t1 = make_test_token(0, 1, 2, portal, 1000, 1600);
    let t2 = make_test_token(0, 1, 2, portal, 1000, 1600);
    assert_eq!(t1.digest, t2.digest);
}

#[test]
fn digest_changes_with_params() {
    let portal = Address::repeat_byte(0xCC);
    let t1 = make_test_token(0, 1, 2, portal, 1000, 1600);
    let t3 = make_test_token(0, 1, 3, portal, 1000, 1600);
    assert_ne!(t1.digest, t3.digest);
}

// ============ Auth Token Validation ============

#[test]
fn validate_accepts_valid_token() {
    let now = now_secs();
    let portal = Address::repeat_byte(0xBB);
    let token = make_test_token(0, 42, 1337, portal, now, now + 600);
    assert!(token.validate(42, 1337, portal).is_ok());
}

#[test]
fn validate_rejects_wrong_version() {
    let now = now_secs();
    let portal = Address::repeat_byte(0xBB);
    let token = make_test_token(1, 42, 1337, portal, now, now + 600);
    assert!(token.validate(42, 1337, portal).is_err());
}

#[test]
fn validate_rejects_zone_id_mismatch() {
    let now = now_secs();
    let portal = Address::repeat_byte(0xBB);
    let token = make_test_token(0, 42, 1337, portal, now, now + 600);
    assert!(token.validate(99, 1337, portal).is_err());
}

#[test]
fn validate_rejects_chain_id_mismatch() {
    let now = now_secs();
    let portal = Address::repeat_byte(0xBB);
    let token = make_test_token(0, 42, 1337, portal, now, now + 600);
    assert!(token.validate(42, 9999, portal).is_err());
}

#[test]
fn validate_rejects_portal_mismatch() {
    let now = now_secs();
    let portal = Address::repeat_byte(0xBB);
    let other = Address::repeat_byte(0xCC);
    let token = make_test_token(0, 42, 1337, portal, now, now + 600);
    assert!(token.validate(42, 1337, other).is_err());
}

#[test]
fn validate_rejects_expired() {
    let now = now_secs();
    let portal = Address::repeat_byte(0xBB);
    // Token that expired 100s ago
    let token = make_test_token(0, 42, 1337, portal, now - 700, now - 100);
    assert!(token.validate(42, 1337, portal).is_err());
}

#[test]
fn validate_rejects_window_too_large() {
    let now = now_secs();
    let portal = Address::repeat_byte(0xBB);
    // 2000s window > 1800s max
    let token = make_test_token(0, 42, 1337, portal, now, now + 2000);
    assert!(token.validate(42, 1337, portal).is_err());
}

#[test]
fn validate_rejects_issued_at_far_future() {
    let now = now_secs();
    let portal = Address::repeat_byte(0xBB);
    // issuedAt is 200s in the future (> 60s max skew)
    let token = make_test_token(0, 42, 1337, portal, now + 200, now + 800);
    assert!(token.validate(42, 1337, portal).is_err());
}

// ============ secp256k1 Recovery ============

#[tokio::test]
async fn secp256k1_recovery_roundtrip() {
    use alloy::signers::{Signer, local::PrivateKeySigner};
    use zone::rpc::auth::recover_secp256k1;

    let signer = PrivateKeySigner::random();
    let expected_addr = signer.address();

    let digest = keccak256(b"test message");
    let sig = signer.sign_hash(&digest).await.unwrap();

    let mut sig_bytes = Vec::with_capacity(65);
    sig_bytes.extend_from_slice(&sig.r().to_be_bytes::<32>());
    sig_bytes.extend_from_slice(&sig.s().to_be_bytes::<32>());
    sig_bytes.push(sig.v() as u8);

    let recovered = recover_secp256k1(&sig_bytes, &digest).unwrap();
    assert_eq!(recovered, expected_addr);
}

#[test]
fn secp256k1_rejects_wrong_length() {
    use zone::rpc::auth::recover_secp256k1;

    let digest = keccak256(b"test");
    assert!(recover_secp256k1(&[0u8; 64], &digest).is_err());
    assert!(recover_secp256k1(&[0u8; 66], &digest).is_err());
    assert!(recover_secp256k1(&[], &digest).is_err());
}

#[test]
fn tempo_signature_roundtrip_p256_from_token_bytes() {
    let signing_key = P256SigningKey::random(&mut thread_rng());
    let now = now_secs();
    let (fields, digest) = build_token_fields(1, 2, Address::ZERO, now, now + 600);
    let expected = sign_p256_signature(digest, &signing_key)
        .recover_signer(&digest)
        .expect("p256 recovery should succeed");
    let blob = build_signed_token_blob(sign_p256_signature(digest, &signing_key), &fields);
    let token = AuthorizationToken::parse(&blob).unwrap();
    let parsed = TempoSignature::from_bytes(&token.signature).unwrap();

    assert_eq!(parsed.recover_signer(&token.digest).unwrap(), expected);
}

#[test]
fn tempo_signature_roundtrip_webauthn_from_token_bytes() {
    let signing_key = P256SigningKey::random(&mut thread_rng());
    let now = now_secs();
    let (fields, digest) = build_token_fields(1, 2, Address::ZERO, now, now + 600);
    let signature = sign_webauthn_signature(digest, &signing_key);
    let expected = signature
        .recover_signer(&digest)
        .expect("webauthn recovery should succeed");
    let blob = build_signed_token_blob(signature, &fields);
    let token = AuthorizationToken::parse(&blob).unwrap();
    let parsed = TempoSignature::from_bytes(&token.signature).unwrap();

    assert_eq!(parsed.recover_signer(&token.digest).unwrap(), expected);
}

#[test]
fn tempo_signature_roundtrip_keychain_v1_from_token_bytes() {
    let signing_key = P256SigningKey::random(&mut thread_rng());
    let root_account = Address::repeat_byte(0x44);
    let now = now_secs();
    let (fields, digest) = build_token_fields(1, 2, Address::ZERO, now, now + 600);
    let signature = sign_keychain_signature(digest, &signing_key, root_account, 0x03);
    let expected_key_id = match &signature {
        TempoSignature::Keychain(keychain) => keychain.key_id(&digest).unwrap(),
        TempoSignature::Primitive(_) => unreachable!("keychain signature expected"),
    };
    let blob = build_signed_token_blob(signature, &fields);
    let token = AuthorizationToken::parse(&blob).unwrap();
    let parsed = TempoSignature::from_bytes(&token.signature).unwrap();

    assert_eq!(parsed.recover_signer(&token.digest).unwrap(), root_account);
    match parsed {
        TempoSignature::Keychain(keychain) => {
            assert_eq!(keychain.key_id(&token.digest).unwrap(), expected_key_id);
        }
        TempoSignature::Primitive(_) => panic!("expected keychain signature"),
    }
}

#[test]
fn tempo_signature_roundtrip_keychain_v2_from_token_bytes() {
    let signing_key = P256SigningKey::random(&mut thread_rng());
    let root_account = Address::repeat_byte(0x55);
    let now = now_secs();
    let (fields, digest) = build_token_fields(1, 2, Address::ZERO, now, now + 600);
    let signature = sign_keychain_signature(digest, &signing_key, root_account, 0x04);
    let expected_key_id = match &signature {
        TempoSignature::Keychain(keychain) => keychain.key_id(&digest).unwrap(),
        TempoSignature::Primitive(_) => unreachable!("keychain signature expected"),
    };
    let blob = build_signed_token_blob(signature, &fields);
    let token = AuthorizationToken::parse(&blob).unwrap();
    let parsed = TempoSignature::from_bytes(&token.signature).unwrap();

    assert_eq!(parsed.recover_signer(&token.digest).unwrap(), root_account);
    match parsed {
        TempoSignature::Keychain(keychain) => {
            assert_eq!(keychain.key_id(&token.digest).unwrap(), expected_key_id);
        }
        TempoSignature::Primitive(_) => panic!("expected keychain signature"),
    }
}

#[test]
fn tempo_signature_rejects_malformed_signature_bytes() {
    let malformed = [0x04, 0x11, 0x22];
    assert!(TempoSignature::from_bytes(&malformed).is_err());
}

// ============ Method Classification ============

#[test]
fn classify_public_methods() {
    for method in [
        "eth_chainId",
        "eth_blockNumber",
        "eth_gasPrice",
        "eth_maxPriorityFeePerGas",
        "eth_feeHistory",
        "eth_getBalance",
        "eth_getTransactionCount",
        "eth_call",
        "eth_estimateGas",
        "eth_syncing",
        "eth_coinbase",
        "eth_getBlockByNumber",
        "eth_getBlockByHash",
        "eth_getTransactionByHash",
        "eth_getTransactionReceipt",
        "eth_getLogs",
        "eth_getFilterLogs",
        "eth_getFilterChanges",
        "eth_newFilter",
        "eth_newBlockFilter",
        "eth_uninstallFilter",
        "eth_sendRawTransaction",
        "net_version",
        "net_listening",
        "web3_clientVersion",
        "web3_sha3",
    ] {
        assert_eq!(
            classify_method(method),
            Some(MethodTier::Public),
            "expected {method} to be Public"
        );
    }
}

#[test]
fn classify_restricted_methods() {
    for method in [
        "eth_getCode",
        "eth_getStorageAt",
        "eth_getBlockReceipts",
        "eth_sendTransaction",
        "eth_createAccessList",
        "eth_getBlockTransactionCountByNumber",
        "eth_getBlockTransactionCountByHash",
        "eth_getTransactionByBlockNumberAndIndex",
        "eth_getTransactionByBlockHashAndIndex",
        "eth_getUncleCountByBlockNumber",
        "eth_getUncleCountByBlockHash",
        "debug_traceTransaction",
        "debug_traceBlockByNumber",
        "debug_traceBlockByHash",
        "txpool_content",
        "txpool_status",
        "txpool_inspect",
    ] {
        assert_eq!(
            classify_method(method),
            Some(MethodTier::Restricted),
            "expected {method} to be Restricted"
        );
    }
}

#[test]
fn classify_disabled_methods() {
    for method in [
        "eth_mining",
        "eth_hashrate",
        "eth_submitWork",
        "eth_submitHashrate",
        "eth_subscribe",
        "eth_unsubscribe",
    ] {
        assert_eq!(
            classify_method(method),
            Some(MethodTier::Disabled),
            "expected {method} to be Disabled"
        );
    }
}

#[test]
fn classify_admin_wildcard() {
    for method in [
        "admin_addPeer",
        "admin_removePeer",
        "admin_nodeInfo",
        "admin_peers",
    ] {
        assert_eq!(
            classify_method(method),
            Some(MethodTier::Restricted),
            "expected {method} to be Restricted (admin_* wildcard)"
        );
    }
}

#[test]
fn classify_unknown_is_none() {
    assert_eq!(classify_method("eth_someNewMethod"), None);
    assert_eq!(classify_method("foo_bar"), None);
    assert_eq!(classify_method(""), None);
}
