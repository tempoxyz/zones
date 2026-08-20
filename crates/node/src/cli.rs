//! Tempo Zone CLI.

use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use alloy_primitives::Address;
use alloy_signer_local::PrivateKeySigner;
use clap::{Args, CommandFactory, FromArgMatches};
use reth_ethereum::cli::Cli;
use reth_tracing::tracing::info;
use tempo_evm::consensus::TempoConsensus;
use zeroize::Zeroizing;
use zone_chainspec::{ZoneChainSpec, ZoneChainSpecParser};
use zone_evm::ZoneEvmConfig;
use zone_p2p::{MAX_TRANSACTION_MESSAGE_SIZE, P2pConfig, Role};
use zone_payload::DEFAULT_WITHDRAWAL_BATCH_INTERVAL_BLOCKS;

use crate::{
    ZoneNode, ZoneRedactedRpcConfig, ZoneSequencerAddOnsConfig, dev::DevCommand,
    rpc::auth::DEFAULT_MAX_AUTH_TOKEN_VALIDITY_SECS,
};
use zone_sequencer::{
    BatchAnchorConfig, DEFAULT_MAX_IN_FLIGHT_WITHDRAWAL_BATCHES, DEFAULT_MAX_WITHDRAWAL_BATCH_GAS,
    MAX_WITHDRAWAL_BATCH_GAS, WithdrawalBatchLimits,
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
            ZoneEvmConfig::new_without_l1(spec.clone()),
            TempoConsensus::new(spec),
        )
    };

    cli.run_with_components::<ZoneNode>(components, async move |mut builder, args| {
        info!(target: "reth::cli", "Launching Tempo Zone node");

        validate_l1_rpc_url(&args.l1_rpc_url)?;
        validate_portal_address(args.portal_address)?;
        let zone_id = builder.config().chain.zone_id();
        validate_deprecated_zone_id(args.zone_id, zone_id)?;

        let p2p_config = args
            .sequencer_manifest
            .as_ref()
            .map(|manifest_path| {
                let ed25519_key_path = args.p2p_key.as_ref().ok_or_else(|| {
                    eyre::eyre!("--p2p.key is required with --sequencer.manifest")
                })?;
                // Required for a quorum member and rejected for an `rpc_only` node, which never
                // signs a settlement attestation; the manifest decides which this node is.
                P2pConfig::load(
                    manifest_path,
                    ed25519_key_path,
                    args.secp256k1_key.as_ref(),
                    args.p2p_listen,
                    args.p2p_bypass_ip_check,
                    zone_id,
                    args.sequencer_role,
                )
            })
            .transpose()?;
        if let Some(config) = p2p_config.as_ref() {
            info!(
                target: "reth::cli",
                ed25519_public_key = %config.ed25519_public_key(),
                secp256k1_address = ?config.secp256k1_address(),
                listen = %config.listen(),
                "Validated multi-sequencer manifest and local identity"
            );
        }

        let manifest_mode = p2p_config.is_some();
        validate_p2p_transaction_size_limit(
            manifest_mode,
            builder.config().txpool.max_tx_input_bytes,
        )?;
        if manifest_mode {
            // Replicate only durable blocks. Persist every block immediately so followers can
            // acknowledge each block without waiting for Reth's in-memory buffer to fill.
            builder.config_mut().engine.persistence_threshold = 0;
            builder.config_mut().engine.memory_block_buffer_target = Some(0);
        }
        // Every promotable node constructs all the sequencer resources: activation is gated at
        // runtime by the leadership schedule, so a quorum follower must be able to become a
        // leader without a restart. An rpc-only standby is not promotable at runtime — that
        // needs a new individual key registered with `ZonePortal` and a manifest change — so it
        // is deliberately left without the shared sequencer key, which is also the zone's ECIES
        // private key for encrypted deposits and must not sit on an internet-facing host.
        let rpc_only = p2p_config.as_ref().is_some_and(P2pConfig::is_rpc_only);
        let should_sequence_blocks = sequencer_enabled(args.enable_sequencer, p2p_config.as_ref());
        if rpc_only && args.sequencer_key_file.is_some() {
            return Err(eyre::eyre!(
                "this node is `rpc_only` in the manifest, so --sequencer-key-file must not be provided: the shared key is never used here and is also the zone ECIES private key for encrypted deposits"
            ));
        }
        let sequencer_signer = if should_sequence_blocks {
            Some(load_sequencer_signer(args.sequencer_key_file.as_deref()).await?)
        } else {
            None
        };
        eyre::ensure!(
            !args.enable_prover || should_sequence_blocks,
            "--sequencer.enable-prover requires a promotable sequencer node"
        );
        let additional_decryption_keys =
            load_decryption_keys(args.deposit_decryption_keys_file.as_deref()).await?;

        builder.config_mut().network.discovery.disable_discovery = true;
        builder.config_mut().rpc.disable_auth_server = true;
        builder.config_mut().rpc.rpc_max_logs_per_response = MAX_LOGS_PER_RESPONSE.into();
        builder.config_mut().rpc.rpc_max_blocks_per_filter = MAX_BLOCKS_PER_FILTER.into();

        let mut node = ZoneNode::new(
            args.l1_rpc_url,
            args.portal_address,
            args.l1_fetch_concurrency,
            Duration::from_millis(args.l1_retry_connection_interval_ms),
        )
        .with_withdrawal_batch_interval_blocks(args.zone_batch_interval_blocks)
        .with_redacted_rpc(ZoneRedactedRpcConfig {
            redacted_rpc_port: args.redacted_rpc_port,
            zone_id,
            max_auth_token_validity: Duration::from_secs(
                args.redacted_rpc_max_auth_token_validity_secs,
            ),
        });
        if !additional_decryption_keys.is_empty() {
            node = node.with_deposit_decryption_keys(additional_decryption_keys);
        }

        if should_sequence_blocks {
            let sequencer_signer = sequencer_signer
                .expect("sequencer signer is parsed whenever sequencing is enabled");
            // `None` on an rpc-only node: it holds no individual key, and it is never the
            // scheduled leader, so it never submits an L1 settlement transaction.
            let l1_transaction_signer = p2p_config
                .as_ref()
                .and_then(P2pConfig::block_attestation_signer);
            node = node.with_sequencer(ZoneSequencerAddOnsConfig {
                sequencer_signer,
                l1_transaction_signer,
                zone_id,
                zone_poll_interval: Duration::from_secs(args.zone_poll_interval_secs),
                batch_anchor_config: BatchAnchorConfig::default(),
                withdrawal_poll_interval: Duration::from_secs(args.withdrawal_poll_interval_secs),
                withdrawal_batch_limits: WithdrawalBatchLimits {
                    max_batch_gas: args.withdrawal_max_batch_gas,
                    max_in_flight_batches: args.withdrawal_max_in_flight_batches,
                },
                enable_prover: args.enable_prover,
                prover_address: args.prover_address,
            });
        }
        if let Some(config) = p2p_config {
            node = node.with_p2p(config);
        }

        let handle = builder.node(node).launch_with_debug_capabilities().await?;
        handle.wait_for_node_exit().await
    })
}

