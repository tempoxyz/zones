//! Guarded `zone_setLeader` handoff.

use std::{fmt, time::Duration};

use alloy::{
    primitives::Address,
    providers::{Provider, ProviderBuilder},
};
use eyre::{Context as _, ensure, eyre};
use serde::Serialize;
use tempo_alloy::TempoNetwork;
use zone_rpc::types::SetLeaderResponse;

use super::{
    config::{SharedAdminArgs, format_duration, parse_nonzero_duration},
    invariants::{
        eligible_relayers, evaluate_base_invariants, is_rpc_only, l1_batch_invariant, resolve_node,
    },
    snapshot::{ClusterView, NodeSnapshot},
};

const DEFAULT_HANDOFF_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const HANDOFF_POLL: Duration = Duration::from_millis(500);

#[derive(Debug, clap::Parser)]
pub(crate) struct Leader {
    #[command(subcommand)]
    command: LeaderCommand,
}

#[derive(Debug, clap::Subcommand)]
enum LeaderCommand {
    /// Move finalized leadership to a promotion-ready sequencer.
    Set(LeaderSet),
}

impl Leader {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        match self.command {
            LeaderCommand::Set(command) => command.run().await,
        }
    }
}

#[derive(Debug, clap::Parser)]
struct LeaderSet {
    #[command(flatten)]
    shared: SharedAdminArgs,

    /// Target leader, as a node name or individual sequencer address.
    #[arg(long)]
    target: String,

    /// Operator RPC that relays `zone_setLeader`. Optional when exactly one relayer is eligible.
    #[arg(long)]
    via: Option<String>,

    /// Submit the leadership transaction. Without this flag the command is a dry run.
    #[arg(long)]
    execute: bool,

    /// Deadline for finalized leader agreement. Defaults to 5m.
    #[arg(long, value_parser = parse_nonzero_duration)]
    timeout: Option<Duration>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LeaderSetReport {
    ok: bool,
    dry_run: bool,
    submitted: bool,
    via: String,
    relayer: Address,
    target: Address,
    target_name: String,
    current_leader: Address,
    current_epoch: u64,
    expected_epoch: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    tx_hash: Option<alloy::primitives::B256>,
}

impl LeaderSet {
    async fn run(self) -> eyre::Result<()> {
        progress("Loading and validating configuration...");
        let config = self.shared.load()?;
        let view = ClusterView::collect(config, self.shared.rpc_timeout, |message| {
            progress(message);
        })
        .await?;

        progress("Evaluating cluster preflight...");
        let mut invariants = evaluate_base_invariants(
            &view.config,
            view.manifest.as_ref(),
            &view.portal,
            &view.nodes,
            None,
            None,
            None,
        );
        invariants.push(l1_batch_invariant(&view.portal));
        let failed = invariants
            .iter()
            .filter(|result| result.required_failed())
            .map(|result| format!("{}: {}", result.name, result.detail))
            .collect::<Vec<_>>();
        ensure!(
            failed.is_empty(),
            "cluster preflight failed; refusing leader handoff: {}",
            failed.join("; ")
        );

        let target_node = resolve_node(&view.nodes, &self.target).ok_or_else(|| {
            eyre!(
                "target `{}` does not match a reachable operator node",
                self.target
            )
        })?;
        let target_info = target_node
            .sequencer
            .as_ref()
            .ok_or_else(|| eyre!("target {} has no sequencer status", target_node.name))?;
        ensure!(
            is_rpc_only(target_info) != Some(true),
            "target {} is rpc-only and cannot become leader",
            target_node.name
        );
        let target_address = target_info
            .local
            .as_ref()
            .and_then(|local| local.sequencer_address)
            .or_else(|| {
                target_info
                    .peers
                    .iter()
                    .find(|peer| peer.is_local)
                    .and_then(|peer| peer.sequencer_address)
            })
            .ok_or_else(|| {
                eyre!(
                    "target {} has no individual sequencer address",
                    target_node.name
                )
            })?;
        ensure!(
            view.portal.sequencers.contains(&target_address),
            "target {} ({target_address}) is not a registered Portal sequencer",
            target_node.name
        );
        let ready = target_info
            .readiness
            .as_ref()
            .zip(target_info.progress.as_ref())
            .is_some_and(|(readiness, progress)| {
                readiness.ready_for_promotion && progress.pending_transitions.to::<u64>() == 0
            });
        ensure!(
            ready,
            "target {} is not promotion-ready or has pending transitions",
            target_node.name
        );
        ensure!(
            target_node.common_block.is_some() || target_node.latest_block.is_some(),
            "target {} is not canonical",
            target_node.name
        );

        let via_node = select_via(&view.nodes, self.via.as_deref())?;
        let via_info = via_node.sequencer.as_ref().ok_or_else(|| {
            eyre!(
                "submission endpoint {} has no sequencer status",
                via_node.name
            )
        })?;
        let relayer = via_info
            .local
            .as_ref()
            .and_then(|local| local.sequencer_address)
            .ok_or_else(|| {
                eyre!(
                    "submission endpoint {} does not hold an individual relayer key",
                    via_node.name
                )
            })?;
        ensure!(
            is_rpc_only(via_info) != Some(true),
            "submission endpoint {} is rpc-only and cannot relay setLeader",
            via_node.name
        );

        let already_active = view.portal.leader == target_address;
        let expected_epoch = if already_active {
            view.portal.leader_epoch
        } else {
            view.portal.leader_epoch.saturating_add(1)
        };

        if !self.execute || already_active {
            let report = LeaderSetReport {
                ok: true,
                dry_run: !self.execute,
                submitted: false,
                via: via_node.name.clone(),
                relayer,
                target: target_address,
                target_name: target_node.name.clone(),
                current_leader: view.portal.leader,
                current_epoch: view.portal.leader_epoch,
                expected_epoch,
                tx_hash: None,
            };
            return self.print_report(report);
        }

        progress(format!(
            "Calling zone_setLeader on {} for target {} ({target_address})...",
            via_node.name, target_node.name
        ));
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&via_node.url)
            .await?;
        let response: SetLeaderResponse = provider
            .raw_request("zone_setLeader".into(), [target_address])
            .await
            .wrap_err("zone_setLeader failed")?;

