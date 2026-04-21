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
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};
use tracing::warn;

use crate::{
    auth::{self, AuthContext, AuthError},
    server::{MAX_BATCH_SIZE, RpcState, authenticate_token, dispatch_request},
    subscription::WsSubscriptionStream,
    types::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, to_raw},
};

/// Maximum WebSocket message size (1 MiB).
const MAX_WS_MESSAGE_SIZE: usize = 1 << 20;
/// Maximum number of queued outbound messages before the session is dropped.
const MAX_WS_OUTBOUND_QUEUE: usize = 1024;
/// Maximum number of active or pending subscriptions per WebSocket session.
const MAX_WS_SUBSCRIPTIONS: usize = 32;

type NotificationTx = mpsc::Sender<String>;
type CloseSessionTx = watch::Sender<bool>;

/// Query parameters for the WebSocket upgrade endpoint.
#[derive(serde::Deserialize, Default)]
pub(crate) struct WsQuery {
    /// Auth token passed as query param (fallback when headers are unavailable).
    token: Option<String>,
}

struct ActiveSubscription {
    task: JoinHandle<()>,
}

struct PendingSubscription {
    id: FilterId,
    stream: WsSubscriptionStream,
}

struct WsSession {
    next_subscription_id: u64,
    pending_subscription_count: usize,
    subscriptions: HashMap<FilterId, ActiveSubscription>,
}

impl Default for WsSession {
    fn default() -> Self {
        Self {
            next_subscription_id: 1,
            pending_subscription_count: 0,
            subscriptions: HashMap::new(),
        }
    }
}

impl WsSession {
    fn total_subscription_count(&self) -> usize {
        self.pending_subscription_count + self.subscriptions.len()
    }

    fn next_subscription_id(&mut self) -> FilterId {
        let id = FilterId::from(format!("0x{:x}", self.next_subscription_id));
        self.next_subscription_id += 1;
        id
    }

    fn reserve_subscription_id(&mut self) -> Result<FilterId, JsonRpcError> {
        if self.total_subscription_count() >= MAX_WS_SUBSCRIPTIONS {
            return Err(JsonRpcError::invalid_params(format!(
                "too many active subscriptions ({MAX_WS_SUBSCRIPTIONS} max)"
            )));
        }

        self.pending_subscription_count += 1;
        Ok(self.next_subscription_id())
    }

    fn activate_subscription(&mut self, subscription_id: FilterId, task: JoinHandle<()>) {
        self.pending_subscription_count = self.pending_subscription_count.saturating_sub(1);
        self.subscriptions
            .insert(subscription_id, ActiveSubscription { task });
    }

    fn cleanup(self) {
        for (_, active) in self.subscriptions {
            active.task.abort();
        }
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
) -> Result<T, JsonRpcError> {
    let raw = req
        .params
        .as_deref()
        .map(|params| params.get())
        .unwrap_or("[]");
    serde_json::from_str(raw).map_err(|_| JsonRpcError::invalid_params(message))
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
    mut subscription: WsSubscriptionStream,
    notifications: NotificationTx,
    close_session: CloseSessionTx,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(item) = subscription.next().await {
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

            if !try_queue_notification(
                &notifications,
                &close_session,
                subscription_notification_raw(&subscription_id, result.as_ref()),
            ) {
                return;
            }
        }
    })
}

fn request_session_close(close_session: &CloseSessionTx) {
    let _ = close_session.send(true);
}

fn try_queue_notification(
    notifications: &NotificationTx,
    close_session: &CloseSessionTx,
    message: String,
) -> bool {
    match notifications.try_send(message) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Full(_)) => {
            warn!(
                target: "zone::rpc",
                max_queue = MAX_WS_OUTBOUND_QUEUE,
                "ws outbound queue full, closing session"
            );
            request_session_close(close_session);
            false
        }
        Err(mpsc::error::TrySendError::Closed(_)) => false,
    }
}

struct WsDispatchResult {
    response: JsonRpcResponse,
    pending_subscriptions: Vec<PendingSubscription>,
}

impl WsDispatchResult {
    fn response_only(response: JsonRpcResponse) -> Self {
        Self {
            response,
            pending_subscriptions: Vec::new(),
        }
    }
}

