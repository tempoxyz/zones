//! WebSocket transport for the private zone RPC server.
//!
//! Clients authenticate via the `X-Authorization-Token` header (preferred) or
//! a `?token=0x<hex>` query parameter (for browser clients that cannot set
//! custom headers on the upgrade request).
//!
//! Auth is validated once during the HTTP upgrade handshake — individual
//! messages are not re-authenticated since WS frames don't carry auth headers.

use alloy_primitives::B256;
use alloy_rpc_types_eth::{
    Filter, FilterId,
    pubsub::{Params as SubscriptionParams, SubscriptionKind},
};
use axum::{
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::IntoResponse,
};
use futures::{SinkExt, stream::StreamExt};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{self, MissedTickBehavior},
};
use tracing::warn;

use crate::{
    auth::{self, AuthContext, AuthError},
    handlers::{self, ZoneRpcApi},
    server::{MAX_BATCH_SIZE, RpcState, auth_error_status, authenticate_token},
    types::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, to_raw},
};

/// Maximum WebSocket message size (1 MiB).
const MAX_WS_MESSAGE_SIZE: usize = 1 << 20;
const WS_SUBSCRIPTION_POLL_INTERVAL: Duration = Duration::from_millis(250);

type NotificationTx = mpsc::UnboundedSender<String>;

/// Query parameters for the WebSocket upgrade endpoint.
#[derive(serde::Deserialize, Default)]
pub(crate) struct WsQuery {
    /// Auth token passed as query param (fallback when headers are unavailable).
    token: Option<String>,
}

struct ActiveSubscription {
    upstream_filter_id: FilterId,
    task: JoinHandle<()>,
}

struct WsSession {
    next_subscription_id: u64,
    subscriptions: HashMap<FilterId, ActiveSubscription>,
}

impl Default for WsSession {
    fn default() -> Self {
        Self {
            next_subscription_id: 1,
            subscriptions: HashMap::new(),
        }
    }
}

impl WsSession {
    fn next_subscription_id(&mut self) -> FilterId {
        let id = FilterId::from(format!("0x{:x}", self.next_subscription_id));
        self.next_subscription_id += 1;
        id
    }
}

#[derive(serde::Deserialize)]
struct SubscribeParams(
    SubscriptionKind,
    #[serde(default)] Option<SubscriptionParams>,
);

fn success_response<T: serde::Serialize>(id: Value, result: &T) -> JsonRpcResponse {
    match to_raw(result) {
        Ok(raw) => JsonRpcResponse::success(id, raw),
        Err(err) => JsonRpcResponse::error(id, err),
    }
}

fn parse_ws_params<T: DeserializeOwned>(
    req: &JsonRpcRequest,
    message: &'static str,
) -> Result<T, JsonRpcResponse> {
    let raw = req
        .params
        .as_deref()
        .map(|params| params.get())
        .unwrap_or("[]");
    serde_json::from_str(raw)
        .map_err(|_| JsonRpcResponse::error(req.id.clone(), JsonRpcError::invalid_params(message)))
}

fn subscription_notification(subscription_id: &FilterId, result: Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_subscription",
        "params": {
            "subscription": subscription_id,
            "result": result,
        }
    })
    .to_string()
}

fn spawn_logs_subscription(
    subscription_id: FilterId,
    upstream_filter_id: FilterId,
    auth: AuthContext,
    api: Arc<dyn ZoneRpcApi>,
    notifications: NotificationTx,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = time::interval(WS_SUBSCRIPTION_POLL_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval.tick().await;

        loop {
            interval.tick().await;

            let changes = match api
                .get_filter_changes(upstream_filter_id.clone(), auth.clone())
                .await
            {
                Ok(changes) => changes,
                Err(err) => {
                    warn!(
                        target: "zone::rpc",
                        subscription = ?subscription_id,
                        upstream_filter = ?upstream_filter_id,
                        err = %err,
                        "ws logs subscription poll failed"
                    );
                    break;
                }
            };

            let logs: Vec<Value> = match serde_json::from_str(changes.get()) {
                Ok(logs) => logs,
                Err(err) => {
                    warn!(
                        target: "zone::rpc",
                        subscription = ?subscription_id,
                        upstream_filter = ?upstream_filter_id,
                        err = %err,
                        "ws logs subscription returned invalid payload"
                    );
                    break;
                }
            };

            for log in logs {
                if notifications
                    .send(subscription_notification(&subscription_id, log))
                    .is_err()
                {
                    return;
                }
            }
        }
    })
}

