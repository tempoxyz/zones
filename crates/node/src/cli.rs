//! Tempo Zone CLI.

use std::{
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use alloy_primitives::Address;
use alloy_provider::{Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use clap::{Args, Parser as _};
use eyre::WrapErr as _;
use futures::future::BoxFuture;
use reth_chainspec::EthChainSpec as _;
use reth_ethereum::cli::Cli;
use reth_tracing::tracing::{info, warn};
use tempo_alloy::TempoNetwork;
use tempo_evm::consensus::TempoConsensus;
use zeroize::Zeroizing;
use zone_chainspec::{ZoneChainSpec, ZoneChainSpecParser};
use zone_evm::ZoneEvmConfig;
use zone_l1::state::{L1StateCache, L1StateProvider, L1StateProviderConfig};
use zone_p2p::{MAX_TRANSACTION_MESSAGE_SIZE, P2pConfig, Role};
use zone_payload::DEFAULT_WITHDRAWAL_BATCH_INTERVAL_BLOCKS;

use crate::{
    ZoneNode, ZoneRedactedRpcConfig, ZoneSequencerAddOnsConfig,
    rpc::auth::DEFAULT_MAX_AUTH_TOKEN_VALIDITY_SECS,
};
use zone_checker::{CheckerConfig, CheckerExEx, CheckerMode};
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

/// Runs the Tempo Zone CLI.
pub fn run() -> eyre::Result<()> {
    let mut cli = Cli::<ZoneChainSpecParser, ZoneArgs>::parse();
    prepend_log_filter(&mut cli.logs.log_stdout_filter, ZONE_LOG_FILTER_DIRECTIVES);
    prepend_log_filter(&mut cli.logs.log_file_filter, ZONE_LOG_FILTER_DIRECTIVES);
    validate_node_options(&mut cli)?;

    let (dev, external_l1) = match cli.as_node_command_mut() {
        Some(command) if command.dev.dev => {
            let datadir = command
                .datadir
                .clone()
                .resolve_datadir(command.chain.chain());
            crate::dev::prepare_datadir(datadir.data_dir())?;
            (true, None)
        }
        Some(command) => (false, Some(external_l1_config(&command.ext)?)),
        None => (false, None),
    };
    let l1_config = if dev {
        None
    } else {
        l1_http_config_from_env()?
    };

    let components = move |spec: Arc<ZoneChainSpec>| {
        let evm_config = cli_evm_config(spec.clone(), l1_config.clone());
        (evm_config, TempoConsensus::new(spec))
    };

    cli.run_with_components::<ZoneNode>(components, async move |mut builder, args| {
        info!(target: "reth::cli", "Launching Tempo Zone node");

        if args.block_interval_ms.is_some() {
            warn!(target: "reth::cli", "--block.interval-ms is deprecated, has no effect, and will be removed in the next release");
        }

        let (l1_rpc_url, portal_address, sequencer_signer, l1_exit) = if builder.config().dev.dev {
            eyre::ensure!(
                args.sequencer_manifest.is_none(),
                "--sequencer.manifest cannot be used with --dev"
            );
            let executor = builder.task_executor().clone();
            let dev = crate::dev::init(builder.config_mut(), executor).await?;
            (
                dev.l1_rpc_url,
                dev.portal,
                Some(dev.signer),
                Some(dev.l1_exit),
            )
        } else {
            let (l1_rpc_url, portal_address) =
                external_l1.expect("external L1 config resolved for non-dev node");
            (l1_rpc_url, portal_address, None, None)
        };

        let zone_id = builder.config().chain.zone_id();
        validate_deprecated_zone_id(args.zone_id, zone_id)?;

        let manifest_mode = args.sequencer_manifest.is_some();
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
        let additional_decryption_keys =
            load_decryption_keys(args.deposit_decryption_keys_file.as_deref()).await?;

        builder.config_mut().network.discovery.disable_discovery = true;
        builder.config_mut().rpc.disable_auth_server = true;
        builder.config_mut().rpc.rpc_max_logs_per_response = MAX_LOGS_PER_RESPONSE.into();
        builder.config_mut().rpc.rpc_max_blocks_per_filter = MAX_BLOCKS_PER_FILTER.into();

        let mut node = ZoneNode::new(
            l1_rpc_url.clone(),
            portal_address,
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

        node = configure_sequencing(&args, zone_id, node, sequencer_signer).await?;

        // Install or skip the checker ExEx based on the configured mode.
        let handle = match args.checker_mode {
            CheckerMode::Off => {
                builder.node(node).launch().await?
            }
            CheckerMode::Observe => {
                info!(target: "reth::cli", "Checker ExEx enabled (observe mode)");
                let node = node.with_portal_evidence_retention();
                let checker = CheckerExEx::new(CheckerConfig {
                    l1_rpc_url: l1_rpc_url.clone(),
                    portal_address,
                    zone_id,
                    zone_chain_id: builder.config().chain.chain().id(),
                    database_path: builder.config().datadir().data_dir().join("checker"),
                    l1_block_tracker: node.l1_block_tracker(),
                });
                builder
                    .node(node)
                    .install_exex("zone-checker", async move |ctx| Ok(checker.run(ctx)))
                    .launch()
                    .await?
            }
        };
        wait_for_exit(handle.wait_for_node_exit(), l1_exit).await
    })
}

async fn wait_for_exit(
    zone_exit: impl Future<Output = eyre::Result<()>>,
    l1_exit: Option<BoxFuture<'static, eyre::Result<()>>>,
) -> eyre::Result<()> {
    let Some(l1_exit) = l1_exit else {
        return zone_exit.await;
    };

    tokio::select! {
        result = zone_exit => result,
        result = l1_exit => {
            result.wrap_err("embedded Tempo L1 failed")?;
            Err(eyre::eyre!("embedded Tempo L1 exited unexpectedly"))
        }
    }
}

fn validate_node_options(cli: &mut Cli<ZoneChainSpecParser, ZoneArgs>) -> eyre::Result<()> {
    let Some(command) = cli.as_node_command_mut() else {
        return Ok(());
    };

    eyre::ensure!(
        command.debug.rpc_consensus_url.is_none() && command.debug.etherscan.is_none(),
        "--debug.rpc-consensus-url and --debug.etherscan are not supported"
    );
    if !command.dev.dev {
        return Ok(());
    }

    eyre::ensure!(
        command.chain.as_ref() == ZoneChainSpecParser::dev_chain_spec()?.as_ref(),
        "--chain must be `dev` when --dev is enabled"
    );
    eyre::ensure!(
        !command.with_unused_ports,
        "--with-unused-ports cannot be used with --dev because provisioning requires a stable RPC port"
    );
    eyre::ensure!(
        command.dev.block_max_transactions.is_none(),
        "--dev.block-max-transactions is not supported"
    );

    let mut rpc = command.rpc.clone();
    rpc.adjust_instance_ports(command.instance);
    eyre::ensure!(rpc.http_port != 0, "--http.port must not resolve to zero");
    if let Some(instance) = command.instance {
        command.ext.redacted_rpc_port = command
            .ext
            .redacted_rpc_port
            .checked_sub(instance - 1)
            .ok_or_else(|| eyre::eyre!("--redacted-rpc.port must not resolve to zero"))?;
    }
    eyre::ensure!(
        command.ext.redacted_rpc_port == 0 || command.ext.redacted_rpc_port != rpc.http_port,
        "operator and redacted RPC ports must be different"
    );
    Ok(())
}

fn external_l1_config(args: &ZoneArgs) -> eyre::Result<(String, Address)> {
    let l1_rpc_url = match args.l1_rpc_url.as_ref() {
        Some(url) => url.clone(),
        None => {
            let url = std::env::var("L1_RPC_URL")
                .map_err(|_| eyre::eyre!("--l1.rpc-url or L1_RPC_URL is required without --dev"))?;
            parse_l1_rpc_url(&url).map_err(eyre::Report::msg)?
        }
    };
    let portal_address = match args.portal_address {
        Some(address) => address,
        None => {
            let address = std::env::var("L1_PORTAL_ADDRESS").map_err(|_| {
                eyre::eyre!("--l1.portal-address or L1_PORTAL_ADDRESS is required without --dev")
            })?;
            parse_portal_address(&address).map_err(eyre::Report::msg)?
        }
    };
    Ok((l1_rpc_url, portal_address))
}

fn l1_http_config_from_env() -> eyre::Result<Option<(url::Url, Address)>> {
    match std::env::var("L1_HTTP_RPC_URL") {
        Ok(url) if !url.is_empty() => {
            let url = url
                .parse()
                .map_err(|error| eyre::eyre!("invalid L1_HTTP_RPC_URL: {error}"))?;
            let portal_address: Address = std::env::var("L1_PORTAL_ADDRESS")
                .map_err(|error| {
                    eyre::eyre!(
                        "L1_PORTAL_ADDRESS must be set when L1_HTTP_RPC_URL is set: {error}"
                    )
                })?
                .parse()
                .map_err(|error| eyre::eyre!("invalid L1_PORTAL_ADDRESS: {error}"))?;
            eyre::ensure!(
                !portal_address.is_zero(),
                "L1_PORTAL_ADDRESS must be nonzero"
            );
            Ok(Some((url, portal_address)))
        }
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(eyre::eyre!("invalid L1_HTTP_RPC_URL: {error}")),
    }
}

/// Creates the EVM config used by CLI subcommands.
fn cli_evm_config(
    chain_spec: Arc<ZoneChainSpec>,
    l1_config: Option<(url::Url, Address)>,
) -> ZoneEvmConfig {
    let Some((l1_rpc_url, portal_address)) = l1_config else {
        return ZoneEvmConfig::new_without_l1(chain_spec);
    };

    let cache = L1StateCache::default();
    let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
        .connect_http(l1_rpc_url)
        .erased();
    let runtime_handle = tokio::runtime::Handle::current();
    let config = L1StateProviderConfig::default();
    let l1_provider = L1StateProvider::new_raw(config, cache, provider, runtime_handle);
    ZoneEvmConfig::new(chain_spec, l1_provider, portal_address)
}

/// Load and attach all sequencer resources to the node.
async fn configure_sequencing(
    args: &ZoneArgs,
    zone_id: u32,
    mut node: ZoneNode,
    signer: Option<PrivateKeySigner>,
) -> eyre::Result<ZoneNode> {
    let p2p_config =
        args.sequencer_manifest
            .as_ref()
            .map(|manifest_path| {
                let ed25519_key_path = args.p2p_key.as_ref().ok_or_else(|| {
                    eyre::eyre!("--p2p.key is required with --sequencer.manifest")
                })?;
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

    let rpc_only = p2p_config.as_ref().is_some_and(P2pConfig::is_rpc_only);
    eyre::ensure!(
        signer.is_some() || args.sequencer || p2p_config.is_some(),
        "--sequencer.manifest is required for node startup"
    );
    if args.sequencer {
        warn!(
            target: "reth::cli",
            "--sequencer is deprecated; configure --sequencer.manifest, --p2p.key, and --secp256k1.key"
        );
    }
    let sequencing = signer.is_some()
        || args.sequencer
        || p2p_config
            .as_ref()
            .is_some_and(|config| !config.is_rpc_only());
    if rpc_only && args.sequencer_key_file.is_some() {
        return Err(eyre::eyre!(
            "this node is `rpc_only` in the manifest, so --sequencer-key-file must not be provided: the shared key is never used here and is also the zone ECIES private key for encrypted deposits"
        ));
    }
    eyre::ensure!(
        !args.enable_prover || sequencing,
        "--sequencer.enable-prover requires a promotable sequencer node"
    );

    if sequencing {
        let sequencer_signer = match signer {
            Some(signer) => signer,
            None => load_sequencer_signer(args.sequencer_key_file.as_deref()).await?,
        };
        node = node.with_sequencer(ZoneSequencerAddOnsConfig {
            sequencer_signer,
            // `None` on an rpc-only node: it holds no individual key, and it is never the
            // scheduled leader, so it never submits an L1 settlement transaction.
            l1_transaction_signer: p2p_config
                .as_ref()
                .and_then(P2pConfig::block_attestation_signer),
            zone_id,
            zone_poll_interval: Duration::from_secs(args.zone_poll_interval_secs),
            batch_anchor_config: BatchAnchorConfig::default(),
            withdrawal_poll_interval: Duration::from_secs(args.withdrawal_poll_interval_secs),
            withdrawal_batch_limits: WithdrawalBatchLimits {
                max_batch_gas: args.withdrawal_max_batch_gas,
                max_in_flight_batches: args.withdrawal_max_in_flight_batches,
            },
            enable_prover: args.enable_prover,
            prover_address: args.prover_address.clone(),
        });
    }
    if let Some(config) = p2p_config {
        node = node.with_p2p(config);
    }
    Ok(node)
}

async fn load_sequencer_signer(key_file: Option<&Path>) -> eyre::Result<PrivateKeySigner> {
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

async fn load_decryption_keys(key_file: Option<&Path>) -> eyre::Result<Vec<k256::SecretKey>> {
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
    #[arg(
        long = "l1.rpc-url",
        conflicts_with = "dev",
        value_parser = parse_l1_rpc_url
    )]
    pub l1_rpc_url: Option<String>,

    /// ZonePortal contract address on L1.
    #[arg(
        long = "l1.portal-address",
        conflicts_with = "dev",
        value_parser = parse_portal_address
    )]
    pub portal_address: Option<Address>,

    /// Deprecated compatibility flag. Ignored.
    #[arg(
        long = "block.interval-ms",
        env = "BLOCK_INTERVAL_MS",
        help = "Deprecated: no longer has any effect and will be removed in the next release."
    )]
    pub block_interval_ms: Option<u64>,

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
        conflicts_with = "sequencer"
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

    /// Deprecated manifestless node startup.
    ///
    /// Use `--dev` for local development, or configure `--sequencer.manifest`, `--p2p.key`, and
    /// `--secp256k1.key` for a networked node.
    #[arg(
        long = "sequencer",
        env = "SEQUENCER",
        conflicts_with = "sequencer_manifest",
        help = "Deprecated: start a node without a sequencer manifest"
    )]
    pub sequencer: bool,

    /// Checker ExEx mode: `off` (default) or `observe`.
    #[arg(
        long = "checker.mode",
        env = "CHECKER_MODE",
        default_value = "off",
        value_parser = zone_checker::CheckerMode::parse,
    )]
    pub checker_mode: zone_checker::CheckerMode,

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

