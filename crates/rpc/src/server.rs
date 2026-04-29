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
    time::{Instant, SystemTime, UNIX_EPOCH},
};
use tempo_contracts::precompiles::account_keychain::IAccountKeychain::SignatureType as KeyInfoSignatureType;
use tempo_primitives::transaction::{
    SignatureType as TempoSignatureType,
    tt_signature::{KeychainSignature, TempoSignature},
};
use tracing::info;

use crate::{
    auth::{self, AuthContext},
    config::PrivateRpcConfig,
    error::{AuthError, AuthenticateError},
    handlers::{self, ZoneRpcApi},
    metrics::{PrivateRpcAuthMetrics, PrivateRpcCallMetrics},
    types::{JsonRpcError, JsonRpcRequest, JsonRpcResponse},
    ws::handle_ws_upgrade,
};

/// Maximum number of requests in a single JSON-RPC batch.
pub(crate) const MAX_BATCH_SIZE: usize = 100;

/// Shared state for the private RPC server.
#[derive(Clone)]
pub struct RpcState {
    /// Server configuration.
    pub config: PrivateRpcConfig,
    /// Type-erased EthApi for handling RPC methods.
    pub api: Arc<dyn ZoneRpcApi>,
    /// Authentication failure metric for the private RPC.
    auth_metrics: PrivateRpcAuthMetrics,
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
        auth_metrics: PrivateRpcAuthMetrics::default(),
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
                    responses.push(dispatch_request(req, auth, api).await);
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
            Ok(request) => RpcResult::Single(dispatch_request(&request, auth, api).await),
            Err(e) => RpcResult::Single(JsonRpcResponse::error(
                serde_json::Value::Null,
                JsonRpcError::parse_error(format!("parse error: {e}")),
            )),
        }
    }
}