fn spawn_new_heads_subscription(
    subscription_id: FilterId,
    upstream_filter_id: FilterId,
    auth: AuthContext,
    api: Arc<dyn ZoneRpcApi>,
    notifications: NotificationTx,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = time::interval(WS_SUBSCRIPTION_POLL_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        interval.tick().await;

        loop {
            interval.tick().await;

            let changes = match api
                .get_filter_changes(upstream_filter_id.clone(), auth.clone())
                .await
            {
                Ok(changes) => changes,
                Err(err) => {
                    warn!(
                        target: "zone::rpc",
                        subscription = ?subscription_id,
                        upstream_filter = ?upstream_filter_id,
                        err = %err,
                        "ws newHeads subscription poll failed"
                    );
                    break;
                }
            };

            let hashes: Vec<B256> = match serde_json::from_str(changes.get()) {
                Ok(hashes) => hashes,
                Err(err) => {
                    warn!(
                        target: "zone::rpc",
                        subscription = ?subscription_id,
                        upstream_filter = ?upstream_filter_id,
                        err = %err,
                        "ws newHeads subscription returned invalid payload"
                    );
                    break;
                }
            };

            for hash in hashes {
                let header = match api.block_by_hash(hash, false, auth.clone()).await {
                    Ok(header) => header,
                    Err(err) => {
                        warn!(
                            target: "zone::rpc",
                            subscription = ?subscription_id,
                            upstream_filter = ?upstream_filter_id,
                            err = %err,
                            "ws newHeads subscription failed to load block header"
                        );
                        return;
                    }
                };

                let mut header_json: Value = match serde_json::from_str(header.get()) {
                    Ok(value) => value,
                    Err(err) => {
                        warn!(
                            target: "zone::rpc",
                            subscription = ?subscription_id,
                            upstream_filter = ?upstream_filter_id,
                            err = %err,
                            "ws newHeads subscription returned invalid block payload"
                        );
                        return;
                    }
                };

                if header_json.is_null() {
                    continue;
                }

                if let Some(obj) = header_json.as_object_mut() {
                    obj.remove("transactions");
                    obj.remove("uncles");
                }

                if notifications
                    .send(subscription_notification(&subscription_id, header_json))
                    .is_err()
                {
                    return;
                }
            }
        }
    })
}

