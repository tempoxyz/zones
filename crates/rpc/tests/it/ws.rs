use std::sync::{Arc, OnceLock};

use alloy_primitives::Address;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use futures::{SinkExt, StreamExt};
use metrics_util::{
    CompositeKey, MetricKind,
    debugging::{DebugValue, DebuggingRecorder, Snapshotter},
};
use reth_metrics::metrics::{Label, SharedString, Unit};
use serde_json::Value;
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

struct MockZoneRpcApi;

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

    fn zone_get_authorization_token_info(&self, auth: zone_rpc::auth::AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            zone_rpc::types::to_raw(&serde_json::json!({
                "account": auth.caller,
                "expiresAt": alloy_primitives::U64::from(auth.expires_at),
            }))
        })
    }

    fn zone_get_zone_info(&self, _auth: zone_rpc::auth::AuthContext) -> BoxFut<'_> {
        Box::pin(async move {
            zone_rpc::types::to_raw(&serde_json::json!({
                "zoneId": "0x1",
                "zoneToken": format!("{:#x}", Address::repeat_byte(0x11)),
                "sequencer": format!("{:#x}", Address::repeat_byte(0x22)),
                "chainId": "0x2a",
            }))
        })
    }

    fn zone_get_deposit_status(
        &self,
        tempo_block_number: u64,
        _auth: zone_rpc::auth::AuthContext,
    ) -> BoxFut<'_> {
        Box::pin(async move {
            zone_rpc::types::to_raw(&serde_json::json!({
                "tempoBlockNumber": alloy_primitives::U64::from(tempo_block_number),
                "zoneProcessedThrough": alloy_primitives::U64::from(tempo_block_number),
                "processed": true,
                "deposits": [],
            }))
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
        let addr = start_private_rpc(config, Arc::new(MockZoneRpcApi))
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

    fn http_url(&self) -> String {
        format!("http://{}", self.addr)
    }
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

type SnapshotEntry = (
    CompositeKey,
    (Option<Unit>, Option<SharedString>, DebugValue),
);
// `CompositeKey` trips clippy's `mutable_key_type`, so these tests keep
// snapshot data as a flat list and do linear lookups.
type SnapshotEntries = Vec<SnapshotEntry>;

fn test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn snapshotter() -> &'static Snapshotter {
    static SNAPSHOTTER: OnceLock<Snapshotter> = OnceLock::new();

    SNAPSHOTTER.get_or_init(|| {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let _ = recorder.install();
        snapshotter
    })
}

fn snapshot_metrics() -> SnapshotEntries {
    snapshotter()
        .snapshot()
        .into_hashmap()
        .into_iter()
        .collect()
}

fn metric_value<'a>(
    snapshot: &'a SnapshotEntries,
    kind: MetricKind,
    name: &str,
    labels: &[(&str, &str)],
) -> &'a DebugValue {
    snapshot
        .iter()
        .find(|(key, _)| {
            key.kind() == kind
                && key.key().name() == name
                && labels_match(key.key().labels(), labels)
        })
        .map(|(_, (_, _, value))| value)
        .unwrap_or_else(|| panic!("metric {name} with labels {labels:?} not found"))
}

fn labels_match<'a>(labels: impl Iterator<Item = &'a Label>, expected: &[(&str, &str)]) -> bool {
    let mut actual: Vec<_> = labels.map(|label| (label.key(), label.value())).collect();
    let mut expected = expected.to_vec();
    actual.sort_unstable();
    expected.sort_unstable();
    actual == expected
}

fn counter(snapshot: &SnapshotEntries, name: &str, labels: &[(&str, &str)]) -> u64 {
    match metric_value(snapshot, MetricKind::Counter, name, labels) {
        DebugValue::Counter(value) => *value,
        other => panic!("expected counter for {name}, got {other:?}"),
    }
}

fn try_counter(snapshot: &SnapshotEntries, name: &str, labels: &[(&str, &str)]) -> Option<u64> {
    snapshot
        .iter()
        .find(|(key, _)| {
            key.kind() == MetricKind::Counter
                && key.key().name() == name
                && labels_match(key.key().labels(), labels)
        })
        .map(|(_, (_, _, value))| match value {
            DebugValue::Counter(value) => *value,
            other => panic!("expected counter for {name}, got {other:?}"),
        })
}

