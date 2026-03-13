use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};

use alloy_primitives::{Address, B256, Bytes};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use p256::{
    EncodedPoint,
    ecdsa::{SigningKey as P256SigningKey, signature::hazmat::PrehashSigner},
};
use parking_lot::Mutex;
use rand::thread_rng;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempo_contracts::precompiles::account_keychain::IAccountKeychain::{
    KeyInfo, SignatureType as KeyInfoSignatureType,
};
use tempo_primitives::transaction::tt_signature::{
    KeychainSignature, PrimitiveSignature, TempoSignature, WebAuthnSignature, normalize_p256_s,
};
use tokio_tungstenite::{connect_async, tungstenite};
use zone_rpc::{
    PrivateRpcConfig,
    auth::build_token_fields,
    handlers::ZoneRpcApi,
    start_private_rpc,
    types::{BoxEyreFut, BoxFut, JsonRpcError},
};

// ---------------------------------------------------------------------------
// Mock API
// ---------------------------------------------------------------------------

struct MockZoneRpcApi {
    key_infos: Mutex<HashMap<(Address, Address), KeyInfo>>,
    key_lookup_error: Option<&'static str>,
}

impl MockZoneRpcApi {
    fn with_key(account: Address, key_id: Address, signature_type: KeyInfoSignatureType) -> Self {
        let mut key_infos = HashMap::new();
        key_infos.insert(
            (account, key_id),
            KeyInfo {
                signatureType: signature_type,
                keyId: key_id,
                expiry: u64::MAX,
                enforceLimits: false,
                isRevoked: false,
            },
        );
        Self {
            key_infos: Mutex::new(key_infos),
            key_lookup_error: None,
        }
    }

    fn with_key_lookup_error(message: &'static str) -> Self {
        Self {
            key_infos: Mutex::new(HashMap::new()),
            key_lookup_error: Some(message),
        }
    }
}

macro_rules! stub {
    ($method:ident $(, $arg:ident : $ty:ty)*) => {
        fn $method(&self $(, $arg: $ty)*) -> BoxFut<'_> {
            Box::pin(async { Err(JsonRpcError::internal("not implemented")) })
        }
    };
}

impl ZoneRpcApi for MockZoneRpcApi {
    fn get_keychain_key(&self, account: Address, key_id: Address) -> BoxEyreFut<'_, KeyInfo> {
        if let Some(message) = self.key_lookup_error {
            return Box::pin(async move { Err(eyre::eyre!(message)) });
        }
        let key_info = self
            .key_infos
            .lock()
            .get(&(account, key_id))
            .cloned()
            .unwrap_or(KeyInfo {
                signatureType: KeyInfoSignatureType::Secp256k1,
                keyId: Address::ZERO,
                expiry: 0,
                enforceLimits: false,
                isRevoked: false,
            });
        Box::pin(async move { Ok(key_info) })
    }

    fn block_number(&self) -> BoxFut<'_> {
        Box::pin(async { zone_rpc::types::to_raw(&"0x42") })
    }

    fn chain_id(&self) -> BoxFut<'_> {
        Box::pin(async { zone_rpc::types::to_raw(&"0x1") })
    }

    stub!(net_version);
    stub!(gas_price);
    stub!(max_priority_fee_per_gas);
    stub!(fee_history, _a: u64, _b: alloy_rpc_types_eth::BlockNumberOrTag, _c: Option<Vec<f64>>);
    stub!(get_balance, _a: Address, _b: Option<alloy_rpc_types_eth::BlockId>, _c: zone_rpc::auth::AuthContext);
    stub!(get_transaction_count, _a: Address, _b: Option<alloy_rpc_types_eth::BlockId>, _c: zone_rpc::auth::AuthContext);
    stub!(block_by_number, _a: alloy_rpc_types_eth::BlockNumberOrTag, _b: bool, _c: zone_rpc::auth::AuthContext);
    stub!(block_by_hash, _a: alloy_primitives::B256, _b: bool, _c: zone_rpc::auth::AuthContext);
    stub!(transaction_by_hash, _a: alloy_primitives::B256, _c: zone_rpc::auth::AuthContext);
    stub!(transaction_receipt, _a: alloy_primitives::B256, _c: zone_rpc::auth::AuthContext);
    stub!(call, _a: tempo_alloy::rpc::TempoTransactionRequest, _b: Option<alloy_rpc_types_eth::BlockId>, _c: Option<alloy_rpc_types_eth::state::StateOverride>, _d: zone_rpc::auth::AuthContext);
    stub!(estimate_gas, _a: tempo_alloy::rpc::TempoTransactionRequest, _b: Option<alloy_rpc_types_eth::BlockId>, _c: Option<alloy_rpc_types_eth::state::StateOverride>, _d: zone_rpc::auth::AuthContext);
    stub!(send_raw_transaction, _a: alloy_primitives::Bytes, _c: zone_rpc::auth::AuthContext);
    stub!(send_raw_transaction_sync, _a: alloy_primitives::Bytes, _c: zone_rpc::auth::AuthContext);
    stub!(fill_transaction, _a: tempo_alloy::rpc::TempoTransactionRequest, _c: zone_rpc::auth::AuthContext);
    stub!(get_logs, _a: alloy_rpc_types_eth::Filter, _c: zone_rpc::auth::AuthContext);
    stub!(new_filter, _a: alloy_rpc_types_eth::Filter, _c: zone_rpc::auth::AuthContext);
    stub!(get_filter_logs, _a: alloy_rpc_types_eth::FilterId, _c: zone_rpc::auth::AuthContext);
    stub!(get_filter_changes, _a: alloy_rpc_types_eth::FilterId, _c: zone_rpc::auth::AuthContext);
    stub!(new_block_filter, _c: zone_rpc::auth::AuthContext);
    stub!(uninstall_filter, _a: alloy_rpc_types_eth::FilterId, _c: zone_rpc::auth::AuthContext);
}

