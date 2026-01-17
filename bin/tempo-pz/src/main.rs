//! Tempo Privacy Zone L2 Node.
//!
//! This binary runs an Ethereum node with the Privacy Zone ExEx installed.
//! The ExEx listens to L1 chain notifications, extracts deposits, and processes L2 blocks.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

use eyre::Context;
use pz_exex::PzNodeBuilder;
use reth_chainspec::MAINNET;
use reth_node_builder::NodeHandle;
use reth_node_ethereum::EthereumNode;
use tracing::info;

fn main() -> eyre::Result<()> {
    reth_cli_util::sigsegv_handler::install();


    // TODO: you are not starting the tempo node, alos start the tempo node and install the exex


    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    reth::cli::Cli::parse_args().run(|builder, _| async move {
        // Get data directory from builder config
        let data_dir = builder
            .config()
            .datadir
            .clone()
            .resolve_datadir(builder.config().chain.chain())
            .data_dir()
            .to_path_buf();

        let l2_data_dir = data_dir.join("pz-l2");

        info!(?l2_data_dir, "Starting Privacy Zone L2 ExEx");

        let NodeHandle {
            node: _,
            node_exit_future,
        } = builder
            .node(EthereumNode::default())
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
            .await?;

        info!("Privacy Zone L2 node started");

        node_exit_future.await
    })
}
