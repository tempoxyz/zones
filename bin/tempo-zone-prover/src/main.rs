use std::io;

use clap::Parser;
use tempo_zone_prover::{DEFAULT_MAX_REQUEST_BYTES, DEFAULT_VSOCK_PORT};
use tracing_subscriber::EnvFilter;

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

    #[cfg(target_os = "linux")]
    return linux::serve(cli.port, cli.max_request_bytes)
        .await
        .map_err(Into::into);

    #[cfg(not(target_os = "linux"))]
    {
        let _ = cli;
        Err("AF_VSOCK service is supported only on Linux".into())
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{io, time::Instant};

    use tokio_vsock::{VMADDR_CID_ANY, VsockAddr, VsockListener};
    use tracing::{error, info, warn};

    use super::*;
    use tempo_zone_prover::{
        frame_error_response, process_payload, read_frame, serialize_response, write_frame,
    };

    pub(super) async fn serve(port: u32, maximum: usize) -> io::Result<()> {
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
                    let response = process_payload(&payload);
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