        let timeout = self.timeout.unwrap_or(DEFAULT_HANDOFF_TIMEOUT);
        let deadline = std::time::Instant::now() + timeout;
        let initial_height = view.portal.zone_height;
        loop {
            let later =
                ClusterView::collect(view.config.clone(), self.shared.rpc_timeout, |message| {
                    progress(message)
                })
                .await?;
            let leader_ok = later.portal.leader == target_address
                && later.portal.leader_epoch == expected_epoch;
            let nodes_agree = later.nodes.iter().all(|node| {
                node.sequencer
                    .as_ref()
                    .and_then(|info| info.active_leader.as_ref())
                    .is_some_and(|leader| {
                        leader.sequencer_address == Some(target_address)
                            && leader.epoch.to::<u64>() == expected_epoch
                    })
            });
            let progressed = later.portal.zone_height > initial_height;
            if leader_ok && nodes_agree && progressed {
                return self.print_report(LeaderSetReport {
                    ok: true,
                    dry_run: false,
                    submitted: response.tx_hash.is_some(),
                    via: via_node.name.clone(),
                    relayer: response.relayer,
                    target: target_address,
                    target_name: target_node.name.clone(),
                    current_leader: later.portal.leader,
                    current_epoch: later.portal.leader_epoch,
                    expected_epoch,
                    tx_hash: response.tx_hash,
                });
            }
            if std::time::Instant::now() >= deadline {
                return Err(eyre!(
                    "timed out after {} waiting for leader {} at epoch {expected_epoch}",
                    format_duration(timeout),
                    target_node.name
                ));
            }
            progress(format!(
                "Waiting for finalized leader {} epoch {expected_epoch}...",
                target_node.name
            ));
            tokio::time::sleep(HANDOFF_POLL).await;
        }
    }

    fn print_report(&self, report: LeaderSetReport) -> eyre::Result<()> {
        if self.shared.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!(
                "{}",
                if report.dry_run {
                    "Leader handoff dry run"
                } else if report.submitted {
                    "Leader handoff submitted"
                } else {
                    "Leader already active"
                }
            );
            println!("  Target:  {} ({})", report.target_name, report.target);
            println!("  Via:     {} (relayer {})", report.via, report.relayer);
            println!(
                "  Current: {} epoch {}",
                report.current_leader, report.current_epoch
            );
            println!("  Expected epoch: {}", report.expected_epoch);
            if let Some(tx_hash) = report.tx_hash {
                println!("  Transaction: {tx_hash}");
            }
        }
        Ok(())
    }
}

fn select_via<'a>(nodes: &'a [NodeSnapshot], via: Option<&str>) -> eyre::Result<&'a NodeSnapshot> {
    if let Some(via) = via {
        return resolve_node(nodes, via)
            .ok_or_else(|| eyre!("--via `{via}` does not match a reachable operator node"));
    }
    let relayers = eligible_relayers(nodes);
    match relayers.as_slice() {
        [only] => {
            progress(format!(
                "Selected unique relayer {} for zone_setLeader.",
                only.name
            ));
            Ok(*only)
        }
        [] => Err(eyre!(
            "no reachable node holds an individual relayer key; pass --via"
        )),
        _ => Err(eyre!(
            "multiple relayers are eligible ({}); pass --via",
            relayers
                .iter()
                .map(|node| node.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn progress(message: impl fmt::Display) {
    eprintln!("[admin leader set] {message}");
}

#[cfg(test)]
mod tests {
    use super::select_via;
    use crate::admin::snapshot::{test_node_snapshot, test_sequencer_info};

    #[test]
    fn omits_via_when_exactly_one_relayer_is_eligible() {
        let leader = test_node_snapshot("node-a", test_sequencer_info(false, true));
        let rpc = test_node_snapshot("rpc", test_sequencer_info(true, false));
        let nodes = [leader, rpc];
        let selected = select_via(&nodes, None).unwrap();
        assert_eq!(selected.name, "node-a");
    }

    #[test]
    fn refuses_ambiguous_relayer_selection() {
        let a = test_node_snapshot("node-a", test_sequencer_info(false, true));
        let b = test_node_snapshot("node-b", test_sequencer_info(false, true));
        let err = select_via(&[a, b], None).unwrap_err();
        assert!(err.to_string().contains("pass --via"));
    }
}