fn parse_l1_rpc_url(l1_rpc_url: &str) -> Result<String, String> {
    let url: url::Url = l1_rpc_url
        .parse()
        .map_err(|err| format!("failed parsing --l1.rpc-url as URL: {err}"))?;
    if !matches!(url.scheme(), "ws" | "wss") {
        return Err(format!(
            "--l1.rpc-url must use ws:// or wss://, got `{}`",
            url.scheme()
        ));
    }
    Ok(l1_rpc_url.to_owned())
}

fn parse_portal_address(value: &str) -> Result<Address, String> {
    let address = value
        .parse::<Address>()
        .map_err(|err| format!("invalid --l1.portal-address: {err}"))?;
    if address.is_zero() {
        return Err("--l1.portal-address must be nonzero".to_owned());
    }
    Ok(address)
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
    use std::{io::Write as _, path::Path, process::Command, thread, time::Duration};

    use clap::Parser as _;
    use futures::future::{self, BoxFuture};
    use reth_chainspec::EthChainSpec as _;
    use reth_node_core::args::DevArgs;
    use tempo_chainspec::spec::DEV;

    use super::{
        Role, ZoneArgs, load_decryption_keys, load_sequencer_signer, parse_l1_rpc_url,
        parse_portal_address, validate_deprecated_zone_id, validate_node_options,
        validate_p2p_transaction_size_limit, wait_for_exit,
    };
    use reth_ethereum::cli::Cli;
    use zone_chainspec::ZoneChainSpecParser;
    use zone_primitives::constants::zone_chain_id;
    use zone_sequencer::MAX_WITHDRAWAL_BATCH_GAS;

    #[derive(Debug, clap::Parser)]
    struct ZoneArgsParser {
        #[command(flatten)]
        _dev: DevArgs,

        #[command(flatten)]
        zone: ZoneArgs,
    }

    fn validate_node(args: &[&str]) -> eyre::Result<Cli<ZoneChainSpecParser, ZoneArgs>> {
        let mut argv = vec!["tempo-zone", "node"];
        argv.extend_from_slice(args);
        let mut cli = Cli::try_parse_from(argv).unwrap();
        validate_node_options(&mut cli)?;
        Ok(cli)
    }

    #[test]
    fn checker_mode_defaults_to_off() {
        let args = ZoneArgsParser::try_parse_from([
            "tempo-zone",
            "--l1.rpc-url",
            "ws://localhost:8546",
            "--l1.portal-address",
            "0x0000000000000000000000000000000000000001",
        ])
        .unwrap()
        .zone;
        assert_eq!(args.checker_mode, zone_checker::CheckerMode::Off);
    }

    #[test]
    fn checker_mode_observe_parses() {
        let args = ZoneArgsParser::try_parse_from([
            "tempo-zone",
            "--l1.rpc-url",
            "ws://localhost:8546",
            "--l1.portal-address",
            "0x0000000000000000000000000000000000000001",
            "--checker.mode",
            "observe",
        ])
        .unwrap()
        .zone;
        assert_eq!(args.checker_mode, zone_checker::CheckerMode::Observe);
    }

    #[test]
    fn checker_mode_enforce_is_rejected() {
        let result = ZoneArgsParser::try_parse_from([
            "tempo-zone",
            "--l1.rpc-url",
            "ws://localhost:8546",
            "--l1.portal-address",
            "0x0000000000000000000000000000000000000001",
            "--checker.mode",
            "enforce",
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn node_dev_uses_the_standard_reth_flag() {
        let mut parsed =
            Cli::<ZoneChainSpecParser, ZoneArgs>::try_parse_from(["tempo-zone", "node", "--dev"])
                .unwrap();
        assert!(parsed.as_node_command_mut().unwrap().dev.dev);
    }

    #[test]
    fn node_requires_chain_without_dev() {
        let error = Cli::<ZoneChainSpecParser, ZoneArgs>::try_parse_from(["tempo-zone", "node"])
            .unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn node_dev_rejects_external_l1_arguments() {
        let error = Cli::<ZoneChainSpecParser, ZoneArgs>::try_parse_from([
            "tempo-zone",
            "node",
            "--dev",
            "--l1.rpc-url",
            "ws://localhost:8546",
        ])
        .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn node_dev_rejects_a_custom_chain() {
        let mut genesis = DEV.genesis().clone();
        genesis.config.chain_id = zone_chain_id(DEV.chain().id(), 2).unwrap();
        let chain = serde_json::to_string(&genesis).unwrap();
        let error = validate_node(&["--dev", "--chain", &chain]).unwrap_err();
        assert!(error.to_string().contains("--chain must be `dev`"));
    }

    #[test]
    fn node_dev_rejects_unstable_reth_options() {
        for (args, message) in [
            (&["--dev", "--with-unused-ports"][..], "--with-unused-ports"),
            (&["--dev", "--http.port", "0"][..], "--http.port"),
            (
                &["--dev", "--dev.block-max-transactions", "2"][..],
                "--dev.block-max-transactions",
            ),
        ] {
            let error = validate_node(args).unwrap_err();
            assert!(error.to_string().contains(message));
        }
    }

    #[test]
    fn node_dev_adjusts_the_redacted_rpc_port_for_instances() {
        let mut cli = validate_node(&["--dev", "--instance", "2"]).unwrap();
        assert_eq!(
            cli.as_node_command_mut().unwrap().ext.redacted_rpc_port,
            8543
        );
    }

    #[test]
    fn node_dev_rejects_an_rpc_port_collision() {
        let error = validate_node(&[
            "--dev",
            "--http.port",
            "9544",
            "--redacted-rpc.port",
            "9544",
        ])
        .unwrap_err();
        assert!(error.to_string().contains("must be different"));
    }

    #[test]
    fn node_rejects_debug_consensus_options() {
        let cases: &[&[&str]] = &[
            &[
                "--chain",
                "dev",
                "--debug.rpc-consensus-url",
                "ws://localhost:8546",
            ],
            &["--chain", "dev", "--debug.etherscan"],
        ];

        for args in cases {
            let error = validate_node(args).unwrap_err();
            assert!(error.to_string().contains("not supported"));
        }
    }

    #[tokio::test]
    async fn embedded_l1_exit_terminates_the_dev_stack() {
        let l1_exit: BoxFuture<'static, eyre::Result<()>> = Box::pin(async { Ok(()) });
        let error = wait_for_exit(future::pending(), Some(l1_exit))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("exited unexpectedly"));
    }

    #[tokio::test]
    async fn embedded_l1_error_terminates_the_dev_stack() {
        let l1_exit: BoxFuture<'static, eyre::Result<()>> =
            Box::pin(async { Err(eyre::eyre!("test failure")) });
        let error = wait_for_exit(future::pending(), Some(l1_exit))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("embedded Tempo L1 failed"));
    }

    #[test]
    fn portal_address_must_be_nonzero() {
        assert!(parse_portal_address("0x0000000000000000000000000000000000000000").is_err());
        assert!(parse_portal_address("0x1111111111111111111111111111111111111111").is_ok());
    }

    #[test]
    fn deprecated_zone_id_is_accepted_and_validated() {
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
            Some(Path::new("/run/secrets/sequencer-key"))
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
        assert!(parsed.zone.sequencer);
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
        parse_l1_rpc_url("ws://localhost:8546").unwrap();
        parse_l1_rpc_url("wss://rpc.moderato.tempo.xyz").unwrap();
    }

    #[test]
    fn l1_rpc_url_rejects_non_websocket_schemes() {
        assert!(parse_l1_rpc_url("http://localhost:8545").is_err());
        assert!(parse_l1_rpc_url("https://rpc.moderato.tempo.xyz").is_err());
    }
}
