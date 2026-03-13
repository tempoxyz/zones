//! Private zone RPC server.
//!
//! An axum HTTP server backed by the zone node's EthApi, with
//! authentication and privacy redactions applied per-method.
//!
//! Supports both HTTP POST and WebSocket transports.

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use std::{
    sync::{Arc, atomic::AtomicU64},
    time::Instant,
};
use tracing::{info, warn};

use crate::{
    auth::{self, AuthContext, AuthError, SignatureType},
    config::PrivateRpcConfig,
    handlers::{self, ZoneRpcApi},
    metrics::{PrivateRpcCallMetrics, RpcTransport, record_auth_failure},
    types::{JsonRpcError, JsonRpcRequest, JsonRpcResponse},
    ws::handle_ws_upgrade,
};

/// Maximum number of requests in a single JSON-RPC batch.
const MAX_BATCH_SIZE: usize = 100;

/// Shared state for the private RPC server.
#[derive(Clone)]
pub struct RpcState {
    /// Server configuration.
    pub config: PrivateRpcConfig,
    /// Type-erased EthApi for handling RPC methods.
    pub api: Arc<dyn ZoneRpcApi>,
    /// Number of currently active WebSocket sessions.
    pub ws_sessions_active: Arc<AtomicU64>,
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
    let state = Arc::new(RpcState {
        config,
        api,
        ws_sessions_active: Arc::new(AtomicU64::new(0)),
    });

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

/// Result of processing a JSON-RPC text payload (single or batch).
pub(crate) enum RpcResult {
    Single(JsonRpcResponse),
    Batch(Vec<JsonRpcResponse>),
}

impl RpcResult {
    /// Serialize to a JSON string for the WebSocket transport.
    pub(crate) fn into_json(self) -> String {
        match self {
            Self::Single(resp) => serde_json::to_string(&resp),
            Self::Batch(resps) => serde_json::to_string(&resps),
        }
        .expect("JsonRpcResponse serialization is infallible")
    }
}

impl IntoResponse for RpcResult {
    fn into_response(self) -> axum::response::Response {
        match self {
            Self::Single(resp) => axum::Json(resp).into_response(),
            Self::Batch(resps) => axum::Json(resps).into_response(),
        }
    }
}

/// Parse and dispatch a JSON-RPC text payload, handling both single and batch
/// requests. Shared by HTTP and WebSocket transports.
pub(crate) async fn process_rpc_text(
    text: &str,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
    transport: RpcTransport,
) -> RpcResult {
    let trimmed = text.trim_start();

    if trimmed.starts_with('[') {
        match serde_json::from_str::<Vec<JsonRpcRequest>>(trimmed) {
            Ok(requests) if requests.is_empty() => RpcResult::Single(JsonRpcResponse::error(
                serde_json::Value::Null,
                JsonRpcError::parse_error("empty batch"),
            )),
            Ok(requests) if requests.len() > MAX_BATCH_SIZE => {
                RpcResult::Single(JsonRpcResponse::error(
                    serde_json::Value::Null,
                    JsonRpcError::invalid_params(format!(
                        "batch too large ({} > {MAX_BATCH_SIZE})",
                        requests.len()
                    )),
                ))
            }
            Ok(requests) => {
                let mut responses = Vec::with_capacity(requests.len());
                for req in &requests {
                    responses.push(dispatch_metered(req, auth, api, transport).await);
                }
                RpcResult::Batch(responses)
            }
            Err(e) => RpcResult::Single(JsonRpcResponse::error(
                serde_json::Value::Null,
                JsonRpcError::parse_error(format!("parse error: {e}")),
            )),
        }
    } else {
        match serde_json::from_str::<JsonRpcRequest>(trimmed) {
            Ok(request) => {
                RpcResult::Single(dispatch_metered(&request, auth, api, transport).await)
            }
            Err(e) => RpcResult::Single(JsonRpcResponse::error(
                serde_json::Value::Null,
                JsonRpcError::parse_error(format!("parse error: {e}")),
            )),
        }
    }
}

async fn dispatch_metered(
    req: &JsonRpcRequest,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
    transport: RpcTransport,
) -> JsonRpcResponse {
    let metrics = PrivateRpcCallMetrics::new_for(transport, &req.method);
    let started_at = Instant::now();

    metrics.started_total.increment(1);
    let response = handlers::dispatch(req, auth, api).await;
    metrics
        .time_seconds
        .record(started_at.elapsed().as_secs_f64());

    if response.error.is_some() {
        metrics.failed_total.increment(1);
    } else {
        metrics.successful_total.increment(1);
    }

    response
}

/// Map an [`AuthError`] to the appropriate HTTP status code.
pub(crate) fn auth_error_status(err: &AuthError) -> StatusCode {
    match err {
        AuthError::Missing => StatusCode::UNAUTHORIZED,
        _ => StatusCode::FORBIDDEN,
    }
}

/// Main HTTP RPC handler — authenticates, dispatches, returns response.
async fn handle_rpc(
    State(state): State<Arc<RpcState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let auth = match authenticate(&headers, &state.config) {
        Ok(auth) => auth,
        Err(e) => {
            record_auth_failure(RpcTransport::Http, &e);
            warn!(target: "zone::rpc", err = %e, "auth failed");
            return (auth_error_status(&e), "").into_response();
        }
    };

    let body_str = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => {
            return (StatusCode::BAD_REQUEST, "invalid UTF-8").into_response();
        }
    };

    process_rpc_text(body_str, &auth, state.api.as_ref(), RpcTransport::Http)
        .await
        .into_response()
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
pub(crate) fn authenticate_token(
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
