//! Single-node QMDB test Zone. Uses real Zone execution with a synthetic, empty Tempo L1.

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use clap::Parser;
use jsonrpsee::server::{ServerBuilder, ServerConfig};

mod node;
mod rpc;

/// Run an isolated QMDB Zone; never connects to or settles on a real L1.
#[derive(Debug, Parser)]
struct Args {
    /// Directory holding the exclusively locked, atomically saved test-chain journal.
    #[arg(long, default_value = "./qmdb-zone-data")]
    datadir: PathBuf,
    /// Loopback-only JSON-RPC listener. There is no authentication.
    #[arg(long, default_value = "127.0.0.1:9545")]
    http: SocketAddr,
    /// Seconds between empty blocks; zero selects manual/transaction-driven mining.
    #[arg(long, default_value_t = 1)]
    block_time: u64,
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args = Args::parse();
    eyre::ensure!(
        args.http.ip().is_loopback(),
        "test RPC must bind to loopback"
    );
    let node = Arc::new(Mutex::new(node::Node::open(args.datadir)?));
    let server = ServerBuilder::default()
        .set_config(
            ServerConfig::builder()
                .max_request_body_size(16 * 1024 * 1024)
                .max_response_body_size(128 * 1024 * 1024)
                .build(),
        )
        .build(args.http)
        .await?;
    let address = server.local_addr()?;
    let handle = server.start(rpc::module(node.clone())?);
    println!(
        "QMDB Zone test RPC: http://{address}; chainId={}; MOCK L1, NO settlement",
        node.lock().unwrap().chain_id()
    );
    let mining = if args.block_time > 0 {
        Some(tokio::spawn(async move {
            let mut timer = tokio::time::interval(Duration::from_secs(args.block_time));
            timer.tick().await;
            loop {
                timer.tick().await;
                let node = node.clone();
                let result =
                    tokio::task::spawn_blocking(move || node.lock().unwrap().mine(Vec::new()))
                        .await;
                match result {
                    Ok(Ok(_)) => {}
                    error => {
                        eprintln!("block production failed: {error:?}");
                        break;
                    }
                }
            }
        }))
    } else {
        None
    };
    tokio::signal::ctrl_c().await?;
    handle.stop()?;
    handle.stopped().await;
    if let Some(mining) = mining {
        mining.abort();
    }
    Ok(())
}
