//! Private zone RPC server.
//!
//! An axum HTTP server backed by the zone node's EthApi, with
//! authentication and privacy redactions applied per-method.

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use std::sync::Arc;
use tracing::{info, warn};

use super::{
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

    let app = Router::new().route("/", post(handle_rpc)).with_state(state);

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

    let token = auth::parse_auth_header(header_value)?;

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
