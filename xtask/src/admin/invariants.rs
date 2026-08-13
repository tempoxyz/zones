//! Named cluster health and safety invariants.

use std::collections::BTreeSet;

use alloy::primitives::{Address, B256};
use serde::Serialize;
use zone_p2p::ZoneManifest;
use zone_rpc::types::{SequencerInfoResponse, ZoneInfoResponse};

use super::{
    config::{EffectiveConfig, ExpectedEncryptionKey},
    snapshot::{NodeSnapshot, PortalSnapshot},
};

// Two minutes at Tempo's expected 500 ms block time.
pub(crate) const MAX_ZONE_HEIGHT_LAG_BLOCKS: u64 = 240;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CheckStatus {
    Pass,
    Fail,
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct InvariantResult {
    pub name: &'static str,
    pub status: CheckStatus,
    pub detail: String,
}

impl InvariantResult {
    pub(crate) fn required_failed(&self) -> bool {
        self.status == CheckStatus::Fail
    }
}

#[derive(Debug, Clone)]
pub(crate) struct InvariantInputs<'a> {
    pub config: &'a EffectiveConfig,
    pub manifest: Option<&'a ZoneManifest>,
    pub portal: &'a PortalSnapshot,
    pub nodes: &'a [NodeSnapshot],
    pub required_version: Option<u64>,
    pub required_leader: Option<&'a str>,
    pub required_key: Option<ExpectedEncryptionKey>,
}

pub(crate) fn evaluate_invariants(inputs: InvariantInputs<'_>) -> Vec<InvariantResult> {
    evaluate_base_invariants(
        inputs.config,
        inputs.manifest,
        inputs.portal,
        inputs.nodes,
        inputs.required_version,
        inputs.required_leader,
        inputs.required_key,
    )
}

