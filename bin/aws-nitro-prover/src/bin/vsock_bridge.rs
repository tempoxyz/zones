//! Parent-side TCP <-> vsock bridge for the AWS Nitro prover service.

#![allow(unused_crate_dependencies)]

use std::net::SocketAddr;

use clap::Parser;
use tokio::{io::copy_bidirectional, net::TcpListener};
use tokio_vsock::{VsockAddr, VsockStream};

#[derive(Debug, Clone, Parser)]
struct Args {
    /// Local TCP listen address exposed on the parent instance.
    #[arg(
        long,
        env = "AWS_NITRO_PROVER_BRIDGE_LISTEN_ADDR",
        default_value = "127.0.0.1:8080"
    )]
    listen_addr: SocketAddr,

    /// Nitro enclave CID running the prover service.
    #[arg(long, env = "AWS_NITRO_PROVER_ENCLAVE_CID", default_value_t = 16)]
    enclave_cid: u32,

    /// Nitro enclave vsock port for the prover HTTP service.
    #[arg(long, env = "AWS_NITRO_PROVER_VSOCK_PORT", default_value_t = 8080)]
    vsock_port: u32,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args = Args::parse();
    let listener = TcpListener::bind(args.listen_addr).await?;

    println!(
        "aws-nitro-prover-vsock-bridge listening on {} -> vsock://{}:{}",
        listener.local_addr()?,
        args.enclave_cid,
        args.vsock_port
    );

    loop {
        let (mut tcp_stream, peer_addr) = listener.accept().await?;
        let enclave_addr = VsockAddr::new(args.enclave_cid, args.vsock_port);

        tokio::spawn(async move {
            let result: eyre::Result<()> = async {
                let mut vsock_stream = VsockStream::connect(enclave_addr).await?;
                copy_bidirectional(&mut tcp_stream, &mut vsock_stream).await?;
                Ok(())
            }
            .await;

            if let Err(error) = result {
                eprintln!("bridge connection from {peer_addr} failed: {error:?}");
            }
        });
    }
}
