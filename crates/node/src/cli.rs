//! Tempo Zone CLI.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use clap::{Args, CommandFactory, FromArgMatches};
use reth_consensus::noop::NoopConsensus;
use reth_ethereum::cli::Cli;
use reth_tracing::tracing::info;
use zone_chainspec::{ZoneChainSpec, ZoneChainSpecParser};
use zone_evm::ZoneEvmConfig;
use zone_p2p::{P2pConfig, Role};
use zone_payload::DEFAULT_WITHDRAWAL_BATCH_INTERVAL_BLOCKS;

use crate::{
    ZoneNode, ZonePrivateRpcConfig, ZoneSequencerAddOnsConfig, dev::DevCommand,
    rpc::auth::DEFAULT_MAX_AUTH_TOKEN_VALIDITY_SECS,
};
use zone_sequencer::BatchAnchorConfig;

const MAX_LOGS_PER_RESPONSE: u64 = 1_000_000;
const MAX_BLOCKS_PER_FILTER: u64 = 1_000_000;

const ZONE_LOG_FILTER_DIRECTIVES: &str = concat!(
    "tungstenite=warn,",
    "alloy_pubsub=warn,",
    "alloy_transport_ws=warn,",
    "rustls::client=warn"
);

/// Tempo Zone CLI entry point.
pub enum ZoneCli {
    Node(Box<Cli<ZoneChainSpecParser, ZoneArgs>>),
    Dev(Box<DevCommand>),
}

impl ZoneCli {
    fn command() -> clap::Command {
        Cli::<ZoneChainSpecParser, ZoneArgs>::command()
            .about("Tempo Zone")
            .subcommand(DevCommand::command())
    }

    /// Parse CLI arguments from the environment.
    pub fn parse() -> Self {
        Self::parse_from(std::env::args_os())
    }

    /// Parse CLI arguments from an iterator. The first item is the binary name.
    pub fn parse_from<I, T>(args: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        Self::try_parse_from(args).unwrap_or_else(|err| err.exit())
    }

    /// Try to parse CLI arguments from an iterator.
    pub fn try_parse_from<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let matches = Self::command().try_get_matches_from(args)?;
        if let Some(("dev", dev_matches)) = matches.subcommand() {
            return DevCommand::from_arg_matches(dev_matches)
                .map(Box::new)
                .map(Self::Dev);
        }
        Cli::from_arg_matches(&matches)
            .map(Box::new)
            .map(Self::Node)
    }

    /// Run the Tempo Zone node.
    ///
    /// Configures the node builder, launches the zone node with all sequencer
    /// background tasks, and blocks until exit.
    pub fn run(self) -> eyre::Result<()> {
        match self {
            Self::Node(cli) => run_node(*cli),
            Self::Dev(command) => (*command).run(),
        }
    }
}

/// Main entry point for the `node` command.
fn run_node(mut cli: Cli<ZoneChainSpecParser, ZoneArgs>) -> eyre::Result<()> {
    prepend_log_filter(&mut cli.logs.log_stdout_filter, ZONE_LOG_FILTER_DIRECTIVES);
    prepend_log_filter(&mut cli.logs.log_file_filter, ZONE_LOG_FILTER_DIRECTIVES);

    let components = |spec: Arc<ZoneChainSpec>| {
        (
            ZoneEvmConfig::new_without_l1(spec),
            NoopConsensus::default(),
        )
    };

    cli.run_with_components::<ZoneNode>(components, async move |mut builder, args| {
        info!(target: "reth::cli", "Launching Tempo Zone node");

        validate_l1_rpc_url(&args.l1_rpc_url)?;

        let p2p_config = args
            .sequencer_manifest
            .as_ref()
            .map(|manifest_path| {
                let ed25519_key_path = args.p2p_key.as_ref().ok_or_else(|| {
                    eyre::eyre!("--p2p.key is required with --sequencer.manifest")
                })?;
                P2pConfig::load(
                    manifest_path,
                    ed25519_key_path,
                    args.p2p_listen,
                    args.p2p_bypass_ip_check,
                    args.zone_id,
                    args.sequencer_role,
                )
            })
            .transpose()?;
        let manifest_role = p2p_config.as_ref().map(P2pConfig::role);
        if let Some(config) = p2p_config.as_ref() {
            info!(
                target: "reth::cli",
                role = %config.role(),
                ed25519_public_key = %config.ed25519_public_key(),
                listen = %config.listen(),
                "Validated multi-sequencer manifest and local identity"
            );
        }

        let manifest_mode = p2p_config.is_some();
        if manifest_mode {
            // Replicate only durable blocks. Persist every block immediately so followers can
            // acknowledge each block without waiting for Reth's in-memory buffer to fill.
            builder.config_mut().engine.persistence_threshold = 0;
            builder.config_mut().engine.memory_block_buffer_target = 0;
        }
        let should_sequence_blocks = sequencer_enabled(args.enable_sequencer, manifest_role);
        let sequencer_signer = (should_sequence_blocks || manifest_mode)
            .then(|| {
                args.sequencer_key
                    .parse::<PrivateKeySigner>()
                    .map_err(|err| eyre::eyre!("invalid --sequencer-key: {err}"))
            })
            .transpose()?;

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
        .with_withdrawal_batch_interval_blocks(args.zone_batch_interval_blocks)
        .with_private_rpc(ZonePrivateRpcConfig {
            private_rpc_port: args.private_rpc_port,
            zone_id: args.zone_id,
            max_auth_token_validity: Duration::from_secs(
                args.private_rpc_max_auth_token_validity_secs,
            ),
        });

        if should_sequence_blocks {
            let sequencer_signer = sequencer_signer
                .expect("sequencer signer is parsed whenever sequencing is enabled");
            node = node.with_sequencer(ZoneSequencerAddOnsConfig {
                sequencer_signer,
                zone_id: args.zone_id,
                zone_poll_interval: Duration::from_secs(args.zone_poll_interval_secs),
                batch_interval_blocks: args.zone_batch_interval_blocks,
                batch_anchor_config: BatchAnchorConfig::default(),
                withdrawal_poll_interval: Duration::from_secs(args.withdrawal_poll_interval_secs),
            });
        }
        if manifest_role == Some(Role::Follower) {
            info!(target: "reth::cli", "Starting in follower mode");
        }
        if let Some(config) = p2p_config {
            node = node.with_p2p(config);
        }

        let handle = builder.node(node).launch_with_debug_capabilities().await?;
        handle.wait_for_node_exit().await
    })
}

