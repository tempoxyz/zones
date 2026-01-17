//! Tempo Zone L2 Node.
//!
//! This binary runs a lightweight L2 node using the reth node builder infrastructure.
//! It subscribes to L1 chain events to extract deposit transactions.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

use alloy_primitives::{Address, B256};
use clap::Parser;
use reth_consensus::noop::NoopConsensus;
use reth_ethereum::cli::Cli;
use reth_node_builder::NodeHandle;
use reth_tracing::tracing::info;
use std::sync::Arc;
use tempo_chainspec::spec::{TempoChainSpec, TempoChainSpecParser};
use tempo_evm::{TempoEvmConfig, TempoEvmFactory};
use tempo_zone::{L1SubscriberConfig, ZoneNode, spawn_l1_subscriber};
use tempo_zone_exex::{L1Deposit, install_exex, SubmitterConfig, ZoneProverConfig};
use tokio::sync::mpsc;

/// Tempo Zone CLI arguments.
#[derive(Debug, Clone, clap::Args)]
struct ZoneArgs {
    /// L1 WebSocket RPC URL for subscribing to deposit events.
    ///
    /// Required for L1 deposit tracking. The node will subscribe to deposit
    /// events from the ZonePortal contract on L1.
    ///
    /// Example: wss://tempo-mainnet.example.com/ws
    #[arg(long = "l1.rpc-url", env = "L1_RPC_URL")]
    pub l1_rpc_url: Option<String>,

    /// ZonePortal contract address on L1.
    ///
    /// Required when --zone.prover is enabled. This is the contract that
    /// receives batch submissions with proofs.
    #[arg(long = "zone.portal-address", env = "ZONE_PORTAL_ADDRESS")]
    pub portal_address: Option<Address>,

    /// Sequencer private key for signing L1 transactions (32 bytes hex).
    ///
    /// Required when --zone.prover is enabled. This key is used to sign
    /// submitBatch transactions to the ZonePortal contract. The associated
    /// address must have ETH for gas fees.
    ///
    /// WARNING: Keep this key secure. Use environment variables or a secrets manager.
    #[arg(long = "zone.sequencer-key", env = "ZONE_SEQUENCER_KEY")]
    pub sequencer_key: Option<B256>,

    /// Enable zone prover ExEx for generating and submitting SP1 proofs.
    ///
    /// When enabled, the node will:
    /// - Batch blocks (250ms interval or 100 blocks max)
    /// - Generate proofs (mock or SP1 depending on --zone.mock-prover)
    /// - Submit proof bundles to the ZonePortal contract on L1
    ///
    /// Requires: --l1.rpc-url, --zone.portal-address, --zone.sequencer-key
    #[arg(long = "zone.prover", env = "ZONE_PROVER_ENABLED", default_value = "false")]
    pub prover_enabled: bool,

    /// Use mock prover instead of SP1 for development and testing.
    ///
    /// Mock proofs are dummy 32-byte values that will only be accepted by
    /// a mock verifier. Set to false for production with real SP1 proofs.
    #[arg(long = "zone.mock-prover", env = "ZONE_MOCK_PROVER", default_value = "true")]
    pub mock_prover: bool,
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

            let node = ZoneNode::default();

            // Build the node with optional ExEx
            let mut node_builder = builder.node(node);

            // Create channel for forwarding L1 deposits to the exex
            let (exex_deposit_tx, exex_deposit_rx) = mpsc::unbounded_channel::<L1Deposit>();

            // Install zone prover ExEx if enabled
            if args.prover_enabled {
                let exex_config = ZoneProverConfig {
                    use_mock_prover: args.mock_prover,
                    submitter_config: SubmitterConfig {
                        portal_address: args.portal_address.unwrap_or(Address::ZERO),
                        sequencer_key: args.sequencer_key.unwrap_or(B256::ZERO),
                        l1_rpc_url: args.l1_rpc_url.clone().unwrap_or_default(),
                        ..Default::default()
                    },
                    ..Default::default()
                };

                node_builder = node_builder.install_exex("zone-prover", async move |ctx| {
                    Ok(install_exex(ctx, exex_config, Some(exex_deposit_rx)))
                });

                info!(target: "reth::cli", mock = args.mock_prover, "Zone prover ExEx installed");
            }

            let NodeHandle {
                node_exit_future,
                node,
            } = node_builder.launch().await?;

            info!(target: "reth::cli", "Tempo Zone node started");

            // Spawn L1 subscriber if L1 RPC URL is provided
            if let Some(l1_rpc_url) = args.l1_rpc_url {
                let config = L1SubscriberConfig {
                    l1_rpc_url,
                    ..Default::default()
                };

                let mut deposit_rx = spawn_l1_subscriber(config, node.task_executor.clone());

                info!(target: "reth::cli", "L1 deposit subscriber started");

                // Spawn bridge task to forward L1 deposits to the exex
                let deposit_tx = exex_deposit_tx.clone();
                node.task_executor.spawn_critical(
                    "l1-deposit-bridge",
                    Box::pin(async move {
                        while let Some(deposit) = deposit_rx.recv().await {
                            let l1_deposit = L1Deposit {
                                l1_block_number: deposit.l1_block_number,
                                sender: deposit.sender,
                                to: deposit.to,
                                amount: deposit.amount,
                                data: deposit.data,
                            };
                            if deposit_tx.send(l1_deposit).is_err() {
                                break;
                            }
                        }
                    }),
                );

                info!(target: "reth::cli", "L1 deposit bridge started");
            }

            node_exit_future.await?;
            Ok(())
        })
    {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
