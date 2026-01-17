//! Tempo Zone L2 Node.
//!
//! This binary runs a lightweight L2 node using the reth node builder infrastructure.

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
use tempo_zone::{ZoneNode, ZoneNodeArgs};

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

    let cli = Cli::<TempoChainSpecParser, ZoneNodeArgs>::parse();

    if let Err(err) = cli.run_with_components::<ZoneNode>(components, async move |builder, args| {
        info!(target: "reth::cli", "Launching Tempo Zone node");

        let node = ZoneNode::new(&args);

        let NodeHandle {
            node_exit_future,
            node: _node,
        } = builder.node(node).launch().await?;

        info!(target: "reth::cli", "Tempo Zone node started");

        node_exit_future.await?;
        Ok(())
    }) {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
