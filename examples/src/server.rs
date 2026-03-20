use std::{net::SocketAddr, sync::Arc};

use alloy::primitives::Address;
use anyhow::Result;
use axum::{
    Json, Router,
    extract::State,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use tokio::{net::TcpListener, sync::RwLock, task::JoinHandle};
use tracing::info;

use crate::{
    model::{
        CompleteRegistrationRequest, ErrorResponse, LinkRecipientRequest, MetaResponse,
        MintRouteRequest, ResolveRequest, StartRegistrationRequest, StatusResponse,
    },
    state::{StoreConfig, StoreError, Stores},
};

pub type SharedStores = Arc<RwLock<Stores>>;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub addr: SocketAddr,
    pub l1_rpc_url: String,
    pub portal_address: Address,
    pub token_address: Address,
}

#[derive(Debug)]
pub struct SpawnedServer {
    pub base_url: String,
    pub join_handle: JoinHandle<()>,
}

#[derive(Debug)]
struct AppError(StoreError);

impl From<StoreError> for AppError {
    fn from(value: StoreError) -> Self {
        Self(value)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.0.status_code();
        let body = Json(ErrorResponse {
            error: self.0.to_string(),
        });
        (status, body).into_response()
    }
}

pub async fn run_server(config: ServerConfig) -> Result<()> {
    let listener = TcpListener::bind(config.addr).await?;
    let local_addr = listener.local_addr()?;
    let app = router(shared_state(config));
    info!("handoff demo server listening on http://{local_addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

pub async fn spawn_server(config: ServerConfig) -> Result<SpawnedServer> {
    let listener = TcpListener::bind(config.addr).await?;
    let local_addr = listener.local_addr()?;
    let app = router(shared_state(config));
    let join_handle = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::error!(?error, "handoff demo server exited");
        }
    });

    Ok(SpawnedServer {
        base_url: format!("http://{local_addr}"),
        join_handle,
    })
}

fn shared_state(config: ServerConfig) -> SharedStores {
    Arc::new(RwLock::new(Stores::new(StoreConfig {
        l1_rpc_url: config.l1_rpc_url,
        portal_address: config.portal_address,
        token_address: config.token_address,
    })))
}

fn router(state: SharedStores) -> Router {
    Router::new()
        .route("/meta", get(meta))
        .route("/identity/register/start", post(start_registration))
        .route("/identity/register/complete", post(complete_registration))
        .route("/identity/resolve", post(resolve))
        .route("/settlement/link-recipient", post(link_recipient))
        .route("/settlement/mint-route", post(mint_route))
        .with_state(state)
}

async fn meta(State(state): State<SharedStores>) -> Json<MetaResponse> {
    let state = state.read().await;
    Json(state.meta())
}

async fn start_registration(
    State(state): State<SharedStores>,
    Json(request): Json<StartRegistrationRequest>,
) -> Result<Json<crate::model::StartRegistrationResponse>, AppError> {
    let mut state = state.write().await;
    Ok(Json(state.start_registration(request)?))
}

async fn complete_registration(
    State(state): State<SharedStores>,
    Json(request): Json<CompleteRegistrationRequest>,
) -> Result<Json<StatusResponse>, AppError> {
    let mut state = state.write().await;
    Ok(Json(state.complete_registration(request)?))
}

async fn resolve(
    State(state): State<SharedStores>,
    Json(request): Json<ResolveRequest>,
) -> Result<Json<crate::model::ResolveResponse>, AppError> {
    let mut state = state.write().await;
    Ok(Json(state.resolve(request)?))
}

async fn link_recipient(
    State(state): State<SharedStores>,
    Json(request): Json<LinkRecipientRequest>,
) -> Result<Json<StatusResponse>, AppError> {
    let mut state = state.write().await;
    Ok(Json(state.link_recipient(request)?))
}

async fn mint_route(
    State(state): State<SharedStores>,
    Json(request): Json<MintRouteRequest>,
) -> Result<Json<crate::model::MintRouteResponse>, AppError> {
    let mut state = state.write().await;
    Ok(Json(state.mint_route(request).await?))
}
