//! Tempo Zone L2 Node.
//!
//! This binary runs a lightweight L2 node using the reth node builder infrastructure.
//! It subscribes to L1 chain events to extract deposit transactions and optionally runs
//! sequencer background tasks (batch submission, withdrawal processing).

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use std::{collections::HashSet, sync::Arc, time::Duration};

use alloy_primitives::Address;
use clap::Parser;
use reth_consensus::noop::NoopConsensus;
use reth_ethereum::cli::Cli;

use reth_tracing::tracing::info;
use tempo_chainspec::spec::{TempoChainSpec, TempoChainSpecParser};
use zone::{
    DepositQueue, L1SubscriberConfig, ZoneNode,
    evm::ZoneEvmConfig,
    l1_state::{L1StateListenerConfig, L1StateProviderConfig, SharedL1StateCache},
};

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

/// Tempo Zone CLI arguments.
#[derive(Debug, Clone, clap::Args)]
struct ZoneArgs {
    /// L1 WebSocket RPC URL for subscribing to deposit events and chain notifications.
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

    // ---------------------------------------------------------------
    //  Sequencer-mode arguments (optional — enable with --sequencer.key)
    // ---------------------------------------------------------------
    /// Sequencer private key (hex). When set, enables sequencer background tasks
    /// (batch submission, withdrawal processing, zone monitoring).
    #[arg(long = "sequencer.key", env = "SEQUENCER_KEY")]
    pub sequencer_key: Option<String>,

    /// Zone L2 HTTP RPC URL for the zone monitor to poll.
    /// Only used when sequencer mode is enabled.
    #[arg(
        long = "zone.rpc-url",
        env = "ZONE_RPC_URL",
        default_value = "http://localhost:8546"
    )]
    pub zone_rpc_url: String,

    /// How often (in seconds) the zone monitor polls for new L2 blocks.
    #[arg(
        long = "zone.poll-interval-secs",
        env = "ZONE_POLL_INTERVAL_SECS",
        default_value = "1"
    )]
    pub zone_poll_interval_secs: u64,

    /// How often (in seconds) the withdrawal processor polls the L1 queue.
    #[arg(
        long = "withdrawal.poll-interval-secs",
        env = "WITHDRAWAL_POLL_INTERVAL_SECS",
        default_value = "5"
    )]
    pub poll_interval_secs: u64,

    /// Genesis Tempo L1 block number override. Only needed for portals where
    /// `genesisTempoBlockNumber` is 0 (not created via ZoneFactory).
    #[arg(long = "l1.genesis-block-number", env = "L1_GENESIS_BLOCK_NUMBER")]
    pub l1_genesis_block_number: Option<u64>,
}

fn main() {
    reth_cli_util::sigsegv_handler::install();

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    let components = |spec: Arc<TempoChainSpec>| {
        (
            ZoneEvmConfig::new_without_l1(spec),
            NoopConsensus::default(),
        )
    };

    if let Err(err) = Cli::<TempoChainSpecParser, ZoneArgs>::parse()
        .run_with_components::<ZoneNode>(components, async move |builder, args| {
            info!(target: "reth::cli", "Launching Tempo Zone node");

            // Parse the sequencer key early so we can derive the address for block building.
            // The signer is kept for later use when spawning sequencer background tasks.
            let sequencer_signer: Option<alloy_signer_local::PrivateKeySigner> =
                args.sequencer_key.as_ref().map(|key_hex| {
                    key_hex.parse().expect("invalid sequencer private key")
                });
            let sequencer_addr = sequencer_signer.as_ref().map(|s| s.address());

            let deposits = DepositQueue::default();
            let l1_config = L1SubscriberConfig {
                l1_rpc_url: args.l1_rpc_url.clone(),
                portal_address: args.portal_address,
                genesis_tempo_block_number: args.l1_genesis_block_number,
            };
            let l1_state_provider_config = L1StateProviderConfig {
                l1_rpc_url: args.l1_rpc_url.clone(),
                ..Default::default()
            };
            let l1_state_listener_config = L1StateListenerConfig {
                l1_ws_url: args.l1_rpc_url.clone(),
                ..Default::default()
            };
            let l1_state_cache = SharedL1StateCache::new(HashSet::from([args.portal_address]));
            let node = ZoneNode::new(
                deposits,
                l1_config,
                l1_state_provider_config,
                l1_state_listener_config,
                l1_state_cache,
                sequencer_addr,
            );

            // NOTE: `--dev` is no longer needed for block production — the ZoneEngine
            // (spawned from ZoneAddOns::launch_add_ons) handles L1-driven block building.
            // We keep `launch_with_debug_capabilities()` for its debug RPC features.
            let handle = builder.node(node).launch_with_debug_capabilities().await?;

            info!(target: "reth::cli", "Tempo Zone node started");

            // Spawn sequencer background tasks if a sequencer key is provided.
            if let Some(signer) = sequencer_signer {
                let sequencer_addr = signer.address();

                info!(
                    target: "reth::cli",
                    %sequencer_addr,
                    "Starting sequencer background tasks"
                );

                let sequencer_config = zone::ZoneSequencerConfig {
                    portal_address: args.portal_address,
                    l1_rpc_url: args.l1_rpc_url,
                    withdrawal_poll_interval: Duration::from_secs(
                        args.poll_interval_secs,
                    ),
                    outbox_address: zone::abi::ZONE_OUTBOX_ADDRESS,
                    inbox_address: zone::abi::ZONE_INBOX_ADDRESS,
                    tempo_state_address: zone::abi::TEMPO_STATE_ADDRESS,
                    zone_rpc_url: args.zone_rpc_url,
                    zone_poll_interval: Duration::from_secs(args.zone_poll_interval_secs),
                };

                let seq_handle = zone::spawn_zone_sequencer(sequencer_config, signer).await;

                info!(
                    target: "reth::cli",
                    "Sequencer tasks spawned: zone monitor (with batch submission), withdrawal processor"
                );

                // If any sequencer task exits, log it.
                tokio::spawn(async move {
                    tokio::select! {
                        res = seq_handle.withdrawal_handle => {
                            tracing::error!(target: "reth::cli", ?res, "Withdrawal processor task exited");
                        }
                        res = seq_handle.monitor_handle => {
                            tracing::error!(target: "reth::cli", ?res, "Zone monitor task exited");
                        }
                    }
                });
            }

            handle.node_exit_future.await?;
            Ok(())
        })
    {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
