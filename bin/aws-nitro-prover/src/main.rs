//! AWS Nitro prover echo service.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use aws_nitro_prover::{ProveBatchRequest, ProveBatchResponse};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use clap::Parser;
use const_hex as _;
use serde as _;
use tokio_vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener};

#[derive(Debug, Clone, Parser)]
struct Args {
    /// Vsock port for the enclave-local HTTP service.
    #[arg(long, env = "AWS_NITRO_PROVER_VSOCK_PORT", default_value_t = 8080)]
    vsock_port: u32,
}

#[derive(Clone, Default)]
struct AppState;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args = Args::parse();

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/prove-batch", post(prove_batch))
        .with_state(AppState);

    let listener = VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, args.vsock_port))?;
    println!(
        "aws-nitro-prover listening on vsock://{}:{}",
        VMADDR_CID_ANY, args.vsock_port
    );
    axum::serve(listener, app).await?;
    Ok(())
}

async fn healthz() -> &'static str {
    "ok"
}

async fn prove_batch(
    State(_state): State<AppState>,
    Json(request): Json<ProveBatchRequest>,
) -> Result<Json<ProveBatchResponse>, AppError> {
    let verifier_config = request.verifier_config().to_vec();
    let proof = attest(&verifier_config)?;

    Ok(Json(ProveBatchResponse {
        prev_block_hash: request.prev_block_hash,
        next_block_hash: request.next_block_hash,
        verifier_config,
        proof,
    }))
}

#[derive(Debug)]
struct AppError(eyre::Report);

impl<E> From<E> for AppError
where
    E: Into<eyre::Report>,
{
    fn from(error: E) -> Self {
        Self(error.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()).into_response()
    }
}

#[cfg(target_os = "linux")]
fn attest(user_data: &[u8]) -> eyre::Result<Vec<u8>> {
    use aws_nitro_enclaves_nsm_api::{
        api::{Request, Response},
        driver::{nsm_exit, nsm_init, nsm_process_request},
    };
    use serde_bytes::ByteBuf;

    let nsm_fd = nsm_init();
    eyre::ensure!(nsm_fd >= 0, "failed to open /dev/nsm");

    let response = nsm_process_request(
        nsm_fd,
        Request::Attestation {
            user_data: Some(ByteBuf::from(user_data.to_vec())),
            nonce: None,
            public_key: None,
        },
    );
    nsm_exit(nsm_fd);

    match response {
        Response::Attestation { document } => Ok(document),
        Response::Error(code) => Err(eyre::eyre!("NSM attestation request failed: {code:?}")),
        other => Err(eyre::eyre!(
            "unexpected NSM response while requesting attestation: {other:?}"
        )),
    }
}

#[cfg(not(target_os = "linux"))]
fn attest(_user_data: &[u8]) -> eyre::Result<Vec<u8>> {
    eyre::bail!("AWS Nitro attestation is only supported on Linux enclaves")
}