async fn handle_subscribe(
    req: &JsonRpcRequest,
    auth: &AuthContext,
    state: &Arc<RpcState>,
    session: &mut WsSession,
) -> WsDispatchResult {
    let SubscribeParams(kind, params) =
        match parse_ws_params(req, "expected [subscription, params?]") {
            Ok(params) => params,
            Err(err) => {
                return WsDispatchResult::response_only(JsonRpcResponse::error(
                    req.id.clone(),
                    err,
                ));
            }
        };

    let subscription = match kind {
        SubscriptionKind::NewHeads => {
            if !matches!(params, None | Some(SubscriptionParams::None)) {
                return WsDispatchResult::response_only(JsonRpcResponse::error(
                    req.id.clone(),
                    JsonRpcError::invalid_params("eth_subscribe(newHeads) does not accept params"),
                ));
            }

            match state.api.ws_subscribe_new_heads(auth.clone()).await {
                Ok(subscription) => subscription,
                Err(err) => {
                    return WsDispatchResult::response_only(JsonRpcResponse::error(
                        req.id.clone(),
                        err,
                    ));
                }
            }
        }
        SubscriptionKind::Logs => {
            let filter = match params.unwrap_or_default() {
                SubscriptionParams::None => Filter::default(),
                SubscriptionParams::Logs(filter) => *filter,
                SubscriptionParams::Bool(_) | SubscriptionParams::TransactionReceipts(_) => {
                    return WsDispatchResult::response_only(JsonRpcResponse::error(
                        req.id.clone(),
                        JsonRpcError::invalid_params("eth_subscribe(logs) expects a filter object"),
                    ));
                }
            };

            match state.api.ws_subscribe_logs(filter, auth.clone()).await {
                Ok(subscription) => subscription,
                Err(err) => {
                    return WsDispatchResult::response_only(JsonRpcResponse::error(
                        req.id.clone(),
                        err,
                    ));
                }
            }
        }
        SubscriptionKind::NewPendingTransactions => {
            let full = match params.unwrap_or(SubscriptionParams::None) {
                SubscriptionParams::None | SubscriptionParams::Bool(false) => false,
                SubscriptionParams::Bool(true) => true,
                SubscriptionParams::Logs(_) | SubscriptionParams::TransactionReceipts(_) => {
                    return WsDispatchResult::response_only(JsonRpcResponse::error(
                        req.id.clone(),
                        JsonRpcError::invalid_params(
                            "eth_subscribe(newPendingTransactions) expects an optional boolean",
                        ),
                    ));
                }
            };

            match state
                .api
                .ws_subscribe_pending_transactions(full, auth.clone())
                .await
            {
                Ok(subscription) => subscription,
                Err(err) => {
                    return WsDispatchResult::response_only(JsonRpcResponse::error(
                        req.id.clone(),
                        err,
                    ));
                }
            }
        }
        SubscriptionKind::Syncing => {
            return WsDispatchResult::response_only(JsonRpcResponse::error(
                req.id.clone(),
                JsonRpcError::method_disabled(),
            ));
        }
        SubscriptionKind::TransactionReceipts => {
            return WsDispatchResult::response_only(JsonRpcResponse::error(
                req.id.clone(),
                JsonRpcError::method_disabled(),
            ));
        }
    };

    let subscription_id = match session.reserve_subscription_id() {
        Ok(subscription_id) => subscription_id,
        Err(err) => {
            return WsDispatchResult::response_only(JsonRpcResponse::error(req.id.clone(), err));
        }
    };
    WsDispatchResult {
        response: success_response(req.id.clone(), &subscription_id),
        pending_subscriptions: vec![PendingSubscription {
            id: subscription_id,
            stream: subscription,
        }],
    }
}