fn gauge(snapshot: &SnapshotEntries, name: &str, labels: &[(&str, &str)]) -> f64 {
    match metric_value(snapshot, MetricKind::Gauge, name, labels) {
        DebugValue::Gauge(value) => value.into_inner(),
        other => panic!("expected gauge for {name}, got {other:?}"),
    }
}

fn try_gauge(snapshot: &SnapshotEntries, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
    snapshot
        .iter()
        .find(|(key, _)| {
            key.kind() == MetricKind::Gauge
                && key.key().name() == name
                && labels_match(key.key().labels(), labels)
        })
        .map(|(_, (_, _, value))| match value {
            DebugValue::Gauge(value) => value.into_inner(),
            other => panic!("expected gauge for {name}, got {other:?}"),
        })
}

fn histogram_len(snapshot: &SnapshotEntries, name: &str, labels: &[(&str, &str)]) -> usize {
    match metric_value(snapshot, MetricKind::Histogram, name, labels) {
        DebugValue::Histogram(values) => values.len(),
        other => panic!("expected histogram for {name}, got {other:?}"),
    }
}

async fn wait_for_snapshot(mut predicate: impl FnMut(&SnapshotEntries) -> bool) -> SnapshotEntries {
    for _ in 0..50 {
        tokio::task::yield_now().await;
        let snapshot = snapshot_metrics();
        if predicate(&snapshot) {
            return snapshot;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    panic!("timed out waiting for expected metrics to be recorded");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ws_roundtrip_with_header_auth() {
    let _guard = test_lock().lock().await;
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
    let _guard = test_lock().lock().await;
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
    let _guard = test_lock().lock().await;
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
    let _guard = test_lock().lock().await;
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
    let _guard = test_lock().lock().await;
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
    let _guard = test_lock().lock().await;
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
async fn ws_zone_method_roundtrip() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start().await;
    let mut ws = connect_with_header(&ctx).await;

    ws.send(tungstenite::Message::Text(
        serde_json::json!({
            "jsonrpc":"2.0",
            "method":"zone_getDepositStatus",
            "params":["0x2a"],
            "id":7
        })
        .to_string()
        .into(),
    ))
    .await
    .unwrap();

    let resp = parse_response(ws.next().await.unwrap().unwrap());
    assert_eq!(resp["id"], 7);
    assert_eq!(resp["result"]["tempoBlockNumber"], "0x2a");
    assert_eq!(resp["result"]["processed"], true);
}

#[tokio::test]
async fn ws_invalid_json() {
    let _guard = test_lock().lock().await;
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
    let _guard = test_lock().lock().await;
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
async fn ws_disabled_method() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start().await;
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
    let ctx = TestContext::start().await;
    let mut ws = connect_with_header(&ctx).await;

    ws.send(tungstenite::Message::Text("[]".into()))
        .await
        .unwrap();
    let resp = parse_response(ws.next().await.unwrap().unwrap());

    assert_eq!(resp["error"]["code"], -32700);
}

#[tokio::test]
async fn http_auth_failure_records_metric() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start().await;
    let _ = snapshotter().snapshot();

    let response = reqwest::Client::new()
        .post(ctx.http_url())
        .body(jsonrpc("eth_blockNumber", 1))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);

    let snapshot = snapshot_metrics();
    assert_eq!(
        counter(
            &snapshot,
            "zone_private_rpc.auth.failures_total",
            &[("transport", "http"), ("reason", "missing")],
        ),
        1
    );
}

#[tokio::test]
async fn ws_auth_failure_records_metric() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start().await;
    let _ = snapshotter().snapshot();

    let err = connect_async(ctx.ws_url())
        .await
        .expect_err("should fail without auth");
    let tungstenite::Error::Http(response) = err else {
        panic!("expected HTTP error, got {err:?}");
    };
    assert_eq!(response.status(), 401);

    let snapshot = snapshot_metrics();
    assert_eq!(
        counter(
            &snapshot,
            "zone_private_rpc.auth.failures_total",
            &[("transport", "ws"), ("reason", "missing")],
        ),
        1
    );
}