pub(crate) fn evaluate_base_invariants(
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
            let report = decryption_key_report(nodes, |keys| {
                has_candidate_or_bound(keys, active_key.x, active_key.y_parity)
            });
            add_check(
                &mut results,
                "decryption_key_availability",
                report.failures.is_empty(),
                format!(
                    "all {} sequencer node(s) report active Portal key x={} parity={}; {}",
                    report.sequencer_nodes,
                    active_key.x,
                    active_key.y_parity,
                    report.excluded_detail
                ),
                report.failures.join("; "),
            );
        }
        None => results.push(InvariantResult {
            name: "decryption_key_availability",
            status: CheckStatus::Skipped,
            detail: "Portal has no active encryption key".to_owned(),
        }),
    }

    results.push(canonical_state_invariant(nodes));
    results.push(zone_height_lag_invariant(
        nodes,
        portal.finalized_block_number,
    ));

    if let Some(manifest) = manifest {
        append_manifest_invariants(&mut results, config, manifest, portal, nodes, &live_nodes);
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

/// Require every non-RPC-only sequencer to have each supplied public key loaded
/// as either an unbound candidate or a Portal-bound decryption key.
pub(crate) fn required_decryption_keys_invariant(
    nodes: &[NodeSnapshot],
    required: &[ExpectedEncryptionKey],
) -> InvariantResult {
    let report = decryption_key_report(nodes, |keys| {
        required
            .iter()
            .all(|key| has_candidate_or_bound(keys, key.x, key.y_parity))
    });
    let expected = required
        .iter()
        .map(|key| format!("x={} parity={}", key.x, key.y_parity))
        .collect::<Vec<_>>()
        .join(", ");
    InvariantResult {
        name: "required_decryption_keys",
        status: if report.failures.is_empty() && report.sequencer_nodes > 0 {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: if report.failures.is_empty() && report.sequencer_nodes > 0 {
            format!(
                "all {} sequencer node(s) report required keys {expected}; {}",
                report.sequencer_nodes, report.excluded_detail
            )
        } else {
            report.failures.join("; ")
        },
    }
}

pub(crate) fn evaluate_node_ready(
    node_name: &str,
    portal: &PortalSnapshot,
    nodes: &[NodeSnapshot],
) -> Vec<InvariantResult> {
    let mut results = Vec::new();
    let Some(node) = nodes.iter().find(|node| node.name == node_name) else {
        add_check(
            &mut results,
            "wait_node_present",
            false,
            String::new(),
            format!("no configured operator RPC is named {node_name}"),
        );
        return results;
    };
    add_check(
        &mut results,
        "wait_node_reachable",
        node.error.is_none() && node.sequencer.is_some(),
        format!("{node_name} is reachable"),
        format!(
            "{node_name} is unreachable: {}",
            node.error
                .as_deref()
                .unwrap_or("incomplete sequencer snapshot")
        ),
    );
    let Some(info) = node.sequencer.as_ref() else {
        return results;
    };
    if is_rpc_only(info) == Some(true) {
        results.push(InvariantResult {
            name: "wait_node_ready",
            status: CheckStatus::Skipped,
            detail: format!("{node_name} is rpc-only; reachability is sufficient"),
        });
        return results;
    }
    let ready = info
        .readiness
        .as_ref()
        .zip(info.progress.as_ref())
        .is_some_and(|(readiness, progress)| {
            readiness.ready_for_promotion && progress.pending_transitions.to::<u64>() == 0
        });
    add_check(
        &mut results,
        "wait_node_promotion_ready",
        ready,
        format!("{node_name} is promotion-ready with no pending transitions"),
        format!(
            "{node_name} is not promotion-ready: ready={:?} pending={:?}",
            info.readiness
                .as_ref()
                .map(|readiness| readiness.ready_for_promotion),
            info.progress
                .as_ref()
                .map(|progress| progress.pending_transitions)
        ),
    );
    let canonical = node.common_block.as_ref().is_some_and(|block| {
        nodes
            .iter()
            .filter_map(|other| other.common_block.as_ref())
            .all(|other| other.hash == block.hash && other.state_root == block.state_root)
    }) || node.latest_block.is_some();
    add_check(
        &mut results,
        "wait_node_canonical",
        canonical,
        format!("{node_name} reports canonical Zone state"),
        format!("{node_name} has no canonical block at the cluster common height"),
    );
    let _ = portal;
    results
}

pub(crate) fn l1_batch_invariant(portal: &PortalSnapshot) -> InvariantResult {
    InvariantResult {
        name: "l1_batch",
        status: CheckStatus::Pass,
        detail: format!(
            "finalized L1 batch {} settles Zone height {} at Tempo block {}",
            portal.withdrawal_batch_index, portal.zone_height, portal.finalized_block_number
        ),
    }
}

pub(crate) fn loaded_manifest_agreement_invariant(
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

pub(crate) fn zone_height_lag_invariant(
    nodes: &[NodeSnapshot],
    finalized_l1_block: u64,
) -> InvariantResult {
    let heights = nodes
        .iter()
        .filter_map(|node| {
            node.sequencer.as_ref()?.local_tip.as_ref().map(|tip| {
                (
                    node,
                    tip.zone_height.to::<u64>(),
                    tip.tempo_block_number.to::<u64>(),
                )
            })
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
        .map(|(_, height, _)| *height)
        .max()
        .expect("heights includes every configured node");
    let lagging = heights
        .iter()
        .filter_map(|(node, height, _)| {
            let lag = newest_height - *height;
            (lag > MAX_ZONE_HEIGHT_LAG_BLOCKS)
                .then(|| format!("{} at {} ({lag} blocks behind)", node.name, height))
        })
        .collect::<Vec<_>>();
    let stale = heights
        .iter()
        .filter_map(|(node, _, tempo_block)| {
            let lag = finalized_l1_block.saturating_sub(*tempo_block);
            (lag > MAX_ZONE_HEIGHT_LAG_BLOCKS).then(|| {
                format!(
                    "{} at Tempo block {} ({lag} blocks behind finalized L1)",
                    node.name, tempo_block
                )
            })
        })
        .collect::<Vec<_>>();
    let ok = lagging.is_empty() && stale.is_empty();
    InvariantResult {
        name: "zone_height_lag",
        status: if ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: if ok {
            format!(
                "all nodes are within {MAX_ZONE_HEIGHT_LAG_BLOCKS} Zone blocks of newest height {newest_height} and Tempo blocks of finalized L1 block {finalized_l1_block}"
            )
        } else {
            format!(
                "newest Zone height {newest_height}; finalized L1 block {finalized_l1_block}; allowed lag {MAX_ZONE_HEIGHT_LAG_BLOCKS} blocks; Zone-lagging: {}; stale versus L1: {}",
                lagging.join("; "),
                stale.join("; ")
            )
        },
    }
}

pub(crate) fn zone_height_invariant(
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

pub(crate) fn canonical_state_invariant(nodes: &[NodeSnapshot]) -> InvariantResult {
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
    InvariantResult {
        name: "canonical_state",
        status: if canonical_ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: if canonical_ok {
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
            )
        } else {
            canonical_failure
        },
    }
}

fn append_manifest_invariants(
    results: &mut Vec<InvariantResult>,
    config: &EffectiveConfig,
    manifest: &ZoneManifest,
    portal: &PortalSnapshot,
    nodes: &[NodeSnapshot],
    live_nodes: &[(&NodeSnapshot, &ZoneInfoResponse, &SequencerInfoResponse)],
) {
    let portal_members = address_set(&portal.sequencers);
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
        results,
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
        results,
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
        results,
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
        results,
        "manifest_membership",
        address_set(&manifest_members) == portal_members,
        "expected manifest quorum matches finalized Portal membership".to_owned(),
        format!(
            "manifest quorum {:?} differs from Portal quorum {:?}",
            address_set(&manifest_members),
            portal_members
        ),
    );
    results.push(manifest_node_identity_invariant(manifest, nodes));
}

fn manifest_node_identity_invariant(
    manifest: &ZoneManifest,
    nodes: &[NodeSnapshot],
) -> InvariantResult {
    let mut unmatched_manifest_nodes = manifest.nodes().iter().collect::<Vec<_>>();
    let mut unexpected = Vec::new();
    for node in nodes {
        let Some(local) = node.sequencer.as_ref().and_then(|info| info.local.as_ref()) else {
            unexpected.push(format!("{}=<local identity unavailable>", node.name));
            continue;
        };
        let matching_index = unmatched_manifest_nodes.iter().position(|manifest_node| {
            manifest_node.name() == local.name
                && manifest_node.ed25519_public_key().to_string() == local.p2p_public_key
                && manifest_node.secp256k1_address() == local.sequencer_address
        });
        if let Some(index) = matching_index {
            unmatched_manifest_nodes.remove(index);
        } else {
            unexpected.push(format!("{}={local:?}", node.name));
        }
    }

    let missing = unmatched_manifest_nodes
        .iter()
        .map(|node| node.name())
        .collect::<Vec<_>>();
    let ok = unexpected.is_empty() && missing.is_empty();
    InvariantResult {
        name: "manifest_node_identity",
        status: if ok {
            CheckStatus::Pass
        } else {
            CheckStatus::Fail
        },
        detail: if ok {
            "every manifest node was queried exactly once with the expected local identity"
                .to_owned()
        } else {
            format!(
                "queried identities do not match the manifest exactly once; missing: {}; unexpected or duplicate: {}",
                missing.join(", "),
                unexpected.join("; ")
            )
        },
    }
}

pub(crate) fn add_check(
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

pub(crate) fn address_set(addresses: &[Address]) -> BTreeSet<Address> {
    addresses.iter().copied().collect()
}

pub(crate) fn topology(info: &SequencerInfoResponse) -> Vec<(String, Option<Address>, bool)> {
    let mut topology = info
        .peers
        .iter()
        .map(|peer| (peer.name.clone(), peer.sequencer_address, peer.rpc_only))
        .collect::<Vec<_>>();
    topology.sort_by(|a, b| a.0.cmp(&b.0));
    topology
}

pub(crate) fn is_rpc_only(info: &SequencerInfoResponse) -> Option<bool> {
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

struct KeyReport {
    sequencer_nodes: usize,
    excluded_detail: String,
    failures: Vec<String>,
}

fn decryption_key_report(
    nodes: &[NodeSnapshot],
    predicate: impl Fn(&zone_rpc::types::DecryptionKeyStatus) -> bool,
) -> KeyReport {
    let mut sequencer_nodes = 0_usize;
    let mut rpc_only_nodes = Vec::new();
    let mut failures = Vec::new();
    for node in nodes {
        let Some(info) = node.sequencer.as_ref() else {
            failures.push(format!("{} has no sequencer status", node.name));
            continue;
        };
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
        let Some(keys) = info.decryption_keys.as_ref() else {
            failures.push(format!(
                "{} does not expose decryption-key status",
                node.name
            ));
            continue;
        };
        if !predicate(keys) {
            failures.push(format!(
                "{} is missing the required key ({} candidate(s), {} bound key(s) reported)",
                node.name,
                keys.candidates.len(),
                keys.bound.len()
            ));
        }
    }
    if sequencer_nodes == 0 {
        failures.push("no non-rpc-only sequencer nodes were checked".to_owned());
    }
    KeyReport {
        sequencer_nodes,
        excluded_detail: if rpc_only_nodes.is_empty() {
            "no rpc-only nodes excluded".to_owned()
        } else {
            format!("rpc-only excluded: {}", rpc_only_nodes.join(", "))
        },
        failures,
    }
}

fn has_candidate_or_bound(
    keys: &zone_rpc::types::DecryptionKeyStatus,
    x: B256,
    y_parity: u8,
) -> bool {
    keys.candidates
        .iter()
        .any(|candidate| candidate.x == x && candidate.y_parity == y_parity)
        || keys
            .bound
            .iter()
            .any(|bound| bound.x == x && bound.y_parity == y_parity)
}

pub(crate) fn eligible_relayers(nodes: &[NodeSnapshot]) -> Vec<&NodeSnapshot> {
    nodes
        .iter()
        .filter(|node| {
            node.error.is_none()
                && node.sequencer.as_ref().is_some_and(|info| {
                    is_rpc_only(info) == Some(false)
                        && info
                            .local
                            .as_ref()
                            .and_then(|local| local.sequencer_address)
                            .is_some()
                })
        })
        .collect()
}

pub(crate) fn resolve_node<'a>(
    nodes: &'a [NodeSnapshot],
    target: &str,
) -> Option<&'a NodeSnapshot> {
    if let Ok(address) = target.parse::<Address>() {
        return nodes.iter().find(|node| {
            node.sequencer
                .as_ref()
                .and_then(|info| info.local.as_ref())
                .and_then(|local| local.sequencer_address)
                == Some(address)
                || node
                    .sequencer
                    .as_ref()
                    .and_then(|info| {
                        info.peers
                            .iter()
                            .find(|peer| peer.sequencer_address == Some(address) && peer.is_local)
                    })
                    .is_some()
        });
    }
    nodes.iter().find(|node| {
        node.name == target
            || node
                .sequencer
                .as_ref()
                .and_then(|info| info.local.as_ref())
                .is_some_and(|local| local.name == target)
    })
}

#[cfg(test)]
mod tests {
    use alloy::primitives::{Address, B256, U64};

    use super::*;
    use crate::admin::snapshot::{test_node_snapshot, test_sequencer_info};

    fn with_local_tip(info: SequencerInfoResponse, height: u64) -> SequencerInfoResponse {
        with_local_tip_at(info, height, height)
    }

    fn with_local_tip_at(
        mut info: SequencerInfoResponse,
        zone_height: u64,
        tempo_block_number: u64,
    ) -> SequencerInfoResponse {
        info.local_tip = Some(zone_rpc::types::PeerTipInfo {
            zone_height: U64::from(zone_height),
            zone_hash: B256::ZERO,
            tempo_block_number: U64::from(tempo_block_number),
            tempo_block_hash: B256::ZERO,
        });
        info
    }

    fn with_local_identity(
        mut info: SequencerInfoResponse,
        name: &str,
        p2p_public_key: &str,
        sequencer_address: Address,
    ) -> SequencerInfoResponse {
        info.local = Some(zone_rpc::types::LocalSequencerInfo {
            name: name.to_owned(),
            sequencer_address: Some(sequencer_address),
            p2p_public_key: p2p_public_key.to_owned(),
            role: "follower".to_owned(),
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
    fn promotion_readiness_skips_rpc_only_nodes() {
        let leader = test_node_snapshot("leader", test_sequencer_info(false, true));
        let rpc = test_node_snapshot("rpc", test_sequencer_info(true, false));
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
        let newest = test_node_snapshot(
            "newest",
            with_local_tip(test_sequencer_info(false, true), 1_000),
        );
        let lagging = test_node_snapshot(
            "lagging",
            with_local_tip(
                test_sequencer_info(false, true),
                1_000 - MAX_ZONE_HEIGHT_LAG_BLOCKS,
            ),
        );

        let result = zone_height_lag_invariant(&[newest, lagging], 1_000);
        assert_eq!(result.status, CheckStatus::Pass);
    }

    #[test]
    fn zone_height_lag_rejects_nodes_more_than_two_minutes_behind() {
        let newest = test_node_snapshot(
            "newest",
            with_local_tip(test_sequencer_info(false, true), 1_000),
        );
        let lagging = test_node_snapshot(
            "lagging",
            with_local_tip(
                test_sequencer_info(false, true),
                999 - MAX_ZONE_HEIGHT_LAG_BLOCKS,
            ),
        );

        let result = zone_height_lag_invariant(&[newest, lagging], 1_000);
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.contains("lagging at 759 (241 blocks behind)"));
    }

    #[test]
    fn zone_height_lag_rejects_cluster_stale_against_finalized_l1() {
        let first = test_node_snapshot(
            "first",
            with_local_tip_at(test_sequencer_info(false, true), 1_000, 1_000),
        );
        let second = test_node_snapshot(
            "second",
            with_local_tip_at(test_sequencer_info(false, true), 1_000, 1_000),
        );

        let result = zone_height_lag_invariant(&[first, second], 1_241);
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.contains("241 blocks behind finalized L1"));
    }

    #[test]
    fn manifest_node_identity_rejects_duplicate_response_and_missing_node() {
        const KEY_A: &str = "0xd9231a8b155d9314344dccadf785e8a1a8967a42a0b2fcfdc909ae1567fbfc0d";
        const KEY_B: &str = "0xecfd7c2a20eb8065b538d9f6d8c478bae6d1a5eecdcc5ad639aa69e369adf8fb";
        const KEY_C: &str = "0x12966966abce829c60dca65d648788f70a08b58b62a15fc7e41ab890024ebfa3";
        let manifest = ZoneManifest::parse(&format!(
            r#"
zone_id = 1
leader_ed25519_public_key = "{KEY_A}"

[[nodes]]
name = "node-a"
ed25519_public_key = "{KEY_A}"
secp256k1_address = "0x0000000000000000000000000000000000000001"
address = "node-a.example:9200"

[[nodes]]
name = "node-b"
ed25519_public_key = "{KEY_B}"
secp256k1_address = "0x0000000000000000000000000000000000000002"
address = "node-b.example:9200"

[[nodes]]
name = "node-c"
ed25519_public_key = "{KEY_C}"
secp256k1_address = "0x0000000000000000000000000000000000000003"
address = "node-c.example:9200"
"#
        ))
        .unwrap();
        let key_a = manifest.nodes()[0].ed25519_public_key().to_string();
        let address_a = manifest.nodes()[0].secp256k1_address().unwrap();
        let key_c = manifest.nodes()[2].ed25519_public_key().to_string();
        let address_c = manifest.nodes()[2].secp256k1_address().unwrap();
        let first_a = test_node_snapshot(
            "endpoint-a",
            with_local_identity(
                test_sequencer_info(false, true),
                "node-a",
                &key_a,
                address_a,
            ),
        );
        let duplicate_a = test_node_snapshot(
            "endpoint-b",
            with_local_identity(
                test_sequencer_info(false, true),
                "node-a",
                &key_a,
                address_a,
            ),
        );
        let node_c = test_node_snapshot(
            "endpoint-c",
            with_local_identity(
                test_sequencer_info(false, true),
                "node-c",
                &key_c,
                address_c,
            ),
        );

        let result = manifest_node_identity_invariant(&manifest, &[first_a, duplicate_a, node_c]);
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.contains("missing: node-b"));
        assert!(
            result
                .detail
                .contains("unexpected or duplicate: endpoint-b=")
        );
    }

    #[test]
    fn loaded_manifest_agreement_detects_different_membership_digests() {
        let first = test_node_snapshot(
            "first",
            with_manifest(test_sequencer_info(false, true), 1, 7, B256::ZERO),
        );
        let second = test_node_snapshot(
            "second",
            with_manifest(test_sequencer_info(false, true), 1, 7, B256::from([1; 32])),
        );

        let result = loaded_manifest_agreement_invariant(1, 7, &[first, second]);
        assert_eq!(result.status, CheckStatus::Fail);
        assert!(result.detail.contains("second reports"));
    }

    #[test]
    fn loaded_manifest_agreement_accepts_matching_multi_node_manifests() {
        let first = test_node_snapshot(
            "first",
            with_manifest(test_sequencer_info(false, true), 1, 7, B256::ZERO),
        );
        let second = test_node_snapshot(
            "second",
            with_manifest(test_sequencer_info(false, true), 1, 7, B256::ZERO),
        );

        let result = loaded_manifest_agreement_invariant(1, 7, &[first, second]);
        assert_eq!(result.status, CheckStatus::Pass);
    }

    #[test]
    fn loaded_manifest_agreement_skips_single_node_mode() {
        let mut info = test_sequencer_info(false, true);
        info.mode = "single".to_owned();
        let result =
            loaded_manifest_agreement_invariant(1, 7, &[test_node_snapshot("single", info)]);
        assert_eq!(result.status, CheckStatus::Skipped);
    }

    #[test]
    fn unique_relayer_is_auto_selected_and_two_are_ambiguous() {
        let one = test_node_snapshot("node-a", test_sequencer_info(false, true));
        let rpc = test_node_snapshot("rpc", test_sequencer_info(true, false));
        assert_eq!(eligible_relayers(&[one.clone(), rpc]).len(), 1);

        let two = test_node_snapshot("node-b", test_sequencer_info(false, true));
        assert_eq!(eligible_relayers(&[one, two]).len(), 2);
    }

    #[test]
    fn registration_preload_requires_both_keys_on_every_sequencer() {
        let old = ExpectedEncryptionKey {
            x: B256::repeat_byte(0x11),
            y_parity: 2,
        };
        let new = ExpectedEncryptionKey {
            x: B256::repeat_byte(0x22),
            y_parity: 3,
        };
        let with_keys = |name: &str, keys: &[ExpectedEncryptionKey]| {
            let mut info = test_sequencer_info(false, true);
            info.decryption_keys = Some(zone_rpc::types::DecryptionKeyStatus {
                candidates: keys
                    .iter()
                    .map(|key| zone_rpc::types::DecryptionKeyCandidate {
                        x: key.x,
                        y_parity: key.y_parity,
                    })
                    .collect(),
                bound: Vec::new(),
            });
            test_node_snapshot(name, info)
        };

        let ready = with_keys("ready", &[old, new]);
        let rpc = test_node_snapshot("rpc", test_sequencer_info(true, false));
        assert_eq!(
            required_decryption_keys_invariant(&[ready, rpc], &[old, new]).status,
            CheckStatus::Pass
        );

        let missing = with_keys("missing", &[old]);
        assert_eq!(
            required_decryption_keys_invariant(&[missing], &[old, new]).status,
            CheckStatus::Fail
        );
    }
}
