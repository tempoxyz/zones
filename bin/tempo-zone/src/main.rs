//! Tempo Zone L2 Node.
//!
//! This binary runs a lightweight L2 node using the reth node builder infrastructure.
//! It subscribes to L1 chain events to extract deposit transactions.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

use clap::Parser;
use reth_consensus::noop::NoopConsensus;
use reth_ethereum::cli::Cli;
use reth_node_builder::NodeHandle;
use reth_tracing::tracing::info;
use std::sync::Arc;
use tempo_chainspec::spec::{TempoChainSpec, TempoChainSpecParser};
use tempo_evm::{TempoEvmConfig, TempoEvmFactory};
use tempo_zone::{L1SubscriberConfig, ZoneNode, ZoneNodeArgs, spawn_l1_subscriber};

/// Tempo Zone CLI arguments.
#[derive(Debug, Clone, clap::Args)]
struct ZoneArgs {
    #[command(flatten)]
    pub node_args: ZoneNodeArgs,

    /// L1 WebSocket RPC URL for subscribing to deposit events.
    #[arg(long = "l1.rpc-url", env = "L1_RPC_URL")]
    pub l1_rpc_url: Option<String>,
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

            let node = ZoneNode::new(&args.node_args);

            let NodeHandle {
                node_exit_future,
                node,
            } = builder.node(node).launch().await?;

            info!(target: "reth::cli", "Tempo Zone node started");

            // Spawn L1 subscriber if L1 RPC URL is provided
            if let Some(l1_rpc_url) = args.l1_rpc_url {
                let config = L1SubscriberConfig {
                    l1_rpc_url,
                    ..Default::default()
                };

                let _deposit_rx = spawn_l1_subscriber(config, node.task_executor.clone());

                info!(target: "reth::cli", "L1 deposit subscriber started");

                // TODO: Pass deposit_rx to the block builder when ready
                // For now, deposits will be logged but not processed
            }

            node_exit_future.await?;
            Ok(())
        })
    {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