async fn handle_unsubscribe(req: &JsonRpcRequest, session: &mut WsSession) -> JsonRpcResponse {
    let (subscription_id,) = match parse_ws_params::<(FilterId,)>(req, "expected [subscriptionId]")
    {
        Ok(params) => params,
        Err(err) => return JsonRpcResponse::error(req.id.clone(), err),
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
    session: &mut WsSession,
) -> WsDispatchResult {
    match req.method.as_str() {
        "eth_subscribe" => handle_subscribe(req, auth, state, session).await,
        "eth_unsubscribe" => {
            WsDispatchResult::response_only(handle_unsubscribe(req, session).await)
        }
        _ => WsDispatchResult::response_only(dispatch_request(req, auth, state.api.as_ref()).await),
    }
}

async fn process_ws_text(
    text: &str,
    auth: &AuthContext,
    state: &Arc<RpcState>,
    session: &mut WsSession,
) -> (String, Vec<PendingSubscription>) {
    let trimmed = text.trim_start();

    if trimmed.starts_with('[') {
        match serde_json::from_str::<Vec<JsonRpcRequest>>(trimmed) {
            Ok(requests) if requests.is_empty() => (
                serde_json::to_string(&JsonRpcResponse::error(
                    Value::Null,
                    JsonRpcError::parse_error("empty batch"),
                ))
                .expect("JsonRpcResponse serialization is infallible"),
                Vec::new(),
            ),
            Ok(requests) if requests.len() > MAX_BATCH_SIZE => (
                serde_json::to_string(&JsonRpcResponse::error(
                    Value::Null,
                    JsonRpcError::invalid_params(format!(
                        "batch too large ({} > {MAX_BATCH_SIZE})",
                        requests.len()
                    )),
                ))
                .expect("JsonRpcResponse serialization is infallible"),
                Vec::new(),
            ),
            Ok(requests) => {
                let mut responses = Vec::with_capacity(requests.len());
                let mut pending_subscriptions = Vec::new();
                for req in &requests {
                    let result = dispatch_ws_request(req, auth, state, session).await;
                    responses.push(result.response);
                    pending_subscriptions.extend(result.pending_subscriptions);
                }
                (
                    serde_json::to_string(&responses)
                        .expect("JsonRpcResponse serialization is infallible"),
                    pending_subscriptions,
                )
            }
            Err(err) => (
                serde_json::to_string(&JsonRpcResponse::error(
                    Value::Null,
                    JsonRpcError::parse_error(format!("parse error: {err}")),
                ))
                .expect("JsonRpcResponse serialization is infallible"),
                Vec::new(),
            ),
        }
    } else {
        match serde_json::from_str::<JsonRpcRequest>(trimmed) {
            Ok(request) => {
                let result = dispatch_ws_request(&request, auth, state, session).await;
                (
                    serde_json::to_string(&result.response)
                        .expect("JsonRpcResponse serialization is infallible"),
                    result.pending_subscriptions,
                )
            }
            Err(err) => (
                serde_json::to_string(&JsonRpcResponse::error(
                    Value::Null,
                    JsonRpcError::parse_error(format!("parse error: {err}")),
                ))
                .expect("JsonRpcResponse serialization is infallible"),
                Vec::new(),
            ),
        }
    }
}

fn activate_pending_subscriptions(
    pending_subscriptions: Vec<PendingSubscription>,
    notifications: &NotificationTx,
    close_session: &CloseSessionTx,
    session: &mut WsSession,
) {
    for pending in pending_subscriptions {
        let task = spawn_subscription(
            pending.id.clone(),
            pending.stream,
            notifications.clone(),
            close_session.clone(),
        );
        session.activate_subscription(pending.id, task);
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
    let (notifications, mut outbound) = mpsc::channel::<String>(MAX_WS_OUTBOUND_QUEUE);
    let (close_session, mut close_session_rx) = watch::channel(false);
    let writer = tokio::spawn(async move {
        while let Some(message) = outbound.recv().await {
            if ws_sender.send(Message::Text(message.into())).await.is_err() {
                break;
            }
        }
    });

    let mut session = WsSession::default();

    loop {
        let msg = tokio::select! {
            _ = close_session_rx.changed() => break,
            msg = ws_receiver.next() => match msg {
                Some(msg) => msg,
                None => break,
            },
        };

        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Binary(b)) => match std::str::from_utf8(&b) {
                Ok(s) => s.into(),
                Err(_) => {
                    if !try_queue_notification(
                        &notifications,
                        &close_session,
                        serde_json::to_string(&JsonRpcResponse::error(
                            Value::Null,
                            JsonRpcError::parse_error("invalid UTF-8"),
                        ))
                        .expect("JsonRpcResponse serialization is infallible"),
                    ) {
                        break;
                    }
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

        let (response_json, pending_subscriptions) =
            process_ws_text(&text, &auth, &state, &mut session).await;

        if !try_queue_notification(&notifications, &close_session, response_json) {
            break;
        }

        activate_pending_subscriptions(
            pending_subscriptions,
            &notifications,
            &close_session,
            &mut session,
        );
    }

    session.cleanup();
    drop(notifications);
    let _ = writer.await;
}
