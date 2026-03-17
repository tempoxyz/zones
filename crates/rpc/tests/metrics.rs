use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_primitives::Address;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use futures::{SinkExt, StreamExt};
use metrics_util::{
    CompositeKey, MetricKind,
    debugging::{DebugValue, DebuggingRecorder, Snapshotter},
};
use parking_lot::Mutex;
use reth_metrics::metrics::{Label, SharedString, Unit};
use serde_json::Value;
use tempo_contracts::precompiles::account_keychain::IAccountKeychain::{
    KeyInfo, SignatureType as KeyInfoSignatureType,
};
use tokio_tungstenite::{connect_async, tungstenite};
use zone_rpc::{
    PrivateRpcConfig,
    auth::build_token_fields,
    handlers::ZoneRpcApi,
    start_private_rpc,
    types::{BoxEyreFut, BoxFut, JsonRpcError},
};

#[derive(Default)]
struct MockZoneRpcApi {
    key_infos: Mutex<HashMap<(Address, Address), KeyInfo>>,
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
        let now = now_secs();
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

fn jsonrpc(method: &str, id: u64) -> String {
    serde_json::json!({"jsonrpc":"2.0","method":method,"params":[],"id":id}).to_string()
}

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

fn parse_response(msg: tungstenite::Message) -> Value {
    match msg {
        tungstenite::Message::Text(t) => serde_json::from_str(&t).expect("invalid json"),
        other => panic!("expected text message, got {other:?}"),
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_secs()
}

type SnapshotEntry = (
    CompositeKey,
    (Option<Unit>, Option<SharedString>, DebugValue),
);
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

fn clear_metrics() {
    let _ = snapshotter().snapshot();
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
) -> Option<&'a DebugValue> {
    snapshot
        .iter()
        .find(|(key, _)| {
            key.kind() == kind
                && key.key().name() == name
                && labels_match(key.key().labels(), labels)
        })
        .map(|(_, (_, _, value))| value)
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
        Some(DebugValue::Counter(value)) => *value,
        Some(other) => panic!("expected counter for {name}, got {other:?}"),
        None => 0,
    }
}

fn histogram_len(snapshot: &SnapshotEntries, name: &str, labels: &[(&str, &str)]) -> usize {
    match metric_value(snapshot, MetricKind::Histogram, name, labels) {
        Some(DebugValue::Histogram(values)) => values.len(),
        Some(other) => panic!("expected histogram for {name}, got {other:?}"),
        None => 0,
    }
}

fn gauge(snapshot: &SnapshotEntries, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
    metric_value(snapshot, MetricKind::Gauge, name, labels).map(|value| match value {
        DebugValue::Gauge(value) => value.into_inner(),
        other => panic!("expected gauge for {name}, got {other:?}"),
    })
}

async fn wait_for_snapshot(mut predicate: impl FnMut(&SnapshotEntries) -> bool) -> SnapshotEntries {
    for _ in 0..200 {
        tokio::task::yield_now().await;
        let snapshot = snapshot_metrics();
        if predicate(&snapshot) {
            return snapshot;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    panic!("timed out waiting for expected metrics to be recorded");
}

#[tokio::test]
async fn http_auth_failure_records_metric() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start(MockZoneRpcApi::default()).await;
    clear_metrics();

    let response = reqwest::Client::new()
        .post(ctx.http_url())
        .body(jsonrpc("eth_blockNumber", 1))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);

    let snapshot = wait_for_snapshot(|snapshot| {
        counter(
            snapshot,
            "zone_private_rpc.auth.failures_total",
            &[("transport", "http"), ("reason", "missing")],
        ) == 1
    })
    .await;
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
    let ctx = TestContext::start(MockZoneRpcApi::default()).await;
    clear_metrics();

    let err = connect_async(ctx.ws_url())
        .await
        .expect_err("should fail without auth");
    let tungstenite::Error::Http(response) = err else {
        panic!("expected HTTP error, got {err:?}");
    };
    assert_eq!(response.status(), 401);

    let snapshot = wait_for_snapshot(|snapshot| {
        counter(
            snapshot,
            "zone_private_rpc.auth.failures_total",
            &[("transport", "ws"), ("reason", "missing")],
        ) == 1
    })
    .await;
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
    let ctx = TestContext::start(MockZoneRpcApi::default()).await;
    let token = ctx.build_token();
    let client = reqwest::Client::new();
    clear_metrics();

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

    let snapshot = wait_for_snapshot(|snapshot| {
        counter(
            snapshot,
            "zone_private_rpc.calls.started_total",
            &[("transport", "http"), ("method", "eth_blockNumber")],
        ) == 1
            && counter(
                snapshot,
                "zone_private_rpc.calls.failed_total",
                &[("transport", "http"), ("method", "unknown")],
            ) == 1
    })
    .await;

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
    let ctx = TestContext::start(MockZoneRpcApi::default()).await;
    let mut ws = connect_with_header(&ctx).await;
    clear_metrics();

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

    let snapshot = wait_for_snapshot(|snapshot| {
        counter(
            snapshot,
            "zone_private_rpc.calls.started_total",
            &[("transport", "ws"), ("method", "eth_blockNumber")],
        ) == 1
            && counter(
                snapshot,
                "zone_private_rpc.calls.failed_total",
                &[("transport", "ws"), ("method", "admin_*")],
            ) == 1
    })
    .await;

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
    ws.close(None).await.unwrap();
}

#[tokio::test]
async fn ws_session_metrics_track_connect_and_disconnect() {
    let _guard = test_lock().lock().await;
    let ctx = TestContext::start(MockZoneRpcApi::default()).await;
    clear_metrics();
    let mut ws = connect_with_header(&ctx).await;

    ws.send(tungstenite::Message::Text(
        jsonrpc("eth_blockNumber", 99).into(),
    ))
    .await
    .unwrap();
    let _ = parse_response(ws.next().await.unwrap().unwrap());

    let open_snapshot = wait_for_snapshot(|snapshot| {
        gauge(snapshot, "zone_private_rpc.ws.sessions_active", &[]) == Some(1.0)
            && counter(snapshot, "zone_private_rpc.ws.sessions_opened_total", &[]) == 1
    })
    .await;

    assert_eq!(
        gauge(&open_snapshot, "zone_private_rpc.ws.sessions_active", &[]),
        Some(1.0)
    );
    assert_eq!(
        counter(
            &open_snapshot,
            "zone_private_rpc.ws.sessions_opened_total",
            &[]
        ),
        1
    );

    ws.close(None).await.unwrap();
    drop(ws);

    let close_snapshot = wait_for_snapshot(|snapshot| {
        gauge(snapshot, "zone_private_rpc.ws.sessions_active", &[]) == Some(0.0)
            && counter(
                snapshot,
                "zone_private_rpc.ws.disconnects_total",
                &[("reason", "client_close")],
            ) == 1
    })
    .await;

    assert_eq!(
        gauge(&close_snapshot, "zone_private_rpc.ws.sessions_active", &[]),
        Some(0.0)
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
