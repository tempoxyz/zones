use std::{io, path::PathBuf, sync::Arc};

use alloy_genesis::Genesis;
use clap::Parser;
use tempo_chainspec::TempoChainSpec;
use tempo_zone_prover::{DEFAULT_MAX_REQUEST_BYTES, DEFAULT_VSOCK_PORT, TrustedChainSpecs};
use tracing_subscriber::EnvFilter;

const EMBEDDED_TEMPO_GENESIS: &str = "/etc/tempo/genesis/genesis.json";

#[derive(Debug, Parser)]
#[command(
    name = "tempo-zone-prover",
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

    /// Tracing filter. Can also be set with RUST_LOG.
    #[arg(long, env = "RUST_LOG", default_value = "tempo_zone_prover=info")]
    log_filter: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(&cli.log_filter)?)
        .with_writer(io::stderr)
        .try_init()?;

    let mut tempo_genesis = cli.tempo_genesis.clone();
    let embedded_genesis = PathBuf::from(EMBEDDED_TEMPO_GENESIS);
    if embedded_genesis.is_file() && !tempo_genesis.contains(&embedded_genesis) {
        tempo_genesis.push(embedded_genesis);
    }
    let specs = load_trusted_chain_specs(&tempo_genesis)?;

    #[cfg(target_os = "linux")]
    return linux::serve(cli.port, cli.max_request_bytes, specs)
        .await
        .map_err(Into::into);

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (cli, specs);
        Err("AF_VSOCK service is supported only on Linux".into())
    }
}

fn load_trusted_chain_specs(paths: &[PathBuf]) -> io::Result<TrustedChainSpecs> {
    let mut specs = TrustedChainSpecs::default();
    for path in paths {
        let raw = std::fs::read(path)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_repeated_custom_genesis_arguments() {
        let cli = Cli::try_parse_from([
            "tempo-zone-prover",
            "--tempo-genesis",
            "first.json",
            "--tempo-genesis",
            "second.json",
        ])
        .unwrap();

        assert_eq!(
            cli.tempo_genesis,
            [PathBuf::from("first.json"), PathBuf::from("second.json")]
        );
    }

    #[test]
    fn loads_custom_genesis_files() {
        let directory =
            std::env::temp_dir().join(format!("tempo-zone-prover-genesis-{}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let mut genesis = Genesis::default();
        genesis.config.chain_id = 31_318;
        std::fs::write(
            directory.join("custom.json"),
            serde_json::to_vec(&genesis).unwrap(),
        )
        .unwrap();

        let specs = load_trusted_chain_specs(&[directory.join("custom.json")]).unwrap();

        std::fs::remove_dir_all(directory).unwrap();
        assert!(specs.supports(31_318));
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{io, time::Instant};

    use tokio_vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener};
    use tracing::{error, info, warn};

    use super::*;
    use tempo_zone_prover::{
        frame_error_response, process_payload_with_specs, read_frame, serialize_response,
        write_frame,
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
            let mut connection = match listener.accept().await {
                Ok((connection, _peer)) => connection,
                Err(error) => {
                    error!(%error, "failed to accept vsock connection");
                    continue;
                }
            };
            let started = Instant::now();
            match read_frame(&mut connection, maximum).await {
                Ok(payload) => {
                    let request_bytes = payload.len();
                    let response = process_payload_with_specs(&payload, &specs);
                    let encoded = serialize_response(&response);
                    if let Err(error) = write_frame(&mut connection, &encoded).await {
                        warn!(%error, "failed to write SPF response");
                    } else {
                        info!(
                            request_bytes,
                            response_bytes = encoded.len(),
                            elapsed_ms = started.elapsed().as_millis(),
                            "SPF request complete"
                        );
                    }
                }
                Err(frame_error) => {
                    warn!(error = %frame_error, "rejected SPF request frame");
                    let encoded = serialize_response(&frame_error_response(&frame_error));
                    if let Err(error) = write_frame(&mut connection, &encoded).await {
                        warn!(%error, "failed to write frame error response");
                    }
                }
            }
        }
    }
}
