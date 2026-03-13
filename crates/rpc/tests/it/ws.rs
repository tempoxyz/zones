use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use alloy_primitives::{Address, B256, b256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite};
use zone_rpc::{
    PrivateRpcConfig,
    auth::build_token_fields,
    handlers::ZoneRpcApi,
    start_private_rpc,
    types::{BoxFut, JsonRpcError},
};

// ---------------------------------------------------------------------------
// Mock API
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MockZoneRpcApi {
    next_filter_id: AtomicU64,
    filters: Mutex<HashMap<alloy_rpc_types_eth::FilterId, MockFilterKind>>,
}

enum MockFilterKind {
    Logs { emitted: bool },
    Blocks { emitted: bool },
}

impl MockZoneRpcApi {
    fn next_filter_id(&self, prefix: &str) -> alloy_rpc_types_eth::FilterId {
        alloy_rpc_types_eth::FilterId::from(format!(
            "{prefix}-{}",
            self.next_filter_id.fetch_add(1, Ordering::Relaxed)
        ))
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

    fn block_by_hash(
        &self,
        hash: alloy_primitives::B256,
        _b: bool,
        _c: zone_rpc::auth::AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            zone_rpc::types::to_raw(&json!({
                "hash": format!("{hash:#x}"),
                "number": "0x42",
                "parentHash": format!("{:#x}", B256::ZERO),
                "logsBloom": format!("0x{}", "0".repeat(512)),
                "transactions": [],
                "uncles": [],
            }))
        })
    }

    stub!(transaction_by_hash, _a: alloy_primitives::B256, _c: zone_rpc::auth::AuthContext);
    stub!(transaction_receipt, _a: alloy_primitives::B256, _c: zone_rpc::auth::AuthContext);
    stub!(call, _a: tempo_alloy::rpc::TempoTransactionRequest, _b: Option<alloy_rpc_types_eth::BlockId>, _c: Option<alloy_rpc_types_eth::state::StateOverride>, _d: zone_rpc::auth::AuthContext);
    stub!(estimate_gas, _a: tempo_alloy::rpc::TempoTransactionRequest, _b: Option<alloy_rpc_types_eth::BlockId>, _c: Option<alloy_rpc_types_eth::state::StateOverride>, _d: zone_rpc::auth::AuthContext);
    stub!(send_raw_transaction, _a: alloy_primitives::Bytes, _c: zone_rpc::auth::AuthContext);
    stub!(send_raw_transaction_sync, _a: alloy_primitives::Bytes, _c: zone_rpc::auth::AuthContext);
    stub!(fill_transaction, _a: tempo_alloy::rpc::TempoTransactionRequest, _c: zone_rpc::auth::AuthContext);
    stub!(get_logs, _a: alloy_rpc_types_eth::Filter, _c: zone_rpc::auth::AuthContext);

    fn new_filter(
        &self,
        _a: alloy_rpc_types_eth::Filter,
        _c: zone_rpc::auth::AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            let id = self.next_filter_id("logs");
            self.filters
                .lock()
                .await
                .insert(id.clone(), MockFilterKind::Logs { emitted: false });
            zone_rpc::types::to_raw(&id)
        })
    }

    stub!(get_filter_logs, _a: alloy_rpc_types_eth::FilterId, _c: zone_rpc::auth::AuthContext);

    fn get_filter_changes(
        &self,
        id: alloy_rpc_types_eth::FilterId,
        _c: zone_rpc::auth::AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            let mut filters = self.filters.lock().await;
            let Some(kind) = filters.get_mut(&id) else {
                return Err(JsonRpcError::invalid_params("filter not found"));
            };

            match kind {
                MockFilterKind::Logs { emitted } => {
                    if *emitted {
                        zone_rpc::types::to_raw(&Vec::<Value>::new())
                    } else {
                        *emitted = true;
                        zone_rpc::types::to_raw(&vec![json!({
                            "address": format!("{:#x}", Address::ZERO),
                            "topics": [format!("{:#x}", b256!("0x1111111111111111111111111111111111111111111111111111111111111111"))],
                            "data": "0x",
                            "blockHash": format!("{:#x}", b256!("0x2222222222222222222222222222222222222222222222222222222222222222")),
                            "blockNumber": "0x42",
                            "transactionHash": format!("{:#x}", b256!("0x3333333333333333333333333333333333333333333333333333333333333333")),
                            "transactionIndex": "0x0",
                            "logIndex": "0x0",
                            "removed": false
                        })])
                    }
                }
                MockFilterKind::Blocks { emitted } => {
                    if *emitted {
                        zone_rpc::types::to_raw(&Vec::<B256>::new())
                    } else {
                        *emitted = true;
                        zone_rpc::types::to_raw(&vec![b256!(
                            "0x4444444444444444444444444444444444444444444444444444444444444444"
                        )])
                    }
                }
            }
        })
    }

    fn new_block_filter(&self, _c: zone_rpc::auth::AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            let id = self.next_filter_id("blocks");
            self.filters
                .lock()
                .await
                .insert(id.clone(), MockFilterKind::Blocks { emitted: false });
            zone_rpc::types::to_raw(&id)
        })
    }

    fn uninstall_filter(
        &self,
        id: alloy_rpc_types_eth::FilterId,
        _c: zone_rpc::auth::AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            let removed = self.filters.lock().await.remove(&id).is_some();
            zone_rpc::types::to_raw(&removed)
        })
    }
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
    async fn start() -> Self {
        let signer = PrivateKeySigner::random();
        let config = PrivateRpcConfig {
            listen_addr: ([127, 0, 0, 1], 0).into(),
            zone_id: ZONE_ID,
            chain_id: CHAIN_ID,
            zone_portal: PORTAL,
            sequencer: signer.address(),
        };
        let addr = start_private_rpc(config, Arc::new(MockZoneRpcApi::default()))
            .await
            .unwrap();
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

/// Build a JSON-RPC request string.
fn jsonrpc(method: &str, id: u64) -> String {
    serde_json::json!({"jsonrpc":"2.0","method":method,"params":[],"id":id}).to_string()
}

fn jsonrpc_with_params(method: &str, params: Value, id: u64) -> String {
    serde_json::json!({"jsonrpc":"2.0","method":method,"params":params,"id":id}).to_string()
}

/// Connect to the WS endpoint using the X-Authorization-Token header.
async fn connect_with_header(
    ctx: &TestContext,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let token = ctx.build_token();
    let req = tungstenite::http::Request::builder()
        .uri(ctx.ws_url())
        .header("x-authorization-token", &token)
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

    let (ws, _) = connect_async(req).await.expect("ws connect failed");
    ws
}

/// Parse a JSON-RPC response from a WS text message.
fn parse_response(msg: tungstenite::Message) -> Value {
    match msg {
        tungstenite::Message::Text(t) => serde_json::from_str(&t).expect("invalid json"),
        other => panic!("expected text message, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ws_roundtrip_with_header_auth() {
    let ctx = TestContext::start().await;
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
    let ctx = TestContext::start().await;
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
    let ctx = TestContext::start().await;
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
    let ctx = TestContext::start().await;
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
    let ctx = TestContext::start().await;
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
    let ctx = TestContext::start().await;
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
    let ctx = TestContext::start().await;
    let mut ws = connect_with_header(&ctx).await;

    ws.send(tungstenite::Message::Text("{broken".into()))
        .await
        .unwrap();
    let resp = parse_response(ws.next().await.unwrap().unwrap());

    assert_eq!(resp["error"]["code"], -32700);
}

#[tokio::test]
async fn ws_unknown_method() {
    let ctx = TestContext::start().await;
    let mut ws = connect_with_header(&ctx).await;

    ws.send(tungstenite::Message::Text(jsonrpc("eth_foobar", 1).into()))
        .await
        .unwrap();
    let resp = parse_response(ws.next().await.unwrap().unwrap());

    assert_eq!(resp["id"], 1);
    assert_eq!(resp["error"]["code"], -32601);
}

#[tokio::test]
async fn ws_subscribe_logs_emits_notifications() {
    let ctx = TestContext::start().await;
    let mut ws = connect_with_header(&ctx).await;

    ws.send(tungstenite::Message::Text(
        jsonrpc_with_params("eth_subscribe", json!(["logs", {}]), 1).into(),
    ))
    .await
    .unwrap();
    let resp = parse_response(ws.next().await.unwrap().unwrap());

    assert_eq!(resp["id"], 1);
    let subscription_id = resp["result"]
        .as_str()
        .expect("subscription id")
        .to_string();

    let notification = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("timed out waiting for log notification")
        .unwrap()
        .unwrap();
    let notification = parse_response(notification);

    assert_eq!(notification["method"], "eth_subscription");
    assert_eq!(notification["params"]["subscription"], subscription_id);
    assert_eq!(notification["params"]["result"]["blockNumber"], "0x42");
}

#[tokio::test]
async fn ws_subscribe_new_heads_emits_redacted_headers() {
    let ctx = TestContext::start().await;
    let mut ws = connect_with_header(&ctx).await;

    ws.send(tungstenite::Message::Text(
        jsonrpc_with_params("eth_subscribe", json!(["newHeads"]), 1).into(),
    ))
    .await
    .unwrap();
    let resp = parse_response(ws.next().await.unwrap().unwrap());

    assert_eq!(resp["id"], 1);
    let subscription_id = resp["result"]
        .as_str()
        .expect("subscription id")
        .to_string();

    let notification = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("timed out waiting for newHeads notification")
        .unwrap()
        .unwrap();
    let notification = parse_response(notification);

    assert_eq!(notification["method"], "eth_subscription");
    assert_eq!(notification["params"]["subscription"], subscription_id);
    assert_eq!(
        notification["params"]["result"]["hash"],
        format!(
            "{:#x}",
            b256!("0x4444444444444444444444444444444444444444444444444444444444444444")
        )
    );
    assert!(
        notification["params"]["result"]
            .get("transactions")
            .is_none()
    );
    assert!(notification["params"]["result"].get("uncles").is_none());
}

#[tokio::test]
async fn ws_unsubscribe_removes_subscription() {
    let ctx = TestContext::start().await;
    let mut ws = connect_with_header(&ctx).await;

    ws.send(tungstenite::Message::Text(
        jsonrpc_with_params("eth_subscribe", json!(["logs", {}]), 1).into(),
    ))
    .await
    .unwrap();
    let resp = parse_response(ws.next().await.unwrap().unwrap());
    let subscription_id = resp["result"].clone();

    ws.send(tungstenite::Message::Text(
        jsonrpc_with_params("eth_unsubscribe", json!([subscription_id]), 2).into(),
    ))
    .await
    .unwrap();
    let resp = loop {
        let message = parse_response(ws.next().await.unwrap().unwrap());
        if message["id"] == 2 {
            break message;
        }
    };

    assert_eq!(resp["id"], 2);
    assert_eq!(resp["result"], true);
}

#[tokio::test]
async fn ws_rejects_unsupported_subscription_kind() {
    let ctx = TestContext::start().await;
    let mut ws = connect_with_header(&ctx).await;

    ws.send(tungstenite::Message::Text(
        jsonrpc_with_params("eth_subscribe", json!(["newPendingTransactions"]), 1).into(),
    ))
    .await
    .unwrap();
    let resp = parse_response(ws.next().await.unwrap().unwrap());

    assert_eq!(resp["id"], 1);
    assert_eq!(resp["error"]["code"], -32006);
}

#[tokio::test]
async fn ws_empty_batch() {
    let ctx = TestContext::start().await;
    let mut ws = connect_with_header(&ctx).await;

    ws.send(tungstenite::Message::Text("[]".into()))
        .await
        .unwrap();
    let resp = parse_response(ws.next().await.unwrap().unwrap());

    assert_eq!(resp["error"]["code"], -32700);
}
