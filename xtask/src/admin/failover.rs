//! External sequencer failover controller.

use std::{path::PathBuf, time::Duration};

use alloy::{
    primitives::{Address, B256},
    providers::{Provider, ProviderBuilder},
};
use eyre::{Context as _, ensure, eyre};
use serde::{Deserialize, Serialize};
use tempo_alloy::TempoNetwork;
use tokio::time::Instant;
use zone_p2p::{ManifestNode, ZoneManifest};
use zone_rpc::types::{LocalProductionInfo, SetLeaderResponse};

use super::{
    config::{EffectiveConfig, SharedAdminArgs, format_duration, parse_nonzero_duration},
    snapshot::{ClusterView, NodeSnapshot, RpcBlock},
};

const DEFAULT_DRAIN_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, clap::Parser)]
pub(crate) struct Failover {
    #[command(subcommand)]
    command: FailoverCommand,
}

#[derive(Debug, clap::Subcommand)]
enum FailoverCommand {
    /// Drain a healthy leader and prove its successor before the supervisor sends a signal.
    Drain(Drain),
}

impl Failover {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        match self.command {
            FailoverCommand::Drain(command) => command.run().await,
        }
    }
}

#[derive(Debug, clap::Parser)]
struct Drain {
    #[command(flatten)]
    shared: SharedAdminArgs,

    /// Submit the leader change. Without this flag, validate and select a candidate only.
    #[arg(long)]
    execute: bool,

    /// Total drain deadline. The controller never waits longer than 15 seconds.
    #[arg(long, default_value = "15s", value_parser = parse_nonzero_duration)]
    timeout: Duration,