async fn handle_subscribe(
    req: &JsonRpcRequest,
    auth: &AuthContext,
    state: &Arc<RpcState>,
    notifications: &NotificationTx,
    session: &mut WsSession,
) -> JsonRpcResponse {
    let SubscribeParams(kind, params) =
        match parse_ws_params(req, "expected [subscription, params?]") {
            Ok(params) => params,
            Err(resp) => return resp,
        };

    match kind {
        SubscriptionKind::NewHeads => {
            if !matches!(params, None | Some(SubscriptionParams::None)) {
                return JsonRpcResponse::error(
                    req.id.clone(),
                    JsonRpcError::invalid_params("eth_subscribe(newHeads) does not accept params"),
                );
            }

            let raw = match state.api.new_block_filter(auth.clone()).await {
                Ok(raw) => raw,
                Err(err) => return JsonRpcResponse::error(req.id.clone(), err),
            };
            let upstream_filter_id: FilterId = match serde_json::from_str(raw.get()) {
                Ok(id) => id,
                Err(err) => {
                    return JsonRpcResponse::error(
                        req.id.clone(),
                        JsonRpcError::internal(err.to_string()),
                    );
                }
            };

            let subscription_id = session.next_subscription_id();
            let task = spawn_new_heads_subscription(
                subscription_id.clone(),
                upstream_filter_id.clone(),
                auth.clone(),
                state.api.clone(),
                notifications.clone(),
            );
            session.subscriptions.insert(
                subscription_id.clone(),
                ActiveSubscription {
                    upstream_filter_id,
                    task,
                },
            );
            success_response(req.id.clone(), &subscription_id)
        }
        SubscriptionKind::Logs => {
            let filter = match params.unwrap_or_default() {
                SubscriptionParams::None => Filter::default(),
                SubscriptionParams::Logs(filter) => *filter,
                SubscriptionParams::Bool(_) => {
                    return JsonRpcResponse::error(
                        req.id.clone(),
                        JsonRpcError::invalid_params("eth_subscribe(logs) expects a filter object"),
                    );
                }
            };

            let raw = match state.api.new_filter(filter, auth.clone()).await {
                Ok(raw) => raw,
                Err(err) => return JsonRpcResponse::error(req.id.clone(), err),
            };
            let upstream_filter_id: FilterId = match serde_json::from_str(raw.get()) {
                Ok(id) => id,
                Err(err) => {
                    return JsonRpcResponse::error(
                        req.id.clone(),
                        JsonRpcError::internal(err.to_string()),
                    );
                }
            };

            let subscription_id = session.next_subscription_id();
            let task = spawn_logs_subscription(
                subscription_id.clone(),
                upstream_filter_id.clone(),
                auth.clone(),
                state.api.clone(),
                notifications.clone(),
            );
            session.subscriptions.insert(
                subscription_id.clone(),
                ActiveSubscription {
                    upstream_filter_id,
                    task,
                },
            );
            success_response(req.id.clone(), &subscription_id)
        }
        SubscriptionKind::NewPendingTransactions | SubscriptionKind::Syncing => {
            JsonRpcResponse::error(req.id.clone(), JsonRpcError::method_disabled())
        }
    }
}

async fn handle_unsubscribe(
    req: &JsonRpcRequest,
    auth: &AuthContext,
    state: &Arc<RpcState>,
    session: &mut WsSession,
) -> JsonRpcResponse {
    let (subscription_id,) = match parse_ws_params::<(FilterId,)>(req, "expected [subscriptionId]")
    {
        Ok(params) => params,
        Err(resp) => return resp,
    };

    let Some(active) = session.subscriptions.remove(&subscription_id) else {
        return success_response(req.id.clone(), &false);
    };

    active.task.abort();
    let removed = match state
        .api
        .uninstall_filter(active.upstream_filter_id, auth.clone())
        .await
    {
        Ok(raw) => serde_json::from_str(raw.get()).unwrap_or(false),
        Err(err) => return JsonRpcResponse::error(req.id.clone(), err),
    };

    success_response(req.id.clone(), &removed)
}

async fn dispatch_ws_request(
    req: &JsonRpcRequest,
    auth: &AuthContext,
    state: &Arc<RpcState>,
    notifications: &NotificationTx,
    session: &mut WsSession,
) -> JsonRpcResponse {
    match req.method.as_str() {
        "eth_subscribe" => handle_subscribe(req, auth, state, notifications, session).await,
        "eth_unsubscribe" => handle_unsubscribe(req, auth, state, session).await,
        _ => handlers::dispatch(req, auth, state.api.as_ref()).await,
    }
}

