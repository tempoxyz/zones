//! WebSocket transport for the private zone RPC server.
//!
//! Clients authenticate via the `X-Authorization-Token` header (preferred) or
//! a `?token=0x<hex>` query parameter (for browser clients that cannot set
//! custom headers on the upgrade request).
//!
//! Auth is validated once during the HTTP upgrade handshake — individual
//! messages are not re-authenticated since WS frames don't carry auth headers.

use axum::{
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::IntoResponse,
};
use futures::stream::StreamExt;
use std::sync::Arc;
use tracing::warn;

use crate::{
    auth::{self, AuthError},
    server::{RpcState, authenticate_token, process_rpc_text},
};

/// Maximum WebSocket message size (1 MiB).
const MAX_WS_MESSAGE_SIZE: usize = 1 << 20;

/// Query parameters for the WebSocket upgrade endpoint.
#[derive(serde::Deserialize, Default)]
pub(crate) struct WsQuery {
    /// Auth token passed as query param (fallback when headers are unavailable).
    token: Option<String>,
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
    mut socket: WebSocket,
    auth: crate::auth::AuthContext,
    state: Arc<RpcState>,
) {
    while let Some(msg) = socket.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Binary(b)) => match std::str::from_utf8(&b) {
                Ok(s) => s.into(),
                Err(_) => {
                    if socket
                        .send(Message::Text(
                            r#"{"jsonrpc":"2.0","error":{"code":-32700,"message":"invalid UTF-8"},"id":null}"#.into(),
                        ))
                        .await
                        .is_err()
                    {
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

        let response_json = process_rpc_text(&text, &auth, state.api.as_ref())
            .await
            .into_json();

        if socket
            .send(Message::Text(response_json.into()))
            .await
            .is_err()
        {
            break;
        }
    }
}