pub(crate) async fn dispatch_request(
    req: &JsonRpcRequest,
    auth: &AuthContext,
    api: &dyn ZoneRpcApi,
) -> JsonRpcResponse {
    let metrics = PrivateRpcCallMetrics::new_for(&req.method);
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

/// Main HTTP RPC handler — authenticates, dispatches, returns response.
async fn handle_rpc(
    State(state): State<Arc<RpcState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let auth = match authenticate(&headers, &state.config, state.api.as_ref()).await {
        Ok(auth) => auth,
        Err(e) => {
            if e.is_invalid() {
                state.auth_metrics.auth_failures_total.increment(1);
            }
            e.log("http");
            return (e.status_code(), "").into_response();
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
    let max_auth_token_validity = config
        .max_auth_token_validity
        .min(auth::DEFAULT_MAX_AUTH_TOKEN_VALIDITY);

    // Validate token fields against server config
    token.validate_with_max_auth_token_validity(
        config.zone_id,
        config.chain_id,
        max_auth_token_validity,
    )?;

    let signature =
        TempoSignature::from_bytes(&token.signature).map_err(|_| AuthError::InvalidSignature)?;
    let caller = signature
        .recover_signer(&token.digest)
        .map_err(|_| AuthError::InvalidSignature)?;

    if let TempoSignature::Keychain(keychain_signature) = &signature {
        validate_keychain_signature(api, caller, keychain_signature, &token.digest).await?;
    }

    Ok(AuthContext {
        caller,
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

    if key_info.isRevoked {
        return Err(AuthError::RevokedKeychainKey.into());
    }
    if key_info.keyId.is_zero() {
        return Err(AuthError::UnauthorizedKeychainKey.into());
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

#[cfg(test)]
mod tests {
    use super::authenticate_token;
    use crate::{
        PrivateRpcConfig,
        auth::build_token_fields,
        error::AuthenticateError,
        handlers::ZoneRpcApi,
        types::{BoxEyreFut, BoxFut, JsonRpcError},
    };
    use alloy_primitives::{Address, Bytes};
    use axum::http::StatusCode;
    use p256::ecdsa::SigningKey as P256SigningKey;
    use parking_lot::Mutex;
    use rand::thread_rng;
    use std::collections::HashMap;
    use tempo_contracts::precompiles::account_keychain::IAccountKeychain::{
        KeyInfo, SignatureType as KeyInfoSignatureType,
    };

    #[allow(dead_code)]
    mod auth_tokens {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/test-utils/auth_tokens.rs"
        ));
    }

    use auth_tokens::{build_token_with_signature, now_secs, sign_keychain_signature};

    const ZONE_ID: u32 = 7;
    const CHAIN_ID: u64 = 99;
    const PORTAL: Address = Address::repeat_byte(0x22);

    struct TestApi {
        key_infos: Mutex<HashMap<(Address, Address), KeyInfo>>,
    }

    impl TestApi {
        fn with_key_info(account: Address, key_id: Address, key_info: KeyInfo) -> Self {
            let mut key_infos = HashMap::new();
            key_infos.insert((account, key_id), key_info);
            Self {
                key_infos: Mutex::new(key_infos),
            }
        }
    }

    macro_rules! stub {
        ($method:ident $(, $arg:ident : $ty:ty)*) => {
            fn $method(&self $(, $arg: $ty)*) -> BoxFut<'_> {
                Box::pin(async { Err(JsonRpcError::internal("not implemented")) })
            }
        };
    }

    impl ZoneRpcApi for TestApi {
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

        stub!(block_number);
        stub!(chain_id);
        stub!(net_version);
        stub!(gas_price);
        stub!(max_priority_fee_per_gas);
        stub!(fee_history, _a: u64, _b: alloy_rpc_types_eth::BlockNumberOrTag, _c: Option<Vec<f64>>);
        stub!(get_balance, _a: Address, _b: Option<alloy_rpc_types_eth::BlockId>, _c: crate::auth::AuthContext);
        stub!(get_transaction_count, _a: Address, _b: Option<alloy_rpc_types_eth::BlockId>, _c: crate::auth::AuthContext);
        stub!(block_by_number, _a: alloy_rpc_types_eth::BlockNumberOrTag, _b: bool, _c: crate::auth::AuthContext);
        stub!(block_by_hash, _a: alloy_primitives::B256, _b: bool, _c: crate::auth::AuthContext);
        stub!(transaction_by_hash, _a: alloy_primitives::B256, _c: crate::auth::AuthContext);
        stub!(transaction_receipt, _a: alloy_primitives::B256, _c: crate::auth::AuthContext);
        stub!(call, _a: tempo_alloy::rpc::TempoTransactionRequest, _b: Option<alloy_rpc_types_eth::BlockId>, _c: Option<alloy_rpc_types_eth::state::StateOverride>, _d: crate::auth::AuthContext);
        stub!(estimate_gas, _a: tempo_alloy::rpc::TempoTransactionRequest, _b: Option<alloy_rpc_types_eth::BlockId>, _c: Option<alloy_rpc_types_eth::state::StateOverride>, _d: crate::auth::AuthContext);
        stub!(send_raw_transaction, _a: Bytes, _c: crate::auth::AuthContext);
        stub!(send_raw_transaction_sync, _a: Bytes, _c: crate::auth::AuthContext);
        stub!(fill_transaction, _a: tempo_alloy::rpc::TempoTransactionRequest, _c: crate::auth::AuthContext);
        stub!(get_logs, _a: alloy_rpc_types_eth::Filter, _c: crate::auth::AuthContext);
        stub!(new_filter, _a: alloy_rpc_types_eth::Filter, _c: crate::auth::AuthContext);
        stub!(get_filter_logs, _a: alloy_rpc_types_eth::FilterId, _c: crate::auth::AuthContext);
        stub!(get_filter_changes, _a: alloy_rpc_types_eth::FilterId, _c: crate::auth::AuthContext);
        stub!(new_block_filter, _c: crate::auth::AuthContext);
        stub!(uninstall_filter, _a: alloy_rpc_types_eth::FilterId, _c: crate::auth::AuthContext);
        stub!(zone_get_authorization_token_info, _c: crate::auth::AuthContext);
        stub!(zone_get_zone_info, _c: crate::auth::AuthContext);
        stub!(zone_get_deposit_status, _a: u64, _c: crate::auth::AuthContext);
    }

    fn test_config() -> PrivateRpcConfig {
        PrivateRpcConfig {
            listen_addr: ([127, 0, 0, 1], 0).into(),
            l1_rpc_url: "http://127.0.0.1:1".to_string(),
            zone_rpc_url: "http://127.0.0.1:1".to_string(),
            retry_connection_interval: std::time::Duration::from_millis(100),
            zone_id: ZONE_ID,
            chain_id: CHAIN_ID,
            max_auth_token_validity: crate::auth::DEFAULT_MAX_AUTH_TOKEN_VALIDITY,
            zone_portal: PORTAL,
        }
    }

    #[tokio::test]
    async fn configured_auth_token_validity_limit_is_enforced() {
        let mut config = test_config();
        config.max_auth_token_validity = std::time::Duration::from_secs(60);

        let now = now_secs();
        let (fields, _digest) = build_token_fields(ZONE_ID, CHAIN_ID, now, now + 600);
        let mut blob = vec![0u8; 65];
        blob.extend_from_slice(&fields);
        let token = alloy_primitives::hex::encode(blob);
        let api = TestApi {
            key_infos: Mutex::new(HashMap::new()),
        };

        let err = authenticate_token(&token, &config, &api)
            .await
            .expect_err("token window should exceed configured maximum");
        assert!(matches!(
            err,
            AuthenticateError::Invalid(crate::auth::AuthError::WindowTooLarge)
        ));
        assert_eq!(err.status_code(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn protocol_max_auth_token_validity_is_enforced_even_if_configured_higher() {
        let mut config = test_config();
        config.max_auth_token_validity =
            crate::auth::DEFAULT_MAX_AUTH_TOKEN_VALIDITY + std::time::Duration::from_secs(60);

        let now = now_secs();
        let (fields, _digest) = build_token_fields(
            ZONE_ID,
            CHAIN_ID,
            now,
            now + crate::auth::DEFAULT_MAX_AUTH_TOKEN_VALIDITY.as_secs() + 1,
        );
        let mut blob = vec![0u8; 65];
        blob.extend_from_slice(&fields);
        let token = alloy_primitives::hex::encode(blob);
        let api = TestApi {
            key_infos: Mutex::new(HashMap::new()),
        };

        let err = authenticate_token(&token, &config, &api)
            .await
            .expect_err("token window should exceed protocol maximum");
        assert!(matches!(
            err,
            AuthenticateError::Invalid(crate::auth::AuthError::WindowTooLarge)
        ));
        assert_eq!(err.status_code(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn revoked_keychain_key_is_classified_as_revoked() {
        let root_account = Address::repeat_byte(0x55);
        let access_signer = P256SigningKey::random(&mut thread_rng());
        let now = now_secs();
        let (fields, digest) = build_token_fields(ZONE_ID, CHAIN_ID, now, now + 600);
        let (signature, key_id) =
            sign_keychain_signature(digest, &access_signer, root_account, 0x04)
                .expect("keychain signing failed");
        let token = build_token_with_signature(signature, &fields);
        let api = TestApi::with_key_info(
            root_account,
            key_id,
            KeyInfo {
                signatureType: KeyInfoSignatureType::P256,
                keyId: Address::ZERO,
                expiry: 0,
                enforceLimits: false,
                isRevoked: true,
            },
        );

        let err = authenticate_token(&token, &test_config(), &api)
            .await
            .expect_err("revoked key should fail authentication");
        assert!(matches!(
            err,
            AuthenticateError::Invalid(crate::auth::AuthError::RevokedKeychainKey)
        ));
        assert_eq!(err.status_code(), StatusCode::FORBIDDEN);
    }
}
