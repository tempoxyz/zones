//! Private zone RPC server.
//!
//! An axum HTTP server backed by the zone node's EthApi, with
//! authentication and privacy redactions applied per-method.
//!
//! Supports both HTTP POST and WebSocket transports. WebSocket clients
//! authenticate by passing their token as a query parameter:
//! `ws://host/?token=0x<hex>`.

use axum::{
    Router,
    body::Bytes,
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use futures::stream::StreamExt;
use std::sync::Arc;
use tracing::{info, warn};

use crate::{
    auth::{self, AuthContext, AuthError, SignatureType},
    config::PrivateRpcConfig,
    handlers::{self, ZoneRpcApi},
    types::{JsonRpcError, JsonRpcRequest, JsonRpcResponse},
};

/// Shared state for the private RPC server.
#[derive(Clone)]
pub struct RpcState {
    /// Server configuration.
    pub config: PrivateRpcConfig,
    /// Type-erased EthApi for handling RPC methods.
    pub api: Arc<dyn ZoneRpcApi>,
}

/// Start the private zone RPC server.
///
/// The `api` argument provides the underlying EthApi methods (obtained from
/// the zone node's launched handle).
pub async fn start_private_rpc(
    config: PrivateRpcConfig,
    api: Arc<dyn ZoneRpcApi>,
) -> eyre::Result<std::net::SocketAddr> {
    let listen_addr = config.listen_addr;
    let state = Arc::new(RpcState { config, api });

    let app = Router::new()
        .route("/", post(handle_rpc))
        .route("/", get(handle_ws_upgrade))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    let local_addr = listener.local_addr()?;

    info!(target: "zone::rpc", %local_addr, "Starting private zone RPC server");

    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::error!(target: "zone::rpc", %err, "Private RPC server failed");
        }
    });

    Ok(local_addr)
}

/// Main RPC handler — authenticates, dispatches, returns response.
async fn handle_rpc(
    State(state): State<Arc<RpcState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // 1. Authenticate
    let auth = match authenticate(&headers, &state.config) {
        Ok(auth) => auth,
        Err(e) => {
            let status = match &e {
                AuthError::Missing => StatusCode::UNAUTHORIZED,
                _ => StatusCode::FORBIDDEN,
            };
            warn!(target: "zone::rpc", err = %e, "auth failed");
            return (status, "").into_response();
        }
    };

    // 2. Parse the JSON-RPC request body
    let body_str = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => {
            return (StatusCode::BAD_REQUEST, "invalid UTF-8").into_response();
        }
    };

    let trimmed = body_str.trim_start();
    let is_batch = trimmed.starts_with('[');

    if is_batch {
        let requests: Vec<JsonRpcRequest> = match serde_json::from_str(trimmed) {
            Ok(reqs) => reqs,
            Err(e) => {
                return (
                    StatusCode::OK,
                    axum::Json(JsonRpcResponse::error(
                        serde_json::Value::Null,
                        JsonRpcError::invalid_params(format!("parse error: {e}")),
                    )),
                )
                    .into_response();
            }
        };

        let mut responses = Vec::with_capacity(requests.len());
        for req in &requests {
            responses.push(handlers::dispatch(req, &auth, state.api.as_ref()).await);
        }

        (StatusCode::OK, axum::Json(responses)).into_response()
    } else {
        let request: JsonRpcRequest = match serde_json::from_str(trimmed) {
            Ok(req) => req,
            Err(e) => {
                return (
                    StatusCode::OK,
                    axum::Json(JsonRpcResponse::error(
                        serde_json::Value::Null,
                        JsonRpcError::invalid_params(format!("parse error: {e}")),
                    )),
                )
                    .into_response();
            }
        };

        let response = handlers::dispatch(&request, &auth, state.api.as_ref()).await;
        (StatusCode::OK, axum::Json(response)).into_response()
    }
}

/// Authenticate the request using the `X-Authorization-Token` header.
fn authenticate(headers: &HeaderMap, config: &PrivateRpcConfig) -> Result<AuthContext, AuthError> {
    let header_value = headers
        .get(auth::X_AUTHORIZATION_TOKEN)
        .and_then(|v| v.to_str().ok())
        .ok_or(AuthError::Missing)?;

    authenticate_token(header_value, config)
}

