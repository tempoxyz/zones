use std::{io, path::PathBuf, process::ExitCode, sync::Arc, time::Duration};

use alloy_genesis::Genesis;
use clap::{Args, Parser};
use tempo_chainspec::TempoChainSpec;
use tracing::error;
use tracing_subscriber::EnvFilter;
use zone_chainspec::ZoneChainSpec;
use zone_primitives::constants::zone_chain_id;
use zone_prover::{
    DEFAULT_MAX_REQUEST_BYTES, ErrorCode, NITRO_VERIFIER_CONFIG_V1, PROTOCOL_VERSION, ProofBundle,
    ProverConnection, TrustedChainSpecs, VerifyRequest, VerifyResponse,
    nitro_batch_attestation_hash, request_error_response,
};
use zone_spf::{BatchOutput, CancelToken, PublicInputs, SpfConfig, prove_zone_batch_with_cancel};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(tracing::Level::INFO.into())
                .from_env_lossy(),
        )
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            error!(%error, "prover failed");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "tempo-zone-prover-enclave",
    about = "AWS Nitro Enclave service for the Tempo Zone SPF"
)]
struct Cli {
    /// Port on which the service accepts verification requests.
    #[arg(long, env = "SPF_PORT", default_value_t = 5000)]
    port: u32,

    /// Maximum accepted JSON request size in bytes.
    #[arg(
        long,
        env = "SPF_MAX_REQUEST_BYTES",
        default_value_t = DEFAULT_MAX_REQUEST_BYTES
    )]
    max_request_bytes: usize,

    /// Directory containing trusted custom Tempo genesis JSON files.
    #[arg(long, env = "SPF_TEMPO_GENESIS", value_name = "DIR")]
    tempo_genesis: Option<PathBuf>,

    /// Listen on TCP instead of AF_VSOCK.
    #[arg(long)]
    use_tcp: bool,

    #[command(flatten)]
    timeouts: Timeouts,
}

impl Cli {
    async fn run(self) -> io::Result<()> {
        let specs = Arc::new(self.load_trusted_chain_specs()?);

        if self.use_tcp {
            return serve_tcp(self.port, self.max_request_bytes, specs, self.timeouts).await;
        }

        #[cfg(target_os = "linux")]
        {
            linux::serve_vsock(self.port, self.max_request_bytes, specs, self.timeouts).await
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = (self, specs);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "AF_VSOCK service is supported only on Linux".to_string(),
            ))
        }
    }

    fn load_trusted_chain_specs(&self) -> io::Result<TrustedChainSpecs> {
        let mut specs = TrustedChainSpecs::default();
        let Some(directory) = &self.tempo_genesis else {
            return Ok(specs);
        };
        let mut paths = std::fs::read_dir(directory)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<Vec<_>>>()?;
        paths.retain(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        });
        paths.sort();

        for path in paths {
            let raw = std::fs::read(&path)?;
            let genesis = serde_json::from_slice::<Genesis>(&raw).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("parse Tempo genesis {}: {error}", path.display()),
                )
            })?;
            let chain_id = genesis.config.chain_id;
            specs
                .insert(chain_id, Arc::new(TempoChainSpec::from_genesis(genesis)))
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            tracing::info!(chain_id, path = %path.display(), "loaded trusted custom Tempo genesis");
        }

        Ok(specs)
    }
}

#[derive(Clone, Debug, Args)]
struct Timeouts {
    /// Deadline for receiving one complete logical request.
    #[arg(long = "request-timeout-secs", env = "SPF_REQUEST_TIMEOUT_SECS", default_value = "5", value_parser = non_zero_secs)]
    request: Duration,
    /// Deadline for processing and proving one request.
    #[arg(long = "proving-timeout-secs", env = "SPF_PROVING_TIMEOUT_SECS", default_value = "30", value_parser = non_zero_secs)]
    proving: Duration,
    /// Deadline for writing one complete logical response.
    #[arg(long = "response-timeout-secs", env = "SPF_RESPONSE_TIMEOUT_SECS", default_value = "5", value_parser = non_zero_secs)]
    response: Duration,
}

fn non_zero_secs(value: &str) -> Result<Duration, String> {
    let seconds = value
        .parse::<u64>()
        .map_err(|error| format!("invalid duration: {error}"))?;
    if seconds == 0 {
        return Err("duration must be greater than zero".into());
    }
    Ok(Duration::from_secs(seconds))
}