async fn load_sequencer_signer(
    key_file: Option<&std::path::Path>,
) -> eyre::Result<PrivateKeySigner> {
    let path = key_file.ok_or_else(|| {
        eyre::eyre!("--sequencer-key-file is required when sequencing is enabled")
    })?;
    let path = path.to_path_buf();
    let source = format!("--sequencer-key-file {}", path.display());
    let display_path = path.display().to_string();
    let key = tokio::task::spawn_blocking(move || std::fs::read_to_string(path))
        .await
        .map_err(|err| eyre::eyre!("sequencer key reader task failed for {display_path}: {err}"))?
        .map_err(|err| eyre::eyre!("failed to read sequencer key from {display_path}: {err}"))?;
    let key = Zeroizing::new(key);

    key.trim()
        .parse::<PrivateKeySigner>()
        .map_err(|_| eyre::eyre!("invalid sequencer key from {source}"))
}

async fn load_decryption_keys(
    key_file: Option<&std::path::Path>,
) -> eyre::Result<Vec<k256::SecretKey>> {
    let Some(path) = key_file else {
        return Ok(Vec::new());
    };
    let path = path.to_path_buf();
    let display_path = path.display().to_string();
    let contents = tokio::task::spawn_blocking(move || std::fs::read_to_string(path))
        .await
        .map_err(|err| eyre::eyre!("decryption key reader task failed for {display_path}: {err}"))?
        .map_err(|err| eyre::eyre!("failed to read decryption keys from {display_path}: {err}"))?;
    let contents = Zeroizing::new(contents);
    let mut keys = Vec::new();
    for (line_index, value) in contents.lines().enumerate() {
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let signer = value.parse::<PrivateKeySigner>().map_err(|_| {
            eyre::eyre!(
                "invalid decryption key on line {} of {display_path}",
                line_index + 1
            )
        })?;
        keys.push(k256::SecretKey::from(signer.credential()));
    }
    eyre::ensure!(
        !keys.is_empty(),
        "decryption key file {display_path} contains no keys"
    );
    Ok(keys)
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

    /// Path to a file or FIFO containing the shared sequencer private key.
    ///
    /// Required by every node that can produce blocks. Requiredness is checked after the
    /// manifest is read rather than by `clap`, because an `rpc_only` node must not hold this
    /// key: it is also the zone's ECIES private key for encrypted deposits.
    #[arg(
        long = "sequencer-key-file",
        env = "SEQUENCER_KEY_FILE",
        value_name = "PATH"
    )]
    pub sequencer_key_file: Option<PathBuf>,

    /// File containing additional deposit decryption keys, one hex key per line.
    #[arg(
        long = "deposit-decryption-keys-file",
        env = "DEPOSIT_DECRYPTION_KEYS_FILE",
        value_name = "PATH"
    )]
    pub deposit_decryption_keys_file: Option<PathBuf>,

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

    /// Path to a file or FIFO containing this node's hex-encoded Ed25519 P2P identity key.
    #[arg(
        long = "p2p.key",
        env = "P2P_KEY",
        value_name = "PATH",
        requires = "sequencer_manifest"
    )]
    pub p2p_key: Option<PathBuf>,

    /// Path to a file or FIFO containing this node's hex-encoded individual secp256k1 private key.
    #[arg(
        long = "secp256k1.key",
        env = "SECP256K1_KEY",
        value_name = "PATH",
        requires = "sequencer_manifest"
    )]
    pub secp256k1_key: Option<PathBuf>,

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
    ///
    /// One of `leader`, `follower`, or `rpc-follower`.
    #[arg(
        long = "sequencer.role",
        env = "SEQUENCER_ROLE",
        value_name = "ROLE",
        requires = "sequencer_manifest"
    )]
    pub sequencer_role: Option<Role>,

    /// How often (in seconds) the zone monitor reconciles with the canonical head if no
    /// canonical-state notification triggers it first.
    #[arg(
        long = "zone.poll-interval-secs",
        env = "ZONE_POLL_INTERVAL_SECS",
        default_value_t = 1
    )]
    pub zone_poll_interval_secs: u64,

    /// Number of zone blocks between withdrawal batch boundaries.
    ///
    /// Default 120 is ~1 minute at Tempo's expected 500 ms block time.
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

    /// Maximum gas reserved by one processWithdrawals transaction, up to 20,000,000. An oversized
    /// withdrawal is submitted alone.
    #[arg(
        long = "withdrawal-max-batch-gas",
        env = "WITHDRAWAL_MAX_BATCH_GAS",
        default_value_t = DEFAULT_MAX_WITHDRAWAL_BATCH_GAS,
        value_parser = clap::builder::RangedU64ValueParser::<u64>::new()
            .range(1..=MAX_WITHDRAWAL_BATCH_GAS)
    )]
    pub withdrawal_max_batch_gas: u64,

    /// Maximum number of ordered processWithdrawals transactions kept in flight.
    #[arg(
        long = "withdrawal-max-in-flight-batches",
        env = "WITHDRAWAL_MAX_IN_FLIGHT_BATCHES",
        default_value_t = DEFAULT_MAX_IN_FLIGHT_WITHDRAWAL_BATCHES,
        value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..)
    )]
    pub withdrawal_max_in_flight_batches: usize,

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

    /// Deprecated: validates the Zone ID encoded in the genesis chain ID.
    #[arg(long = "zone.id", env = "ZONE_ID")]
    pub zone_id: Option<u32>,

    /// Port for the redacted zone RPC server (0 for OS-assigned).
    #[arg(
        long = "redacted-rpc.port",
        alias = "private-rpc.port",
        env = "REDACTED_RPC_PORT",
        default_value_t = 8544
    )]
    pub redacted_rpc_port: u16,

    /// Maximum auth token validity window the redacted RPC accepts, in seconds.
    #[arg(
        long = "redacted-rpc.max-auth-token-validity-secs",
        env = "REDACTED_RPC_MAX_AUTH_TOKEN_VALIDITY_SECS",
        default_value_t = DEFAULT_MAX_AUTH_TOKEN_VALIDITY_SECS
    )]
    pub redacted_rpc_max_auth_token_validity_secs: u64,

    /// Enable the Zone node in sequencer mode. This advances block production and submits
    /// withdrawal batches.
    #[arg(
        long = "sequencer",
        env = "SEQUENCER",
        conflicts_with = "sequencer_manifest"
    )]
    pub enable_sequencer: bool,

    /// Validate finalized batch candidates with the SPF without changing settlement.
    #[arg(long = "sequencer.enable-prover", env = "SEQUENCER_ENABLE_PROVER")]
    pub enable_prover: bool,

    /// Send witnesses to this remote prover instead of executing the SPF locally.
    #[arg(
        long = "sequencer.prover-address",
        env = "SEQUENCER_PROVER_ADDRESS",
        value_name = "HOST:PORT",
        requires = "enable_prover"
    )]
    pub prover_address: Option<String>,
}

