//! Tempo Zone L2 Node.
//!
//! This binary runs a Tempo node with the Tempo Zone ExEx installed.
//! The ExEx listens to L1 chain notifications, extracts deposits, and processes L2 blocks.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

use clap::Parser;
use eyre::Context;
use tempo_zone_exex::ZoneNodeBuilder;
use reth_chainspec::{EthChainSpec as _, MAINNET};
use reth_ethereum::cli::Cli;
use reth_node_builder::NodeHandle;
use std::sync::Arc;
use tempo_chainspec::spec::{TempoChainSpec, TempoChainSpecParser};
use tempo_consensus::TempoConsensus;
use tempo_evm::{TempoEvmConfig, TempoEvmFactory};
use tempo_node::{TempoNodeArgs, node::TempoNode};
use tracing::info;


// TODO:  setup with remote exex eventually
//
//
// TODO: setup with subscribe chain notifications and listen to testnet


/// Tempo Zone specific arguments.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
struct ZoneArgs {
    #[command(flatten)]
    pub node_args: TempoNodeArgs,
}

fn main() -> eyre::Result<()> {
    reth_cli_util::sigsegv_handler::install();

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    tempo_node::init_version_metadata();

    let cli = Cli::<TempoChainSpecParser, ZoneArgs>::parse();

    let components = |spec: Arc<TempoChainSpec>| {
        (
            TempoEvmConfig::new(spec.clone(), TempoEvmFactory::default()),
            TempoConsensus::new(spec),
        )
    };

    cli.run_with_components::<TempoNode>(components, async move |builder, args| {
        // Get data directory from builder config
        let data_dir = builder
            .config()
            .datadir
            .clone()
            .resolve_datadir(builder.config().chain.chain())
            .data_dir()
            .to_path_buf();

        let l2_data_dir = data_dir.join("zone-l2");

        info!(?l2_data_dir, "Starting Tempo Zone L2 ExEx on Tempo node");

        let NodeHandle {
            node: _,
            node_exit_future,
        } = builder
            .node(TempoNode::new(&args.node_args, None))
            .install_exex("TempoZone", move |ctx| async move {
                // Build the Zone node with the ExEx context
                let zone_node = ZoneNodeBuilder::new()
                    .with_ctx(ctx)
                    // TODO: update this to take in flags
                    .with_chain_spec(MAINNET.clone())
                    .with_data_dir(l2_data_dir)
                    .build()
                    .wrap_err("failed to build Zone node")?;

                info!("Tempo Zone L2 ExEx initialized");

                // Return the node's start future
                Ok(zone_node.start())
            })
            .launch_with_debug_capabilities()
            .await
            .wrap_err("failed launching Tempo node with Zone ExEx")?;

        info!("Tempo Zone node started");

        node_exit_future.await
    })
    .wrap_err("Tempo Zone node failed")?;

    Ok(())
}
