//! Tempo Zone L2 Node.
//!
//! This binary runs a lightweight L2 node using the reth node builder infrastructure.
//! It subscribes to L1 chain events to extract deposit transactions and optionally runs
//! sequencer background tasks (batch submission, withdrawal processing).

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use clap::Parser;
use reth_consensus::noop::NoopConsensus;
use reth_ethereum::cli::Cli;

use reth_ethereum::chainspec::EthChainSpec;
use reth_tracing::tracing::info;
use tempo_chainspec::spec::{TempoChainSpec, TempoChainSpecParser};
use zone::{ZoneNode, evm::ZoneEvmConfig};
use zone_primitives::constants::zone_chain_id;

type ZoneCli = Cli<TempoChainSpecParser, ZoneArgs>;

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

const ZONE_LOG_FILTER_DIRECTIVES: &str = concat!(
    "tungstenite=warn,",
    "alloy_pubsub=warn,",
    "alloy_transport_ws=warn,",
    "rustls::client=warn"
);

/// Tempo Zone CLI arguments.
#[derive(Debug, Clone, clap::Args)]
struct ZoneArgs {
    /// L1 WebSocket RPC URL for subscribing to deposit events and chain notifications.
    #[arg(long = "l1.rpc-url", env = "L1_RPC_URL")]
    pub l1_rpc_url: String,

    /// ZonePortal contract address on L1.
    #[arg(long = "l1.portal-address", env = "L1_PORTAL_ADDRESS")]
    pub portal_address: Address,

    /// Block building interval in milliseconds.
    #[arg(
        long = "block.interval-ms",
        env = "BLOCK_INTERVAL_MS",
        default_value_t = 250
    )]
    pub block_interval_ms: u64,

    /// Sequencer private key (hex, with or without 0x prefix).
    /// Used for block building and ECIES decryption of encrypted deposits.
    #[arg(long = "sequencer-key", env = "SEQUENCER_KEY")]
    pub sequencer_key: String,

    /// How often (in seconds) the zone monitor polls for new L2 blocks.
    #[arg(
        long = "zone.poll-interval-secs",
        env = "ZONE_POLL_INTERVAL_SECS",
        default_value_t = 1
    )]
    pub zone_poll_interval_secs: u64,

    /// Maximum time (in seconds) to accumulate zone blocks before submitting a
    /// batch to L1. Batches are flushed early when withdrawals are pending.
    #[arg(
        long = "zone.batch-interval-secs",
        env = "ZONE_BATCH_INTERVAL_SECS",
        default_value_t = 60
    )]
    pub zone_batch_interval_secs: u64,

    #[arg(
        long = "withdrawal-poll-interval-secs",
        env = "WITHDRAWAL_POLL_INTERVAL_SECS",
        default_value_t = 5
    )]
    pub poll_interval_secs: u64,

    /// Genesis Tempo L1 block number override. Only needed for portals where
    /// `genesisTempoBlockNumber` is 0 (not created via ZoneFactory).
    #[arg(long = "l1.genesis-block-number", env = "L1_GENESIS_BLOCK_NUMBER")]
    pub l1_genesis_block_number: Option<u64>,

    /// Maximum number of concurrent L1 receipt fetches. Used directly for
    /// the live stream; halved for backfill (which sends 2 requests per block).
    #[arg(
        long = "l1.fetch-concurrency",
        env = "L1_FETCH_CONCURRENCY",
        default_value_t = 4
    )]
    pub l1_fetch_concurrency: usize,

    #[arg(
        long = "l1.retry-connection-interval",
        env = "L1_RETRY_CONNECTION_INTERVAL_MS",
        default_value_t = 100
    )]
    pub l1_retry_connection_interval_ms: u64,

    #[arg(long = "zone.id", env = "ZONE_ID", default_value_t = 0)]
    pub zone_id: u32,

    /// Port for the private zone RPC server (0 for OS-assigned).
    #[arg(
        long = "private-rpc.port",
        env = "PRIVATE_RPC_PORT",
        default_value_t = 8544
    )]
    pub private_rpc_port: u16,
}

fn prepend_log_filter(filter: &mut String, directives: &str) {
    if filter.is_empty() {
        *filter = directives.to_owned();
    } else {
        *filter = format!("{directives},{filter}");
    }
}

fn apply_zone_log_filters(cli: &mut ZoneCli) {
    prepend_log_filter(&mut cli.logs.log_stdout_filter, ZONE_LOG_FILTER_DIRECTIVES);
    prepend_log_filter(&mut cli.logs.log_file_filter, ZONE_LOG_FILTER_DIRECTIVES);
}