// ---------------------------------------------------------------------------
// Test context
// ---------------------------------------------------------------------------

const ZONE_ID: u64 = 1;
const CHAIN_ID: u64 = 42;
const PORTAL: Address = Address::ZERO;

struct TestContext {
    addr: std::net::SocketAddr,
    signer: PrivateKeySigner,
}

impl TestContext {
    async fn start(api: MockZoneRpcApi) -> Self {
        let signer = PrivateKeySigner::random();
        let config = PrivateRpcConfig {
            listen_addr: ([127, 0, 0, 1], 0).into(),
            zone_id: ZONE_ID,
            chain_id: CHAIN_ID,
            zone_portal: PORTAL,
            sequencer: signer.address(),
        };
        let addr = start_private_rpc(config, Arc::new(api)).await.unwrap();
        Self { addr, signer }
    }

    fn build_token(&self) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let (fields, digest) = build_token_fields(ZONE_ID, CHAIN_ID, PORTAL, now, now + 600);
        let sig = self.signer.sign_hash_sync(&digest).expect("signing failed");

        let mut blob = Vec::with_capacity(65 + fields.len());
        blob.extend_from_slice(&sig.r().to_be_bytes::<32>());
        blob.extend_from_slice(&sig.s().to_be_bytes::<32>());
        blob.push(sig.v() as u8);
        blob.extend_from_slice(&fields);

        alloy_primitives::hex::encode(&blob)
    }

    fn ws_url(&self) -> String {
        format!("ws://{}", self.addr)
    }
}

fn default_api() -> MockZoneRpcApi {
    MockZoneRpcApi {
        key_infos: Mutex::new(HashMap::new()),
        key_lookup_error: None,
    }
}

fn p256_public_key(signing_key: &P256SigningKey) -> (B256, B256) {
    let encoded = EncodedPoint::from(signing_key.verifying_key());
    let pub_key_x = B256::from_slice(encoded.x().expect("x coordinate present"));
    let pub_key_y = B256::from_slice(encoded.y().expect("y coordinate present"));
    (pub_key_x, pub_key_y)
}

fn sign_p256_token(digest: B256, signing_key: &P256SigningKey) -> eyre::Result<TempoSignature> {
    let pre_hashed = Sha256::digest(digest);
    let signature: p256::ecdsa::Signature = signing_key.sign_prehash(&pre_hashed)?;
    let sig_bytes = signature.to_bytes();
    let (pub_key_x, pub_key_y) = p256_public_key(signing_key);

    Ok(TempoSignature::Primitive(PrimitiveSignature::P256(
        tempo_primitives::transaction::tt_signature::P256SignatureWithPreHash {
            r: B256::from_slice(&sig_bytes[0..32]),
            s: normalize_p256_s(&sig_bytes[32..64]),
            pub_key_x,
            pub_key_y,
            pre_hash: true,
        },
    )))
}

