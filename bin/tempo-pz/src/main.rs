//! Tempo Privacy Zone L2 Node.
//!
//! This binary runs a Tempo node with the Privacy Zone ExEx installed.
//! The ExEx listens to L1 chain notifications, extracts deposits, and processes L2 blocks.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

use clap::Parser;
use eyre::Context;
use pz_exex::PzNodeBuilder;
use reth_chainspec::{EthChainSpec as _, MAINNET};
use reth_ethereum::cli::Cli;
use reth_node_builder::NodeHandle;
use std::sync::Arc;
use tempo_chainspec::spec::{TempoChainSpec, TempoChainSpecParser};
use tempo_consensus::TempoConsensus;
use tempo_evm::{TempoEvmConfig, TempoEvmFactory};
use tempo_node::{TempoNodeArgs, node::TempoNode};
use tracing::info;


// TODO:  setup with remote exex

/// Privacy Zone specific arguments.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
struct PzArgs {
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

    let cli = Cli::<TempoChainSpecParser, PzArgs>::parse();

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

        let l2_data_dir = data_dir.join("pz-l2");

        info!(?l2_data_dir, "Starting Privacy Zone L2 ExEx on Tempo node");

        let NodeHandle {
            node: _,
            node_exit_future,
        } = builder
            .node(TempoNode::new(&args.node_args, None))
            .install_exex("PrivacyZone", move |ctx| async move {
                // Build the PZ node with the ExEx context
                let pz_node = PzNodeBuilder::new()
                    .with_ctx(ctx)
                    .with_chain_spec(MAINNET.clone())
                    .with_data_dir(l2_data_dir)
                    .build()
                    .wrap_err("failed to build PZ node")?;

                info!("Privacy Zone L2 ExEx initialized");

                // Return the node's start future
                Ok(pz_node.start())
            })
            .launch_with_debug_capabilities()
            .await
            .wrap_err("failed launching Tempo node with PZ ExEx")?;

        info!("Tempo Privacy Zone node started");

        node_exit_future.await
    })
    .wrap_err("Tempo PZ node failed")?;

    Ok(())
}