/// Tempo Zone CLI arguments.
#[derive(Debug, Clone, Args)]
pub struct ZoneArgs {
    /// Certified Tempo follower WebSocket RPC URL for finalized L1 state, deposit events, and chain notifications.
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

    /// Path to the static multi-sequencer manifest. Its presence activates
    /// multi-sequencer mode and makes the manifest authoritative for role selection.
    #[arg(
        long = "sequencer.manifest",
        env = "SEQUENCER_MANIFEST",
        value_name = "PATH",
        requires = "p2p_key",
        conflicts_with = "enable_sequencer"
    )]
    pub sequencer_manifest: Option<PathBuf>,

    /// Path to this node's hex-encoded Ed25519 P2P identity key.
    #[arg(
        long = "p2p.key",
        env = "P2P_KEY",
        value_name = "PATH",
        requires = "sequencer_manifest"
    )]
    pub p2p_key: Option<PathBuf>,

    /// Socket address bound for multi-sequencer Commonware traffic.
    #[arg(
        long = "p2p.listen",
        env = "P2P_LISTEN",
        default_value = "0.0.0.0:9200"
    )]
    pub p2p_listen: SocketAddr,

    /// Disable Commonware's pre-authentication source-IP filter.
    ///
    /// Required for DNS peer addresses whose egress IPs are not known in advance.
    /// Only enable this when network-level policy restricts access to the P2P port.
    #[arg(
        long = "p2p.bypass-ip-check",
        env = "P2P_BYPASS_IP_CHECK",
        requires = "sequencer_manifest"
    )]
    pub p2p_bypass_ip_check: bool,

    /// (Optional) Checked against the role derived from the manifest.
    #[arg(
        long = "sequencer.role",
        env = "SEQUENCER_ROLE",
        value_name = "ROLE",
        requires = "sequencer_manifest"
    )]
    pub sequencer_role: Option<Role>,

    /// How often (in seconds) the zone monitor polls for new L2 blocks.
    #[arg(
        long = "zone.poll-interval-secs",
        env = "ZONE_POLL_INTERVAL_SECS",
        default_value_t = 1
    )]
    pub zone_poll_interval_secs: u64,

    /// Number of zone blocks between withdrawal batch boundaries.
    ///
    /// Also used by the sequencer monitor to decide when enough chain progress has
    /// occurred to look for empty finalized batches to submit to L1. Default 120 is
    /// ~1 minute at Tempo's expected 500 ms block time.
    #[arg(
        long = "zone.batch-interval-blocks",
        env = "ZONE_BATCH_INTERVAL_BLOCKS",
        default_value_t = DEFAULT_WITHDRAWAL_BATCH_INTERVAL_BLOCKS
    )]
    pub zone_batch_interval_blocks: u64,

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
    #[arg(
        long = "sequencer",
        env = "SEQUENCER",
        conflicts_with = "sequencer_manifest"
    )]
    pub enable_sequencer: bool,
}

fn prepend_log_filter(filter: &mut String, directives: &str) {
    if filter.is_empty() {
        *filter = directives.to_owned();
    } else {
        *filter = format!("{directives},{filter}");
    }
}