fn sign_webauthn_token(
    signing_key: &P256SigningKey,
    challenge: B256,
) -> eyre::Result<TempoSignature> {
    let mut authenticator_data = vec![0u8; 37];
    authenticator_data[0..32].copy_from_slice(&[0xAA; 32]);
    authenticator_data[32] = 0x01;
    let challenge_b64url = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(challenge);
    let client_data_json = format!(
        r#"{{"type":"webauthn.get","challenge":"{challenge_b64url}","origin":"https://example.com","crossOrigin":false}}"#
    );
    let client_data_hash = Sha256::digest(client_data_json.as_bytes());
    let mut final_hasher = Sha256::new();
    final_hasher.update(&authenticator_data);
    final_hasher.update(client_data_hash);
    let message_hash = final_hasher.finalize();

    let signature: p256::ecdsa::Signature = signing_key.sign_prehash(&message_hash)?;
    let sig_bytes = signature.to_bytes();
    let (pub_key_x, pub_key_y) = p256_public_key(signing_key);
    let mut webauthn_data = authenticator_data;
    webauthn_data.extend_from_slice(client_data_json.as_bytes());

    Ok(TempoSignature::Primitive(PrimitiveSignature::WebAuthn(
        WebAuthnSignature {
            webauthn_data: Bytes::from(webauthn_data),
            r: B256::from_slice(&sig_bytes[0..32]),
            s: normalize_p256_s(&sig_bytes[32..64]),
            pub_key_x,
            pub_key_y,
        },
    )))
}

fn sign_keychain_token(
    digest: B256,
    access_signer: &P256SigningKey,
    root_account: Address,
    version: u8,
) -> eyre::Result<(TempoSignature, Address)> {
    let signing_hash = match version {
        0x03 => digest,
        0x04 => KeychainSignature::signing_hash(digest, root_account),
        _ => eyre::bail!("unsupported keychain version"),
    };
    let primitive = match sign_p256_token(signing_hash, access_signer)? {
        TempoSignature::Primitive(primitive) => primitive,
        TempoSignature::Keychain(_) => unreachable!("primitive signature expected"),
    };
    let signature = if version == 0x03 {
        TempoSignature::Keychain(KeychainSignature::new_v1(root_account, primitive))
    } else {
        TempoSignature::Keychain(KeychainSignature::new(root_account, primitive))
    };
    let key_id = signature
        .recover_signer(&digest)
        .expect("keychain signer recovery should succeed");
    let access_key_id = match &signature {
        TempoSignature::Keychain(keychain) => keychain
            .key_id(&digest)
            .expect("inner key recovery should succeed"),
        TempoSignature::Primitive(_) => unreachable!("keychain signature expected"),
    };
    assert_eq!(key_id, root_account);
    Ok((signature, access_key_id))
}

fn build_token_with_signature(signature: TempoSignature, fields: &[u8]) -> String {
    let mut blob = Vec::with_capacity(signature.encoded_length() + fields.len());
    blob.extend_from_slice(signature.to_bytes().as_ref());
    blob.extend_from_slice(fields);
    alloy_primitives::hex::encode(&blob)
}

/// Build a JSON-RPC request string.
fn jsonrpc(method: &str, id: u64) -> String {
    serde_json::json!({"jsonrpc":"2.0","method":method,"params":[],"id":id}).to_string()
}

/// Connect to the WS endpoint using the X-Authorization-Token header.
async fn connect_with_header(
    ctx: &TestContext,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let token = ctx.build_token();
    connect_with_token(&ctx.ws_url(), ctx.addr, &token)
        .await
        .expect("ws connect failed")
}

async fn connect_with_token(
    ws_url: &str,
    addr: std::net::SocketAddr,
    token: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tungstenite::Error,
> {
    let req = tungstenite::http::Request::builder()
        .uri(ws_url)
        .header("x-authorization-token", token)
        .header(
            "sec-websocket-key",
            tungstenite::handshake::client::generate_key(),
        )
        .header("host", addr.to_string())
        .header("upgrade", "websocket")
        .header("connection", "upgrade")
        .header("sec-websocket-version", "13")
        .body(())
        .unwrap();

    connect_async(req).await.map(|(ws, _)| ws)
}

/// Parse a JSON-RPC response from a WS text message.
fn parse_response(msg: tungstenite::Message) -> Value {
    match msg {
        tungstenite::Message::Text(t) => serde_json::from_str(&t).expect("invalid json"),
        other => panic!("expected text message, got {other:?}"),
    }
}

fn test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ws_roundtrip_with_header_auth() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start(default_api()).await;
    let mut ws = connect_with_header(&ctx).await;

    ws.send(tungstenite::Message::Text(
        jsonrpc("eth_blockNumber", 1).into(),
    ))
    .await
    .unwrap();
    let resp = parse_response(ws.next().await.unwrap().unwrap());

    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"], "0x42");
}