/// Authenticate using a raw token string (shared by HTTP and WebSocket paths).
fn authenticate_token(
    token_value: &str,
    config: &PrivateRpcConfig,
) -> Result<AuthContext, AuthError> {
    let token = auth::parse_auth_header(token_value)?;

    // Validate token fields against server config
    token.validate(config.zone_id, config.chain_id, config.zone_portal)?;

    // Verify the signature and recover the signer address
    let sig_type = token.signature_type()?;
    let caller = match sig_type {
        SignatureType::Secp256k1 => auth::recover_secp256k1(&token.signature, &token.digest)?,
        // P256 / WebAuthn / Keychain signature types are not yet supported
        _ => return Err(AuthError::UnsupportedSignatureType),
    };

    let is_sequencer = caller == config.sequencer;

    Ok(AuthContext {
        caller,
        is_sequencer,
        expires_at: token.expires_at,
    })
}

// ---------------------------------------------------------------------------
// WebSocket transport
// ---------------------------------------------------------------------------

/// Query parameters for the WebSocket upgrade endpoint.
#[derive(serde::Deserialize)]
struct WsQuery {
    token: String,
}

/// WebSocket upgrade handler — authenticates via `?token=` query param.
async fn handle_ws_upgrade(
    State(state): State<Arc<RpcState>>,
    Query(query): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // Authenticate before upgrading so we reject early on bad tokens.
    let auth = match authenticate_token(&query.token, &state.config) {
        Ok(auth) => auth,
        Err(e) => {
            warn!(target: "zone::rpc", err = %e, "ws auth failed");
            let status = match &e {
                AuthError::Missing => StatusCode::UNAUTHORIZED,
                _ => StatusCode::FORBIDDEN,
            };
            return (status, "").into_response();
        }
    };

    ws.on_upgrade(move |socket| handle_ws_session(socket, auth, state))
        .into_response()
}

/// Run a single authenticated WebSocket session, dispatching JSON-RPC
/// messages through the same pipeline as HTTP.
async fn handle_ws_session(mut socket: WebSocket, auth: AuthContext, state: Arc<RpcState>) {
    while let Some(msg) = socket.next().await {
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Binary(b)) => match String::from_utf8(b.to_vec()) {
                Ok(s) => s.into(),
                Err(_) => {
                    let _ = socket
                            .send(Message::Text(
                                r#"{"jsonrpc":"2.0","error":{"code":-32700,"message":"invalid UTF-8"},"id":null}"#.into(),
                            ))
                            .await;
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

        let trimmed = text.trim_start();
        let is_batch = trimmed.starts_with('[');

        let response_json = if is_batch {
            match serde_json::from_str::<Vec<JsonRpcRequest>>(trimmed) {
                Ok(requests) => {
                    let mut responses = Vec::with_capacity(requests.len());
                    for req in &requests {
                        responses.push(handlers::dispatch(req, &auth, state.api.as_ref()).await);
                    }
                    serde_json::to_string(&responses).unwrap_or_default()
                }
                Err(e) => {
                    let err_resp = JsonRpcResponse::error(
                        serde_json::Value::Null,
                        JsonRpcError::invalid_params(format!("parse error: {e}")),
                    );
                    serde_json::to_string(&err_resp).unwrap_or_default()
                }
            }
        } else {
            let request: JsonRpcRequest = match serde_json::from_str(trimmed) {
                Ok(req) => req,
                Err(e) => {
                    let err_resp = JsonRpcResponse::error(
                        serde_json::Value::Null,
                        JsonRpcError::invalid_params(format!("parse error: {e}")),
                    );
                    let _ = socket
                        .send(Message::Text(
                            serde_json::to_string(&err_resp).unwrap_or_default().into(),
                        ))
                        .await;
                    continue;
                }
            };

            let response = handlers::dispatch(&request, &auth, state.api.as_ref()).await;
            serde_json::to_string(&response).unwrap_or_default()
        };

        if socket
            .send(Message::Text(response_json.into()))
            .await
            .is_err()
        {
            break;
        }
    }
}