async fn serve_tcp(
    port: u32,
    maximum: usize,
    specs: Arc<TrustedChainSpecs>,
    timeouts: Timeouts,
) -> io::Result<()> {
    use tokio::net::TcpListener;
    use tracing::info;

    let port = u16::try_from(port).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("TCP port {port} exceeds the maximum of {}", u16::MAX),
        )
    })?;
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    info!(
        port,
        max_request_bytes = maximum,
        request_timeout_secs = timeouts.request.as_secs(),
        proving_timeout_secs = timeouts.proving.as_secs(),
        response_timeout_secs = timeouts.response.as_secs(),
        "SPF TCP service listening"
    );

    loop {
        let (connection, _peer) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                error!(%error, "failed to accept TCP connection");
                continue;
            }
        };
        handle_connection(connection, maximum, Arc::clone(&specs), &timeouts).await;
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io;

    use tokio_vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener};
    use tracing::{error, info};

    use super::*;

    pub(super) async fn serve_vsock(
        port: u32,
        maximum: usize,
        specs: Arc<TrustedChainSpecs>,
        timeouts: Timeouts,
    ) -> io::Result<()> {
        let listener = VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, port))?;
        info!(
            port,
            max_request_bytes = maximum,
            request_timeout_secs = timeouts.request.as_secs(),
            proving_timeout_secs = timeouts.proving.as_secs(),
            response_timeout_secs = timeouts.response.as_secs(),
            "SPF enclave service listening"
        );

        loop {
            let connection = match listener.accept().await {
                Ok((connection, _peer)) => connection,
                Err(error) => {
                    error!(%error, "failed to accept vsock connection");
                    continue;
                }
            };
            handle_connection(connection, maximum, Arc::clone(&specs), &timeouts).await;
        }
    }
}

async fn handle_connection<T>(
    stream: T,
    maximum: usize,
    specs: Arc<TrustedChainSpecs>,
    timeouts: &Timeouts,
) where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use std::time::Instant;

    use tracing::{info, warn};
    let mut connection = ProverConnection::new(stream, maximum);
    let started = Instant::now();
    let request: VerifyRequest =
        match tokio::time::timeout(timeouts.request, connection.receive()).await {
            Err(_) => {
                warn!(
                    elapsed_ms = started.elapsed().as_millis(),
                    limit_secs = timeouts.request.as_secs(),
                    "SPF request receive timed out"
                );
                return;
            }
            Ok(Ok(Some(request))) => request,
            Ok(Err(error)) => {
                warn!(%error, "rejected SPF request frame");
                timed_send(
                    &mut connection,
                    request_error_response(&error),
                    timeouts.response,
                )
                .await;
                return;
            }
            Ok(Ok(None)) => {
                warn!("connection closed before sending an SPF request frame");
                return;
            }
        };
    let request_bytes = connection.last_received_bytes().unwrap_or_default();
    let request_id = request.request_id.clone();
    let cancellation = CancelToken::default();
    let worker_cancellation = cancellation.clone();
    let mut worker =
        tokio::task::spawn_blocking(move || process_request(request, &specs, &worker_cancellation));
    let deadline = tokio::time::sleep(timeouts.proving);
    tokio::pin!(deadline);
    let response = tokio::select! {
        biased;
        result = &mut worker => match result {
            Ok(response) => response,
            Err(error) => err_response(
                request_id.clone(),
                ErrorCode::InternalError,
                format!("proving worker failed: {error}"),
            ),
        },
        () = &mut deadline => {
            cancellation.cancel();
            let _ = worker.await;
            warn!(elapsed_ms = started.elapsed().as_millis(), limit_secs = timeouts.proving.as_secs(), "SPF proving timed out; cancelled worker joined");
            err_response(
                request_id,
                ErrorCode::ProvingTimedOut,
                format!("proving exceeded the configured {} second deadline", timeouts.proving.as_secs()),
            )
        }
    };
    if let Some(response_bytes) = timed_send(&mut connection, response, timeouts.response).await {
        info!(
            request_bytes,
            response_bytes,
            elapsed_ms = started.elapsed().as_millis(),
            "SPF request complete"
        );
    }
}

async fn timed_send<T>(
    connection: &mut ProverConnection<T>,
    response: VerifyResponse,
    limit: Duration,
) -> Option<usize>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tracing::warn;
    let started = std::time::Instant::now();
    match tokio::time::timeout(limit, connection.send(response)).await {
        Ok(Ok(bytes)) => Some(bytes),
        Ok(Err(error)) => {
            warn!(%error, "failed to write SPF response");
            None
        }
        Err(_) => {
            warn!(
                elapsed_ms = started.elapsed().as_millis(),
                limit_secs = limit.as_secs(),
                "SPF response send timed out"
            );
            None
        }
    }
}