#[tokio::test]
async fn ws_roundtrip_with_query_auth() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start(default_api()).await;
    let token = ctx.build_token();
    let url = format!("{}/?token={token}", ctx.ws_url());

    let (mut ws, _) = connect_async(&url).await.expect("ws connect failed");

    ws.send(tungstenite::Message::Text(
        jsonrpc("eth_blockNumber", 1).into(),
    ))
    .await
    .unwrap();
    let resp = parse_response(ws.next().await.unwrap().unwrap());

    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"], "0x42");
}

#[tokio::test]
async fn ws_reject_no_auth() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start(default_api()).await;
    let result = connect_async(ctx.ws_url()).await;
    // Server should reject the upgrade — tungstenite surfaces this as an error
    // with the HTTP 401 status.
    let err = result.expect_err("should fail without auth");
    let tungstenite::Error::Http(response) = err else {
        panic!("expected HTTP error, got {err:?}");
    };
    assert_eq!(response.status(), 401);
}

#[tokio::test]
async fn ws_reject_invalid_token() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start(default_api()).await;
    let req = tungstenite::http::Request::builder()
        .uri(ctx.ws_url())
        .header("x-authorization-token", "deadbeef")
        .header(
            "sec-websocket-key",
            tungstenite::handshake::client::generate_key(),
        )
        .header("host", ctx.addr.to_string())
        .header("upgrade", "websocket")
        .header("connection", "upgrade")
        .header("sec-websocket-version", "13")
        .body(())
        .unwrap();

    let err = connect_async(req)
        .await
        .expect_err("should fail with bad token");
    let tungstenite::Error::Http(response) = err else {
        panic!("expected HTTP error, got {err:?}");
    };
    assert_eq!(response.status(), 403);
}

#[tokio::test]
async fn ws_multiple_requests() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start(default_api()).await;
    let mut ws = connect_with_header(&ctx).await;

    for i in 1..=5 {
        ws.send(tungstenite::Message::Text(
            jsonrpc("eth_blockNumber", i).into(),
        ))
        .await
        .unwrap();
        let resp = parse_response(ws.next().await.unwrap().unwrap());
        assert_eq!(resp["id"], i);
        assert_eq!(resp["result"], "0x42");
    }
}

#[tokio::test]
async fn ws_batch_request() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start(default_api()).await;
    let mut ws = connect_with_header(&ctx).await;

    let batch = serde_json::json!([
        {"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1},
        {"jsonrpc":"2.0","method":"eth_chainId","params":[],"id":2},
    ]);
    ws.send(tungstenite::Message::Text(batch.to_string().into()))
        .await
        .unwrap();

    let resp = parse_response(ws.next().await.unwrap().unwrap());
    let arr = resp.as_array().expect("expected batch response array");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["id"], 1);
    assert_eq!(arr[0]["result"], "0x42");
    assert_eq!(arr[1]["id"], 2);
    assert_eq!(arr[1]["result"], "0x1");
}

#[tokio::test]
async fn ws_invalid_json() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start(default_api()).await;
    let mut ws = connect_with_header(&ctx).await;

    ws.send(tungstenite::Message::Text("{broken".into()))
        .await
        .unwrap();
    let resp = parse_response(ws.next().await.unwrap().unwrap());

    assert_eq!(resp["error"]["code"], -32700);
}

#[tokio::test]
async fn ws_unknown_method() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start(default_api()).await;
    let mut ws = connect_with_header(&ctx).await;

    ws.send(tungstenite::Message::Text(jsonrpc("eth_foobar", 1).into()))
        .await
        .unwrap();
    let resp = parse_response(ws.next().await.unwrap().unwrap());

    assert_eq!(resp["id"], 1);
    assert_eq!(resp["error"]["code"], -32601);
}

#[tokio::test]
async fn ws_disabled_method() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start(default_api()).await;
    let mut ws = connect_with_header(&ctx).await;

    ws.send(tungstenite::Message::Text(
        jsonrpc("eth_subscribe", 1).into(),
    ))
    .await
    .unwrap();
    let resp = parse_response(ws.next().await.unwrap().unwrap());

    assert_eq!(resp["id"], 1);
    assert_eq!(resp["error"]["code"], -32006);
}

#[tokio::test]
async fn ws_empty_batch() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start(default_api()).await;
    let mut ws = connect_with_header(&ctx).await;

    ws.send(tungstenite::Message::Text("[]".into()))
        .await
        .unwrap();
    let resp = parse_response(ws.next().await.unwrap().unwrap());

    assert_eq!(resp["error"]["code"], -32700);
}