    /// Durable operation record used to resume observation without changing the selected target.
    #[arg(long, default_value = ".zone-failover.json")]
    state_file: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Operation {
    zone_id: u32,
    portal: Address,
    expected_epoch: u64,
    baseline: LocalProductionInfo,
    checkpoint: LocalProductionInfo,
    outgoing_name: String,
    target_name: String,
    target: Address,
    #[serde(skip_serializing_if = "Option::is_none")]
    tx_hash: Option<B256>,
    phase: OperationPhase,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum OperationPhase {
    Prepared,
    Submitted,
    Complete,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DrainReport {
    ok: bool,
    dry_run: bool,
    outgoing: String,
    target: String,
    target_address: Address,
    checkpoint_height: u64,
    checkpoint_hash: B256,
    expected_epoch: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    tx_hash: Option<B256>,
}

impl Drain {
    async fn run(self) -> eyre::Result<()> {
        ensure!(
            self.timeout <= DEFAULT_DRAIN_TIMEOUT,
            "failover drain timeout must not exceed {}",
            format_duration(DEFAULT_DRAIN_TIMEOUT)
        );
        let deadline = Instant::now() + self.timeout;
        let config = self.shared.load()?;

        if self.execute && self.state_file.exists() {
            let operation = read_operation(&self.state_file)?;
            ensure!(
                operation.zone_id == config.zone_id,
                "persisted operation belongs to another Zone"
            );
            if let Some(portal) = config.portal {
                ensure!(
                    operation.portal == portal,
                    "persisted operation belongs to another Portal"
                );
            }
            return self.observe(config, operation, deadline).await;
        }

        let initial = collect(&config, self.shared.rpc_timeout, deadline).await?;
        let manifest = initial
            .manifest
            .as_ref()
            .ok_or_else(|| eyre!("automated failover requires --zone-manifest"))?;
        let outgoing = node_for_address(&initial, initial.portal.leader)
            .ok_or_else(|| eyre!("finalized leader has no reachable manifest operator endpoint"))?;
        let baseline = local_production(outgoing)?;

        let mut advanced = wait_for_advance(
            &config,
            self.shared.rpc_timeout,
            outgoing.name.as_str(),
            baseline,
            deadline,
        )
        .await?;
        let checkpoint = local_production(
            advanced
                .nodes
                .iter()
                .find(|node| node.name == outgoing.name)
                .ok_or_else(|| eyre!("outgoing leader disappeared during drain"))?,
        )?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        super::snapshot::query_common_blocks(
            &mut advanced.nodes,
            checkpoint.zone_height.to::<u64>(),
            self.shared.rpc_timeout.min(remaining),
        )
        .await;
        let (target_name, target_address) = select_candidate(&advanced, manifest)?;

        let report = DrainReport {
            ok: true,
            dry_run: !self.execute,
            outgoing: outgoing.name.clone(),
            target: target_name.clone(),
            target_address,
            checkpoint_height: checkpoint.zone_height.to::<u64>(),
            checkpoint_hash: checkpoint.zone_hash,
            expected_epoch: advanced.portal.leader_epoch.saturating_add(1),
            tx_hash: None,
        };
        if !self.execute {
            return print_report(self.shared.json, &report);
        }

        let mut operation = Operation {
            zone_id: config.zone_id,
            portal: advanced.portal.portal,
            expected_epoch: advanced.portal.leader_epoch.saturating_add(1),
            baseline,
            checkpoint,
            outgoing_name: outgoing.name.clone(),
            target_name,
            target: target_address,
            tx_hash: None,
            phase: OperationPhase::Prepared,
        };
        write_operation(&self.state_file, &operation)?;

        let leader = advanced
            .nodes
            .iter()
            .find(|node| node.name == outgoing.name)
            .ok_or_else(|| eyre!("outgoing leader endpoint is unavailable"))?;
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&leader.url)
            .await?;
        let response: SetLeaderResponse = tokio::time::timeout_at(
            deadline,
            provider.raw_request("zone_setLeader".into(), [operation.target]),
        )
        .await
        .map_err(|_| {
            eyre!("leader change invocation timed out; resume using the persisted operation")
        })?
        .wrap_err("leader change invocation is ambiguous; resume using the persisted operation")?;
        operation.tx_hash = response.tx_hash;
        operation.phase = OperationPhase::Submitted;
        write_operation(&self.state_file, &operation)?;
        self.observe(config, operation, deadline).await
    }

    async fn observe(
        &self,
        config: EffectiveConfig,
        mut operation: Operation,
        deadline: Instant,
    ) -> eyre::Result<()> {
        ensure!(
            operation.phase != OperationPhase::Complete,
            "failover operation is already complete"
        );
        loop {
            let view = collect(&config, self.shared.rpc_timeout, deadline).await?;
            if takeover_proven(&view, &operation).await? {
                operation.phase = OperationPhase::Complete;
                write_operation(&self.state_file, &operation)?;
                return print_report(
                    self.shared.json,
                    &DrainReport {
                        ok: true,
                        dry_run: false,
                        outgoing: operation.outgoing_name,
                        target: operation.target_name,
                        target_address: operation.target,
                        checkpoint_height: operation.checkpoint.zone_height.to::<u64>(),
                        checkpoint_hash: operation.checkpoint.zone_hash,
                        expected_epoch: operation.expected_epoch,
                        tx_hash: operation.tx_hash,
                    },
                );
            }
            ensure!(
                Instant::now() < deadline,
                "timed out waiting for successor production; supervisor may terminate the old process and start forced recovery"
            );
            tokio::time::sleep_until(deadline.min(Instant::now() + POLL_INTERVAL)).await;
        }
    }
}

async fn collect(
    config: &EffectiveConfig,
    rpc_timeout: Duration,
    deadline: Instant,
) -> eyre::Result<ClusterView> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(!remaining.is_zero(), "failover deadline expired");
    ClusterView::collect(config.clone(), rpc_timeout.min(remaining), |_| {}).await
}

async fn wait_for_advance(
    config: &EffectiveConfig,
    rpc_timeout: Duration,
    leader_name: &str,
    baseline: LocalProductionInfo,
    deadline: Instant,
) -> eyre::Result<ClusterView> {
    loop {
        let view = collect(config, rpc_timeout, deadline).await?;
        let leader = view
            .nodes
            .iter()
            .find(|node| node.name == leader_name)
            .ok_or_else(|| {
                eyre!("leader became unavailable; coordinated forced recovery is required")
            })?;
        let current = local_production(leader)
            .wrap_err("leader cannot prove post-trigger canonical production; coordinated forced recovery is required")?;
        if marker_advances(baseline, current) {
            return Ok(view);
        }
        ensure!(
            Instant::now() < deadline,
            "leader did not advance during the drain; coordinated forced recovery is required"
        );
        tokio::time::sleep_until(deadline.min(Instant::now() + POLL_INTERVAL)).await;
    }
}

fn select_candidate(
    view: &ClusterView,
    manifest: &ZoneManifest,
) -> eyre::Result<(String, Address)> {
    let leader = view.portal.leader;
    for member in manifest.nodes() {
        let Some(address) = member.secp256k1_address() else {
            continue;
        };
        if address == leader || member.is_rpc_only() || member.operator_rpc_url().is_none() {
            continue;
        }
        let Some(node) = view.nodes.iter().find(|node| node.name == member.name()) else {
            continue;
        };
        if candidate_ready(view, member, node) {
            return Ok((member.name().to_owned(), address));
        }
    }
    Err(eyre!(
        "no manifest-ordered candidate proved readiness and the outgoing checkpoint"
    ))
}

fn candidate_ready(view: &ClusterView, member: &ManifestNode, node: &NodeSnapshot) -> bool {
    let Some(info) = node.sequencer.as_ref() else {
        return false;
    };
    let Some(local) = info.local.as_ref() else {
        return false;
    };
    let Some(active) = info.active_leader.as_ref() else {
        return false;
    };
    let Some(readiness) = info.readiness.as_ref() else {
        return false;
    };
    let Some(checkpoint) = view
        .nodes
        .iter()
        .find(|candidate| candidate.name == node.name)
        .and_then(|candidate| candidate.common_block.as_ref())
    else {
        return false;
    };
    let Some(leader_node) = node_for_address(view, view.portal.leader) else {
        return false;
    };
    let Ok(produced) = local_production(leader_node) else {
        return false;
    };

    info.mode == "multi"
        && info.portal == view.portal.portal
        && info
            .manifest_zone_id
            .is_some_and(|id| id.to::<u32>() == view.config.zone_id)
        && info
            .manifest_sequencer_set_version
            .is_some_and(|version| version.to::<u64>() == view.portal.sequencer_set_version)
        && info.manifest_membership_digest
            == view.manifest.as_ref().map(ZoneManifest::membership_digest)
        && info.decryption_keys
            == leader_node
                .sequencer
                .as_ref()
                .and_then(|info| info.decryption_keys.clone())
        && local.name == member.name()
        && local.p2p_public_key == member.ed25519_public_key().to_string()
        && local.sequencer_address == member.secp256k1_address()
        && local.role == "follower"
        && active.sequencer_address == Some(view.portal.leader)
        && active.epoch.to::<u64>() == view.portal.leader_epoch
        && readiness.ready_for_promotion
        && info
            .progress
            .as_ref()
            .is_some_and(|progress| progress.pending_transitions.to::<u64>() == 0)
        && checkpoint.number == produced.zone_height
        && checkpoint.hash == produced.zone_hash
        && view.portal.sequencers.contains(
            &member
                .secp256k1_address()
                .expect("candidate is a quorum member"),
        )
}

async fn takeover_proven(view: &ClusterView, operation: &Operation) -> eyre::Result<bool> {
    if view.portal.leader != operation.target
        || view.portal.leader_epoch != operation.expected_epoch
    {
        return Ok(false);
    }
    let Some(node) = view
        .nodes
        .iter()
        .find(|node| node.name == operation.target_name)
    else {
        return Ok(false);
    };
    let Some(info) = node.sequencer.as_ref() else {
        return Ok(false);
    };
    let Some(local) = info.local.as_ref() else {
        return Ok(false);
    };
    let Some(produced) = info.last_locally_produced.as_ref() else {
        return Ok(false);
    };
    if local.role != "leader"
        || produced.zone_height <= operation.checkpoint.zone_height
        || produced.tempo_block_number < view.portal.leader_activation_tempo_block
    {
        return Ok(false);
    }
    let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
        .connect(&node.url)
        .await?;
    let block: Option<RpcBlock> = provider
        .raw_request(
            "eth_getBlockByNumber".into(),
            (
                format!("0x{:x}", operation.checkpoint.zone_height.to::<u64>()),
                false,
            ),
        )
        .await?;
    Ok(block.is_some_and(|block| block.hash == operation.checkpoint.zone_hash))
}

fn marker_advances(baseline: LocalProductionInfo, current: LocalProductionInfo) -> bool {
    current.zone_height > baseline.zone_height
        && current.tempo_block_number > baseline.tempo_block_number
}

fn local_production(node: &NodeSnapshot) -> eyre::Result<LocalProductionInfo> {
    node.sequencer
        .as_ref()
        .and_then(|info| info.last_locally_produced)
        .ok_or_else(|| eyre!("{} has no canonical local-production marker", node.name))
}

fn node_for_address(view: &ClusterView, address: Address) -> Option<&NodeSnapshot> {
    view.nodes.iter().find(|node| {
        node.sequencer
            .as_ref()
            .and_then(|info| info.local.as_ref())
            .and_then(|local| local.sequencer_address)
            == Some(address)
    })
}

fn read_operation(path: &PathBuf) -> eyre::Result<Operation> {
    serde_json::from_slice(&std::fs::read(path).wrap_err("failed reading failover state")?)
        .wrap_err("failed parsing failover state")
}

fn write_operation(path: &PathBuf, operation: &Operation) -> eyre::Result<()> {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(operation)?)?;
    std::fs::rename(temporary, path).wrap_err("failed persisting failover state")
}

