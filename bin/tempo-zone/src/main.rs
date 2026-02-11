//! Tempo Zone L2 Node.
//!
//! This binary runs a lightweight L2 node using the reth node builder infrastructure.
//! It subscribes to L1 chain events to extract deposit transactions.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use std::sync::Arc;

use alloy_primitives::Address;
use clap::Parser;
use reth_consensus::noop::NoopConsensus;
use reth_ethereum::cli::Cli;

use reth_tracing::tracing::info;
use tempo_chainspec::spec::{TempoChainSpec, TempoChainSpecParser};
use tempo_evm::{TempoEvmConfig, TempoEvmFactory};
use zone::{DepositQueue, L1SubscriberConfig, ZoneNode};

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

/// Tempo Zone CLI arguments.
#[derive(Debug, Clone, clap::Args)]
struct ZoneArgs {
    /// L1 WebSocket RPC URL for subscribing to deposit events.
    #[arg(long = "l1.rpc-url", env = "L1_RPC_URL")]
    pub l1_rpc_url: String,

    /// ZonePortal contract address on L1.
    #[arg(long = "l1.portal-address", env = "L1_PORTAL_ADDRESS")]
    pub portal_address: Address,

    /// TIP-20 token address to mint on deposit.
    #[arg(long = "l1.token-address", env = "L1_TOKEN_ADDRESS")]
    pub token_address: Address,

    /// Block building interval in milliseconds.
    #[arg(
        long = "block.interval-ms",
        env = "BLOCK_INTERVAL_MS",
        default_value = "250"
    )]
    pub block_interval_ms: u64,
}

fn main() {
    reth_cli_util::sigsegv_handler::install();

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    let components = |spec: Arc<TempoChainSpec>| {
        (
            TempoEvmConfig::new(spec, TempoEvmFactory::default()),
            NoopConsensus::default(),
        )
    };

    if let Err(err) = Cli::<TempoChainSpecParser, ZoneArgs>::parse()
        .run_with_components::<ZoneNode>(components, async move |builder, args| {
            info!(target: "reth::cli", "Launching Tempo Zone node");

            let deposits = DepositQueue::default();
            let l1_config = L1SubscriberConfig {
                l1_rpc_url: args.l1_rpc_url,
                portal_address: args.portal_address,
            };
            let node = ZoneNode::new(deposits, args.token_address, l1_config);

            let handle = builder.node(node).launch_with_debug_capabilities().await?;

            info!(target: "reth::cli", "Tempo Zone node started");

            handle.node_exit_future.await?;
            Ok(())
        })
    {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