#[tokio::test]
async fn ws_roundtrip_with_p256_auth() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start(default_api()).await;
    let signing_key = P256SigningKey::random(&mut thread_rng());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (fields, digest) = build_token_fields(ZONE_ID, CHAIN_ID, PORTAL, now, now + 600);
    let token = build_token_with_signature(
        sign_p256_token(digest, &signing_key).expect("p256 signing should succeed"),
        &fields,
    );
    let mut ws = connect_with_token(&ctx.ws_url(), ctx.addr, &token)
        .await
        .expect("p256 ws connect failed");

    ws.send(tungstenite::Message::Text(
        jsonrpc("eth_blockNumber", 9).into(),
    ))
    .await
    .unwrap();
    let resp = parse_response(ws.next().await.unwrap().unwrap());

    assert_eq!(resp["id"], 9);
    assert_eq!(resp["result"], "0x42");
}

#[tokio::test]
async fn ws_roundtrip_with_webauthn_auth() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start(default_api()).await;
    let signing_key = P256SigningKey::random(&mut thread_rng());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (fields, digest) = build_token_fields(ZONE_ID, CHAIN_ID, PORTAL, now, now + 600);
    let token = build_token_with_signature(
        sign_webauthn_token(&signing_key, digest).expect("webauthn signing should succeed"),
        &fields,
    );
    let mut ws = connect_with_token(&ctx.ws_url(), ctx.addr, &token)
        .await
        .expect("webauthn ws connect failed");

    ws.send(tungstenite::Message::Text(
        jsonrpc("eth_chainId", 10).into(),
    ))
    .await
    .unwrap();
    let resp = parse_response(ws.next().await.unwrap().unwrap());

    assert_eq!(resp["id"], 10);
    assert_eq!(resp["result"], "0x1");
}

#[tokio::test]
async fn ws_roundtrip_with_keychain_auth() {
    let _guard = test_lock().lock().await;
    let root_account = Address::repeat_byte(0x55);
    let access_signer = P256SigningKey::random(&mut thread_rng());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (fields, digest) = build_token_fields(ZONE_ID, CHAIN_ID, PORTAL, now, now + 600);
    let (signature, key_id) =
        sign_keychain_token(digest, &access_signer, root_account, 0x04).unwrap();
    let ctx = TestContext::start(MockZoneRpcApi::with_key(
        root_account,
        key_id,
        KeyInfoSignatureType::P256,
    ))
    .await;
    let token = build_token_with_signature(signature, &fields);
    let mut ws = connect_with_token(&ctx.ws_url(), ctx.addr, &token)
        .await
        .expect("authorized keychain ws connect failed");

    ws.send(tungstenite::Message::Text(
        jsonrpc("eth_blockNumber", 11).into(),
    ))
    .await
    .unwrap();
    let resp = parse_response(ws.next().await.unwrap().unwrap());

    assert_eq!(resp["id"], 11);
    assert_eq!(resp["result"], "0x42");
}

#[tokio::test]
async fn ws_reject_unauthorized_keychain_token() {
    let _guard = test_lock().lock().await;
    let root_account = Address::repeat_byte(0x44);
    let access_signer = P256SigningKey::random(&mut thread_rng());
    let ctx = TestContext::start(default_api()).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (fields, digest) = build_token_fields(ZONE_ID, CHAIN_ID, PORTAL, now, now + 600);
    let (signature, _key_id) =
        sign_keychain_token(digest, &access_signer, root_account, 0x04).unwrap();
    let token = build_token_with_signature(signature, &fields);

    let err = connect_with_token(&ctx.ws_url(), ctx.addr, &token)
        .await
        .expect_err("missing keychain authorization should fail");
    let tungstenite::Error::Http(response) = err else {
        panic!("expected HTTP error, got {err:?}");
    };
    assert_eq!(response.status(), 403);
}

#[tokio::test]
async fn ws_keychain_lookup_failure_returns_500() {
    let _guard = test_lock().lock().await;
    let root_account = Address::repeat_byte(0x66);
    let access_signer = P256SigningKey::random(&mut thread_rng());
    let ctx = TestContext::start(MockZoneRpcApi::with_key_lookup_error("key lookup failed")).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (fields, digest) = build_token_fields(ZONE_ID, CHAIN_ID, PORTAL, now, now + 600);
    let (signature, _key_id) =
        sign_keychain_token(digest, &access_signer, root_account, 0x04).unwrap();
    let token = build_token_with_signature(signature, &fields);

    let err = connect_with_token(&ctx.ws_url(), ctx.addr, &token)
        .await
        .expect_err("keychain lookup failure should fail");
    let tungstenite::Error::Http(response) = err else {
        panic!("expected HTTP error, got {err:?}");
    };
    assert_eq!(response.status(), 500);
}
