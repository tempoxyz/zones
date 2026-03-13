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
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tempo_contracts::precompiles::account_keychain::IAccountKeychain::SignatureType as KeyInfoSignatureType;
use tempo_primitives::transaction::{
    SignatureType as TempoSignatureType,
    tt_signature::{KeychainSignature, TempoSignature},
};
use tracing::{error, info, warn};

use crate::{
    auth::{self, AuthContext, AuthError},
    config::PrivateRpcConfig,
    handlers::{self, ZoneRpcApi},
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
                    responses.push(handlers::dispatch(req, auth, api).await);
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
            Ok(request) => RpcResult::Single(handlers::dispatch(&request, auth, api).await),
            Err(e) => RpcResult::Single(JsonRpcResponse::error(
                serde_json::Value::Null,
                JsonRpcError::parse_error(format!("parse error: {e}")),
            )),
        }
    }
}

/// Map an [`AuthError`] to the appropriate HTTP status code.
pub(crate) fn auth_error_status(err: &AuthenticateError) -> StatusCode {
    match err {
        AuthenticateError::Invalid(AuthError::Missing) => StatusCode::UNAUTHORIZED,
        AuthenticateError::Invalid(_) => StatusCode::FORBIDDEN,
        AuthenticateError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Authentication failures split into invalid caller credentials vs server-side failures.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AuthenticateError {
    #[error(transparent)]
    Invalid(#[from] AuthError),
    #[error(transparent)]
    Internal(#[from] eyre::Report),
}

pub(crate) fn log_auth_error(err: &AuthenticateError, transport: &str) {
    match err {
        AuthenticateError::Invalid(cause) => {
            warn!(target: "zone::rpc", %transport, err = %cause, "auth failed");
        }
        AuthenticateError::Internal(cause) => {
            error!(target: "zone::rpc", %transport, err = %cause, "auth failed");
        }
    }
}

/// Main HTTP RPC handler — authenticates, dispatches, returns response.
async fn handle_rpc(
    State(state): State<Arc<RpcState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let auth = match authenticate(&headers, &state.config, state.api.as_ref()).await {
        Ok(auth) => auth,
        Err(e) => {
            log_auth_error(&e, "http");
            return (auth_error_status(&e), "").into_response();
        }
    };

    let body_str = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => {
            return (StatusCode::BAD_REQUEST, "invalid UTF-8").into_response();
        }
    };

    process_rpc_text(body_str, &auth, state.api.as_ref())
        .await
        .into_response()
}

/// Authenticate the request using the `X-Authorization-Token` header.
async fn authenticate(
    headers: &HeaderMap,
    config: &PrivateRpcConfig,
    api: &dyn ZoneRpcApi,
) -> Result<AuthContext, AuthenticateError> {
    let header_value = headers
        .get(auth::X_AUTHORIZATION_TOKEN)
        .and_then(|v| v.to_str().ok())
        .ok_or(AuthError::Missing)?;

    authenticate_token(header_value, config, api).await
}

/// Authenticate using a raw token string (shared by HTTP and WebSocket paths).
pub(crate) async fn authenticate_token(
    token_value: &str,
    config: &PrivateRpcConfig,
    api: &dyn ZoneRpcApi,
) -> Result<AuthContext, AuthenticateError> {
    let token = auth::parse_auth_header(token_value)?;

    // Validate token fields against server config
    token.validate(config.zone_id, config.chain_id, config.zone_portal)?;

    let signature =
        TempoSignature::from_bytes(&token.signature).map_err(|_| AuthError::InvalidSignature)?;
    let caller = signature
        .recover_signer(&token.digest)
        .map_err(|_| AuthError::InvalidSignature)?;

    if let TempoSignature::Keychain(keychain_signature) = &signature {
        validate_keychain_signature(api, caller, keychain_signature, &token.digest).await?;
    }

    let is_sequencer = caller == config.sequencer;

    Ok(AuthContext {
        caller,
        is_sequencer,
        expires_at: token.expires_at,
    })
}

async fn validate_keychain_signature(
    api: &dyn ZoneRpcApi,
    caller: alloy_primitives::Address,
    keychain_signature: &KeychainSignature,
    digest: &alloy_primitives::B256,
) -> Result<(), AuthenticateError> {
    let key_id = keychain_signature
        .key_id(digest)
        .map_err(|_| AuthError::InvalidSignature)?;
    let key_info = api.get_keychain_key(caller, key_id).await?;

    if key_info.keyId.is_zero() {
        return Err(AuthError::UnauthorizedKeychainKey.into());
    }
    if key_info.isRevoked {
        return Err(AuthError::RevokedKeychainKey.into());
    }
    if key_info.expiry <= now_unix_seconds() {
        return Err(AuthError::ExpiredKeychainKey.into());
    }

    let expected_signature_type = match keychain_signature.signature.signature_type() {
        TempoSignatureType::Secp256k1 => KeyInfoSignatureType::Secp256k1,
        TempoSignatureType::P256 => KeyInfoSignatureType::P256,
        TempoSignatureType::WebAuthn => KeyInfoSignatureType::WebAuthn,
    };

    if key_info.signatureType != expected_signature_type {
        return Err(AuthError::KeychainSignatureTypeMismatch.into());
    }

    Ok(())
}

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_secs()
}