fn process_request(
    request: VerifyRequest,
    specs: &TrustedChainSpecs,
    cancel: &CancelToken,
) -> VerifyResponse {
    if request.version != PROTOCOL_VERSION {
        return err_response(
            request.request_id,
            ErrorCode::UnsupportedVersion,
            format!(
                "unsupported protocol version {}; expected {PROTOCOL_VERSION}",
                request.version
            ),
        );
    }

    let tempo_chain_id = request.witness.public_inputs.parent_chain_id;
    let Some(tempo_spec) = specs.resolve(tempo_chain_id) else {
        return err_response(
            request.request_id,
            ErrorCode::UnsupportedChain,
            format!("unsupported Tempo chain ID {tempo_chain_id}"),
        );
    };
    let zone_chain_id = match zone_chain_id(tempo_chain_id, request.witness.public_inputs.zone_id) {
        Ok(chain_id) => chain_id,
        Err(error) => {
            return err_response(
                request.request_id,
                ErrorCode::VerificationFailed,
                error.to_string(),
            );
        }
    };
    let mut zone_genesis = tempo_spec.inner.genesis.clone();
    zone_genesis.config.chain_id = zone_chain_id;
    let zone_spec = match ZoneChainSpec::from_genesis_with_l1(zone_genesis, tempo_spec.as_ref()) {
        Ok(spec) => spec,
        Err(error) => {
            return err_response(
                request.request_id,
                ErrorCode::UnsupportedChain,
                error.to_string(),
            );
        }
    };
    let config = SpfConfig::new(Arc::new(zone_spec));

    let public_inputs = request.witness.public_inputs.clone();
    match prove_zone_batch_with_cancel(&config, request.witness, cancel) {
        Ok(_output) if cancel.is_cancelled() => err_response(
            request.request_id,
            ErrorCode::ProvingTimedOut,
            "proving was cancelled before attestation",
        ),
        Ok(output) => match build_proof_bundle(&public_inputs, &output, nitro_attestation) {
            Ok(proof_bundle) => VerifyResponse::Ok {
                version: PROTOCOL_VERSION,
                request_id: request.request_id,
                output: Box::new(output),
                proof_bundle,
            },
            Err(message) => err_response(
                request.request_id,
                ErrorCode::AttestationUnavailable,
                message,
            ),
        },
        Err(error) => err_response(
            request.request_id,
            ErrorCode::VerificationFailed,
            error.to_string(),
        ),
    }
}

fn err_response(request_id: String, code: ErrorCode, message: impl Into<String>) -> VerifyResponse {
    VerifyResponse::Error {
        version: PROTOCOL_VERSION,
        request_id: Some(request_id),
        code,
        message: message.into(),
    }
}

fn build_proof_bundle<F>(
    public_inputs: &PublicInputs,
    output: &BatchOutput,
    attestor: F,
) -> Result<ProofBundle, String>
where
    F: FnOnce(alloy_primitives::B256) -> Result<Vec<u8>, String>,
{
    let digest = nitro_batch_attestation_hash(public_inputs, output);
    let document = attestor(digest)?;
    Ok(ProofBundle {
        verifier_config: NITRO_VERIFIER_CONFIG_V1.to_vec().into(),
        proof: document.into(),
    })
}

#[cfg(target_os = "linux")]
fn nitro_attestation(digest: alloy_primitives::B256) -> Result<Vec<u8>, String> {
    use aws_nitro_enclaves_nsm_api::{
        api::{Request, Response},
        driver::{nsm_exit, nsm_init, nsm_process_request},
    };
    use serde_bytes::ByteBuf;

    let descriptor = nsm_init();
    if descriptor < 0 {
        return Err("Nitro Secure Module device is unavailable".into());
    }
    let response = nsm_process_request(
        descriptor,
        Request::Attestation {
            user_data: Some(ByteBuf::from(digest.to_vec())),
            nonce: None,
            public_key: None,
        },
    );
    nsm_exit(descriptor);
    match response {
        Response::Attestation { document } => Ok(document),
        Response::Error(code) => Err(format!("Nitro attestation request failed: {code:?}")),
        _ => Err("Nitro Secure Module returned an unexpected response".into()),
    }
}

