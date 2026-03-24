//! WebSocket transport for the private zone RPC server.
//!
//! Clients authenticate via the `X-Authorization-Token` header (preferred) or
//! a `?token=0x<hex>` query parameter (for browser clients that cannot set
//! custom headers on the upgrade request).
//!
//! Auth is validated once during the HTTP upgrade handshake — individual
//! messages are not re-authenticated since WS frames don't carry auth headers.

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
use serde_json::{Value, value::RawValue};
use std::{collections::HashMap, sync::Arc};
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::warn;

use crate::{
    auth::{self, AuthContext, AuthError},
    server::{MAX_BATCH_SIZE, RpcState, authenticate_token, dispatch_request},
    subscription::WsSubscription,
    types::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, to_raw},
};

/// Maximum WebSocket message size (1 MiB).
const MAX_WS_MESSAGE_SIZE: usize = 1 << 20;

type NotificationTx = mpsc::UnboundedSender<String>;

/// Query parameters for the WebSocket upgrade endpoint.
#[derive(serde::Deserialize, Default)]
pub(crate) struct WsQuery {
    /// Auth token passed as query param (fallback when headers are unavailable).
    token: Option<String>,
}

struct ActiveSubscription {
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

#[derive(serde::Serialize)]
struct SubscriptionNotification<'a> {
    jsonrpc: &'static str,
    method: &'static str,
    params: SubscriptionNotificationParams<'a>,
}

#[derive(serde::Serialize)]
struct SubscriptionNotificationParams<'a> {
    subscription: &'a FilterId,
    result: &'a RawValue,
}

fn subscription_notification_raw(subscription_id: &FilterId, result: &RawValue) -> String {
    serde_json::to_string(&SubscriptionNotification {
        jsonrpc: "2.0",
        method: "eth_subscription",
        params: SubscriptionNotificationParams {
            subscription: subscription_id,
            result,
        },
    })
    .expect("subscription notification serialization is infallible")
}

fn spawn_subscription(
    subscription_id: FilterId,
    mut subscription: WsSubscription,
    notifications: NotificationTx,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // Let the subscribe response get enqueued before the first notification.
        tokio::task::yield_now().await;

        while let Some(item) = subscription.stream.next().await {
            let result = match item {
                Ok(result) => result,
                Err(err) => {
                    warn!(
                        target: "zone::rpc",
                        subscription = ?subscription_id,
                        err = %err,
                        "ws subscription stream failed"
                    );
                    break;
                }
            };

            if notifications
                .send(subscription_notification_raw(
                    &subscription_id,
                    result.as_ref(),
                ))
                .is_err()
            {
                return;
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

    let subscription = match kind {
        SubscriptionKind::NewHeads => {
            if !matches!(params, None | Some(SubscriptionParams::None)) {
                return JsonRpcResponse::error(
                    req.id.clone(),
                    JsonRpcError::invalid_params("eth_subscribe(newHeads) does not accept params"),
                );
            }

            match state.api.ws_subscribe_new_heads(auth.clone()).await {
                Ok(subscription) => subscription,
                Err(err) => return JsonRpcResponse::error(req.id.clone(), err),
            }
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

            match state.api.ws_subscribe_logs(filter, auth.clone()).await {
                Ok(subscription) => subscription,
                Err(err) => return JsonRpcResponse::error(req.id.clone(), err),
            }
        }
        SubscriptionKind::NewPendingTransactions => {
            let full = match params.unwrap_or(SubscriptionParams::None) {
                SubscriptionParams::None | SubscriptionParams::Bool(false) => false,
                SubscriptionParams::Bool(true) => true,
                SubscriptionParams::Logs(_) => {
                    return JsonRpcResponse::error(
                        req.id.clone(),
                        JsonRpcError::invalid_params(
                            "eth_subscribe(newPendingTransactions) expects an optional boolean",
                        ),
                    );
                }
            };

            match state
                .api
                .ws_subscribe_pending_transactions(full, auth.clone())
                .await
            {
                Ok(subscription) => subscription,
                Err(err) => return JsonRpcResponse::error(req.id.clone(), err),
            }
        }
        SubscriptionKind::Syncing => {
            return JsonRpcResponse::error(req.id.clone(), JsonRpcError::method_disabled());
        }
    };

    let subscription_id = session.next_subscription_id();
    let task = spawn_subscription(subscription_id.clone(), subscription, notifications.clone());
    session
        .subscriptions
        .insert(subscription_id.clone(), ActiveSubscription { task });
    success_response(req.id.clone(), &subscription_id)
}

async fn handle_unsubscribe(req: &JsonRpcRequest, session: &mut WsSession) -> JsonRpcResponse {
    let (subscription_id,) = match parse_ws_params::<(FilterId,)>(req, "expected [subscriptionId]")
    {
        Ok(params) => params,
        Err(resp) => return resp,
    };

    let Some(active) = session.subscriptions.remove(&subscription_id) else {
        return success_response(req.id.clone(), &false);
    };

    active.task.abort();
    success_response(req.id.clone(), &true)
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
        "eth_unsubscribe" => handle_unsubscribe(req, session).await,
        _ => dispatch_request(req, auth, state.api.as_ref()).await,
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

fn cleanup_ws_session(session: WsSession) {
    for (_, active) in session.subscriptions {
        active.task.abort();
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
        Some(token) => authenticate_token(token, &state.config, state.api.as_ref()).await,
        None => Err(AuthError::Missing.into()),
    };

    let auth = match auth {
        Ok(auth) => auth,
        Err(e) => {
            e.log("ws");
            return (e.status_code(), "").into_response();
        }
    };

    ws.max_message_size(MAX_WS_MESSAGE_SIZE)
        .on_upgrade(move |socket| handle_ws_session(socket, auth, state))
        .into_response()
}

/// Run a single authenticated WebSocket session, dispatching JSON-RPC
/// messages through the same pipeline as HTTP.
async fn handle_ws_session(
    socket: WebSocket,
    auth: crate::auth::AuthContext,
    state: Arc<RpcState>,
) {
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

    cleanup_ws_session(session);
    drop(notifications);
    let _ = writer.await;
}
