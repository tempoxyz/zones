//! Tempo Zone CLI.

use std::{collections::BTreeMap, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use alloy_eips::NumHash;
use alloy_network::EthereumWallet;
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_signer::Signer as _;
use alloy_signer_local::PrivateKeySigner;
use clap::{Args, CommandFactory, FromArgMatches, ValueEnum};
use commonware_codec::DecodeExt as _;
use openraft::BasicNode;
use reth_chainspec::EthChainSpec as _;
use reth_ethereum::cli::Cli;
use reth_tracing::tracing::{info, warn};
use tempo_alloy::{TempoNetwork, provider::ext::TempoProviderBuilderExt as _};
use tempo_evm::consensus::TempoConsensus;
use zeroize::Zeroizing;
use zone_chainspec::{ZoneChainSpec, ZoneChainSpecParser};
use zone_evm::ZoneEvmConfig;
use zone_l1::state::{L1StateCache, L1StateProvider, L1StateProviderConfig};
use zone_p2p::{
    InterZoneRoutingConfig, InterZoneRoutingPeer, MAX_TRANSACTION_MESSAGE_SIZE, ManifestAddress,
    NextRosterHandoffRoutingConfig, P2pConfig, P2pPeerId, Role, spawn_next_roster_handoff_endpoint,
};
use zone_payload::DEFAULT_WITHDRAWAL_BATCH_INTERVAL_BLOCKS;

use crate::{
    ZoneNode, ZoneProverConfig, ZoneRedactedRpcConfig, ZoneSequencerAddOnsConfig,
    dev::DevCommand,
    fast_drain_adapters::{
        AuthenticatedNextRosterCheckpointSigner, DrainCommonwareEndpoint, DrainCommonwareRoute,
        NextRosterCheckpointInstallConfig,
    },
    fast_runtime::{
        ExpectedCandidateRoster, FastDrainRuntimeConfig, FastExposureRuntimeConfig,
        FastExposureSourceRuntimeConfig, FastReplenishmentRuntimeConfig, FastRuntimeConfig,
        FastServicePeerConfig, FastServiceRouteConfig, FastServiceRuntimeConfig,
        FinalizedFastProofPolicy, load_fast_activation, load_finalized_next_roster,
    },
    fast_service::PeerEndpoint,
    fast_service_adapters::{next_roster_handoff_authority, spawn_next_roster_checkpoint_signer},
    rpc::auth::DEFAULT_MAX_AUTH_TOKEN_VALIDITY_SECS,
};
use zone_checker::{CheckerConfig, CheckerExEx, CheckerMode};
use zone_fast_transfer::{DurableJournal, EpochRoster, ProtocolLimits, admission::ValueCaps};
use zone_primitives::fast_transfer::{QuoteCertificate, ZoneDomain};
use zone_sequencer::{
    BatchAnchorConfig, DEFAULT_MAX_IN_FLIGHT_WITHDRAWAL_BATCHES, DEFAULT_MAX_WITHDRAWAL_BATCH_GAS,
    HardforkProverAddress, MAX_WITHDRAWAL_BATCH_GAS, ProverAddresses, SettlementProofMode,
    WithdrawalBatchLimits,
    fast_replenishment::{
        AlloyReplenishmentProviderHandles, ReplenishmentRouteConfig, ReplenishmentSigningConfig,
    },
};

const MAX_LOGS_PER_RESPONSE: u64 = 1_000_000;
const MAX_BLOCKS_PER_FILTER: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum SettlementProofModeArg {
    /// Two enrolled operators attest; no execution proof is claimed.
    OperatorAttested,
    /// A non-prototype verifier must accept a persisted execution proof.
    ProofRequired,
}

impl From<SettlementProofModeArg> for SettlementProofMode {
    fn from(value: SettlementProofModeArg) -> Self {
        match value {
            SettlementProofModeArg::OperatorAttested => Self::OperatorAttested,
            SettlementProofModeArg::ProofRequired => Self::ProofRequired,
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FastReplenishmentFile {
    routes: Vec<FastReplenishmentRouteFile>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FastReplenishmentRouteFile {
    source_zone_rpc: String,
    tempo_l1_rpc: String,
    destination_zone_rpc: String,
    source_signer_key_file: PathBuf,
    treasury_signer_key_file: PathBuf,
    source_fee_payer_key_file: Option<PathBuf>,
    treasury_fee_payer_key_file: Option<PathBuf>,
    storage_directory: PathBuf,
    poll_interval_ms: u64,
    l1_chain_id: u64,
    source_chain_id: u64,
    destination_chain_id: u64,
    protocol_version: u32,
    source_portal: Address,
    destination_portal: Address,
    source_fast_transfer: Address,
    destination_fast_transfer: Address,
    l1_token: Address,
    source_token: Address,
    destination_token: Address,
    source_inventory: Address,
    source_fallback: Address,
    source_fee_payer: Address,
    treasury_fee_payer: Address,
    source_fee_token: Address,
    treasury_fee_token: Address,
    minimum_source_fee_reserve: U256,
    minimum_treasury_fee_reserve: U256,
    maximum_replenishment_amount: U256,
    maximum_source_withdrawal_fee: U256,
    maximum_destination_deposit_fee: U256,
    treasury: Address,
    destination_pool_operator: Address,
    expected_source_fast_epoch: u64,
    expected_destination_fast_epoch: u64,
    expected_token_decimals: u8,
    expected_destination_key_index: U256,
    expected_destination_key_x: alloy_primitives::B256,
    expected_destination_key_y_parity: u8,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FastInterZoneFile {
    l1_chain_id: u64,
    listen: SocketAddr,
    bypass_ip_check: bool,
    peers: Vec<FastInterZonePeerFile>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FastInterZonePeerFile {
    zone_id: u32,
    ed25519_public_key: String,
    endpoint: String,
}

/// Runs only the authenticated pre-activation checkpoint receiver for one disjoint next-roster
/// member. It never starts the old roster's Raft or Zone RPC services.
#[derive(Debug, clap::Parser)]
#[command(
    name = "next-roster-handoff",
    about = "Install and acknowledge a finalized T14 next-roster checkpoint"
)]
pub struct NextRosterHandoffCommand {
    /// Zone genesis whose chain/fork configuration must match the finalized handoff authority.
    #[arg(long, value_name = "GENESIS_JSON")]
    chain: PathBuf,

    /// Strict routing, key, storage, and finalized-anchor configuration.
    #[arg(long, value_name = "CONFIG_JSON")]
    config: PathBuf,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NextRosterHandoffFile {
    tempo_l1_rpc_endpoint: String,
    portal: Address,
    imported_anchor_number: u64,
    imported_anchor_hash: B256,
    listen: SocketAddr,
    bypass_ip_check: bool,
    ed25519_key_file: PathBuf,
    secp256k1_key_file: PathBuf,
    storage_directory: PathBuf,
    expected_candidate_roster: CandidateRosterFile,
    old_peers: Vec<FastServicePeerFile>,
    next_peers: Vec<FastServicePeerFile>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FastServiceFile {
    l1_chain_id: u64,
    native_rpc_endpoint: String,
    operator_signer_key_file: PathBuf,
    native_chain_id: u64,
    fee_token: Address,
    commit_timeout_ms: u64,
    drain_tempo_l1_rpc_endpoint: String,
    drain_factory: Address,
    drain_fee_token: Address,
    drain_journal_directory: PathBuf,
    drain_response_timeout_ms: u64,
    drain_retry_interval_ms: u64,
    exposure_tempo_l1_rpc_endpoint: String,
    exposure_poll_interval_ms: u64,
    exposure_rpc_timeout_ms: u64,
    exposure_sources: Vec<FastExposureSourceFile>,
    response_timeout_ms: u64,
    health_max_age_ms: u64,
    reserved_terminal_bytes: usize,
    limits: FastServiceLimitsFile,
    peers: Vec<FastServicePeerFile>,
    expected_candidate_roster: Option<CandidateRosterFile>,
    next_roster_peers: Vec<FastServicePeerFile>,
    routes: Vec<FastServiceRouteFile>,
}

#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateRosterFile {
    l1_chain_id: u64,
    zone_id: u32,
    chain_id: u64,
    portal: Address,
    authority_epoch: u64,
    roster_hash: B256,
    protocol_version: u16,
    threshold: u8,
    proof_mode: u8,
    expected_verifier_code_hash: B256,
    expected_verifier_config_hash: B256,
    members: [Address; 3],
    peer_portals: [Address; 9],
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FastExposureSourceFile {
    zone_id: u32,
    chain_id: u64,
    portal: Address,
    rpc_endpoint: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FastServiceLimitsFile {
    unresolved_zone: usize,
    unresolved_route: usize,
    unresolved_sender: usize,
    queued_zone_bytes: usize,
    queued_peer_bytes: usize,
    journal_bytes: usize,
    verification_concurrency_per_peer: usize,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FastServicePeerFile {
    zone_id: u32,
    certificate_member: Address,
    ed25519_public_key: String,
    endpoint: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FastServiceRouteFile {
    zone_id: u32,
    chain_id: u64,
    portal: Address,
    remote_quote: String,
    local_quote: String,
    outgoing_route_cap: U256,
    outgoing_sender_cap: U256,
    incoming_route_cap: U256,
    incoming_sender_cap: U256,
}

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
    NextRosterHandoff(Box<NextRosterHandoffCommand>),
}

impl ZoneCli {
    fn command() -> clap::Command {
        Cli::<ZoneChainSpecParser, ZoneArgs>::command()
            .about("Tempo Zone")
            .subcommand(DevCommand::command())
            .subcommand(NextRosterHandoffCommand::command())
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
        if let Some(("next-roster-handoff", handoff_matches)) = matches.subcommand() {
            return NextRosterHandoffCommand::from_arg_matches(handoff_matches)
                .map(Box::new)
                .map(Self::NextRosterHandoff);
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
            Self::NextRosterHandoff(command) => (*command).run(),
        }
    }
}

impl NextRosterHandoffCommand {
    pub fn run(self) -> eyre::Result<()> {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(run_next_roster_handoff(self))
    }
}

async fn run_next_roster_handoff(command: NextRosterHandoffCommand) -> eyre::Result<()> {
    let genesis_bytes = std::fs::read(&command.chain)
        .map_err(|error| eyre::eyre!("failed to read {}: {error}", command.chain.display()))?;
    let genesis: alloy_genesis::Genesis = serde_json::from_slice(&genesis_bytes)
        .map_err(|error| eyre::eyre!("invalid {}: {error}", command.chain.display()))?;
    let chain_spec = ZoneChainSpec::from_genesis(genesis)?;
    let zone_id = chain_spec.zone_id();
    let zone_chain_id = chain_spec.chain_id();

    let config_bytes = std::fs::read(&command.config)
        .map_err(|error| eyre::eyre!("failed to read {}: {error}", command.config.display()))?;
    let config: NextRosterHandoffFile = serde_json::from_slice(&config_bytes)
        .map_err(|error| eyre::eyre!("invalid {}: {error}", command.config.display()))?;
    eyre::ensure!(
        !config.tempo_l1_rpc_endpoint.is_empty()
            && !config.portal.is_zero()
            && config.imported_anchor_number != 0
            && !config.imported_anchor_hash.is_zero()
            && !config.storage_directory.as_os_str().is_empty()
            && config.old_peers.len() == 3
            && config.next_peers.len() == 3,
        "next-roster handoff configuration is incomplete"
    );
    let imported_anchor = NumHash::new(config.imported_anchor_number, config.imported_anchor_hash);
    let l1 = ProviderBuilder::new_with_network::<TempoNetwork>()
        .connect(&config.tempo_l1_rpc_endpoint)
        .await?
        .erased();
    let loaded = load_fast_activation(
        &l1,
        config.portal,
        &chain_spec,
        imported_anchor,
        zone_id,
        zone_chain_id,
    )
    .await?
    .ok_or_else(|| eyre::eyre!("T14 is not active at the configured finalized anchor"))?;
    eyre::ensure!(
        loaded.drain.closed && !loaded.drain.retired,
        "next-roster handoff requires a closed, unretired finalized old epoch"
    );
    let epoch = loaded.activation.epoch();
    let protocol_version = u16::try_from(epoch.protocol_version)
        .map_err(|_| eyre::eyre!("finalized protocol version does not fit the wire domain"))?;
    let old_roster = EpochRoster::from_finalized_registry(
        ZoneDomain {
            l1_chain_id: epoch.l1_chain_id,
            zone_id: epoch.zone_id,
            chain_id: epoch.zone_chain_id,
            portal: epoch.portal,
            authority_epoch: epoch.epoch,
            roster_hash: epoch.roster_hash,
            protocol_version,
        },
        epoch.members,
    )
    .map_err(|error| eyre::eyre!("invalid finalized old roster: {error}"))?;
    let candidate = decode_candidate_roster(config.expected_candidate_roster)?;
    eyre::ensure!(
        candidate.peer_portals == epoch.peer_portals,
        "candidate successor peer graph differs from the finalized old ten-Zone graph"
    );
    let next_roster = load_finalized_next_roster(&old_roster, loaded.drain, Some(&candidate))?
        .ok_or_else(|| eyre::eyre!("expected candidate roster is unavailable"))?;

    let (old_endpoints, mut routing_peers) =
        decode_handoff_endpoints(config.old_peers, &old_roster)?;
    let (next_endpoints, next_routing_peers) =
        decode_handoff_endpoints(config.next_peers, &next_roster)?;
    let next_membership = next_endpoints
        .iter()
        .enumerate()
        .map(|(index, endpoint)| {
            (
                u64::try_from(index + 1).expect("three members fit u64"),
                BasicNode::new(endpoint.ed25519.to_string()),
            )
        })
        .collect();
    routing_peers.extend(next_routing_peers);
    let next_route = DrainCommonwareRoute {
        zone_id,
        portal: config.portal,
        endpoints: next_endpoints.map(|endpoint| DrainCommonwareEndpoint {
            member: endpoint.member,
            identity: endpoint.ed25519,
        }),
    };
    let authority =
        next_roster_handoff_authority(&old_roster, &old_endpoints, &next_roster, &next_route)
            .map_err(|error| eyre::eyre!("invalid finalized handoff authority: {error}"))?;
    let routing = NextRosterHandoffRoutingConfig {
        l1_chain_id: epoch.l1_chain_id,
        local_zone_id: zone_id,
        listen: config.listen,
        bypass_ip_check: config.bypass_ip_check,
        peers: routing_peers,
    };
    let signer = load_private_key_signer(&config.secp256k1_key_file, "next-roster signer").await?;
    eyre::ensure!(
        next_roster.members.contains(&signer.address()),
        "next-roster signer is absent from the finalized successor roster"
    );
    let local_next_member = signer.address();
    std::fs::create_dir_all(&config.storage_directory)?;
    let signing_journal = Arc::new(DurableJournal::open(
        config.storage_directory.join("handoff-signing-journal"),
    )?);
    let handler = Arc::new(AuthenticatedNextRosterCheckpointSigner::new_installing(
        NextRosterCheckpointInstallConfig {
            storage_directory: config.storage_directory.clone(),
            old_roster,
            next_roster,
            local_next_member,
            expected_imported_anchor_number: imported_anchor.number,
            expected_imported_anchor_hash: imported_anchor.hash,
            expected_final_zone_height: loaded.drain.final_settlement_height,
            expected_final_block_hash: loaded.drain.final_settlement_block_hash,
            expected_final_withdrawal_batch_index: loaded
                .drain
                .final_settlement_withdrawal_batch_index,
            expected_final_settlement_hash: loaded.drain.final_settlement_hash,
            next_membership,
        },
        signer,
        signing_journal,
    )?);
    let mut endpoint = spawn_next_roster_handoff_endpoint(
        routing,
        &config.ed25519_key_file,
        &config.secp256k1_key_file,
        Some(config.storage_directory.join("handoff-commonware")),
        authority.clone(),
    )?;
    let ports = endpoint
        .take_ports()
        .ok_or_else(|| eyre::eyre!("next-roster handoff carrier did not expose service ports"))?;
    let stop = tokio_util::sync::CancellationToken::new();
    let mut worker = spawn_next_roster_checkpoint_signer(ports, handler, stop.clone()).await?;
    info!(target: "zone::fast", zone_id, member = %local_next_member, "next-roster checkpoint handoff endpoint ready");
    tokio::select! {
        signal = tokio::signal::ctrl_c() => {
            signal?;
            stop.cancel();
            (&mut worker).await?;
            endpoint.shutdown().await
        }
        result = endpoint.wait_for_exit() => {
            stop.cancel();
            (&mut worker).await?;
            result
        }
        result = &mut worker => {
            result?;
            endpoint.shutdown().await?;
            Err(eyre::eyre!("next-roster checkpoint signer stopped unexpectedly"))
        }
    }
}

fn decode_handoff_endpoints(
    configured: Vec<FastServicePeerFile>,
    roster: &EpochRoster,
) -> eyre::Result<([PeerEndpoint; 3], Vec<InterZoneRoutingPeer>)> {
    let mut configured = configured
        .into_iter()
        .map(|peer| (peer.certificate_member, peer))
        .collect::<BTreeMap<_, _>>();
    eyre::ensure!(
        configured.len() == 3,
        "handoff peers contain duplicate members"
    );
    let mut endpoints = Vec::with_capacity(3);
    let mut routing = Vec::with_capacity(3);
    for member in roster.members {
        let peer = configured
            .remove(&member)
            .ok_or_else(|| eyre::eyre!("handoff endpoint is missing finalized member {member}"))?;
        eyre::ensure!(
            peer.zone_id == roster.domain.zone_id,
            "handoff peer Zone does not match its finalized roster"
        );
        let encoded = const_hex::decode(&peer.ed25519_public_key)
            .map_err(|error| eyre::eyre!("invalid handoff Ed25519 key: {error}"))?;
        let ed25519 = P2pPeerId::decode(&encoded[..])
            .map_err(|error| eyre::eyre!("invalid handoff Ed25519 identity: {error}"))?;
        let address = peer
            .endpoint
            .parse::<ManifestAddress>()
            .map_err(|error| eyre::eyre!("invalid handoff endpoint: {error}"))?;
        routing.push(InterZoneRoutingPeer {
            zone_id: peer.zone_id,
            ed25519: ed25519.clone(),
            endpoint: address,
        });
        endpoints.push(PeerEndpoint {
            member,
            ed25519,
            endpoint: peer.endpoint,
        });
    }
    let endpoints: [PeerEndpoint; 3] = endpoints
        .try_into()
        .map_err(|_| eyre::eyre!("handoff roster must contain exactly three members"))?;
    Ok((endpoints, routing))
}

fn decode_candidate_roster(file: CandidateRosterFile) -> eyre::Result<ExpectedCandidateRoster> {
    let roster = EpochRoster::from_finalized_registry(
        ZoneDomain {
            l1_chain_id: file.l1_chain_id,
            zone_id: file.zone_id,
            chain_id: file.chain_id,
            portal: file.portal,
            authority_epoch: file.authority_epoch,
            roster_hash: file.roster_hash,
            protocol_version: file.protocol_version,
        },
        file.members,
    )
    .map_err(|error| eyre::eyre!("invalid expected candidate roster: {error}"))?;
    Ok(ExpectedCandidateRoster {
        roster,
        threshold: file.threshold,
        proof_policy: FinalizedFastProofPolicy {
            mode: file.proof_mode,
            expected_verifier_code_hash: file.expected_verifier_code_hash,
            expected_verifier_config_hash: file.expected_verifier_config_hash,
        },
        peer_portals: file.peer_portals,
    })
}

/// Main entry point for the `node` command.
fn run_node(mut cli: Cli<ZoneChainSpecParser, ZoneArgs>) -> eyre::Result<()> {
    prepend_log_filter(&mut cli.logs.log_stdout_filter, ZONE_LOG_FILTER_DIRECTIVES);
    prepend_log_filter(&mut cli.logs.log_file_filter, ZONE_LOG_FILTER_DIRECTIVES);

    let l1_rpc_url = match std::env::var("L1_HTTP_RPC_URL") {
        Ok(url) if !url.is_empty() => {
            let url = url
                .parse()
                .map_err(|error| eyre::eyre!("invalid L1_HTTP_RPC_URL: {error}"))?;
            Some(url)
        }
        Ok(_) | Err(std::env::VarError::NotPresent) => None,
        Err(error) => return Err(eyre::eyre!("invalid L1_HTTP_RPC_URL: {error}")),
    };

    let components = move |spec: Arc<ZoneChainSpec>| {
        let evm_config = cli_evm_config(spec.clone(), l1_rpc_url.clone());
        (
            evm_config,
            TempoConsensus::new(spec).with_allow_equal_timestamps(true),
        )
    };

    cli.run_with_components::<ZoneNode>(components, async move |mut builder, args| {
        info!(target: "reth::cli", "Launching Tempo Zone node");

        if args.block_interval_ms.is_some() {
            warn!(target: "reth::cli", "--block.interval-ms is deprecated, has no effect, and will be removed in the next release");
        }

        let zone_id = builder.config().chain.zone_id();
        validate_deprecated_zone_id(args.zone_id, zone_id)?;

        let manifest_mode = args.sequencer_manifest.is_some();
        validate_p2p_transaction_size_limit(
            manifest_mode,
            builder.config().txpool.max_tx_input_bytes,
        )?;
        if manifest_mode || args.enable_sequencer {
            // Settlement and replication only consume durable blocks. Persist every block
            // immediately so they do not wait for Reth's in-memory buffer to fill.
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
            args.l1_rpc_url.clone(),
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

        let legacy_settlement_store = builder.config().datadir().data_dir().join("settlement");
        node = configure_sequencing(&args, zone_id, node, legacy_settlement_store).await?;

        // Install or skip the checker ExEx based on the configured mode.
        match args.checker_mode {
            CheckerMode::Off => {
                let handle = builder
                    .node(node)
                    .launch_with_debug_capabilities()
                    .await?;
                handle.wait_for_node_exit().await
            }
            CheckerMode::Observe => {
                info!(target: "reth::cli", "Checker ExEx enabled (observe mode)");
                let node = node.with_portal_evidence_retention();
                let checker = CheckerExEx::new(CheckerConfig {
                    l1_rpc_url: args.l1_rpc_url.clone(),
                    portal_address: args.portal_address,
                    zone_id,
                    zone_chain_id: builder.config().chain.chain_id(),
                    database_path: builder.config().datadir().data_dir().join("checker"),
                    l1_block_tracker: node.l1_block_tracker(),
                });
                builder
                    .node(node)
                    .install_exex("zone-checker", async move |ctx| Ok(checker.run(ctx)))
                    .launch_with_debug_capabilities()
                    .await?
                    .wait_for_node_exit()
                    .await
            }
        }
    })
}

/// Creates the EVM config used by CLI subcommands, deriving the portal from the chain's zone ID.
fn cli_evm_config(chain_spec: Arc<ZoneChainSpec>, l1_rpc_url: Option<url::Url>) -> ZoneEvmConfig {
    let Some(l1_rpc_url) = l1_rpc_url else {
        return ZoneEvmConfig::new_without_l1(chain_spec);
    };

    let portal_address = tempo_precompiles::zone_factory::portal_address(chain_spec.zone_id());
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
    legacy_settlement_store: PathBuf,
) -> eyre::Result<ZoneNode> {
    let mut p2p_config =
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
    let mut inter_zone_routing = None;
    if let Some(path) = &args.fast_inter_zone_config {
        let config = p2p_config
            .take()
            .ok_or_else(|| eyre::eyre!("--fast.inter-zone-config requires --sequencer.manifest"))?;
        let routing = load_fast_inter_zone_routing(path, zone_id)?;
        inter_zone_routing = Some(routing.clone());
        p2p_config = Some(config.with_inter_zone(routing)?);
    }
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
    let should_sequence_blocks = args.enable_sequencer
        || p2p_config
            .as_ref()
            .is_some_and(|config| !config.is_rpc_only());
    if rpc_only && args.sequencer_key_file.is_some() {
        return Err(eyre::eyre!(
            "this node is `rpc_only` in the manifest, so --sequencer-key-file must not be provided: the shared key is never used here and is also the zone ECIES private key for encrypted deposits"
        ));
    }
    eyre::ensure!(
        args.shadow_prover_pcrs.is_none() || (rpc_only && !should_sequence_blocks),
        "--shadow-prover.pcrs requires an rpc_only follower; it is not a settlement policy"
    );
    let policy = args
        .prover_attestation_policy
        .as_deref()
        .map(std::fs::read)
        .transpose()?;
    let prover_addresses = ProverAddresses::new(args.prover_addresses.clone(), policy.as_deref())?;
    let prover_config = if !args.enable_prover {
        None
    } else if should_sequence_blocks {
        let addresses = prover_addresses.ok_or_else(|| {
            eyre::eyre!(
                "settlement proving requires --sequencer.prover-address for Nitro attestation"
            )
        })?;
        Some(ZoneProverConfig::Settlement(addresses))
    } else {
        eyre::ensure!(
            rpc_only,
            "--sequencer.enable-prover requires a sequencer or an rpc_only P2P follower"
        );
        Some(ZoneProverConfig::Shadow {
            prover_addresses,
            proof_verifier: args.shadow_prover_pcrs.clone(),
        })
    };

    if should_sequence_blocks {
        let fast_requested = args.fast_storage.is_some()
            || args.fast_service_config.is_some()
            || args.fast_inter_zone_config.is_some()
            || args.fast_replenishment_config.is_some();
        let (settlement_proof_mode, settlement_store_path) = settlement_runtime_policy(
            fast_requested,
            args.enable_prover,
            args.settlement_proof_mode,
            args.settlement_store.clone(),
            legacy_settlement_store,
        )?;
        eyre::ensure!(
            !settlement_store_path.as_os_str().is_empty(),
            "--sequencer.settlement-store must not be empty"
        );
        let sequencer_signer = load_sequencer_signer(args.sequencer_key_file.as_deref()).await?;
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
            settlement_proof_mode: settlement_proof_mode.into(),
            settlement_store_path,
        });
    }
    if let Some(config) = prover_config {
        node = node.with_prover(config);
    }
    if args.fast_storage.is_some() {
        let p2p = p2p_config.as_ref().ok_or_else(|| {
            eyre::eyre!("T14 fast runtime requires --sequencer.manifest and its authenticated individual secp256k1 identity")
        })?;
        let signer = p2p.block_attestation_signer().ok_or_else(|| {
            eyre::eyre!("T14 fast runtime requires a local individual --secp256k1.key")
        })?;
        let member_transports = p2p
            .block_attestation_addresses()
            .into_iter()
            .map(|(transport, member)| (member, transport))
            .collect::<BTreeMap<_, _>>();
        eyre::ensure!(
            member_transports.len() == 3,
            "T14 fast runtime requires exactly three authenticated quorum members in --sequencer.manifest"
        );
        let replenishment_routes = match &args.fast_replenishment_config {
            Some(path) => load_fast_replenishment_routes(path).await?,
            None => Vec::new(),
        };
        let service = match &args.fast_service_config {
            Some(path) => Some(load_fast_service(path).await?),
            None => None,
        };
        eyre::ensure!(
            service.is_some() == args.fast_inter_zone_config.is_some(),
            "--fast.service-config and --fast.inter-zone-config must be configured together"
        );
        eyre::ensure!(
            service.is_some(),
            "T14 fast runtime requires --fast.service-config and --fast.inter-zone-config"
        );
        if let (Some(service), Some(routing)) = (&service, &inter_zone_routing) {
            validate_service_routing_bindings(service, routing)?;
        }
        node = node.with_fast_runtime(FastRuntimeConfig {
            signer,
            local_transport: p2p.ed25519_public_key(),
            member_transports,
            storage: args.fast_storage.clone().expect("checked above"),
            rpc_timeout: Duration::from_millis(args.fast_rpc_timeout_ms),
            service,
            replenishment_routes,
        });
    }
    if let Some(config) = p2p_config {
        node = node.with_p2p(config);
    }
    Ok(node)
}

fn settlement_runtime_policy(
    fast_requested: bool,
    prover_enabled: bool,
    configured_mode: Option<SettlementProofModeArg>,
    configured_store: Option<PathBuf>,
    legacy_store: PathBuf,
) -> eyre::Result<(SettlementProofModeArg, PathBuf)> {
    let mode = match configured_mode {
        Some(mode) => mode,
        None if !fast_requested && prover_enabled => SettlementProofModeArg::ProofRequired,
        None if !fast_requested => SettlementProofModeArg::OperatorAttested,
        None => {
            return Err(eyre::eyre!(
                "T14 fast sequencing requires explicit --sequencer.settlement-proof-mode"
            ));
        }
    };
    let store = match configured_store {
        Some(path) => path,
        None if !fast_requested => legacy_store,
        None => {
            return Err(eyre::eyre!(
                "T14 fast sequencing requires explicit --sequencer.settlement-store"
            ));
        }
    };
    Ok((mode, store))
}

async fn load_fast_service(path: &std::path::Path) -> eyre::Result<FastServiceRuntimeConfig> {
    let bytes = std::fs::read(path)
        .map_err(|error| eyre::eyre!("failed to read {}: {error}", path.display()))?;
    let file: FastServiceFile = serde_json::from_slice(&bytes)
        .map_err(|error| eyre::eyre!("invalid {}: {error}", path.display()))?;
    eyre::ensure!(
        file.l1_chain_id != 0
            && file.native_chain_id != 0
            && !file.native_rpc_endpoint.is_empty()
            && !file.fee_token.is_zero()
            && file.commit_timeout_ms != 0
            && !file.drain_tempo_l1_rpc_endpoint.is_empty()
            && !file.drain_factory.is_zero()
            && !file.drain_fee_token.is_zero()
            && !file.drain_journal_directory.as_os_str().is_empty()
            && file.drain_response_timeout_ms != 0
            && file.drain_retry_interval_ms != 0
            && !file.exposure_tempo_l1_rpc_endpoint.is_empty()
            && file.exposure_poll_interval_ms != 0
            && file.exposure_rpc_timeout_ms != 0
            && file.exposure_sources.len() == 9
            && file.response_timeout_ms != 0
            && file.health_max_age_ms != 0
            && file.peers.len() == 30
            && matches!(file.next_roster_peers.len(), 0 | 3)
            && file.expected_candidate_roster.is_some() == (file.next_roster_peers.len() == 3)
            && file.routes.len() == 9,
        "fast service requires explicit nonzero provider settings, thirty peers, and nine routes"
    );
    let operator_signer = load_private_key_signer(
        &file.operator_signer_key_file,
        "fast service operator signer",
    )
    .await?;
    let decode_peer = |peer: FastServicePeerFile| {
        eyre::ensure!(
            peer.zone_id != 0 && !peer.certificate_member.is_zero() && !peer.endpoint.is_empty(),
            "fast service peer fields must be explicit and nonzero"
        );
        let encoded = const_hex::decode(&peer.ed25519_public_key).map_err(|error| {
            eyre::eyre!(
                "invalid fast service Ed25519 key for Zone {}: {error}",
                peer.zone_id
            )
        })?;
        let ed25519 = P2pPeerId::decode(&encoded[..]).map_err(|error| {
            eyre::eyre!(
                "invalid fast service Ed25519 key for Zone {}: {error}",
                peer.zone_id
            )
        })?;
        let endpoint = peer.endpoint.parse::<ManifestAddress>().map_err(|error| {
            eyre::eyre!(
                "invalid fast service endpoint for Zone {}: {error}",
                peer.zone_id
            )
        })?;
        Ok(FastServicePeerConfig {
            zone_id: peer.zone_id,
            certificate_member: peer.certificate_member,
            ed25519,
            endpoint: endpoint.to_string(),
        })
    };
    let peers = file
        .peers
        .into_iter()
        .map(&decode_peer)
        .collect::<eyre::Result<Vec<_>>>()?;
    let expected_candidate_roster = file
        .expected_candidate_roster
        .map(decode_candidate_roster)
        .transpose()?;
    let next_roster_peers = file
        .next_roster_peers
        .into_iter()
        .map(decode_peer)
        .collect::<eyre::Result<Vec<_>>>()?;
    let routes = file
        .routes
        .into_iter()
        .map(|route| {
            eyre::ensure!(
                route.zone_id != 0
                    && route.chain_id != 0
                    && !route.portal.is_zero()
                    && route.outgoing_route_cap != U256::ZERO
                    && route.outgoing_sender_cap != U256::ZERO
                    && route.incoming_route_cap != U256::ZERO
                    && route.incoming_sender_cap != U256::ZERO,
                "fast service route fields and caps must be explicit and nonzero"
            );
            let remote_quote = decode_quote(&route.remote_quote, route.zone_id, "remote")?;
            let local_quote = decode_quote(&route.local_quote, route.zone_id, "local")?;
            Ok(FastServiceRouteConfig {
                zone_id: route.zone_id,
                chain_id: route.chain_id,
                portal: route.portal,
                remote_quote,
                local_quote,
                outgoing_caps: ValueCaps {
                    route: route.outgoing_route_cap,
                    sender: route.outgoing_sender_cap,
                },
                incoming_caps: ValueCaps {
                    route: route.incoming_route_cap,
                    sender: route.incoming_sender_cap,
                },
            })
        })
        .collect::<eyre::Result<Vec<_>>>()?;
    Ok(FastServiceRuntimeConfig {
        l1_chain_id: file.l1_chain_id,
        peers,
        expected_candidate_roster,
        next_roster_peers,
        routes,
        limits: ProtocolLimits {
            unresolved_zone: file.limits.unresolved_zone,
            unresolved_route: file.limits.unresolved_route,
            unresolved_sender: file.limits.unresolved_sender,
            queued_zone_bytes: file.limits.queued_zone_bytes,
            queued_peer_bytes: file.limits.queued_peer_bytes,
            journal_bytes: file.limits.journal_bytes,
            verification_concurrency_per_peer: file.limits.verification_concurrency_per_peer,
        },
        reserved_terminal_bytes: file.reserved_terminal_bytes,
        health_max_age: Duration::from_millis(file.health_max_age_ms),
        response_timeout: Duration::from_millis(file.response_timeout_ms),
        native_rpc_endpoint: file.native_rpc_endpoint,
        operator_signer,
        native_chain_id: file.native_chain_id,
        fee_token: file.fee_token,
        commit_timeout: Duration::from_millis(file.commit_timeout_ms),
        drain: FastDrainRuntimeConfig {
            tempo_l1_rpc_endpoint: file.drain_tempo_l1_rpc_endpoint,
            factory: file.drain_factory,
            fee_token: file.drain_fee_token,
            journal_directory: file.drain_journal_directory,
            response_timeout: Duration::from_millis(file.drain_response_timeout_ms),
            retry_interval: Duration::from_millis(file.drain_retry_interval_ms),
        },
        exposure: FastExposureRuntimeConfig {
            tempo_l1_rpc_endpoint: file.exposure_tempo_l1_rpc_endpoint,
            sources: file
                .exposure_sources
                .into_iter()
                .map(|source| FastExposureSourceRuntimeConfig {
                    zone_id: source.zone_id,
                    chain_id: source.chain_id,
                    portal: source.portal,
                    rpc_endpoint: source.rpc_endpoint,
                })
                .collect(),
            poll_interval: Duration::from_millis(file.exposure_poll_interval_ms),
            rpc_timeout: Duration::from_millis(file.exposure_rpc_timeout_ms),
        },
    })
}

fn validate_service_routing_bindings(
    service: &FastServiceRuntimeConfig,
    routing: &InterZoneRoutingConfig,
) -> eyre::Result<()> {
    eyre::ensure!(
        service.l1_chain_id == routing.l1_chain_id,
        "C4 service and Commonware routing use different L1 chain IDs"
    );
    let routed = routing
        .peers
        .iter()
        .map(|peer| {
            (
                peer.ed25519.clone(),
                (peer.zone_id, peer.endpoint.to_string()),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let configured = service
        .peers
        .iter()
        .chain(&service.next_roster_peers)
        .map(|peer| (peer.ed25519.clone(), (peer.zone_id, peer.endpoint.clone())))
        .collect::<BTreeMap<_, _>>();
    eyre::ensure!(
        configured == routed && matches!(configured.len(), 30 | 33),
        "C4/C5 peer identities/endpoints must exactly match all Commonware routes"
    );
    Ok(())
}

fn decode_quote(encoded: &str, zone_id: u32, direction: &str) -> eyre::Result<QuoteCertificate> {
    let bytes = const_hex::decode(encoded).map_err(|error| {
        eyre::eyre!("invalid {direction} quote hex for Zone {zone_id}: {error}")
    })?;
    QuoteCertificate::decode(&bytes).map_err(|error| {
        eyre::eyre!("invalid canonical {direction} quote for Zone {zone_id}: {error}")
    })
}

async fn load_fast_replenishment_routes(
    path: &std::path::Path,
) -> eyre::Result<Vec<FastReplenishmentRuntimeConfig>> {
    let bytes = std::fs::read(path)
        .map_err(|error| eyre::eyre!("failed to read {}: {error}", path.display()))?;
    let file: FastReplenishmentFile = serde_json::from_slice(&bytes)
        .map_err(|error| eyre::eyre!("invalid {}: {error}", path.display()))?;
    eyre::ensure!(
        !file.routes.is_empty(),
        "{} must contain at least one replenishment route",
        path.display()
    );
    let mut routes = Vec::with_capacity(file.routes.len());
    for entry in file.routes {
        eyre::ensure!(
            !entry.source_zone_rpc.is_empty()
                && !entry.tempo_l1_rpc.is_empty()
                && !entry.destination_zone_rpc.is_empty()
                && entry.poll_interval_ms > 0
                && !entry.storage_directory.as_os_str().is_empty(),
            "replenishment endpoints, storage, and poll interval must be explicit and nonzero"
        );
        let source_signer =
            load_private_key_signer(&entry.source_signer_key_file, "replenishment source signer")
                .await?;
        let treasury_signer = load_private_key_signer(
            &entry.treasury_signer_key_file,
            "replenishment treasury signer",
        )
        .await?;
        eyre::ensure!(
            source_signer.address() == entry.source_inventory,
            "replenishment source signer does not match source_inventory"
        );
        eyre::ensure!(
            treasury_signer.address() == entry.treasury,
            "replenishment treasury signer does not match treasury"
        );
        validate_fee_payer_signer(
            entry.source_fee_payer_key_file.as_deref(),
            entry.source_fee_payer,
            &source_signer,
            "source fee payer",
        )
        .await?;
        validate_fee_payer_signer(
            entry.treasury_fee_payer_key_file.as_deref(),
            entry.treasury_fee_payer,
            &treasury_signer,
            "treasury fee payer",
        )
        .await?;
        // The concrete provider currently submits locally signed Tempo requests. Until the C6
        // provider attaches a distinct sponsor signature, fail closed instead of silently charging
        // the sender while canonical reconciliation expects another payer.
        eyre::ensure!(
            entry.source_fee_payer == source_signer.address()
                && entry.treasury_fee_payer == treasury_signer.address(),
            "distinct replenishment fee payers require provider-backed Tempo sponsor signatures"
        );

        let source_zone = ProviderBuilder::new_with_network::<TempoNetwork>()
            .with_nonce_key_filler()
            .wallet(EthereumWallet::new(source_signer.clone()))
            .connect(&entry.source_zone_rpc)
            .await?
            .erased();
        let tempo_l1_treasury = ProviderBuilder::new_with_network::<TempoNetwork>()
            .with_nonce_key_filler()
            .wallet(EthereumWallet::new(treasury_signer.clone()))
            .connect(&entry.tempo_l1_rpc)
            .await?
            .erased();
        let destination_zone = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&entry.destination_zone_rpc)
            .await?
            .erased();
        let route = ReplenishmentRouteConfig {
            l1_chain_id: entry.l1_chain_id,
            source_chain_id: entry.source_chain_id,
            destination_chain_id: entry.destination_chain_id,
            protocol_version: entry.protocol_version,
            source_portal: entry.source_portal,
            destination_portal: entry.destination_portal,
            source_fast_transfer: entry.source_fast_transfer,
            destination_fast_transfer: entry.destination_fast_transfer,
            l1_token: entry.l1_token,
            source_token: entry.source_token,
            destination_token: entry.destination_token,
            source_inventory: entry.source_inventory,
            source_fallback: entry.source_fallback,
            source_fee_payer: entry.source_fee_payer,
            treasury_fee_payer: entry.treasury_fee_payer,
            source_fee_token: entry.source_fee_token,
            treasury_fee_token: entry.treasury_fee_token,
            minimum_source_fee_reserve: entry.minimum_source_fee_reserve,
            minimum_treasury_fee_reserve: entry.minimum_treasury_fee_reserve,
            maximum_replenishment_amount: entry.maximum_replenishment_amount,
            maximum_source_withdrawal_fee: entry.maximum_source_withdrawal_fee,
            maximum_destination_deposit_fee: entry.maximum_destination_deposit_fee,
            treasury: entry.treasury,
            destination_pool_operator: entry.destination_pool_operator,
        };
        let providers = AlloyReplenishmentProviderHandles::new(
            source_zone,
            tempo_l1_treasury,
            destination_zone,
            ReplenishmentSigningConfig {
                sender: source_signer.address(),
                effective_fee_payer: entry.source_fee_payer,
                fee_token: entry.source_fee_token,
            },
            ReplenishmentSigningConfig {
                sender: treasury_signer.address(),
                effective_fee_payer: entry.treasury_fee_payer,
                fee_token: entry.treasury_fee_token,
            },
        );
        routes.push(FastReplenishmentRuntimeConfig {
            route,
            providers,
            expected_source_fast_epoch: entry.expected_source_fast_epoch,
            expected_destination_fast_epoch: entry.expected_destination_fast_epoch,
            expected_token_decimals: entry.expected_token_decimals,
            expected_destination_key_index: entry.expected_destination_key_index,
            expected_destination_key_x: entry.expected_destination_key_x,
            expected_destination_key_y_parity: entry.expected_destination_key_y_parity,
            storage: entry.storage_directory,
            poll_interval: Duration::from_millis(entry.poll_interval_ms),
        });
    }
    Ok(routes)
}

fn load_fast_inter_zone_routing(
    path: &std::path::Path,
    local_zone_id: u32,
) -> eyre::Result<InterZoneRoutingConfig> {
    let bytes = std::fs::read(path)
        .map_err(|error| eyre::eyre!("failed to read {}: {error}", path.display()))?;
    let file: FastInterZoneFile = serde_json::from_slice(&bytes)
        .map_err(|error| eyre::eyre!("invalid {}: {error}", path.display()))?;
    eyre::ensure!(
        file.l1_chain_id != 0,
        "inter-Zone L1 chain ID must be nonzero"
    );
    let peers = file
        .peers
        .into_iter()
        .map(|peer| {
            let encoded = const_hex::decode(&peer.ed25519_public_key).map_err(|error| {
                eyre::eyre!(
                    "invalid inter-Zone Ed25519 key for Zone {}: {error}",
                    peer.zone_id
                )
            })?;
            let ed25519 = P2pPeerId::decode(&encoded[..]).map_err(|error| {
                eyre::eyre!(
                    "invalid inter-Zone Ed25519 key for Zone {}: {error}",
                    peer.zone_id
                )
            })?;
            let endpoint = peer.endpoint.parse::<ManifestAddress>().map_err(|error| {
                eyre::eyre!(
                    "invalid inter-Zone endpoint for Zone {}: {error}",
                    peer.zone_id
                )
            })?;
            Ok(InterZoneRoutingPeer {
                zone_id: peer.zone_id,
                ed25519,
                endpoint,
            })
        })
        .collect::<eyre::Result<Vec<_>>>()?;
    Ok(InterZoneRoutingConfig {
        l1_chain_id: file.l1_chain_id,
        local_zone_id,
        listen: file.listen,
        bypass_ip_check: file.bypass_ip_check,
        peers,
    })
}

async fn validate_fee_payer_signer(
    path: Option<&std::path::Path>,
    expected: Address,
    sender: &PrivateKeySigner,
    label: &str,
) -> eyre::Result<()> {
    let actual = match path {
        Some(path) => load_private_key_signer(path, label).await?.address(),
        None => sender.address(),
    };
    eyre::ensure!(
        actual == expected,
        "{label} key does not match configured address"
    );
    Ok(())
}

async fn load_sequencer_signer(
    key_file: Option<&std::path::Path>,
) -> eyre::Result<PrivateKeySigner> {
    let path = key_file.ok_or_else(|| {
        eyre::eyre!("--sequencer-key-file is required when sequencing is enabled")
    })?;
    load_private_key_signer(path, "sequencer key").await
}

async fn load_private_key_signer(
    path: &std::path::Path,
    label: &str,
) -> eyre::Result<PrivateKeySigner> {
    let path = path.to_path_buf();
    let display_path = path.display().to_string();
    let key = tokio::task::spawn_blocking(move || std::fs::read_to_string(path))
        .await
        .map_err(|err| eyre::eyre!("{label} reader task failed for {display_path}: {err}"))?
        .map_err(|err| eyre::eyre!("failed to read {label} from {display_path}: {err}"))?;
    let key = Zeroizing::new(key);

    key.trim()
        .parse::<PrivateKeySigner>()
        .map_err(|_| eyre::eyre!("invalid {label} from {display_path}"))
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
    #[arg(
        long = "l1.rpc-url",
        env = "L1_RPC_URL",
        value_parser = parse_l1_rpc_url
    )]
    pub l1_rpc_url: String,

    /// ZonePortal contract address on L1.
    #[arg(
        long = "l1.portal-address",
        env = "L1_PORTAL_ADDRESS",
        value_parser = parse_portal_address
    )]
    pub portal_address: Address,

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

    /// Durable T14 Raft, replay, witness, signing, and retained-outcome directory.
    #[arg(long = "fast.storage", env = "FAST_STORAGE", value_name = "PATH")]
    pub fast_storage: Option<PathBuf>,

    /// Per-request authenticated Raft transport timeout.
    #[arg(
        long = "fast.rpc-timeout-ms",
        env = "FAST_RPC_TIMEOUT_MS",
        default_value_t = 2_000
    )]
    pub fast_rpc_timeout_ms: u64,

    /// JSON file containing explicit provider endpoints, signer key paths, fee assets, route
    /// policies, limits, treasuries, pool bindings, and durable C6 storage for each route.
    #[arg(
        long = "fast.replenishment-config",
        env = "FAST_REPLENISHMENT_CONFIG",
        value_name = "PATH",
        requires = "fast_storage"
    )]
    pub fast_replenishment_config: Option<PathBuf>,

    /// Deny-unknown-fields JSON containing the complete C4 provider, signer, finalized-member
    /// bindings, quotes, route caps, and queue limits. Requires the dedicated inter-Zone carrier.
    #[arg(
        long = "fast.service-config",
        env = "FAST_SERVICE_CONFIG",
        value_name = "PATH",
        requires = "fast_storage"
    )]
    pub fast_service_config: Option<PathBuf>,

    /// JSON routing resources for the separate encrypted ten-Zone Commonware carrier. The file
    /// contains exactly three Ed25519 identities/endpoints per Zone; finalized ECDSA rosters still
    /// supply authority after the imported anchor is validated.
    #[arg(
        long = "fast.inter-zone-config",
        env = "FAST_INTER_ZONE_CONFIG",
        value_name = "PATH",
        requires = "fast_storage"
    )]
    pub fast_inter_zone_config: Option<PathBuf>,

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

    /// Checker ExEx mode: `off` (default) or `observe`.
    #[arg(
        long = "checker.mode",
        env = "CHECKER_MODE",
        default_value = "off",
        value_parser = zone_checker::CheckerMode::parse,
    )]
    pub checker_mode: zone_checker::CheckerMode,

    /// Require Nitro-attested SPF validation for settlement, or run observational SPF validation
    /// on an rpc_only follower.
    ///
    /// Enables durable witness persistence.
    #[arg(long = "sequencer.enable-prover", env = "SEQUENCER_ENABLE_PROVER")]
    pub enable_prover: bool,

    /// Explicit settlement security mode. Finalized T14 enrollment must match this value exactly;
    /// enabling a prover never selects or upgrades the mode implicitly.
    #[arg(
        long = "sequencer.settlement-proof-mode",
        env = "SEQUENCER_SETTLEMENT_PROOF_MODE",
        value_enum,
        value_name = "MODE"
    )]
    pub settlement_proof_mode: Option<SettlementProofModeArg>,

    /// Fsync-backed prepared settlement and proof-evidence directory.
    #[arg(
        long = "sequencer.settlement-store",
        env = "SEQUENCER_SETTLEMENT_STORE",
        value_name = "PATH"
    )]
    pub settlement_store: Option<PathBuf>,

    /// Route to an immutable prover release for each exact live L1 hardfork. Repeat per hardfork.
    #[arg(
        long = "sequencer.prover-address",
        env = "SEQUENCER_PROVER_ADDRESS",
        value_name = "HARDFORK=HOST:PORT",
        value_delimiter = ',',
        requires_all = ["enable_prover", "prover_attestation_policy"]
    )]
    pub prover_addresses: Vec<HardforkProverAddress>,

    /// JSON PCR allowlist required to authenticate remote provers.
    #[arg(
        long = "sequencer.prover-attestation-policy",
        env = "SEQUENCER_PROVER_ATTESTATION_POLICY",
        value_name = "PATH",
        requires_all = ["enable_prover", "prover_addresses"]
    )]
    pub prover_attestation_policy: Option<PathBuf>,

    /// Verify shadow Nitro proofs locally against independently approved PCR0, PCR1 and PCR2.
    /// Works before T13; applies only to RPC followers and never enables settlement enforcement.
    #[arg(
        long = "shadow-prover.pcrs",
        env = "SHADOW_PROVER_PCRS",
        value_name = "PCR0,PCR1,PCR2",
        requires = "prover_addresses"
    )]
    pub shadow_prover_pcrs: Option<zone_prover::ShadowProofVerifier>,
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
    use std::{io::Write as _, process::Command, thread, time::Duration};

    use clap::Parser as _;

    use super::{
        Role, ZoneArgs, ZoneCli, load_decryption_keys, load_sequencer_signer, parse_l1_rpc_url,
        parse_portal_address, validate_deprecated_zone_id, validate_p2p_transaction_size_limit,
    };
    use zone_sequencer::MAX_WITHDRAWAL_BATCH_GAS;

    #[derive(Debug, clap::Parser)]
    struct ZoneArgsParser {
        #[command(flatten)]
        zone: ZoneArgs,
    }

    #[test]
    fn next_roster_handoff_is_a_dedicated_process_mode() {
        let parsed = ZoneCli::try_parse_from([
            "tempo-zone",
            "next-roster-handoff",
            "--chain",
            "genesis.json",
            "--config",
            "handoff.json",
        ])
        .unwrap();
        assert!(matches!(parsed, ZoneCli::NextRosterHandoff(_)));
    }

    #[test]
    fn re_execute_parses_without_portal_address() {
        use reth_chainspec::EthChainSpec as _;

        let parent = tempo_chainspec::spec::MODERATO.clone();
        let mut genesis = parent.genesis().clone();
        genesis.config.chain_id =
            zone_primitives::constants::zone_chain_id(parent.chain_id(), 11).unwrap();
        let genesis = serde_json::to_string(&genesis).unwrap();
        let parsed =
            ZoneCli::try_parse_from(["tempo-zone", "re-execute", "--chain", &genesis]).unwrap();
        assert!(matches!(parsed, ZoneCli::Node(_)));
    }

    #[tokio::test]
    async fn cli_evm_derives_portal_from_chain_spec() {
        use alloy_evm::EvmFactory as _;
        use reth_chainspec::EthChainSpec as _;
        use reth_evm::ConfigureEvm as _;

        let parent = tempo_chainspec::spec::MODERATO.clone();
        for (zone_id, expected) in [
            (
                11,
                alloy_primitives::address!("5ad000000000000000000000000000000000000b"),
            ),
            (
                0x0102_0304,
                alloy_primitives::address!("5ad0000000000000000000000000000001020304"),
            ),
        ] {
            let mut genesis = parent.genesis().clone();
            genesis.config.chain_id =
                zone_primitives::constants::zone_chain_id(parent.chain_id(), zone_id).unwrap();
            let spec =
                std::sync::Arc::new(zone_chainspec::ZoneChainSpec::from_genesis(genesis).unwrap());
            let config =
                super::cli_evm_config(spec, Some("http://localhost:8545".parse().unwrap()));
            let evm = config
                .evm_factory()
                .create_evm(revm::database::EmptyDB::default(), Default::default());
            assert_eq!(
                evm.ctx().journaled_state.database.l1_state().portal(),
                expected
            );
        }
    }

    #[test]
    fn shadow_verification_requires_remote_proving_and_complete_measurements() {
        let common = [
            "tempo-zone",
            "--l1.rpc-url",
            "ws://localhost:8546",
            "--l1.portal-address",
            "0x5ad0000000000000000000000000000000000002",
        ];
        let pcr = "11".repeat(48);
        let pcrs = format!("{pcr},{pcr},{pcr}");
        assert!(
            ZoneArgsParser::try_parse_from(
                common
                    .into_iter()
                    .chain(["--shadow-prover.pcrs", pcrs.as_str(),])
            )
            .is_err()
        );
        let args = ZoneArgsParser::try_parse_from(common.into_iter().chain([
            "--sequencer.enable-prover",
            "--sequencer.prover-attestation-policy",
            "policy.json",
            "--sequencer.prover-address",
            "T13=localhost:5000",
            "--shadow-prover.pcrs",
            pcrs.as_str(),
        ]))
        .unwrap();
        assert!(args.zone.shadow_prover_pcrs.is_some());
        assert!(
            ZoneArgsParser::try_parse_from(common.into_iter().chain([
                "--sequencer.enable-prover",
                "--sequencer.prover-attestation-policy",
                "policy.json",
                "--sequencer.prover-address",
                "T13=localhost:5000",
                "--shadow-prover.pcrs",
                "00,00,00",
            ]))
            .is_err()
        );
    }

    #[test]
    fn prover_addresses_accept_repeated_and_comma_separated_assignments() {
        let common = [
            "tempo-zone",
            "--l1.rpc-url",
            "ws://localhost:8546",
            "--l1.portal-address",
            "0x0000000000000000000000000000000000000001",
            "--sequencer.enable-prover",
            "--sequencer.prover-attestation-policy",
            "policy.json",
        ];
        for flags in [
            vec![
                "--sequencer.prover-address",
                "T12=old:5000",
                "--sequencer.prover-address",
                "T13=new:5000",
                "--sequencer.prover-address",
                "T14=t14:5000",
            ],
            vec![
                "--sequencer.prover-address",
                "T12=old:5000,T13=new:5000,T14=t14:5000",
            ],
        ] {
            let args = ZoneArgsParser::try_parse_from(common.into_iter().chain(flags))
                .unwrap()
                .zone;
            assert_eq!(args.prover_addresses.len(), 3);
            assert!(args.prover_attestation_policy.is_some());
        }
        let local_prover =
            ZoneArgsParser::try_parse_from(common[..common.len() - 2].iter().copied())
                .unwrap()
                .zone;
        assert!(local_prover.enable_prover);
        assert!(local_prover.prover_addresses.is_empty());
        assert!(local_prover.prover_attestation_policy.is_none());
        let missing_policy = ZoneArgsParser::try_parse_from(
            common[..common.len() - 2]
                .iter()
                .copied()
                .chain(["--sequencer.prover-address", "T12=old:5000"]),
        )
        .unwrap_err();
        assert!(
            missing_policy
                .to_string()
                .contains("--sequencer.prover-attestation-policy")
        );
        let error = ZoneArgsParser::try_parse_from(
            common
                .into_iter()
                .chain(["--sequencer.prover-address", "old:5000"]),
        )
        .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
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
            alloy_primitives::address!("0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf")
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
            alloy_primitives::address!("0x7E5F4552091A69125d5DfCb7b8C2659029395Bdf")
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
    fn settlement_policy_preserves_legacy_defaults_but_fences_fast_mode() {
        let legacy = PathBuf::from("legacy/settlement");
        assert_eq!(
            settlement_runtime_policy(false, false, None, None, legacy.clone()).unwrap(),
            (SettlementProofModeArg::OperatorAttested, legacy.clone())
        );
        assert_eq!(
            settlement_runtime_policy(false, true, None, None, legacy.clone()).unwrap(),
            (SettlementProofModeArg::ProofRequired, legacy.clone())
        );
        assert!(settlement_runtime_policy(true, true, None, None, legacy.clone()).is_err());
        assert!(
            settlement_runtime_policy(
                true,
                true,
                Some(SettlementProofModeArg::ProofRequired),
                None,
                legacy.clone(),
            )
            .is_err()
        );
        assert_eq!(
            settlement_runtime_policy(
                true,
                true,
                Some(SettlementProofModeArg::ProofRequired),
                Some(PathBuf::from("fast/settlement")),
                legacy,
            )
            .unwrap(),
            (
                SettlementProofModeArg::ProofRequired,
                PathBuf::from("fast/settlement")
            )
        );
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