fn prepend_log_filter(filter: &mut String, directives: &str) {
    if filter.is_empty() {
        *filter = directives.to_owned();
    } else {
        *filter = format!("{directives},{filter}");
    }
}

/// Whether the sequencer add-on is configured at boot.
///
/// `rpc_only` nodes are excluded: they never produce blocks and cannot be promoted without a
/// restart, so they must not be given the shared sequencer key.
fn sequencer_enabled(cli_flag: bool, p2p_config: Option<&P2pConfig>) -> bool {
    cli_flag || p2p_config.is_some_and(|config| !config.is_rpc_only())
}

fn validate_p2p_transaction_size_limit(
    manifest_mode: bool,
    max_tx_input_bytes: usize,
) -> eyre::Result<()> {
    if manifest_mode {
        eyre::ensure!(
            max_tx_input_bytes <= MAX_TRANSACTION_MESSAGE_SIZE,
            "--txpool.max-tx-input-bytes ({max_tx_input_bytes}) exceeds the multi-sequencer P2P transaction limit ({MAX_TRANSACTION_MESSAGE_SIZE})"
        );
    }
    Ok(())
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

fn validate_portal_address(portal_address: Address) -> eyre::Result<()> {
    eyre::ensure!(
        !portal_address.is_zero(),
        "--l1.portal-address must be nonzero"
    );
    Ok(())
}

fn validate_deprecated_zone_id(configured: Option<u32>, derived: u32) -> eyre::Result<()> {
    if let Some(configured) = configured {
        eyre::ensure!(
            configured == derived,
            "deprecated --zone.id value {configured} does not match zone ID {derived} encoded in the genesis chain ID"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{io::Write as _, process::Command, thread, time::Duration};

    use clap::Parser as _;

    use super::{
        Role, ZoneArgs, ZoneCli, load_decryption_keys, load_sequencer_signer, sequencer_enabled,
        validate_deprecated_zone_id, validate_l1_rpc_url, validate_p2p_transaction_size_limit,
        validate_portal_address,
    };
    use zone_sequencer::MAX_WITHDRAWAL_BATCH_GAS;

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
    fn portal_address_must_be_nonzero() {
        assert!(validate_portal_address(alloy_primitives::Address::ZERO).is_err());
        assert!(validate_portal_address(alloy_primitives::Address::repeat_byte(0x11)).is_ok());
    }

    #[test]
    fn deprecated_zone_id_is_optional_and_validated() {
        assert!(validate_deprecated_zone_id(None, 7).is_ok());
        assert!(validate_deprecated_zone_id(Some(7), 7).is_ok());
        assert!(validate_deprecated_zone_id(Some(8), 7).is_err());

        let parsed = ZoneArgsParser::try_parse_from([
            "tempo-zone",
            "--l1.rpc-url",
            "ws://localhost:8546",
            "--l1.portal-address",
            "0x0000000000000000000000000000000000000001",
            "--zone.id",
            "7",
        ])
        .unwrap();
        assert_eq!(parsed.zone.zone_id, Some(7));
    }

    #[test]
    fn manifest_mode_rejects_a_txpool_limit_above_the_p2p_wire_limit() {
        assert!(
            validate_p2p_transaction_size_limit(true, zone_p2p::MAX_TRANSACTION_MESSAGE_SIZE,)
                .is_ok()
        );
        let error =
            validate_p2p_transaction_size_limit(true, zone_p2p::MAX_TRANSACTION_MESSAGE_SIZE + 1)
                .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("exceeds the multi-sequencer P2P transaction limit")
        );
        assert!(
            validate_p2p_transaction_size_limit(false, zone_p2p::MAX_TRANSACTION_MESSAGE_SIZE + 1,)
                .is_ok()
        );
    }

    #[test]
    fn sequencer_key_file_is_accepted_and_inline_key_option_is_rejected() {
        let common = [
            "tempo-zone",
            "--l1.rpc-url",
            "ws://localhost:8546",
            "--l1.portal-address",
            "0x0000000000000000000000000000000000000001",
        ];

        let parsed = ZoneArgsParser::try_parse_from(
            common
                .into_iter()
                .chain(["--sequencer-key-file", "/run/secrets/sequencer-key"]),
        )
        .unwrap();
        assert_eq!(
            parsed.zone.sequencer_key_file.as_deref(),
            Some(std::path::Path::new("/run/secrets/sequencer-key"))
        );
        let error =
            ZoneArgsParser::try_parse_from(common.into_iter().chain(["--sequencer-key", "0x01"]))
                .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loads_sequencer_key_from_file_with_trailing_newline() {
        let path =
            std::env::temp_dir().join(format!("tempo-zone-sequencer-key-{}", std::process::id()));
        std::fs::write(
            &path,
            "0000000000000000000000000000000000000000000000000000000000000001\n",
        )
        .unwrap();

        let signer = load_sequencer_signer(Some(&path)).await.unwrap();
        std::fs::remove_file(path).unwrap();

        assert_eq!(
            signer.address(),
            "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf"
                .parse::<alloy_primitives::Address>()
                .unwrap()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loads_additional_decryption_keys_one_per_line() {
        let path =
            std::env::temp_dir().join(format!("tempo-zone-decryption-keys-{}", std::process::id()));
        std::fs::write(
            &path,
            concat!(
                "0000000000000000000000000000000000000000000000000000000000000001\n",
                "\n",
                "0000000000000000000000000000000000000000000000000000000000000002\n"
            ),
        )
        .unwrap();

        let keys = load_decryption_keys(Some(&path)).await.unwrap();
        std::fs::remove_file(path).unwrap();

        assert_eq!(keys.len(), 2);
        assert_eq!(
            keys[0].to_bytes().as_slice(),
            alloy_primitives::B256::with_last_byte(1).as_slice()
        );
        assert_eq!(
            keys[1].to_bytes().as_slice(),
            alloy_primitives::B256::with_last_byte(2).as_slice()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loads_sequencer_key_from_fifo() {
        let path = std::env::temp_dir().join(format!(
            "tempo-zone-sequencer-key-{}.fifo",
            std::process::id()
        ));
        let status = Command::new("mkfifo")
            .args(["-m", "600"])
            .arg(&path)
            .status()
            .expect("mkfifo must be available");
        assert!(status.success(), "mkfifo failed: {status}");

        let writer_path = path.clone();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            let mut fifo = std::fs::OpenOptions::new()
                .write(true)
                .open(writer_path)
                .unwrap();
            writeln!(
                fifo,
                "0000000000000000000000000000000000000000000000000000000000000001"
            )
            .unwrap();
        });

        let signer =
            tokio::time::timeout(Duration::from_secs(2), load_sequencer_signer(Some(&path)))
                .await
                .expect("FIFO read timed out")
                .unwrap();
        writer.join().unwrap();
        std::fs::remove_file(path).unwrap();

        assert_eq!(
            signer.address(),
            "0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf"
                .parse::<alloy_primitives::Address>()
                .unwrap()
        );
    }

    #[test]
    fn manifest_mode_requires_the_p2p_key_and_conflicts_with_legacy_sequencer() {
        let common = [
            "tempo-zone",
            "--l1.rpc-url",
            "ws://localhost:8546",
            "--l1.portal-address",
            "0x0000000000000000000000000000000000000001",
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

        // `--secp256k1.key` is deliberately not a parse-time requirement: an `rpc_only` node
        // holds no individual key. Whether one is expected is decided against the manifest in
        // `ZoneManifest::validate_node`.
        let without_secp256k1_key = ZoneArgsParser::try_parse_from(common.into_iter().chain([
            "--sequencer.manifest",
            "zone.toml",
            "--p2p.key",
            "node.key",
        ]))
        .unwrap();
        assert_eq!(without_secp256k1_key.zone.secp256k1_key, None);

        let conflict = ZoneArgsParser::try_parse_from(common.into_iter().chain([
            "--sequencer.manifest",
            "zone.toml",
            "--p2p.key",
            "node.key",
            "--secp256k1.key",
            "node-secp256k1.key",
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
            "--sequencer",
        ])
        .unwrap();
        assert!(parsed.zone.enable_sequencer);
        assert!(parsed.zone.sequencer_manifest.is_none());
    }

    #[test]
    fn zone_poll_interval_keeps_one_second_default_and_accepts_override() {
        let common = [
            "tempo-zone",
            "--l1.rpc-url",
            "ws://localhost:8546",
            "--l1.portal-address",
            "0x0000000000000000000000000000000000000001",
        ];

        let default = ZoneArgsParser::try_parse_from(common).unwrap();
        assert_eq!(default.zone.zone_poll_interval_secs, 1);

        let overridden = ZoneArgsParser::try_parse_from(
            common.into_iter().chain(["--zone.poll-interval-secs", "3"]),
        )
        .unwrap();
        assert_eq!(overridden.zone.zone_poll_interval_secs, 3);
    }

    #[test]
    fn private_rpc_port_alias_is_accepted() {
        let common = [
            "tempo-zone",
            "--l1.rpc-url",
            "ws://localhost:8546",
            "--l1.portal-address",
            "0x0000000000000000000000000000000000000001",
        ];

        let redacted = ZoneArgsParser::try_parse_from(
            common.into_iter().chain(["--redacted-rpc.port", "9544"]),
        )
        .unwrap();
        let private = ZoneArgsParser::try_parse_from(
            common.into_iter().chain(["--private-rpc.port", "9544"]),
        )
        .unwrap();

        assert_eq!(redacted.zone.redacted_rpc_port, 9544);
        assert_eq!(private.zone.redacted_rpc_port, 9544);
    }

    #[test]
    fn withdrawal_batch_gas_rejects_values_above_the_safe_limit() {
        let above_limit = (MAX_WITHDRAWAL_BATCH_GAS + 1).to_string();
        let error = ZoneArgsParser::try_parse_from([
            "tempo-zone",
            "--l1.rpc-url",
            "ws://localhost:8546",
            "--l1.portal-address",
            "0x0000000000000000000000000000000000000001",
            "--withdrawal-max-batch-gas",
            &above_limit,
        ])
        .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn p2p_ip_check_bypass_is_explicit_and_requires_manifest_mode() {
        let common = [
            "tempo-zone",
            "--l1.rpc-url",
            "ws://localhost:8546",
            "--l1.portal-address",
            "0x0000000000000000000000000000000000000001",
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
            "--secp256k1.key",
            "node-secp256k1.key",
            "--p2p.bypass-ip-check",
        ]))
        .unwrap();
        assert!(enabled.zone.p2p_bypass_ip_check);
    }

    #[test]
    fn sequencer_resources_follow_the_cli_flag_without_a_manifest() {
        // Manifest mode is covered by `sequencer_enabled`'s other arm, which needs a real
        // `P2pConfig`; see `manifest_mode_configures_sequencer_resources_except_on_standbys`
        // in the integration tests.
        assert!(sequencer_enabled(true, None));
        assert!(!sequencer_enabled(false, None));
    }

    #[test]
    fn sequencer_role_argument_accepts_the_rpc_follower_spelling() {
        let parsed = ZoneArgsParser::try_parse_from([
            "tempo-zone",
            "--l1.rpc-url",
            "ws://localhost:8546",
            "--l1.portal-address",
            "0x0000000000000000000000000000000000000001",
            "--sequencer.manifest",
            "zone.toml",
            "--p2p.key",
            "node.key",
            "--sequencer.role",
            "rpc-follower",
        ])
        .unwrap();
        assert_eq!(parsed.zone.sequencer_role, Some(Role::RpcFollower));
        // A standby holds neither key. Requiredness is checked against the manifest after it is
        // read, so neither flag is a parse-time requirement.
        assert_eq!(parsed.zone.secp256k1_key, None);
        assert_eq!(parsed.zone.sequencer_key_file, None);
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
