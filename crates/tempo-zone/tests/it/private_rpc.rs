//! Tests for the private zone RPC module.

use alloy_primitives::{Address, keccak256};
use zone::rpc::{
    auth::{AuthorizationToken, SignatureType},
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
fn build_token_blob(
    version: u8,
    zone_id: u64,
    chain_id: u64,
    portal: Address,
    issued_at: u64,
    expires_at: u64,
) -> Vec<u8> {
    let mut blob = vec![0u8; 65]; // fake secp256k1 sig
    blob.push(version);
    blob.extend_from_slice(&zone_id.to_be_bytes());
    blob.extend_from_slice(&chain_id.to_be_bytes());
    blob.extend_from_slice(portal.as_slice());
    blob.extend_from_slice(&issued_at.to_be_bytes());
    blob.extend_from_slice(&expires_at.to_be_bytes());
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
        "eth_getCode",
        "eth_getStorageAt",
        "eth_getBlockByNumber",
        "eth_getBlockByHash",
        "eth_getBlockReceipts",
        "eth_getTransactionByHash",
        "eth_getTransactionReceipt",
        "eth_getLogs",
        "eth_sendRawTransaction",
        "net_version",
        "net_listening",
        "web3_clientVersion",
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
        "eth_sendTransaction",
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
fn classify_unknown_is_none() {
    assert_eq!(classify_method("eth_someNewMethod"), None);
    assert_eq!(classify_method("foo_bar"), None);
    assert_eq!(classify_method(""), None);
}


