//! Standalone HTTP server for the `zone_getBatchWitness` JSON-RPC method.
//!
//! Exposes the latest unsubmitted batch witness over a simple JSON-RPC endpoint.
//! Protected by nginx Basic Auth at the infrastructure level (no crypto auth).

use std::sync::Arc;

use alloy_primitives::{B256, U256};
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::post,
};
use reth_provider::StateProviderFactory;
use reth_storage_api::StateProvider;
use tracing::{info, warn};

use crate::{
    abi::{TEMPO_PACKED_SLOT, TEMPO_STATE_ADDRESS, ZONE_OUTBOX_ADDRESS},
    proof::BatchProofGenerator,
    witness::SharedWitnessStore,
};

/// Shared state for the witness RPC server.
#[derive(Clone)]
struct WitnessRpcState {
    proof_generator: Arc<dyn BatchProofGenerator>,
    witness_store: SharedWitnessStore,
    /// Zone state provider factory for reading on-chain state (tempo_block_number,
    /// withdrawal_batch_index) directly from the local trie.
    state_provider_factory: Arc<dyn StateProviderFactory>,
}

/// Start the witness RPC server on the given address.
///
/// Returns the bound local address on success.
pub async fn start_witness_rpc(
    listen_addr: std::net::SocketAddr,
    proof_generator: Arc<dyn BatchProofGenerator>,
    witness_store: SharedWitnessStore,
    state_provider_factory: Arc<dyn StateProviderFactory>,
) -> eyre::Result<std::net::SocketAddr> {
    let state = Arc::new(WitnessRpcState {
        proof_generator,
        witness_store,
        state_provider_factory,
    });
    let app = Router::new()
        .route("/", post(handle_witness_rpc))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    let local_addr = listener.local_addr()?;

    info!(target: "zone::witness_rpc", %local_addr, "Starting witness RPC server");

    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::error!(target: "zone::witness_rpc", %err, "Witness RPC server failed");
        }
    });

    Ok(local_addr)
}

/// JSON-RPC handler for `zone_getBatchWitness`.
async fn handle_witness_rpc(
    State(state): State<Arc<WitnessRpcState>>,
    body: Bytes,
) -> impl IntoResponse {
    let body_str = match std::str::from_utf8(&body) {
        Ok(s) => s,
        Err(_) => return (StatusCode::BAD_REQUEST, "invalid UTF-8").into_response(),
    };

    let req: serde_json::Value = match serde_json::from_str(body_str) {
        Ok(v) => v,
        Err(e) => {
            return json_rpc_error(serde_json::Value::Null, -32700, format!("parse error: {e}"));
        }
    };

    let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

    if method != "zone_getBatchWitness" {
        return json_rpc_error(id, -32601, "method not found".into());
    }

    match build_latest_witness(&state).await {
        Ok(Some(witness)) => match serde_json::to_value(&witness) {
            Ok(result) => json_rpc_ok(id, result),
            Err(e) => json_rpc_error(id, -32000, format!("serialization error: {e}")),
        },
        Ok(None) => json_rpc_error(id, -32004, "no unsubmitted batch available".into()),
        Err(e) => {
            warn!(target: "zone::witness_rpc", error = %e, "Failed to generate batch witness");
            json_rpc_error(id, -32000, format!("failed to build witness: {e}"))
        }
    }
}

/// Build a [`BatchWitness`] for the full range of blocks currently in the
/// witness store, deriving the required public input parameters from local
/// zone state.
async fn build_latest_witness(
    state: &WitnessRpcState,
) -> eyre::Result<Option<zone_prover::types::BatchWitness>> {
    // 1. Read the available block range from the store.
    let (from, to, prev_block_hash) = {
        let store = state.witness_store.lock().expect("witness store poisoned");
        let (Some(from), Some(to)) = (store.first_block(), store.last_block()) else {
            return Ok(None);
        };
        let first = store
            .get_range(from, from)
            .map_err(|b| eyre::eyre!("missing block {b}"))?;
        let prev_block_hash = first[0].1.parent_block_hash;
        (from, to, prev_block_hash)
    };

    // 2. Read tempo_block_number from TempoState storage at the latest zone state.
    let (tempo_block_number, expected_withdrawal_batch_index) = {
        let sp = state
            .state_provider_factory
            .latest()
            .map_err(|e| eyre::eyre!("failed to open latest state provider: {e}"))?;
        let tempo_block_number = read_tempo_block_number(&*sp)?;
        let withdrawal_batch_index = read_withdrawal_batch_index(&*sp)?;
        (tempo_block_number, withdrawal_batch_index + 1)
    };

    info!(
        target: "zone::witness_rpc",
        from, to, tempo_block_number, expected_withdrawal_batch_index,
        "Building batch witness for RPC"
    );

    let witness = state
        .proof_generator
        .generate_batch_witness(
            from,
            to,
            tempo_block_number,
            prev_block_hash,
            expected_withdrawal_batch_index,
        )
        .await?;

    Ok(Some(witness))
}

/// Read `tempoBlockNumber` from the TempoState packed slot.
fn read_tempo_block_number(sp: &dyn StateProvider) -> eyre::Result<u64> {
    let packed = sp
        .storage(TEMPO_STATE_ADDRESS, TEMPO_PACKED_SLOT)?
        .unwrap_or_default();
    Ok((packed & U256::from(u64::MAX)).to::<u64>())
}

/// Read `withdrawal_batch_index` from `ZoneOutbox._lastBatch` (base slot 5 + 1).
fn read_withdrawal_batch_index(sp: &dyn StateProvider) -> eyre::Result<u64> {
    let slot = B256::from(
        (zone_prover::execute::storage::ZONE_OUTBOX_LAST_BATCH_BASE_SLOT + U256::from(1))
            .to_be_bytes(),
    );
    let value = sp
        .storage(ZONE_OUTBOX_ADDRESS, slot)?
        .unwrap_or_default();
    Ok(value.to::<u64>())
}

fn json_rpc_ok(id: serde_json::Value, result: serde_json::Value) -> axum::response::Response {
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        })),
    )
        .into_response()
}

fn json_rpc_error(
    id: serde_json::Value,
    code: i32,
    message: String,
) -> axum::response::Response {
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message}
        })),
    )
        .into_response()
}