async fn process_ws_text(
    text: &str,
    auth: &AuthContext,
    state: &Arc<RpcState>,
    notifications: &NotificationTx,
    session: &mut WsSession,
) -> String {
    let trimmed = text.trim_start();

    let response = if trimmed.starts_with('[') {
        match serde_json::from_str::<Vec<JsonRpcRequest>>(trimmed) {
            Ok(requests) if requests.is_empty() => {
                JsonRpcResponse::error(Value::Null, JsonRpcError::parse_error("empty batch"))
            }
            Ok(requests) if requests.len() > MAX_BATCH_SIZE => JsonRpcResponse::error(
                Value::Null,
                JsonRpcError::invalid_params(format!(
                    "batch too large ({} > {MAX_BATCH_SIZE})",
                    requests.len()
                )),
            ),
            Ok(requests) => {
                let mut responses = Vec::with_capacity(requests.len());
                for req in &requests {
                    responses
                        .push(dispatch_ws_request(req, auth, state, notifications, session).await);
                }
                return serde_json::to_string(&responses)
                    .expect("JsonRpcResponse serialization is infallible");
            }
            Err(err) => JsonRpcResponse::error(
                Value::Null,
                JsonRpcError::parse_error(format!("parse error: {err}")),
            ),
        }
    } else {
        match serde_json::from_str::<JsonRpcRequest>(trimmed) {
            Ok(request) => dispatch_ws_request(&request, auth, state, notifications, session).await,
            Err(err) => JsonRpcResponse::error(
                Value::Null,
                JsonRpcError::parse_error(format!("parse error: {err}")),
            ),
        }
    };

    serde_json::to_string(&response).expect("JsonRpcResponse serialization is infallible")
}

async fn cleanup_ws_session(session: WsSession, auth: &AuthContext, api: Arc<dyn ZoneRpcApi>) {
    for (_, active) in session.subscriptions {
        active.task.abort();
        let _ = api
            .uninstall_filter(active.upstream_filter_id, auth.clone())
            .await;
    }
}

/// WebSocket upgrade handler — authenticates via header or `?token=` query param.
pub(crate) async fn handle_ws_upgrade(
    State(state): State<Arc<RpcState>>,
    headers: HeaderMap,
    Query(query): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // Prefer header auth; fall back to query param for browser clients.
    let token_str = headers
        .get(auth::X_AUTHORIZATION_TOKEN)
        .and_then(|v| v.to_str().ok())
        .or(query.token.as_deref());

    let auth = match token_str {
        Some(token) => authenticate_token(token, &state.config),
        None => Err(AuthError::Missing),
    };

    let auth = match auth {
        Ok(auth) => auth,
        Err(e) => {
            warn!(target: "zone::rpc", err = %e, "ws auth failed");
            return (auth_error_status(&e), "").into_response();
        }
    };

    ws.max_message_size(MAX_WS_MESSAGE_SIZE)
        .on_upgrade(move |socket| handle_ws_session(socket, auth, state))
        .into_response()
}

/// Run a single authenticated WebSocket session, dispatching JSON-RPC
/// messages through the same pipeline as HTTP.
async fn handle_ws_session(socket: WebSocket, auth: AuthContext, state: Arc<RpcState>) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (notifications, mut outbound) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(message) = outbound.recv().await {
            if ws_sender.send(Message::Text(message.into())).await.is_err() {
                break;
            }
        }
    });

    let mut session = WsSession::default();

    while let Some(msg) = ws_receiver.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Binary(b)) => match std::str::from_utf8(&b) {
                Ok(s) => s.into(),
                Err(_) => {
                    let _ = notifications.send(
                        serde_json::to_string(&JsonRpcResponse::error(
                            Value::Null,
                            JsonRpcError::parse_error("invalid UTF-8"),
                        ))
                        .expect("JsonRpcResponse serialization is infallible"),
                    );
                    continue;
                }
            },
            Ok(Message::Close(_)) => break,
            Ok(_) => continue, // Ping/Pong handled by axum
            Err(e) => {
                warn!(target: "zone::rpc", err = %e, "ws recv error");
                break;
            }
        };

        let response_json =
            process_ws_text(&text, &auth, &state, &notifications, &mut session).await;

        if notifications.send(response_json).is_err() {
            break;
        }
    }

    cleanup_ws_session(session, &auth, state.api.clone()).await;
    drop(notifications);
    let _ = writer.await;
}