#[tokio::test]
async fn http_call_metrics_record_success_and_unknown_method() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start().await;
    let token = ctx.build_token();
    let client = reqwest::Client::new();
    let _ = snapshotter().snapshot();

    let ok = client
        .post(ctx.http_url())
        .header("x-authorization-token", &token)
        .body(jsonrpc("eth_blockNumber", 1))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), reqwest::StatusCode::OK);

    let err = client
        .post(ctx.http_url())
        .header("x-authorization-token", &token)
        .body(jsonrpc("eth_foobar", 2))
        .send()
        .await
        .unwrap();
    assert_eq!(err.status(), reqwest::StatusCode::OK);

    let snapshot = snapshot_metrics();
    assert_eq!(
        counter(
            &snapshot,
            "zone_private_rpc.calls.started_total",
            &[("transport", "http"), ("method", "eth_blockNumber")],
        ),
        1
    );
    assert_eq!(
        counter(
            &snapshot,
            "zone_private_rpc.calls.successful_total",
            &[("transport", "http"), ("method", "eth_blockNumber")],
        ),
        1
    );
    assert_eq!(
        counter(
            &snapshot,
            "zone_private_rpc.calls.failed_total",
            &[("transport", "http"), ("method", "unknown")],
        ),
        1
    );
    assert_eq!(
        histogram_len(
            &snapshot,
            "zone_private_rpc.calls.time_seconds",
            &[("transport", "http"), ("method", "eth_blockNumber")],
        ),
        1
    );
    assert_eq!(
        histogram_len(
            &snapshot,
            "zone_private_rpc.calls.time_seconds",
            &[("transport", "http"), ("method", "unknown")],
        ),
        1
    );
}

#[tokio::test]
async fn ws_call_metrics_record_batch_items_and_admin_label() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start().await;
    let mut ws = connect_with_header(&ctx).await;
    let _ = snapshotter().snapshot();

    let batch = serde_json::json!([
        {"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1},
        {"jsonrpc":"2.0","method":"admin_peers","params":[],"id":2},
    ]);

    ws.send(tungstenite::Message::Text(batch.to_string().into()))
        .await
        .unwrap();

    let resp = parse_response(ws.next().await.unwrap().unwrap());
    let arr = resp.as_array().expect("expected batch response array");
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["result"], "0x42");
    assert_eq!(arr[1]["error"]["code"], -32603);

    let snapshot = snapshot_metrics();
    assert_eq!(
        counter(
            &snapshot,
            "zone_private_rpc.calls.started_total",
            &[("transport", "ws"), ("method", "eth_blockNumber")],
        ),
        1
    );
    assert_eq!(
        counter(
            &snapshot,
            "zone_private_rpc.calls.successful_total",
            &[("transport", "ws"), ("method", "eth_blockNumber")],
        ),
        1
    );
    assert_eq!(
        counter(
            &snapshot,
            "zone_private_rpc.calls.failed_total",
            &[("transport", "ws"), ("method", "admin_*")],
        ),
        1
    );
    assert_eq!(
        histogram_len(
            &snapshot,
            "zone_private_rpc.calls.time_seconds",
            &[("transport", "ws"), ("method", "admin_*")],
        ),
        1
    );
}

#[tokio::test]
async fn ws_session_metrics_track_connect_and_disconnect() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start().await;
    let _ = snapshotter().snapshot();

    let mut ws = connect_with_header(&ctx).await;
    let open_snapshot = wait_for_snapshot(|snapshot| {
        matches!(
            try_gauge(snapshot, "zone_private_rpc.ws.sessions_active", &[]),
            Some(1.0)
        ) && try_counter(snapshot, "zone_private_rpc.ws.sessions_opened_total", &[]) == Some(1)
    })
    .await;
    assert_eq!(
        gauge(&open_snapshot, "zone_private_rpc.ws.sessions_active", &[]),
        1.0
    );
    assert_eq!(
        counter(
            &open_snapshot,
            "zone_private_rpc.ws.sessions_opened_total",
            &[],
        ),
        1
    );

    ws.close(None).await.unwrap();
    drop(ws);

    let close_snapshot = wait_for_snapshot(|snapshot| {
        matches!(
            try_gauge(snapshot, "zone_private_rpc.ws.sessions_active", &[]),
            Some(0.0)
        ) && try_counter(
            snapshot,
            "zone_private_rpc.ws.disconnects_total",
            &[("reason", "client_close")],
        ) == Some(1)
    })
    .await;
    assert_eq!(
        gauge(&close_snapshot, "zone_private_rpc.ws.sessions_active", &[]),
        0.0
    );
    assert_eq!(
        counter(
            &close_snapshot,
            "zone_private_rpc.ws.disconnects_total",
            &[("reason", "client_close")],
        ),
        1
    );
}
