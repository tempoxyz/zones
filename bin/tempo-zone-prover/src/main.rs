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

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(&cli.log_filter)?)
        .with_writer(io::stderr)
        .try_init()?;

    #[cfg(target_os = "linux")]
    return linux::serve(cli.port, cli.max_request_bytes).map_err(Into::into);

    #[cfg(not(target_os = "linux"))]
    {
        let _ = cli;
        Err("AF_VSOCK service is supported only on Linux".into())
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        fs::File,
        io,
        mem::{self, size_of},
        os::fd::{FromRawFd as _, RawFd},
        time::Instant,
    };

    use tracing::{error, info, warn};

    use super::*;
    use tempo_zone_prover::{
        frame_error_response, process_payload, read_frame, serialize_response, write_frame,
    };

    pub(super) fn serve(port: u32, maximum: usize) -> io::Result<()> {
        let listener = VsockListener::bind(port)?;
        info!(
            port,
            max_request_bytes = maximum,
            "SPF enclave service listening"
        );

        loop {
            let mut connection = match listener.accept() {
                Ok(connection) => connection,
                Err(error) => {
                    error!(%error, "failed to accept vsock connection");
                    continue;
                }
            };
            let started = Instant::now();
            match read_frame(&mut connection, maximum) {
                Ok(payload) => {
                    let request_bytes = payload.len();
                    let response = process_payload(&payload);
                    let encoded = serialize_response(&response);
                    if let Err(error) = write_frame(&mut connection, &encoded) {
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
                    if let Err(error) = write_frame(&mut connection, &encoded) {
                        warn!(%error, "failed to write frame error response");
                    }
                }
            }
        }
    }

    struct VsockListener(RawFd);

    impl VsockListener {
        fn bind(port: u32) -> io::Result<Self> {
            // SAFETY: socket has no pointer arguments and its return value is checked.
            let fd =
                unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let listener = Self(fd);

            // SAFETY: sockaddr_vm is a plain C socket address for which all-zero is valid.
            let mut address: libc::sockaddr_vm = unsafe { mem::zeroed() };
            address.svm_family = libc::AF_VSOCK as libc::sa_family_t;
            address.svm_cid = libc::VMADDR_CID_ANY;
            address.svm_port = port;
            // SAFETY: address points to an initialized sockaddr_vm and its exact length is passed.
            let result = unsafe {
                libc::bind(
                    fd,
                    (&raw const address).cast::<libc::sockaddr>(),
                    size_of::<libc::sockaddr_vm>() as libc::socklen_t,
                )
            };
            if result < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fd is a valid socket owned by listener and the return value is checked.
            if unsafe { libc::listen(fd, 16) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(listener)
        }

        fn accept(&self) -> io::Result<File> {
            // SAFETY: self.0 is a listening socket; no peer address is requested.
            let fd = unsafe {
                libc::accept4(
                    self.0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: accept4 returned a new owned file descriptor.
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    impl Drop for VsockListener {
        fn drop(&mut self) {
            // SAFETY: this type exclusively owns the descriptor.
            unsafe { libc::close(self.0) };
        }
    }
}