#[cfg(not(target_os = "linux"))]
fn nitro_attestation(_digest: alloy_primitives::B256) -> Result<Vec<u8>, String> {
    Err("Nitro attestation is supported only on Linux".into())
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;
    use alloy_primitives::{B256, Bytes};
    use reth_trie_common::EMPTY_ROOT_HASH;
    use tempo_primitives::TempoHeader;
    use zone_spf::{
        BatchWitness, BlockTransition, DepositQueueTransition, LastBatchCommitment, PublicInputs,
        TempoStateWitness, TokenEnablementTransition, ZoneStateWitness,
    };

    use super::*;

    #[test]
    fn rejects_unsupported_protocol_version() {
        let request = VerifyRequest {
            version: PROTOCOL_VERSION + 1,
            request_id: "version-test".into(),
            witness: empty_witness(),
        };
        let response = process_request(
            request,
            &TrustedChainSpecs::default(),
            &CancelToken::default(),
        );

        assert!(matches!(
            response,
            VerifyResponse::Error {
                request_id: Some(id),
                code: ErrorCode::UnsupportedVersion,
                ..
            } if id == "version-test"
        ));
    }

    #[test]
    fn rejects_unknown_chain_before_execution() {
        let mut witness = empty_witness();
        witness.public_inputs.parent_chain_id = 99;
        let request = VerifyRequest {
            version: PROTOCOL_VERSION,
            request_id: "chain-test".into(),
            witness,
        };
        let response = process_request(
            request,
            &TrustedChainSpecs::default(),
            &CancelToken::default(),
        );

        assert!(matches!(
            response,
            VerifyResponse::Error {
                request_id: Some(id),
                code: ErrorCode::UnsupportedChain,
                ..
            } if id == "chain-test"
        ));
    }

    #[test]
    fn binds_the_canonical_digest_into_the_proof_bundle() {
        let public_inputs = empty_witness().public_inputs;
        let output = BatchOutput {
            block_transition: BlockTransition {
                prevBlockHash: B256::with_last_byte(1),
                nextBlockHash: B256::with_last_byte(2),
            },
            deposit_queue_transition: DepositQueueTransition {
                prevProcessedHash: B256::with_last_byte(3),
                nextProcessedHash: B256::with_last_byte(4),
                prevDepositNumber: 5,
                nextDepositNumber: 6,
            },
            token_enablement_transition: TokenEnablementTransition {
                prevProcessedTokenCount: 7,
                nextProcessedTokenCount: 8,
            },
            withdrawal_queue_hash: B256::with_last_byte(9),
            last_batch_commitment: LastBatchCommitment {
                withdrawal_batch_index: 10,
            },
        };
        let expected_digest = nitro_batch_attestation_hash(&public_inputs, &output);
        let document = vec![0xd2, 0x84, 0x43];

        let bundle = build_proof_bundle(&public_inputs, &output, |digest| {
            assert_eq!(digest, expected_digest);
            Ok(document.clone())
        })
        .unwrap();

        assert_eq!(bundle.verifier_config.as_ref(), NITRO_VERIFIER_CONFIG_V1);
        assert_eq!(bundle.proof.as_ref(), document);
    }

    #[test]
    fn reports_spf_verification_errors_without_panicking() {
        let request = VerifyRequest {
            version: PROTOCOL_VERSION,
            request_id: "spf-test".into(),
            witness: empty_witness(),
        };
        let response = process_request(
            request,
            &TrustedChainSpecs::default(),
            &CancelToken::default(),
        );

        assert!(matches!(
            response,
            VerifyResponse::Error {
                request_id: Some(id),
                code: ErrorCode::VerificationFailed,
                ..
            } if id == "spf-test"
        ));
    }

    #[test]
    fn accepts_a_configured_custom_chain() {
        let chain_id = 31_318;
        let mut genesis = Genesis::default();
        genesis.config.chain_id = chain_id;
        let mut specs = TrustedChainSpecs::default();
        specs
            .insert(chain_id, Arc::new(TempoChainSpec::from_genesis(genesis)))
            .unwrap();
        let request = VerifyRequest {
            version: PROTOCOL_VERSION,
            request_id: "custom-chain-test".into(),
            witness: empty_witness(),
        };

        let response = process_request(request, &specs, &CancelToken::default());

        assert!(matches!(
            response,
            VerifyResponse::Error {
                request_id: Some(id),
                code: ErrorCode::VerificationFailed,
                ..
            } if id == "custom-chain-test"
        ));
    }

    fn empty_witness() -> BatchWitness {
        let tempo_header = TempoHeader {
            inner: Header {
                number: 2,
                state_root: EMPTY_ROOT_HASH,
                ..Default::default()
            },
            ..Default::default()
        };
        BatchWitness {
            public_inputs: PublicInputs {
                parent_chain_id: 42_431,
                zone_id: 1,
                tempo_block_number: 2,
                anchor_block_number: 2,
                anchor_block_hash: B256::ZERO,
                expected_withdrawal_batch_index: 3,
            },
            parent_header: TempoHeader::default(),
            zone_blocks: Vec::new(),
            zone_state_witness: ZoneStateWitness {
                node_pool: Vec::new(),
                bytecodes: Vec::new(),
            },
            tempo_state_witness: TempoStateWitness {
                initial_tempo_header_rlp: Bytes::from(alloy_rlp::encode(tempo_header)),
                node_pool: Vec::new(),
            },
            tempo_ancestry_headers: Vec::new(),
        }
    }
}