fn print_report(json: bool, report: &DrainReport) -> eyre::Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(report)?);
    } else {
        println!("Failover drain ready");
        println!("  Outgoing:   {}", report.outgoing);
        println!(
            "  Successor:  {} ({})",
            report.target, report.target_address
        );
        println!(
            "  Checkpoint: {} ({})",
            report.checkpoint_height, report.checkpoint_hash
        );
        println!("  Epoch:      {}", report.expected_epoch);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{B256, U64};

    use super::*;

    fn marker(tempo: u64, height: u64, hash: u8) -> LocalProductionInfo {
        LocalProductionInfo {
            tempo_block_number: U64::from(tempo),
            zone_height: U64::from(height),
            zone_hash: B256::with_last_byte(hash),
        }
    }

    #[test]
    fn production_must_advance_both_coordinates() {
        let baseline = marker(10, 20, 1);
        assert!(!marker_advances(baseline, marker(11, 20, 2)));
        assert!(!marker_advances(baseline, marker(10, 21, 2)));
        assert!(marker_advances(baseline, marker(11, 21, 2)));
    }

    #[test]
    fn persisted_target_round_trips_without_reselection() {
        let operation = Operation {
            zone_id: 1,
            portal: Address::with_last_byte(1),
            expected_epoch: 4,
            baseline: marker(10, 20, 1),
            checkpoint: marker(11, 21, 2),
            outgoing_name: "a".to_owned(),
            target_name: "b".to_owned(),
            target: Address::with_last_byte(2),
            tx_hash: Some(B256::with_last_byte(3)),
            phase: OperationPhase::Submitted,
        };
        let encoded = serde_json::to_vec(&operation).unwrap();
        let decoded: Operation = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.target, operation.target);
        assert_eq!(decoded.tx_hash, operation.tx_hash);
        assert_eq!(decoded.phase, OperationPhase::Submitted);
    }
}