fn sequencer_enabled(cli_flag: bool, manifest_role: Option<Role>) -> bool {
    match manifest_role {
        Some(Role::Leader) => true,
        Some(Role::Follower) => false,
        None => cli_flag,
    }
}

fn validate_l1_rpc_url(l1_rpc_url: &str) -> eyre::Result<()> {
    let url: url::Url = l1_rpc_url
        .parse()
        .map_err(|err| eyre::eyre!("failed parsing --l1.rpc-url as URL: {err}"))?;
    eyre::ensure!(
        matches!(url.scheme(), "ws" | "wss"),
        "--l1.rpc-url must use ws:// or wss://, got `{}`",
        url.scheme()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::{ZoneArgs, ZoneCli, sequencer_enabled, validate_l1_rpc_url};
    use zone_p2p::Role;

    #[derive(Debug, clap::Parser)]
    struct ZoneArgsParser {
        #[command(flatten)]
        zone: ZoneArgs,
    }

    #[test]
    fn top_level_help_lists_dev_subcommand() {
        let result = ZoneCli::try_parse_from(["tempo-zone", "--help"]);
        let error = result.err().expect("--help exits through clap");
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        assert!(error.to_string().contains("  dev"));
    }

    #[test]
    fn dev_is_parsed_by_the_top_level_cli() {
        let parsed = ZoneCli::try_parse_from(["tempo-zone", "dev"]).unwrap();
        assert!(matches!(parsed, ZoneCli::Dev(_)));
    }

    #[test]
    fn manifest_mode_requires_a_p2p_key_and_conflicts_with_legacy_sequencer() {
        let common = [
            "tempo-zone",
            "--l1.rpc-url",
            "ws://localhost:8546",
            "--l1.portal-address",
            "0x0000000000000000000000000000000000000001",
            "--sequencer-key",
            "0x01",
        ];

        let missing_key = ZoneArgsParser::try_parse_from(
            common
                .into_iter()
                .chain(["--sequencer.manifest", "zone.toml"]),
        )
        .unwrap_err();
        assert_eq!(
            missing_key.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );

        let conflict = ZoneArgsParser::try_parse_from(common.into_iter().chain([
            "--sequencer.manifest",
            "zone.toml",
            "--p2p.key",
            "node.key",
            "--sequencer",
        ]))
        .unwrap_err();
        assert_eq!(conflict.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn legacy_mode_still_accepts_the_sequencer_flag_without_a_manifest() {
        let parsed = ZoneArgsParser::try_parse_from([
            "tempo-zone",
            "--l1.rpc-url",
            "ws://localhost:8546",
            "--l1.portal-address",
            "0x0000000000000000000000000000000000000001",
            "--sequencer-key",
            "0x01",
            "--sequencer",
        ])
        .unwrap();
        assert!(parsed.zone.enable_sequencer);
        assert!(parsed.zone.sequencer_manifest.is_none());
    }

    #[test]
    fn p2p_ip_check_bypass_is_explicit_and_requires_manifest_mode() {
        let common = [
            "tempo-zone",
            "--l1.rpc-url",
            "ws://localhost:8546",
            "--l1.portal-address",
            "0x0000000000000000000000000000000000000001",
            "--sequencer-key",
            "0x01",
        ];

        let without_manifest =
            ZoneArgsParser::try_parse_from(common.into_iter().chain(["--p2p.bypass-ip-check"]))
                .unwrap_err();
        assert_eq!(
            without_manifest.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );

        let default = ZoneArgsParser::try_parse_from(common).unwrap();
        assert!(!default.zone.p2p_bypass_ip_check);

        let enabled = ZoneArgsParser::try_parse_from(common.into_iter().chain([
            "--sequencer.manifest",
            "zone.toml",
            "--p2p.key",
            "node.key",
            "--p2p.bypass-ip-check",
        ]))
        .unwrap();
        assert!(enabled.zone.p2p_bypass_ip_check);
    }

    #[test]
    fn manifest_role_is_authoritative_for_sequencer_startup() {
        assert!(sequencer_enabled(false, Some(Role::Leader)));
        assert!(!sequencer_enabled(true, Some(Role::Follower)));
        assert!(sequencer_enabled(true, None));
        assert!(!sequencer_enabled(false, None));
    }

    #[test]
    fn l1_rpc_url_accepts_websocket_schemes() {
        validate_l1_rpc_url("ws://localhost:8546").unwrap();
        validate_l1_rpc_url("wss://rpc.moderato.tempo.xyz").unwrap();
    }

    #[test]
    fn l1_rpc_url_rejects_non_websocket_schemes() {
        assert!(validate_l1_rpc_url("http://localhost:8545").is_err());
        assert!(validate_l1_rpc_url("https://rpc.moderato.tempo.xyz").is_err());
    }
}
