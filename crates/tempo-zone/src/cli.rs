//! Tempo Zone CLI.

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use clap::{Args, Parser};
use reth_consensus::noop::NoopConsensus;
use reth_ethereum::cli::Cli;
use reth_tracing::tracing::info;
use tempo_chainspec::spec::{TempoChainSpec, TempoChainSpecParser};

use crate::{
    BatchAnchorConfig, ZoneNode, ZonePrivateRpcConfig, ZoneSequencerAddOnsConfig,
    evm::ZoneEvmConfig, rpc::auth::DEFAULT_MAX_AUTH_TOKEN_VALIDITY_SECS,
};

const MAX_LOGS_PER_RESPONSE: u64 = 1_000_000;
const MAX_BLOCKS_PER_FILTER: u64 = 1_000_000;

const ZONE_LOG_FILTER_DIRECTIVES: &str = concat!(
    "tungstenite=warn,",
    "alloy_pubsub=warn,",
    "alloy_transport_ws=warn,",
    "rustls::client=warn"
);

/// Tempo Zone CLI entry point.
pub struct ZoneCli(Cli<TempoChainSpecParser, ZoneArgs>);

impl ZoneCli {
    /// Parse CLI arguments from the environment.
    pub fn parse() -> Self {
        Self(Cli::parse())
    }

    /// Run the Tempo Zone node.
    ///
    /// Configures the node builder, launches the zone node with all sequencer
    /// background tasks, and blocks until exit.
    pub fn run(self) -> eyre::Result<()> {
        let mut cli = self.0;

        prepend_log_filter(&mut cli.logs.log_stdout_filter, ZONE_LOG_FILTER_DIRECTIVES);
        prepend_log_filter(&mut cli.logs.log_file_filter, ZONE_LOG_FILTER_DIRECTIVES);

        let components = |spec: Arc<TempoChainSpec>| {
            (
                ZoneEvmConfig::new_without_l1(spec),
                NoopConsensus::default(),
            )
        };

        cli.run_with_components::<ZoneNode>(components, async move |mut builder, args| {
            info!(target: "reth::cli", "Launching Tempo Zone node");

            builder.config_mut().network.discovery.disable_discovery = true;
            builder.config_mut().rpc.disable_auth_server = true;
            builder.config_mut().rpc.rpc_max_logs_per_response = MAX_LOGS_PER_RESPONSE.into();
            builder.config_mut().rpc.rpc_max_blocks_per_filter = MAX_BLOCKS_PER_FILTER.into();

            let mut node = ZoneNode::new(
                args.l1_rpc_url,
                args.portal_address,
                args.l1_genesis_block_number,
                args.l1_fetch_concurrency,
                Duration::from_millis(args.l1_retry_connection_interval_ms),
            )
            .with_private_rpc(ZonePrivateRpcConfig {
                private_rpc_port: args.private_rpc_port,
                zone_id: args.zone_id,
                max_auth_token_validity: Duration::from_secs(
                    args.private_rpc_max_auth_token_validity_secs,
                ),
            });

            if args.enable_sequencer {
                let sequencer_signer: PrivateKeySigner = args
                    .sequencer_key
                    .parse()
                    .expect("invalid sequencer private key");
                node = node.with_sequencer(ZoneSequencerAddOnsConfig {
                    sequencer_signer,
                    zone_id: args.zone_id,
                    zone_poll_interval: Duration::from_secs(args.zone_poll_interval_secs),
                    batch_interval: Duration::from_secs(args.zone_batch_interval_secs),
                    batch_anchor_config: BatchAnchorConfig::default(),
                    withdrawal_poll_interval: Duration::from_secs(
                        args.withdrawal_poll_interval_secs,
                    ),
                });
            }

            let handle = builder.node(node).launch_with_debug_capabilities().await?;
            handle.wait_for_node_exit().await
        })
    }
}

/// Tempo Zone CLI arguments.
#[derive(Debug, Clone, Args)]
pub struct ZoneArgs {
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
    #[arg(long = "sequencer-key", env = "SEQUENCER_KEY")]
    pub sequencer_key: String,

    /// How often (in seconds) the zone monitor polls for new L2 blocks.
    #[arg(
        long = "zone.poll-interval-secs",
        env = "ZONE_POLL_INTERVAL_SECS",
        default_value_t = 1
    )]
    pub zone_poll_interval_secs: u64,

    /// Maximum time (in seconds) to accumulate zone blocks before submitting a batch to L1.
    #[arg(
        long = "zone.batch-interval-secs",
        env = "ZONE_BATCH_INTERVAL_SECS",
        default_value_t = 60
    )]
    pub zone_batch_interval_secs: u64,

    /// How often (in seconds) the withdrawal processor polls the L1 queue.
    #[arg(
        long = "withdrawal-poll-interval-secs",
        env = "WITHDRAWAL_POLL_INTERVAL_SECS",
        default_value_t = 5
    )]
    pub withdrawal_poll_interval_secs: u64,

    /// Genesis Tempo L1 block number override.
    #[arg(long = "l1.genesis-block-number", env = "L1_GENESIS_BLOCK_NUMBER")]
    pub l1_genesis_block_number: Option<u64>,

    /// Maximum number of concurrent L1 receipt fetches.
    #[arg(
        long = "l1.fetch-concurrency",
        env = "L1_FETCH_CONCURRENCY",
        default_value_t = 4
    )]
    pub l1_fetch_concurrency: usize,

    /// Interval in milliseconds between WebSocket reconnection attempts to L1.
    #[arg(
        long = "l1.retry-connection-interval",
        env = "L1_RETRY_CONNECTION_INTERVAL_MS",
        default_value_t = 100
    )]
    pub l1_retry_connection_interval_ms: u64,

    /// Zone ID for the private RPC auth token validation.
    #[arg(long = "zone.id", env = "ZONE_ID", default_value_t = 0)]
    pub zone_id: u32,

    /// Port for the private zone RPC server (0 for OS-assigned).
    #[arg(
        long = "private-rpc.port",
        env = "PRIVATE_RPC_PORT",
        default_value_t = 8544
    )]
    pub private_rpc_port: u16,

    /// Maximum auth token validity window the private RPC accepts, in seconds.
    #[arg(
        long = "private-rpc.max-auth-token-validity-secs",
        env = "PRIVATE_RPC_MAX_AUTH_TOKEN_VALIDITY_SECS",
        default_value_t = DEFAULT_MAX_AUTH_TOKEN_VALIDITY_SECS
    )]
    pub private_rpc_max_auth_token_validity_secs: u64,

    /// Enable the Zone node in sequencer mode. This advances block production and submits
    /// withdrawal batches.
    #[arg(long = "sequencer", env = "SEQUENCER")]
    pub enable_sequencer: bool,
}

fn prepend_log_filter(filter: &mut String, directives: &str) {
    if filter.is_empty() {
        *filter = directives.to_owned();
    } else {
        *filter = format!("{directives},{filter}");
    }
}