fn main() {
    reth_cli_util::sigsegv_handler::install();

    // Install the default rustls CryptoProvider for WSS connections to L1.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install rustls CryptoProvider");

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

    let mut cli = ZoneCli::parse();
    apply_zone_log_filters(&mut cli);

    let run_result = cli.run_with_components::<ZoneNode>(components, async move |mut builder, args| {
            info!(target: "reth::cli", "Launching Tempo Zone node");


            builder.config_mut().network.discovery.disable_discovery = true;
            // Disable the auth (Engine API) server — the zone node derives blocks
            // from L1, so no external consensus client or Engine API is needed.
            builder.config_mut().rpc.disable_auth_server = true;
            builder.config_mut().rpc.rpc_max_logs_per_response = 1_000_000u64.into();
            builder.config_mut().rpc.rpc_max_blocks_per_filter = 1_000_000u64.into();

            // Parse the sequencer key to derive the address for block building
            // and the k256 secret key for ECIES decryption of encrypted deposits.
            let sequencer_signer: alloy_signer_local::PrivateKeySigner =
                args.sequencer_key.parse().expect("invalid sequencer private key");
            let sequencer_addr = sequencer_signer.address();

            let key_hex = &args.sequencer_key;
            let sequencer_secret_key: k256::SecretKey = {
                let bytes = const_hex::decode(key_hex.strip_prefix("0x").unwrap_or(key_hex))
                    .expect("invalid sequencer key hex");
                k256::SecretKey::from_slice(&bytes).expect("invalid secp256k1 secret key")
            };

            let node = ZoneNode::new(
                args.l1_rpc_url.clone(),
                args.portal_address,
                args.l1_genesis_block_number,
                sequencer_addr,
                sequencer_secret_key,
                args.l1_fetch_concurrency,
                Duration::from_millis(args.l1_retry_connection_interval_ms),
            );

            let handle = builder.node(node).launch_with_debug_capabilities().await?;
            info!(target: "reth::cli", "Tempo Zone node started");

            // Verify the chain ID matches the deterministic derivation from the zone ID.
            // Skip when zone_id is 0 (local testing default).
            if args.zone_id != 0 {
                let expected_chain_id = zone_chain_id(args.zone_id);
                let actual_chain_id = handle.node.chain_spec().chain().id();
                if actual_chain_id != expected_chain_id {
                    eyre::bail!(
                        "chain ID mismatch: zone.id={} requires chain_id={}, but genesis has {}",
                        args.zone_id,
                        expected_chain_id,
                        actual_chain_id,
                    );
                }
            }

            // Launch the private zone RPC server.
            let eth_handlers = handle.node.eth_handlers().clone();
            let zone_rpc_url = handle
                .node
                .rpc_server_handle()
                .http_url()
                .expect("HTTP RPC server must be enabled for sequencer mode");
            let private_rpc_config = zone::rpc::PrivateRpcConfig {
                listen_addr: ([0, 0, 0, 0], args.private_rpc_port).into(),
                l1_rpc_url: args.l1_rpc_url.clone(),
                zone_rpc_url: zone_rpc_url.clone(),
                retry_connection_interval: Duration::from_millis(
                    args.l1_retry_connection_interval_ms,
                ),
                zone_id: args.zone_id,
                chain_id: handle.node.chain_spec().chain().id(),
                zone_portal: args.portal_address,
                sequencer: sequencer_addr,
            };
            let api: Arc<dyn zone::rpc::ZoneRpcApi> = Arc::new(zone::rpc::TempoZoneRpc::new(
                eth_handlers,
                private_rpc_config.clone(),
            )
            .await?);
            let local_addr = zone::rpc::start_private_rpc(private_rpc_config, api).await?;
            info!(target: "reth::cli", %local_addr, "Private zone RPC server started");

            // Spawn sequencer background tasks.
            info!(
                target: "reth::cli",
                %sequencer_addr,
                "Starting sequencer background tasks"
            );

            let sequencer_config = zone::ZoneSequencerConfig {
                portal_address: args.portal_address,
                l1_rpc_url: args.l1_rpc_url,
                retry_connection_interval: Duration::from_millis(
                    args.l1_retry_connection_interval_ms,
                ),
                withdrawal_poll_interval: Duration::from_secs(
                    args.poll_interval_secs,
                ),
                outbox_address: zone::abi::ZONE_OUTBOX_ADDRESS,
                inbox_address: zone::abi::ZONE_INBOX_ADDRESS,
                tempo_state_address: zone::abi::TEMPO_STATE_ADDRESS,
                zone_rpc_url,
                zone_poll_interval: Duration::from_secs(args.zone_poll_interval_secs),
                batch_interval: Duration::from_secs(args.zone_batch_interval_secs),
            };


            // NOTE: is this batcher, or what exactly is this doing
            let seq_handle = zone::spawn_zone_sequencer(sequencer_config, sequencer_signer).await;

            info!(
                target: "reth::cli",
                "Sequencer tasks spawned: zone monitor (with batch submission), withdrawal processor"
            );

            // Spawn as critical tasks — node shuts down if either exits.
            handle.node.task_executor.spawn_critical_task("zone-monitor", async move {
                tokio::select! {
                    res = seq_handle.withdrawal_handle => {
                        tracing::error!(target: "reth::cli", ?res, "Withdrawal processor task exited");
                    }
                    res = seq_handle.monitor_handle => {
                        tracing::error!(target: "reth::cli", ?res, "Zone monitor task exited");
                    }
                }
            });

            // Ensure all unpersisted blocks are flushed when the node exits.
            let engine_shutdown = handle.node.engine_shutdown.clone();
            handle.node.task_executor.spawn_critical_with_graceful_shutdown_signal(
                "zone-engine-shutdown",
                |shutdown| async move {
                    let _guard = shutdown.await;
                    info!(target: "reth::cli", "Shutdown signal received — flushing engine state");
                    if let Some(done) = engine_shutdown.shutdown() {
                        let _ = done.await;
                    }
                },
            );

            handle.node_exit_future.await?;
            Ok(())
        });

    if let Err(err) = run_result {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
