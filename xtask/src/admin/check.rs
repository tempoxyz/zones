use std::{
    collections::{BTreeSet, HashSet},
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use alloy::{
    primitives::{Address, B256, U64, U256},
    providers::{Provider, ProviderBuilder},
};
use alloy_rpc_types_eth::BlockId;
use eyre::{Context as _, ensure, eyre};
use futures::future::{join_all, try_join_all};
use serde::{Deserialize, Serialize};
use tempo_alloy::TempoNetwork;
use tempo_zone_contracts::{ZoneFactory, ZonePortal};
use tokio::time::timeout;
use zone_p2p::ZoneManifest;
use zone_rpc::types::{SequencerInfoResponse, ZoneInfoResponse};

use crate::zone_utils::MODERATO_ZONE_FACTORY;

// Two minutes at Tempo's expected 500 ms block time.
const MAX_ZONE_HEIGHT_LAG_BLOCKS: u64 = 240;

#[cfg(test)]
const DEFAULT_OBSERVE_FOR: Duration = Duration::from_secs(5);
#[cfg(test)]
const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// Read-only consistency and health audit for a Zone.
#[derive(Debug, clap::Parser)]
pub(crate) struct Check {
    /// Optional TOML file containing public operational inputs.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Expected Zone ID. Overrides `zone.id` from --config.
    #[arg(long)]
    zone_id: Option<u32>,

    /// Optional expected deployed Zone manifest. Overrides `zone.manifest` from --config.
    #[arg(long)]
    zone_manifest: Option<PathBuf>,

    /// Tempo L1 HTTP RPC URL. Overrides `l1.rpc_url` from --config.
    #[arg(long)]
    l1_rpc_url: Option<String>,

    /// ZoneFactory address. Overrides `l1.zone_factory` from --config.
    #[arg(long)]
    zone_factory: Option<Address>,

    /// Expected ZonePortal address. It must match ZoneFactory when supplied.
    #[arg(long)]
    portal: Option<Address>,

    /// Operator RPC endpoint, optionally labeled NAME=URL. Repeat to replace config nodes.
    #[arg(long = "operator-rpc", value_name = "[NAME=]URL")]
    operator_rpcs: Vec<OperatorEndpoint>,

    /// Interval over which operator Zone progress is observed; use 0s for a snapshot only.
    #[arg(long, default_value = "5s", value_parser = parse_duration)]
    observe_for: Duration,

    /// Timeout applied to each operator snapshot and Portal snapshot.
    #[arg(long, default_value = "10s", value_parser = parse_nonzero_duration)]
    rpc_timeout: Duration,

    /// Require the finalized Portal to have exactly this sequencer-set version.
    #[arg(long)]
    require_sequencer_set_version: Option<u64>,

    /// Require this finalized leader, expressed as a sequencer address or manifest/node name.
    #[arg(long)]
    require_leader: Option<String>,

    /// Require the active encryption public key, formatted as X:PARITY.
    #[arg(long, value_parser = parse_encryption_key)]
    require_encryption_key: Option<ExpectedEncryptionKey>,

    /// Emit a stable machine-readable report.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone)]
struct EffectiveConfig {
    zone_id: u32,
    manifest: Option<PathBuf>,
    l1_rpc_url: String,
    zone_factory: Address,
    portal: Option<Address>,
    nodes: Vec<OperatorEndpoint>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    #[serde(default)]
    zone: FileZone,
    #[serde(default)]
    l1: FileL1,
    #[serde(default)]
    nodes: Vec<FileNode>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileZone {
    id: Option<u32>,
    manifest: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileL1 {
    rpc_url: Option<String>,
    zone_factory: Option<Address>,
    portal: Option<Address>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileNode {
    name: Option<String>,
    operator_rpc_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OperatorEndpoint {
    name: Option<String>,
    url: String,
}

impl OperatorEndpoint {
    fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.url)
    }
}

impl FromStr for OperatorEndpoint {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (name, url) = match value.split_once('=') {
            Some((name, url)) if url.starts_with("http://") || url.starts_with("https://") => {
                if name.trim().is_empty() {
                    return Err("operator RPC label cannot be empty".to_owned());
                }
                (Some(name.to_owned()), url)
            }
            _ => (None, value),
        };
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err("operator RPC must be an http:// or https:// URL".to_owned());
        }
        Ok(Self {
            name,
            url: url.to_owned(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExpectedEncryptionKey {
    x: B256,
    y_parity: u8,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PortalSnapshot {
    pub(crate) finalized_block_number: u64,
    pub(crate) finalized_block_hash: B256,
    pub(crate) zone_id: u32,
    pub(crate) portal: Address,
    pub(crate) admin: Address,
    pub(crate) sequencers: Vec<Address>,
    pub(crate) threshold: u8,
    pub(crate) sequencer_set_version: u64,
    pub(crate) leader: Address,
    pub(crate) leader_epoch: u64,
    pub(crate) leader_activation_tempo_block: u64,
    pub(crate) zone_height: U256,
    pub(crate) last_synced_tempo_block: u64,
    pub(crate) block_hash: B256,
    pub(crate) zone_gas_rate: u128,
    pub(crate) withdrawal_batch_index: u64,
    pub(crate) current_deposit_queue_hash: B256,
    pub(crate) enabled_tokens: Vec<Address>,
    pub(crate) encryption_key: Option<EncryptionKey>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EncryptionKey {
    pub(crate) x: B256,
    pub(crate) y_parity: u8,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RpcBlock {
    number: U64,
    hash: B256,
    state_root: B256,
}

#[derive(Debug, Clone, Deserialize)]
struct RpcBlockRef {
    number: U64,
    hash: B256,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct NodeSnapshot {
    name: String,
    url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    zone: Option<ZoneInfoResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sequencer: Option<SequencerInfoResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    common_block: Option<RpcBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum CheckStatus {
    Pass,
    Fail,
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
struct InvariantResult {
    name: &'static str,
    status: CheckStatus,
    detail: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckReport {
    ok: bool,
    zone_id: u32,
    manifest_supplied: bool,
    #[serde(skip)]
    manifest_path: Option<PathBuf>,
    desired_topology_verified: Option<bool>,
    observe_for_ms: u64,
    portal: PortalSnapshot,
    nodes: Vec<NodeSnapshot>,
    #[serde(skip)]
    follow_up_nodes: Option<Vec<NodeSnapshot>>,
    invariants: Vec<InvariantResult>,
}

impl Check {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        progress("Loading and validating configuration...");
        let config = self.effective_config()?;
        progress(format!(
            "Configuration ready: Zone {}, {} operator RPC(s), snapshot timeout {}.",
            config.zone_id,
            config.nodes.len(),
            format_duration(self.rpc_timeout)
        ));

        if let Some(path) = config.manifest.as_ref() {
            progress(format!("Loading expected manifest {}...", path.display()));
        } else {
            progress("No expected manifest supplied; checking live consistency only.");
        }
        let manifest = config
            .manifest
            .as_ref()
            .map(ZoneManifest::read_from_file)
            .transpose()
            .wrap_err("failed to load expected Zone manifest")?;

        progress(format!(
            "Connecting to Tempo L1 at {}...",
            config.l1_rpc_url
        ));
        let l1 = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&config.l1_rpc_url)
            .await
            .wrap_err("failed connecting to Tempo L1 RPC")?;
        progress(format!(
            "Reading ZoneFactory and Portal at one finalized L1 block (timeout {})...",
            format_duration(self.rpc_timeout)
        ));
        let portal = timeout(
            self.rpc_timeout,
            read_portal_snapshot(&l1, config.zone_factory, config.zone_id, config.portal),
        )
        .await
        .map_err(|_| eyre!("timed out reading finalized Portal state"))??;
        progress(format!(
            "Finalized Portal snapshot received at L1 block {}.",
            portal.finalized_block_number
        ));

        progress(format!(
            "Querying {} operator RPC(s) concurrently (timeout {} per endpoint)...",
            config.nodes.len(),
            format_duration(self.rpc_timeout)
        ));
        let mut nodes = query_nodes(&config.nodes, self.rpc_timeout).await;
        let responsive_nodes = nodes.iter().filter(|node| node.error.is_none()).count();
        progress(format!(
            "Operator snapshots received from {responsive_nodes}/{} node(s).",
            nodes.len()
        ));
        let common_height = nodes
            .iter()
            .filter_map(|node| {
                node.sequencer
                    .as_ref()?
                    .local_tip
                    .as_ref()
                    .map(|tip| tip.zone_height.to::<u64>())
            })
            .min();
        if let Some(height) = common_height {
            progress(format!(
                "Comparing block hash and state root at common Zone height {height}..."
            ));
            query_common_blocks(&mut nodes, height, self.rpc_timeout).await;
        } else {
            progress("No common Zone height available; canonical-state check will fail.");
        }

        progress("Evaluating consistency and safety invariants...");
        let mut invariants = evaluate_invariants(
            &config,
            manifest.as_ref(),
            &portal,
            &nodes,
            self.require_sequencer_set_version,
            self.require_leader.as_deref(),
            self.require_encryption_key,
        );
        invariants.push(l1_batch_invariant(&portal));

        let follow_up_nodes = if self.observe_for.is_zero() {
            invariants.push(InvariantResult {
                name: "zone_height",
                status: CheckStatus::Skipped,
                detail: "liveness observation disabled with --observe-for 0s".to_owned(),
            });
            None
        } else {
            progress(format!(
                "Observing Zone progress for {}...",
                format_duration(self.observe_for)
            ));
            tokio::time::sleep(self.observe_for).await;
            progress("Observation interval complete; collecting follow-up snapshots...");
            progress(format!(
                "Querying {} operator RPC(s) for follow-up progress...",
                config.nodes.len()
            ));
            let later_nodes = query_nodes(&config.nodes, self.rpc_timeout).await;
            invariants.push(zone_height_invariant(&nodes, &later_nodes));
            Some(later_nodes)
        };

        progress("Rendering final report...");
        let desired_topology_verified = manifest.as_ref().map(|_| {
            invariants.iter().all(|result| {
                !result.name.starts_with("manifest_") || result.status == CheckStatus::Pass
            })
        });
        let ok = invariants
            .iter()
            .all(|result| result.status != CheckStatus::Fail);
        let report = CheckReport {
            ok,
            zone_id: config.zone_id,
            manifest_supplied: manifest.is_some(),
            manifest_path: config.manifest,
            desired_topology_verified,
            observe_for_ms: self.observe_for.as_millis().try_into().unwrap_or(u64::MAX),
            portal,
            nodes,
            follow_up_nodes,
            invariants,
        };

        if self.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            render_human(&report);
        }
        ensure!(report.ok, "one or more admin checks failed");
        Ok(())
    }

    fn effective_config(&self) -> eyre::Result<EffectiveConfig> {
        let (file, config_dir) = match &self.config {
            Some(path) => {
                let input = std::fs::read_to_string(path)
                    .wrap_err_with(|| format!("failed reading config {}", path.display()))?;
                let file = toml::from_str::<FileConfig>(&input)
                    .wrap_err_with(|| format!("failed parsing config {}", path.display()))?;
                (
                    file,
                    path.parent().unwrap_or_else(|| Path::new(".")).to_owned(),
                )
            }
            None => (FileConfig::default(), PathBuf::from(".")),
        };

        let zone_id = self
            .zone_id
            .or(file.zone.id)
            .ok_or_else(|| eyre!("missing Zone ID; pass --zone-id or set zone.id in --config"))?;
        let l1_rpc_url = self.l1_rpc_url.clone().or(file.l1.rpc_url).ok_or_else(|| {
            eyre!("missing Tempo L1 RPC URL; pass --l1-rpc-url or set l1.rpc_url in --config")
        })?;
        let manifest = self.zone_manifest.clone().or_else(|| {
            file.zone.manifest.map(|path| {
                if path.is_relative() {
                    config_dir.join(path)
                } else {
                    path
                }
            })
        });
        let nodes = if self.operator_rpcs.is_empty() {
            file.nodes
                .into_iter()
                .map(|node| OperatorEndpoint {
                    name: node.name,
                    url: node.operator_rpc_url,
                })
                .collect()
        } else {
            self.operator_rpcs.clone()
        };
        validate_endpoints(&nodes)?;
        ensure!(
            !nodes.is_empty(),
            "missing operator RPCs; repeat --operator-rpc or configure [[nodes]]"
        );

        Ok(EffectiveConfig {
            zone_id,
            manifest,
            l1_rpc_url,
            zone_factory: self
                .zone_factory
                .or(file.l1.zone_factory)
                .unwrap_or(MODERATO_ZONE_FACTORY),
            portal: self.portal.or(file.l1.portal),
            nodes,
        })
    }
}

fn progress(message: impl fmt::Display) {
    eprintln!("[admin check] {message}");
}

fn format_duration(duration: Duration) -> String {
    if duration.as_millis().is_multiple_of(1_000) {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

fn validate_endpoints(nodes: &[OperatorEndpoint]) -> eyre::Result<()> {
    let mut urls = HashSet::new();
    let mut names = HashSet::new();
    for node in nodes {
        ensure!(
            node.url.starts_with("http://") || node.url.starts_with("https://"),
            "operator RPC must be an http:// or https:// URL: {}",
            node.url
        );
        ensure!(
            urls.insert(&node.url),
            "duplicate operator RPC URL: {}",
            node.url
        );
        if let Some(name) = &node.name {
            ensure!(
                !name.trim().is_empty(),
                "operator RPC label cannot be empty"
            );
            ensure!(names.insert(name), "duplicate operator RPC label: {name}");
        }
    }
    Ok(())
}

pub(crate) async fn read_portal_snapshot<P>(
    provider: &P,
    factory_address: Address,
    expected_zone_id: u32,
    expected_portal: Option<Address>,
) -> eyre::Result<PortalSnapshot>
where
    P: Provider<TempoNetwork>,
{
    let finalized: Option<RpcBlockRef> = provider
        .raw_request("eth_getBlockByNumber".into(), ("finalized", false))
        .await
        .wrap_err("failed reading finalized L1 block")?;
    let finalized = finalized.ok_or_else(|| eyre!("Tempo L1 returned no finalized block"))?;
    let block_number = finalized.number.to::<u64>();
    let block_id = BlockId::number(block_number);

    let factory = ZoneFactory::new(factory_address, provider);
    let factory_info = factory
        .zones(expected_zone_id)
        .block(block_id)
        .call()
        .await
        .wrap_err("failed resolving Zone through ZoneFactory")?;
    ensure!(
        factory_info.portal != Address::ZERO,
        "ZoneFactory has no Zone {expected_zone_id} at finalized block {block_number}"
    );
    ensure!(
        factory_info.zoneId == expected_zone_id,
        "ZoneFactory returned Zone ID {}, expected {expected_zone_id}",
        factory_info.zoneId
    );
    if let Some(expected) = expected_portal {
        ensure!(
            expected == factory_info.portal,
            "supplied Portal {expected} does not match ZoneFactory Portal {}",
            factory_info.portal
        );
    }

    let portal_address = factory_info.portal;
    let portal = ZonePortal::new(portal_address, provider);
    let zone_id_call = portal.zoneId().block(block_id);
    let admin_call = portal.admin().block(block_id);
    let sequencer_count_call = portal.sequencerCount().block(block_id);
    let threshold_call = portal.sequencerThreshold().block(block_id);
    let version_call = portal.sequencerSetVersion().block(block_id);
    let leader_call = portal.leader().block(block_id);
    let leader_epoch_call = portal.leaderEpoch().block(block_id);
    let leader_activation_call = portal.leaderActivationTempoBlock().block(block_id);
    let zone_height_call = portal.zoneHeight().block(block_id);
    let last_synced_call = portal.lastSyncedTempoBlockNumber().block(block_id);
    let block_hash_call = portal.blockHash().block(block_id);
    let zone_gas_rate_call = portal.zoneGasRate().block(block_id);
    let withdrawal_batch_call = portal.withdrawalBatchIndex().block(block_id);
    let deposit_queue_call = portal.currentDepositQueueHash().block(block_id);
    let enabled_token_count_call = portal.enabledTokenCount().block(block_id);
    let encryption_key_call = async {
        let key_count = portal.encryptionKeyCount().block(block_id).call().await?;
        if key_count == U256::ZERO {
            Ok(None)
        } else {
            portal
                .sequencerEncryptionKey()
                .block(block_id)
                .call()
                .await
                .map(Some)
        }
    };
    let (
        zone_id,
        admin,
        sequencer_count,
        threshold,
        sequencer_set_version,
        leader,
        leader_epoch,
        leader_activation_tempo_block,
        zone_height,
        last_synced_tempo_block,
        block_hash,
        zone_gas_rate,
        withdrawal_batch_index,
        current_deposit_queue_hash,
        enabled_token_count,
        encryption_key,
    ) = tokio::try_join!(
        zone_id_call.call(),
        admin_call.call(),
        sequencer_count_call.call(),
        threshold_call.call(),
        version_call.call(),
        leader_call.call(),
        leader_epoch_call.call(),
        leader_activation_call.call(),
        zone_height_call.call(),
        last_synced_call.call(),
        block_hash_call.call(),
        zone_gas_rate_call.call(),
        withdrawal_batch_call.call(),
        deposit_queue_call.call(),
        enabled_token_count_call.call(),
        encryption_key_call,
    )
    .wrap_err("failed reading finalized ZonePortal snapshot")?;
    ensure!(
        zone_id == expected_zone_id,
        "Portal reports Zone ID {zone_id}, expected {expected_zone_id}"
    );

    let sequencers = try_join_all((0..sequencer_count.to::<usize>()).map(|index| {
        let portal = &portal;
        async move {
            portal
                .sequencerAt(U256::from(index))
                .block(block_id)
                .call()
                .await
                .wrap_err_with(|| format!("failed reading finalized sequencer index {index}"))
        }
    }))
    .await?;
    let enabled_tokens = try_join_all((0..enabled_token_count.to::<usize>()).map(|index| {
        let portal = &portal;
        async move {
            portal
                .enabledTokenAt(U256::from(index))
                .block(block_id)
                .call()
                .await
                .wrap_err_with(|| format!("failed reading finalized enabled token index {index}"))
        }
    }))
    .await?;

    let encryption_key = encryption_key
        .filter(|key| key.x != B256::ZERO)
        .map(|key| {
            let y_parity = key.normalized_y_parity().ok_or_else(|| {
                eyre!(
                    "Portal returned invalid encryption-key yParity {:#x}; expected 0/1 or 0x02/0x03",
                    key.yParity
                )
            })?;
            Ok::<_, eyre::Report>(EncryptionKey {
                x: key.x,
                y_parity,
            })
        })
        .transpose()?;

    Ok(PortalSnapshot {
        finalized_block_number: block_number,
        finalized_block_hash: finalized.hash,
        zone_id,
        portal: portal_address,
        admin,
        sequencers,
        threshold,
        sequencer_set_version,
        leader,
        leader_epoch,
        leader_activation_tempo_block,
        zone_height,
        last_synced_tempo_block,
        block_hash,
        zone_gas_rate,
        withdrawal_batch_index,
        current_deposit_queue_hash,
        enabled_tokens,
        encryption_key,
    })
}

async fn query_nodes(nodes: &[OperatorEndpoint], rpc_timeout: Duration) -> Vec<NodeSnapshot> {
    join_all(nodes.iter().map(|endpoint| async move {
        let name = endpoint.display_name().to_owned();
        let url = endpoint.url.clone();
        let result = timeout(rpc_timeout, async {
            let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
                .connect(&endpoint.url)
                .await?;
            let zone: ZoneInfoResponse =
                provider.raw_request("zone_getZoneInfo".into(), ()).await?;
            let sequencer: SequencerInfoResponse = provider
                .raw_request("zone_getSequencerInfo".into(), ())
                .await?;
            Ok::<_, eyre::Report>((zone, sequencer))
        })
        .await;
        match result {
            Ok(Ok((zone, sequencer))) => NodeSnapshot {
                name,
                url,
                zone: Some(zone),
                sequencer: Some(sequencer),
                common_block: None,
                error: None,
            },
            Ok(Err(error)) => NodeSnapshot {
                name,
                url,
                zone: None,
                sequencer: None,
                common_block: None,
                error: Some(format!("{error:#}")),
            },
            Err(_) => NodeSnapshot {
                name,
                url,
                zone: None,
                sequencer: None,
                common_block: None,
                error: Some(format!("timed out after {} ms", rpc_timeout.as_millis())),
            },
        }
    }))
    .await
}

async fn query_common_blocks(nodes: &mut [NodeSnapshot], height: u64, rpc_timeout: Duration) {
    let results = join_all(nodes.iter().map(|node| {
        let url = node.url.clone();
        async move {
            timeout(rpc_timeout, async {
                let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
                    .connect(&url)
                    .await?;
                let block: Option<RpcBlock> = provider
                    .raw_request(
                        "eth_getBlockByNumber".into(),
                        (format!("0x{height:x}"), false),
                    )
                    .await?;
                block.ok_or_else(|| eyre!("block {height} not found"))
            })
            .await
        }
    }))
    .await;
    for (node, result) in nodes.iter_mut().zip(results) {
        if node.error.is_some() {
            continue;
        }
        match result {
            Ok(Ok(block)) => node.common_block = Some(block),
            Ok(Err(error)) => node.error = Some(format!("common block query failed: {error:#}")),
            Err(_) => {
                node.error = Some(format!(
                    "common block query timed out after {} ms",
                    rpc_timeout.as_millis()
                ))
            }
        }
    }
}

fn evaluate_invariants(
    config: &EffectiveConfig,
    manifest: Option<&ZoneManifest>,
    portal: &PortalSnapshot,
    nodes: &[NodeSnapshot],
    required_version: Option<u64>,
    required_leader: Option<&str>,
    required_key: Option<ExpectedEncryptionKey>,
) -> Vec<InvariantResult> {
    let mut results = Vec::new();
    add_check(
        &mut results,
        "portal_quorum",
        portal.threshold > 0 && usize::from(portal.threshold) <= portal.sequencers.len(),
        format!(
            "threshold {} across {} sequencers",
            portal.threshold,
            portal.sequencers.len()
        ),
        format!(
            "invalid threshold {} for {} sequencers; expected 1..={}",
            portal.threshold,
            portal.sequencers.len(),
            portal.sequencers.len()
        ),
    );
    add_check(
        &mut results,
        "portal_leader",
        portal.sequencers.contains(&portal.leader),
        format!("finalized leader {} is an active sequencer", portal.leader),
        format!(
            "finalized leader {} is absent from Portal sequencers {:?}",
            portal.leader, portal.sequencers
        ),
    );
    add_check(
        &mut results,
        "operator_reachability",
        nodes.iter().all(|node| node.error.is_none()),
        format!("all {} operator RPCs responded", nodes.len()),
        node_error_detail(nodes),
    );

    let live_nodes = nodes
        .iter()
        .filter_map(|node| Some((node, node.zone.as_ref()?, node.sequencer.as_ref()?)))
        .collect::<Vec<_>>();
    let identity_failures = nodes
        .iter()
        .filter_map(|node| match (&node.zone, &node.sequencer) {
            (Some(zone), Some(sequencer))
                if zone.zone_id.to::<u32>() == config.zone_id
                    && sequencer.portal == portal.portal =>
            {
                None
            }
            (Some(zone), Some(sequencer)) => Some(format!(
                "{} reports Zone {} Portal {}",
                node.name, zone.zone_id, sequencer.portal
            )),
            _ => Some(format!("{} has no complete operator snapshot", node.name)),
        })
        .collect::<Vec<_>>();
    add_check(
        &mut results,
        "zone_identity",
        identity_failures.is_empty() && !nodes.is_empty(),
        format!(
            "all {} nodes report Zone {} and Portal {}",
            nodes.len(),
            config.zone_id,
            portal.portal
        ),
        format!(
            "expected Zone {} Portal {}; mismatches: {}",
            config.zone_id,
            portal.portal,
            identity_failures.join("; ")
        ),
    );

    let label_failures = config
        .nodes
        .iter()
        .filter_map(|endpoint| {
            let expected = endpoint.name.as_deref()?;
            let observed = nodes
                .iter()
                .find(|node| node.url == endpoint.url)
                .and_then(|node| node.sequencer.as_ref())
                .and_then(|info| info.local.as_ref())
                .map(|local| local.name.as_str());
            (observed != Some(expected)).then(|| {
                format!(
                    "{} expected label {expected}, reported {}",
                    endpoint.url,
                    observed.unwrap_or("<unavailable>")
                )
            })
        })
        .collect::<Vec<_>>();
    add_check(
        &mut results,
        "operator_labels",
        label_failures.is_empty(),
        "every explicit operator RPC label matches the node's manifest name".to_owned(),
        format!("label mismatches: {}", label_failures.join("; ")),
    );

    let portal_members = address_set(&portal.sequencers);
    let membership_failures = live_nodes
        .iter()
        .filter_map(|(node, _, sequencer)| {
            let observed = address_set(
                &sequencer
                    .peers
                    .iter()
                    .filter_map(|peer| peer.sequencer_address)
                    .collect::<Vec<_>>(),
            );
            (observed != portal_members).then(|| {
                format!(
                    "{} loaded {:?}, Portal has {:?}",
                    node.name, observed, portal_members
                )
            })
        })
        .collect::<Vec<_>>();
    let membership_ok = live_nodes.len() == nodes.len() && membership_failures.is_empty();
    add_check(
        &mut results,
        "live_membership",
        membership_ok,
        "every reachable node's loaded quorum matches finalized Portal membership".to_owned(),
        if live_nodes.len() != nodes.len() {
            format!(
                "only {}/{} nodes supplied membership data; {}",
                live_nodes.len(),
                nodes.len(),
                membership_failures.join("; ")
            )
        } else {
            format!("membership mismatches: {}", membership_failures.join("; "))
        },
    );

    let first_topology = live_nodes.first().map(|(_, _, info)| topology(info));
    let topology_failures = first_topology.as_ref().map_or_else(Vec::new, |expected| {
        live_nodes
            .iter()
            .filter_map(|(node, _, info)| {
                let observed = topology(info);
                (observed != *expected).then(|| format!("{} reports {observed:?}", node.name))
            })
            .collect::<Vec<_>>()
    });
    let topology_ok =
        live_nodes.len() == nodes.len() && first_topology.is_some() && topology_failures.is_empty();
    add_check(
        &mut results,
        "live_topology",
        topology_ok,
        "all reachable nodes report the same loaded topology".to_owned(),
        if first_topology.is_none() {
            "no node supplied loaded topology data".to_owned()
        } else if live_nodes.len() != nodes.len() {
            format!(
                "only {}/{} nodes supplied topology data",
                live_nodes.len(),
                nodes.len()
            )
        } else {
            format!("topology disagreements: {}", topology_failures.join("; "))
        },
    );
    results.push(loaded_manifest_agreement_invariant(
        config.zone_id,
        portal.sequencer_set_version,
        nodes,
    ));

    let leader_failures = live_nodes
        .iter()
        .filter_map(|(node, _, info)| match info.active_leader.as_ref() {
            Some(leader)
                if leader.sequencer_address == Some(portal.leader)
                    && leader.epoch.to::<u64>() == portal.leader_epoch =>
            {
                None
            }
            Some(leader) => Some(format!(
                "{} reports leader {:?} epoch {}",
                node.name, leader.sequencer_address, leader.epoch
            )),
            None => Some(format!("{} reports no active leader", node.name)),
        })
        .collect::<Vec<_>>();
    let leader_ok = live_nodes.len() == nodes.len() && leader_failures.is_empty();
    add_check(
        &mut results,
        "leader_agreement",
        leader_ok,
        format!(
            "all nodes report finalized leader {} at epoch {}",
            portal.leader, portal.leader_epoch
        ),
        format!(
            "expected leader {} epoch {}; mismatches: {}",
            portal.leader,
            portal.leader_epoch,
            leader_failures.join("; ")
        ),
    );

    let readiness = assess_readiness(&live_nodes);
    let readiness_ok = live_nodes.len() == nodes.len()
        && readiness.sequencer_nodes > 0
        && readiness.failures.is_empty();
    let excluded_detail = if readiness.rpc_only_nodes.is_empty() {
        "no rpc-only nodes excluded".to_owned()
    } else {
        format!("rpc-only excluded: {}", readiness.rpc_only_nodes.join(", "))
    };
    add_check(
        &mut results,
        "promotion_readiness",
        readiness_ok,
        format!(
            "all {} sequencer node(s) are promotion-ready with no pending transitions; {excluded_detail}",
            readiness.sequencer_nodes
        ),
        readiness.failures.join("; "),
    );

    match portal.encryption_key {
        Some(active_key) => {
            let mut sequencer_nodes = 0_usize;
            let mut rpc_only_nodes = Vec::new();
            let mut key_failures = Vec::new();
            for node in nodes {
                let Some(info) = node.sequencer.as_ref() else {
                    key_failures.push(format!("{} has no sequencer status", node.name));
                    continue;
                };
                match is_rpc_only(info) {
                    Some(true) => {
                        rpc_only_nodes.push(node.name.as_str());
                        continue;
                    }
                    Some(false) => sequencer_nodes += 1,
                    None => {
                        key_failures.push(format!(
                            "{} does not report whether its local node is rpc-only",
                            node.name
                        ));
                        continue;
                    }
                }
                let Some(keys) = info.decryption_keys.as_ref() else {
                    key_failures.push(format!(
                        "{} does not expose decryption-key status",
                        node.name
                    ));
                    continue;
                };
                let present = keys.candidates.iter().any(|candidate| {
                    candidate.x == active_key.x && candidate.y_parity == active_key.y_parity
                }) || keys
                    .bound
                    .iter()
                    .any(|bound| bound.x == active_key.x && bound.y_parity == active_key.y_parity);
                if !present {
                    key_failures.push(format!(
                        "{} is missing active key x={} parity={} ({} candidate(s), {} bound key(s) reported)",
                        node.name,
                        active_key.x,
                        active_key.y_parity,
                        keys.candidates.len(),
                        keys.bound.len()
                    ));
                }
            }
            if sequencer_nodes == 0 {
                key_failures.push("no non-rpc-only sequencer nodes were checked".to_owned());
            }
            let excluded_detail = if rpc_only_nodes.is_empty() {
                "no rpc-only nodes excluded".to_owned()
            } else {
                format!("rpc-only excluded: {}", rpc_only_nodes.join(", "))
            };
            add_check(
                &mut results,
                "decryption_key_availability",
                key_failures.is_empty(),
                format!(
                    "all {sequencer_nodes} sequencer node(s) report active Portal key x={} parity={}; {excluded_detail}",
                    active_key.x, active_key.y_parity,
                ),
                key_failures.join("; "),
            );
        }
        None => results.push(InvariantResult {
            name: "decryption_key_availability",
            status: CheckStatus::Skipped,
            detail: "Portal has no active encryption key".to_owned(),
        }),
    }

    let common_blocks = nodes
        .iter()
        .filter_map(|node| node.common_block.as_ref())
        .collect::<Vec<_>>();
    let canonical_ok = common_blocks.len() == nodes.len()
        && common_blocks.first().is_some_and(|first| {
            common_blocks
                .iter()
                .all(|block| block.hash == first.hash && block.state_root == first.state_root)
        });
    let canonical_failure = if common_blocks.len() != nodes.len() {
        let missing = nodes
            .iter()
            .filter(|node| node.common_block.is_none())
            .map(|node| node.name.as_str())
            .collect::<Vec<_>>();
        format!(
            "only {}/{} nodes returned the common block; missing: {}",
            common_blocks.len(),
            nodes.len(),
            missing.join(", ")
        )
    } else if let Some(expected) = common_blocks.first() {
        nodes
            .iter()
            .filter_map(|node| {
                let block = node.common_block.as_ref()?;
                (block.hash != expected.hash || block.state_root != expected.state_root).then(
                    || {
                        format!(
                            "{} reports hash {} state root {}",
                            node.name, block.hash, block.state_root
                        )
                    },
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    } else {
        "no common block was available".to_owned()
    };
    add_check(
        &mut results,
        "canonical_state",
        canonical_ok,
        common_blocks.first().map_or_else(
            || "no common block was available".to_owned(),
            |block| {
                format!(
                    "{} nodes checked block {} hash {} state root {}",
                    common_blocks.len(),
                    block.number,
                    block.hash,
                    block.state_root
                )
            },
        ),
        canonical_failure,
    );
    results.push(zone_height_lag_invariant(nodes));

    if let Some(manifest) = manifest {
        let node_manifest_zones = live_nodes
            .iter()
            .map(|(node, _, info)| {
                format!(
                    "{}={:?}",
                    node.name,
                    info.manifest_zone_id.map(|id| id.to::<u32>())
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        add_check(
            &mut results,
            "manifest_zone",
            manifest.zone_id() == config.zone_id
                && live_nodes.iter().all(|(_, _, info)| {
                    info.manifest_zone_id.map(|id| id.to::<u32>()) == Some(config.zone_id)
                }),
            format!("manifest and all nodes declare Zone {}", config.zone_id),
            format!(
                "expected Zone {}; file={}, nodes=[{}]",
                config.zone_id,
                manifest.zone_id(),
                node_manifest_zones
            ),
        );
        let node_manifest_versions = live_nodes
            .iter()
            .map(|(node, _, info)| {
                format!(
                    "{}={:?}",
                    node.name,
                    info.manifest_sequencer_set_version
                        .map(|version| version.to::<u64>())
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        add_check(
            &mut results,
            "manifest_version",
            manifest.sequencer_set_version() == portal.sequencer_set_version
                && live_nodes.iter().all(|(_, _, info)| {
                    info.manifest_sequencer_set_version
                        .map(|version| version.to::<u64>())
                        == Some(manifest.sequencer_set_version())
                }),
            format!(
                "manifest, Portal, and all nodes report version {}",
                manifest.sequencer_set_version()
            ),
            format!(
                "expected version {}; Portal={}, nodes=[{}]",
                manifest.sequencer_set_version(),
                portal.sequencer_set_version,
                node_manifest_versions
            ),
        );
        let digest = manifest.membership_digest();
        let node_digests = live_nodes
            .iter()
            .map(|(node, _, info)| format!("{}={:?}", node.name, info.manifest_membership_digest))
            .collect::<Vec<_>>()
            .join(", ");
        add_check(
            &mut results,
            "manifest_digest",
            live_nodes
                .iter()
                .all(|(_, _, info)| info.manifest_membership_digest == Some(digest)),
            format!("all nodes report expected membership digest {digest}"),
            format!("expected digest {digest}; nodes=[{node_digests}]"),
        );
        let manifest_members = manifest
            .quorum_nodes()
            .map(|(_, address)| address)
            .collect::<Vec<_>>();
        add_check(
            &mut results,
            "manifest_membership",
            address_set(&manifest_members) == portal_members,
            "expected manifest quorum matches finalized Portal membership".to_owned(),
            format!(
                "manifest quorum {:?} differs from Portal quorum {:?}",
                address_set(&manifest_members),
                portal_members
            ),
        );
        let manifest_nodes_ok = live_nodes.iter().all(|(_, _, info)| {
            info.local.as_ref().is_some_and(|local| {
                manifest.nodes().iter().any(|node| {
                    node.name() == local.name
                        && node.ed25519_public_key().to_string() == local.p2p_public_key
                        && node.secp256k1_address() == local.sequencer_address
                })
            })
        });
        add_check(
            &mut results,
            "manifest_node_identity",
            manifest_nodes_ok,
            "each queried node's local identity matches the expected manifest".to_owned(),
            format!(
                "one or more local identities do not match the expected manifest: {}",
                live_nodes
                    .iter()
                    .filter_map(|(node, _, info)| {
                        let local = info.local.as_ref()?;
                        let matches = manifest.nodes().iter().any(|manifest_node| {
                            manifest_node.name() == local.name
                                && manifest_node.ed25519_public_key().to_string()
                                    == local.p2p_public_key
                                && manifest_node.secp256k1_address() == local.sequencer_address
                        });
                        (!matches).then(|| format!("{}={local:?}", node.name))
                    })
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        );
    }

    if let Some(version) = required_version {
        add_check(
            &mut results,
            "required_sequencer_set_version",
            portal.sequencer_set_version == version,
            format!("finalized sequencer-set version matches required {version}"),
            format!(
                "required sequencer-set version {version}, finalized {}",
                portal.sequencer_set_version
            ),
        );
    }
    if let Some(required) = required_leader {
        let resolved = required.parse::<Address>().ok().or_else(|| {
            live_nodes.iter().find_map(|(_, _, info)| {
                info.peers
                    .iter()
                    .find(|peer| peer.name == required)
                    .and_then(|peer| peer.sequencer_address)
            })
        });
        add_check(
            &mut results,
            "required_leader",
            resolved == Some(portal.leader),
            format!(
                "finalized leader {} matches required {required}",
                portal.leader
            ),
            format!(
                "required leader {required} resolved to {resolved:?}, finalized leader is {}",
                portal.leader
            ),
        );
    }
    if let Some(required) = required_key {
        let observed_key = portal
            .encryption_key
            .map(|key| format!("x={} parity={}", key.x, key.y_parity))
            .unwrap_or_else(|| "<not configured>".to_owned());
        add_check(
            &mut results,
            "required_encryption_key",
            portal
                .encryption_key
                .is_some_and(|key| key.x == required.x && key.y_parity == required.y_parity),
            format!(
                "active encryption key matches required x={} parity={}",
                required.x, required.y_parity
            ),
            format!(
                "required x={} parity={}; Portal reports {observed_key}",
                required.x, required.y_parity
            ),
        );
    }
    results
}

fn l1_batch_invariant(portal: &PortalSnapshot) -> InvariantResult {
    InvariantResult {
        name: "l1_batch",
        status: CheckStatus::Pass,
        detail: format!(
            "finalized L1 batch {} settles Zone height {} at Tempo block {}",
            portal.withdrawal_batch_index, portal.zone_height, portal.finalized_block_number
        ),
    }
}

fn loaded_manifest_agreement_invariant(
    expected_zone_id: u32,
    expected_sequencer_set_version: u64,
    nodes: &[NodeSnapshot],
) -> InvariantResult {
    let infos = nodes
        .iter()
        .filter_map(|node| node.sequencer.as_ref().map(|info| (node, info)))
        .collect::<Vec<_>>();
    if infos.len() != nodes.len() {
        let missing = nodes
            .iter()
            .filter(|node| node.sequencer.is_none())
            .map(|node| node.name.as_str())
            .collect::<Vec<_>>();
        return InvariantResult {
            name: "loaded_manifest_agreement",
            status: CheckStatus::Fail,
            detail: format!("no sequencer status reported by: {}", missing.join(", ")),
        };
    }
    if infos.iter().all(|(_, info)| info.mode == "single") {
        return InvariantResult {
            name: "loaded_manifest_agreement",
            status: CheckStatus::Skipped,
            detail: "all nodes report single-node mode; no loaded manifest to compare".to_owned(),
        };
    }
    if !infos.iter().all(|(_, info)| info.mode == "multi") {
        return InvariantResult {
            name: "loaded_manifest_agreement",
            status: CheckStatus::Fail,
            detail: format!(
                "inconsistent node modes: {}",
                infos
                    .iter()
                    .map(|(node, info)| format!("{}={}", node.name, info.mode))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
    }

    let Some((_, first)) = infos.first() else {
        return InvariantResult {
            name: "loaded_manifest_agreement",
            status: CheckStatus::Fail,
            detail: "no operator nodes configured".to_owned(),
        };
    };
    let (Some(zone_id), Some(version), Some(digest)) = (
        first.manifest_zone_id.map(|id| id.to::<u32>()),
        first
            .manifest_sequencer_set_version
            .map(|version| version.to::<u64>()),
        first.manifest_membership_digest,
    ) else {
        return InvariantResult {
            name: "loaded_manifest_agreement",
            status: CheckStatus::Fail,
            detail: format!(
                "{} did not report complete loaded manifest metadata",
                infos[0].0.name
            ),
        };
    };
    let failures = infos
        .iter()
        .filter_map(|(node, info)| {
            let observed = (
                info.manifest_zone_id.map(|id| id.to::<u32>()),
                info.manifest_sequencer_set_version
                    .map(|version| version.to::<u64>()),
                info.manifest_membership_digest,
            );
            (observed != (Some(zone_id), Some(version), Some(digest))).then(|| {
                format!(
                    "{} reports zone={:?}, version={:?}, digest={:?}",
                    node.name, observed.0, observed.1, observed.2
                )
            })
        })
        .collect::<Vec<_>>();
    let expected_matches = zone_id == expected_zone_id && version == expected_sequencer_set_version;
    InvariantResult {
        name: "loaded_manifest_agreement",
        status: if expected_matches && failures.is_empty() {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: if expected_matches && failures.is_empty() {
            format!(
                "all nodes report manifest Zone {zone_id}, version {version}, and membership digest {digest}"
            )
        } else if !expected_matches {
            format!(
                "loaded manifest reports Zone {zone_id}, version {version}; expected Zone {expected_zone_id}, finalized Portal version {expected_sequencer_set_version}; disagreements: {}",
                failures.join("; ")
            )
        } else {
            format!("loaded manifest disagreements: {}", failures.join("; "))
        },
    }
}

fn zone_height_lag_invariant(nodes: &[NodeSnapshot]) -> InvariantResult {
    let heights = nodes
        .iter()
        .filter_map(|node| {
            node.sequencer
                .as_ref()?
                .local_tip
                .as_ref()
                .map(|tip| (node, tip.zone_height.to::<u64>()))
        })
        .collect::<Vec<_>>();
    if heights.len() != nodes.len() {
        let missing = nodes
            .iter()
            .filter(|node| {
                node.sequencer
                    .as_ref()
                    .and_then(|info| info.local_tip.as_ref())
                    .is_none()
            })
            .map(|node| node.name.as_str())
            .collect::<Vec<_>>();
        return InvariantResult {
            name: "zone_height_lag",
            status: CheckStatus::Fail,
            detail: format!("no local Zone height reported by: {}", missing.join(", ")),
        };
    }

    let newest_height = heights
        .iter()
        .map(|(_, height)| *height)
        .max()
        .expect("heights includes every configured node");
    let lagging = heights
        .iter()
        .filter_map(|(node, height)| {
            let lag = newest_height - *height;
            (lag > MAX_ZONE_HEIGHT_LAG_BLOCKS)
                .then(|| format!("{} at {} ({lag} blocks behind)", node.name, height))
        })
        .collect::<Vec<_>>();
    InvariantResult {
        name: "zone_height_lag",
        status: if lagging.is_empty() {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: if lagging.is_empty() {
            format!(
                "all nodes are within {MAX_ZONE_HEIGHT_LAG_BLOCKS} Zone blocks of newest height {newest_height}"
            )
        } else {
            format!(
                "newest Zone height {newest_height}; allowed lag {MAX_ZONE_HEIGHT_LAG_BLOCKS} blocks; lagging: {}",
                lagging.join("; ")
            )
        },
    }
}

fn zone_height_invariant(
    initial_nodes: &[NodeSnapshot],
    later_nodes: &[NodeSnapshot],
) -> InvariantResult {
    let node_observations = initial_nodes
        .iter()
        .map(|initial| {
            let initial_height = initial
                .sequencer
                .as_ref()
                .and_then(|info| info.progress.as_ref())
                .map(|progress| progress.zone_height.to::<u64>());
            let later_height = later_nodes
                .iter()
                .find(|later| later.url == initial.url)
                .and_then(|later| later.sequencer.as_ref())
                .and_then(|info| info.progress.as_ref())
                .map(|progress| progress.zone_height.to::<u64>());
            let progressed = initial_height
                .zip(later_height)
                .is_some_and(|(initial, later)| later > initial);
            (
                progressed,
                format!(
                    "{}: {} -> {}{}",
                    initial.name,
                    initial_height
                        .map_or_else(|| "<unavailable>".to_owned(), |height| height.to_string()),
                    later_height
                        .map_or_else(|| "<unavailable>".to_owned(), |height| height.to_string()),
                    if progressed {
                        ""
                    } else {
                        " (unchanged or unavailable)"
                    }
                ),
            )
        })
        .collect::<Vec<_>>();
    let nodes_progressed = !node_observations.is_empty()
        && node_observations.iter().all(|(progressed, _)| *progressed);
    let node_detail = node_observations
        .iter()
        .map(|(_, detail)| detail.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    InvariantResult {
        name: "zone_height",
        status: if nodes_progressed {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: if nodes_progressed {
            format!("all operator nodes advanced; {node_detail}")
        } else {
            node_observations
                .into_iter()
                .filter_map(|(progressed, detail)| (!progressed).then_some(detail))
                .collect::<Vec<_>>()
                .join("; ")
        },
    }
}

fn add_check(
    results: &mut Vec<InvariantResult>,
    name: &'static str,
    passed: bool,
    pass_detail: String,
    fail_detail: String,
) {
    results.push(InvariantResult {
        name,
        status: if passed {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: if passed { pass_detail } else { fail_detail },
    });
}

fn address_set(addresses: &[Address]) -> BTreeSet<Address> {
    addresses.iter().copied().collect()
}

fn topology(info: &SequencerInfoResponse) -> Vec<(String, Option<Address>, bool)> {
    let mut topology = info
        .peers
        .iter()
        .map(|peer| (peer.name.clone(), peer.sequencer_address, peer.rpc_only))
        .collect::<Vec<_>>();
    topology.sort_by(|a, b| a.0.cmp(&b.0));
    topology
}

fn is_rpc_only(info: &SequencerInfoResponse) -> Option<bool> {
    info.peers
        .iter()
        .find(|peer| peer.is_local)
        .map(|peer| peer.rpc_only)
        .or_else(|| {
            info.local
                .as_ref()
                .map(|local| local.sequencer_address.is_none())
        })
}

fn node_error_detail(nodes: &[NodeSnapshot]) -> String {
    let errors = nodes
        .iter()
        .filter_map(|node| {
            node.error
                .as_ref()
                .map(|error| format!("{}: {error}", node.name))
        })
        .collect::<Vec<_>>();
    if errors.is_empty() {
        format!("all {} operator RPCs responded", nodes.len())
    } else {
        errors.join("; ")
    }
}

struct ReadinessAssessment<'a> {
    sequencer_nodes: usize,
    rpc_only_nodes: Vec<&'a str>,
    failures: Vec<String>,
}

fn assess_readiness<'a>(
    nodes: &'a [(&NodeSnapshot, &ZoneInfoResponse, &SequencerInfoResponse)],
) -> ReadinessAssessment<'a> {
    let mut sequencer_nodes = 0_usize;
    let mut rpc_only_nodes = Vec::new();
    let mut failures = Vec::new();
    for (node, _, info) in nodes {
        match is_rpc_only(info) {
            Some(true) => {
                rpc_only_nodes.push(node.name.as_str());
                continue;
            }
            Some(false) => sequencer_nodes += 1,
            None => {
                failures.push(format!(
                    "{} does not report whether its local node is rpc-only",
                    node.name
                ));
                continue;
            }
        }
        match (&info.readiness, &info.progress) {
            (Some(readiness), Some(progress))
                if readiness.ready_for_promotion
                    && progress.pending_transitions.to::<u64>() == 0 => {}
            (Some(readiness), Some(progress)) => failures.push(format!(
                "{}: ready={}, pending transitions={}, reasons=[{}]",
                node.name,
                readiness.ready_for_promotion,
                progress.pending_transitions,
                readiness.reasons.join(", ")
            )),
            (None, _) => failures.push(format!("{}: readiness status unavailable", node.name)),
            (_, None) => failures.push(format!("{}: progress status unavailable", node.name)),
        }
    }
    if sequencer_nodes == 0 {
        failures.push("no non-rpc-only sequencer nodes were checked".to_owned());
    }
    ReadinessAssessment {
        sequencer_nodes,
        rpc_only_nodes,
        failures,
    }
}

#[derive(Debug, Clone, Copy)]
enum TableStatus {
    Pass,
    Fail,
    NotAvailable,
}

impl fmt::Display for TableStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.pad(match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::NotAvailable => "N/A",
        })
    }
}

struct NodeTableRow {
    name: String,
    height: String,
    reach: TableStatus,
    progress: TableStatus,
    identity: TableStatus,
    membership: TableStatus,
    leader: TableStatus,
    ready: TableStatus,
    key: TableStatus,
}

fn render_node_table(report: &CheckReport) {
    let portal_members = address_set(&report.portal.sequencers);
    let rows = report
        .nodes
        .iter()
        .map(|node| {
            let follow_up = report
                .follow_up_nodes
                .as_ref()
                .and_then(|nodes| nodes.iter().find(|later| later.url == node.url));
            let initial_height = node
                .sequencer
                .as_ref()
                .and_then(|info| info.progress.as_ref())
                .map(|progress| progress.zone_height.to::<u64>());
            let later_height = follow_up
                .and_then(|later| later.sequencer.as_ref())
                .and_then(|info| info.progress.as_ref())
                .map(|progress| progress.zone_height.to::<u64>());
            let initially_reachable = node.error.is_none();
            let reachable =
                initially_reachable && follow_up.map(|later| later.error.is_none()).unwrap_or(true);
            let status = |passed: Option<bool>| {
                if !initially_reachable {
                    TableStatus::NotAvailable
                } else if passed.unwrap_or(false) {
                    TableStatus::Pass
                } else {
                    TableStatus::Fail
                }
            };
            let height = match (
                initial_height,
                later_height,
                report.follow_up_nodes.is_some(),
            ) {
                (Some(initial), Some(later), true) => format!("{initial} -> {later}"),
                (Some(initial), _, false) => initial.to_string(),
                (Some(initial), None, true) => format!("{initial} -> ?"),
                (None, Some(later), true) => format!("? -> {later}"),
                (None, _, _) => "N/A".to_owned(),
            };
            let progress = if report.follow_up_nodes.is_none() || !reachable {
                TableStatus::NotAvailable
            } else {
                status(initial_height.zip(later_height).map(|(a, b)| b > a))
            };
            let identity = status(node.zone.as_ref().zip(node.sequencer.as_ref()).map(
                |(zone, sequencer)| {
                    zone.zone_id.to::<u32>() == report.zone_id
                        && sequencer.portal == report.portal.portal
                },
            ));
            let membership = status(node.sequencer.as_ref().map(|sequencer| {
                address_set(
                    &sequencer
                        .peers
                        .iter()
                        .filter_map(|peer| peer.sequencer_address)
                        .collect::<Vec<_>>(),
                ) == portal_members
            }));
            let leader = status(node.sequencer.as_ref().map(|sequencer| {
                sequencer.active_leader.as_ref().is_some_and(|leader| {
                    leader.sequencer_address == Some(report.portal.leader)
                        && leader.epoch.to::<u64>() == report.portal.leader_epoch
                })
            }));
            let ready = match node.sequencer.as_ref().and_then(is_rpc_only) {
                Some(true) => TableStatus::NotAvailable,
                Some(false) => status(node.sequencer.as_ref().map(|sequencer| {
                    sequencer
                        .readiness
                        .as_ref()
                        .zip(sequencer.progress.as_ref())
                        .is_some_and(|(readiness, progress)| {
                            readiness.ready_for_promotion
                                && progress.pending_transitions.to::<u64>() == 0
                        })
                })),
                None => status(None),
            };
            let key = match report.portal.encryption_key {
                None => TableStatus::NotAvailable,
                Some(active_key) => match node.sequencer.as_ref().and_then(is_rpc_only) {
                    Some(true) => TableStatus::NotAvailable,
                    Some(false) => status(node.sequencer.as_ref().map(|sequencer| {
                        sequencer.decryption_keys.as_ref().is_some_and(|keys| {
                            keys.candidates.iter().any(|candidate| {
                                candidate.x == active_key.x
                                    && candidate.y_parity == active_key.y_parity
                            }) || keys.bound.iter().any(|bound| {
                                bound.x == active_key.x && bound.y_parity == active_key.y_parity
                            })
                        })
                    })),
                    None => status(None),
                },
            };

            NodeTableRow {
                name: truncate_cell(&node.name, 24),
                height,
                reach: if reachable {
                    TableStatus::Pass
                } else {
                    TableStatus::Fail
                },
                progress,
                identity,
                membership,
                leader,
                ready,
                key,
            }
        })
        .collect::<Vec<_>>();

    let name_width = rows
        .iter()
        .map(|row| row.name.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let height_width = rows
        .iter()
        .map(|row| row.height.len())
        .max()
        .unwrap_or(6)
        .max(6);
    println!("Operator nodes");
    println!(
        "{:<name_width$}  {:<height_width$}  {:<5}  {:<8}  {:<8}  {:<10}  {:<6}  {:<5}  {:<16}",
        "Node",
        "Height",
        "Reach",
        "Progress",
        "Identity",
        "Membership",
        "Leader",
        "Ready",
        "SharedEncKey"
    );
    println!(
        "{:-<name_width$}  {:-<height_width$}  {:-<5}  {:-<8}  {:-<8}  {:-<10}  {:-<6}  {:-<5}  {:-<16}",
        "", "", "", "", "", "", "", "", ""
    );
    for row in rows {
        println!(
            "{:<name_width$}  {:<height_width$}  {:<5}  {:<8}  {:<8}  {:<10}  {:<6}  {:<5}  {:<16}",
            row.name,
            row.height,
            row.reach,
            row.progress,
            row.identity,
            row.membership,
            row.leader,
            row.ready,
            row.key
        );
    }
}

fn truncate_cell(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    value
        .chars()
        .take(max_chars.saturating_sub(3))
        .chain("...".chars())
        .collect()
}

fn render_human(report: &CheckReport) {
    println!("Zone {} admin check", report.zone_id);
    println!(
        "Portal {} at finalized Tempo block {} ({})",
        report.portal.portal,
        report.portal.finalized_block_number,
        report.portal.finalized_block_hash
    );
    println!(
        "Sequencers: {}  Threshold: {}  Version: {}",
        report.portal.sequencers.len(),
        report.portal.threshold,
        report.portal.sequencer_set_version
    );
    println!(
        "Leader: {}  Epoch: {}",
        report.portal.leader, report.portal.leader_epoch
    );
    match report.portal.encryption_key {
        Some(key) => println!("Encryption key: x={} parity={}", key.x, key.y_parity),
        None => println!("Encryption key: not configured"),
    }
    if let Some(path) = report.manifest_path.as_ref() {
        println!("Desired manifest: {}", path.display());
    } else {
        println!("Desired manifest: not supplied (live consistency only)");
    }
    println!();
    render_node_table(report);
    println!();
    for invariant in &report.invariants {
        let marker = match invariant.status {
            CheckStatus::Pass => "PASS",
            CheckStatus::Fail => "FAIL",
            CheckStatus::Skipped => "SKIP",
        };
        println!("[{marker}] {:<34} {}", invariant.name, invariant.detail);
    }
    println!();
    println!(
        "Result: {}",
        if report.ok {
            "HEALTHY"
        } else {
            "CHECKS FAILED"
        }
    );
}

fn parse_duration(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
        (number, 1_u64)
    } else if let Some(number) = value.strip_suffix('s') {
        (number, 1_000)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60_000)
    } else if let Some(number) = value.strip_suffix('h') {
        (number, 3_600_000)
    } else {
        return Err("duration must end in ms, s, m, or h".to_owned());
    };
    let number = number
        .parse::<u64>()
        .map_err(|_| format!("invalid duration: {value}"))?;
    number
        .checked_mul(multiplier)
        .map(Duration::from_millis)
        .ok_or_else(|| format!("duration is too large: {value}"))
}

fn parse_nonzero_duration(value: &str) -> Result<Duration, String> {
    let duration = parse_duration(value)?;
    if duration.is_zero() {
        return Err("duration must be greater than zero".to_owned());
    }
    Ok(duration)
}

fn parse_encryption_key(value: &str) -> Result<ExpectedEncryptionKey, String> {
    let (x, parity) = value
        .rsplit_once(':')
        .ok_or_else(|| "expected X:PARITY".to_owned())?;
    let x = x.parse::<B256>().map_err(|error| error.to_string())?;
    let y_parity = if let Some(hex) = parity.strip_prefix("0x") {
        u8::from_str_radix(hex, 16).map_err(|_| "invalid parity".to_owned())?
    } else {
        parity
            .parse::<u8>()
            .map_err(|_| "invalid parity".to_owned())?
    };
    let y_parity = match y_parity {
        0 | 1 => 0x02 + y_parity,
        0x02 | 0x03 => y_parity,
        _ => {
            return Err("parity must be 0, 1, 2, or 3".to_owned());
        }
    };
    Ok(ExpectedEncryptionKey { x, y_parity })
}

impl fmt::Display for OperatorEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.name {
            Some(name) => write!(formatter, "{name}={}", self.url),
            None => formatter.write_str(&self.url),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use clap::Parser as _;

    use super::*;

    static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);

    fn check() -> Check {
        Check {
            config: None,
            zone_id: Some(7),
            zone_manifest: None,
            l1_rpc_url: Some("https://l1.example".to_owned()),
            zone_factory: None,
            portal: None,
            operator_rpcs: vec!["node-a=https://node-a.example".parse().unwrap()],
            observe_for: DEFAULT_OBSERVE_FOR,
            rpc_timeout: DEFAULT_RPC_TIMEOUT,
            require_sequencer_set_version: None,
            require_leader: None,
            require_encryption_key: None,
            json: false,
        }
    }

    #[test]
    fn cli_only_configuration_is_valid() {
        let config = check().effective_config().unwrap();
        assert_eq!(config.zone_id, 7);
        assert_eq!(config.nodes[0].name.as_deref(), Some("node-a"));
        assert_eq!(config.zone_factory, MODERATO_ZONE_FACTORY);
    }

    #[test]
    fn cli_values_override_file_and_cli_nodes_replace_file_nodes() {
        let sequence = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "tempo-xtask-admin-check-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let config_path = directory.join("admin.toml");
        std::fs::write(
            &config_path,
            r#"
[zone]
id = 6
manifest = "expected.toml"

[l1]
rpc_url = "https://file-l1.example"

[[nodes]]
name = "old"
operator_rpc_url = "https://old.example"
"#,
        )
        .unwrap();

        let mut command = check();
        command.config = Some(config_path);
        let config = command.effective_config().unwrap();
        assert_eq!(config.zone_id, 7);
        assert_eq!(config.l1_rpc_url, "https://l1.example");
        assert_eq!(
            config.nodes,
            vec!["node-a=https://node-a.example".parse().unwrap()]
        );
        assert_eq!(config.manifest, Some(directory.join("expected.toml")));

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn parses_supported_durations() {
        assert_eq!(parse_duration("0s").unwrap(), Duration::ZERO);
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert!(parse_nonzero_duration("0s").is_err());
    }

    #[test]
    fn parses_labeled_and_unlabeled_operator_endpoints() {
        let labeled: OperatorEndpoint = "node-a=https://node-a.example".parse().unwrap();
        assert_eq!(labeled.name.as_deref(), Some("node-a"));
        let unlabeled: OperatorEndpoint = "http://127.0.0.1:9000".parse().unwrap();
        assert_eq!(unlabeled.name, None);
    }

    #[test]
    fn invariant_details_describe_the_actual_outcome() {
        let mut results = Vec::new();
        add_check(
            &mut results,
            "example",
            true,
            "observed expected state".to_owned(),
            "observed a mismatch".to_owned(),
        );
        add_check(
            &mut results,
            "example",
            false,
            "observed expected state".to_owned(),
            "observed a mismatch".to_owned(),
        );

        assert_eq!(results[0].status, CheckStatus::Pass);
        assert_eq!(results[0].detail, "observed expected state");
        assert_eq!(results[1].status, CheckStatus::Fail);
        assert_eq!(results[1].detail, "observed a mismatch");
    }

    #[test]
    fn table_status_honors_column_width() {
        assert_eq!(format!("{:<8}", TableStatus::Pass), "PASS    ");
        assert_eq!(format!("{:<8}", TableStatus::Fail), "FAIL    ");
        assert_eq!(format!("{:<8}", TableStatus::NotAvailable), "N/A     ");
    }

    #[test]
    fn clap_accepts_emergency_cli_without_config_or_manifest() {
        let command = Check::try_parse_from([
            "check",
            "--zone-id",
            "7",
            "--l1-rpc-url",
            "https://l1.example",
            "--operator-rpc",
            "node-a=https://node-a.example",
            "--operator-rpc",
            "https://node-b.example",
            "--observe-for",
            "0s",
            "--json",
        ])
        .unwrap();
        assert_eq!(command.zone_id, Some(7));
        assert_eq!(command.operator_rpcs.len(), 2);
        assert_eq!(command.observe_for, Duration::ZERO);
        assert!(command.zone_manifest.is_none());
        assert!(command.json);
    }

    fn node_snapshot(name: &str, sequencer: SequencerInfoResponse) -> NodeSnapshot {
        NodeSnapshot {
            name: name.to_owned(),
            url: format!("http://{name}.example"),
            zone: Some(ZoneInfoResponse {
                zone_id: U64::from(1),
                is_access_enforced: false,
                is_gateway_open: false,
                zone_tokens: Vec::new(),
                sequencers: Vec::new(),
                chain_id: U64::from(1),
                tempo_block_number: U64::from(1),
            }),
            sequencer: Some(sequencer),
            common_block: None,
            error: None,
        }
    }

    fn sequencer_info(rpc_only: bool, ready_for_promotion: bool) -> SequencerInfoResponse {
        SequencerInfoResponse {
            mode: "multi".to_owned(),
            portal: Address::ZERO,
            manifest_zone_id: None,
            manifest_sequencer_set_version: None,
            manifest_membership_digest: None,
            decryption_keys: None,
            local: None,
            active_leader: None,
            local_tip: None,
            peers: vec![zone_rpc::types::SequencerPeerInfo {
                name: "node".to_owned(),
                sequencer_address: (!rpc_only).then_some(Address::ZERO),
                rpc_only,
                is_local: true,
                tip: None,
            }],
            progress: Some(zone_rpc::types::SequencerProgress {
                zone_height: U64::from(1),
                tempo_block_number: U64::from(1),
                latest_observed_leadership_epoch: None,
                locally_applied_leadership_epoch: None,
                pending_transitions: U64::from(0),
            }),
            readiness: Some(zone_rpc::types::SequencerReadiness {
                ready_for_promotion,
                reasons: if ready_for_promotion {
                    Vec::new()
                } else {
                    vec!["rpc-only nodes cannot get promoted".to_owned()]
                },
            }),
        }
    }

    fn with_local_tip(mut info: SequencerInfoResponse, height: u64) -> SequencerInfoResponse {
        info.local_tip = Some(zone_rpc::types::PeerTipInfo {
            zone_height: U64::from(height),
            zone_hash: B256::ZERO,
            tempo_block_number: U64::from(height),
            tempo_block_hash: B256::ZERO,
        });
        info
    }

    fn with_manifest(
        mut info: SequencerInfoResponse,
        zone_id: u32,
        version: u64,
        digest: B256,
    ) -> SequencerInfoResponse {
        info.manifest_zone_id = Some(U64::from(zone_id));
        info.manifest_sequencer_set_version = Some(U64::from(version));
        info.manifest_membership_digest = Some(digest);
        info
    }

    #[test]
    fn promotion_readiness_skips_rpc_only_nodes() {
        let leader = node_snapshot("leader", sequencer_info(false, true));
        let rpc = node_snapshot("rpc", sequencer_info(true, false));
        let live = [
            (
                &leader,
                leader.zone.as_ref().unwrap(),
                leader.sequencer.as_ref().unwrap(),
            ),
            (
                &rpc,
                rpc.zone.as_ref().unwrap(),
                rpc.sequencer.as_ref().unwrap(),
            ),
        ];

        let readiness = assess_readiness(&live);
        assert_eq!(readiness.sequencer_nodes, 1);
        assert_eq!(readiness.rpc_only_nodes, ["rpc"]);
        assert!(readiness.failures.is_empty());
    }

    #[test]
    fn zone_height_lag_allows_nodes_within_two_minutes() {
        let newest = node_snapshot("newest", with_local_tip(sequencer_info(false, true), 1_000));
        let lagging = node_snapshot(
            "lagging",
            with_local_tip(
                sequencer_info(false, true),
                1_000 - MAX_ZONE_HEIGHT_LAG_BLOCKS,
            ),
        );

        let result = zone_height_lag_invariant(&[newest, lagging]);
        assert_eq!(result.status, CheckStatus::Pass);
    }

    #[test]
    fn zone_height_lag_rejects_nodes_more_than_two_minutes_behind() {
        let newest = node_snapshot("newest", with_local_tip(sequencer_info(false, true), 1_000));
        let lagging = node_snapshot(
            "lagging",
            with_local_tip(
                sequencer_info(false, true),
                999 - MAX_ZONE_HEIGHT_LAG_BLOCKS,
            ),
        );

        let result = zone_height_lag_invariant(&[newest, lagging]);
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.contains("lagging at 759 (241 blocks behind)"));
    }

    #[test]
    fn loaded_manifest_agreement_detects_different_membership_digests() {
        let first = node_snapshot(
            "first",
            with_manifest(sequencer_info(false, true), 1, 7, B256::ZERO),
        );
        let second = node_snapshot(
            "second",
            with_manifest(sequencer_info(false, true), 1, 7, B256::from([1; 32])),
        );

        let result = loaded_manifest_agreement_invariant(1, 7, &[first, second]);
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.contains("second reports"));
    }

    #[test]
    fn loaded_manifest_agreement_accepts_matching_multi_node_manifests() {
        let first = node_snapshot(
            "first",
            with_manifest(sequencer_info(false, true), 1, 7, B256::ZERO),
        );
        let second = node_snapshot(
            "second",
            with_manifest(sequencer_info(false, true), 1, 7, B256::ZERO),
        );

        let result = loaded_manifest_agreement_invariant(1, 7, &[first, second]);
        assert_eq!(result.status, CheckStatus::Pass);
    }

    #[test]
    fn loaded_manifest_agreement_skips_single_node_mode() {
        let mut info = sequencer_info(false, true);
        info.mode = "single".to_owned();
        let result = loaded_manifest_agreement_invariant(1, 7, &[node_snapshot("single", info)]);
        assert_eq!(result.status, CheckStatus::Skipped);
    }
}
