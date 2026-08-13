use std::{io, path::PathBuf, process::ExitCode, sync::Arc};

use alloy_genesis::Genesis;
use clap::Parser;
use tempo_chainspec::TempoChainSpec;
use tempo_zone_prover_enclave::{DEFAULT_MAX_REQUEST_BYTES, DEFAULT_VSOCK_PORT, TrustedChainSpecs};
use tracing::error;
use tracing_subscriber::EnvFilter;

const EMBEDDED_TEMPO_GENESIS: &str = "/etc/tempo/genesis/genesis.json";

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
    /// AF_VSOCK port on which the enclave accepts verification requests.
    #[arg(long, env = "SPF_VSOCK_PORT", default_value_t = DEFAULT_VSOCK_PORT)]
    port: u32,

    /// Maximum accepted JSON request size in bytes.
    #[arg(
        long,
        env = "SPF_MAX_REQUEST_BYTES",
        default_value_t = DEFAULT_MAX_REQUEST_BYTES
    )]
    max_request_bytes: usize,

    /// Trusted custom Tempo genesis JSON file. May be specified more than once.
    #[arg(
        long,
        env = "SPF_TEMPO_GENESIS",
        value_name = "PATH",
        value_delimiter = ','
    )]
    tempo_genesis: Vec<PathBuf>,
}

impl Cli {
    async fn run(self) -> io::Result<()> {
        let specs = self.load_trusted_chain_specs()?;

        #[cfg(target_os = "linux")]
        {
            linux::serve(self.port, self.max_request_bytes, specs).await
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
        let mut paths = self.tempo_genesis.clone();
        let embedded = PathBuf::from(EMBEDDED_TEMPO_GENESIS);
        if embedded.is_file() && !paths.contains(&embedded) {
            paths.push(embedded);
        }

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

#[cfg(target_os = "linux")]
mod linux {
    use std::{io, time::Instant};

    use futures::{SinkExt as _, StreamExt as _};
    use tokio_vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener};
    use tracing::{error, info, warn};

    use super::*;
    use tempo_zone_prover_enclave::{
        framed, framing_error_response, process_payload, serialize_response,
    };

    pub(super) async fn serve(
        port: u32,
        maximum: usize,
        specs: TrustedChainSpecs,
    ) -> io::Result<()> {
        let listener = VsockListener::bind(VsockAddr::new(VMADDR_CID_ANY, port))?;
        info!(
            port,
            max_request_bytes = maximum,
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
            let mut connection = framed(connection, maximum);
            let started = Instant::now();
            let payload = match connection.next().await {
                Some(Ok(payload)) => payload,
                Some(Err(error)) => {
                    warn!(%error, "rejected SPF request frame");
                    let encoded = serialize_response(&framing_error_response(&error, maximum));
                    if let Err(error) = connection.send(encoded.into()).await {
                        warn!(%error, "failed to write frame error response");
                    }
                    continue;
                }
                None => {
                    warn!("connection closed before sending an SPF request frame");
                    continue;
                }
            };
            let request_bytes = payload.len();
            let response = process_payload(&payload, &specs);
            let encoded = serialize_response(&response);
            let response_bytes = encoded.len();
            if let Err(error) = connection.send(encoded.into()).await {
                warn!(%error, "failed to write SPF response");
            } else {
                info!(
                    request_bytes,
                    response_bytes,
                    elapsed_ms = started.elapsed().as_millis(),
                    "SPF request complete"
                );
            }
        }
    }
}
