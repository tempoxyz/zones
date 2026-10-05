//! Finalized T14 capability import and production OpenRaft assembly.

#![allow(clippy::result_large_err)] // Startup errors preserve complete OpenRaft diagnostics.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::OpenOptions,
    future::Future,
    io::Write as _,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use alloy_consensus::{BlockHeader as _, Sealable as _, Transaction as _};
use alloy_contract::CallBuilder;
use alloy_eips::{BlockNumberOrTag, NumHash};
use alloy_primitives::{Address, B256, b256, keccak256};
use alloy_provider::{DynProvider, Provider as _};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall as _, SolValue as _};
use openraft::{BasicNode, Config};
use rand::{RngCore as _, rngs::OsRng};
use reth_storage_api::{BlockNumReader, BlockReader, HeaderProvider, ReceiptProvider};
use tempo_alloy::TempoNetwork;
use tempo_zone_contracts::{ZonePortal, imported_barrier_call};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zone_chainspec::ZoneChainSpec;
use zone_p2p::{P2pCommand, P2pPeerId, RaftPorts, RaftRequestFrame};

use crate::{
    engine::{FastActivationRefresh, FastActivationRefreshFuture, FastAuthorityRefresh},
    fast_drain::{
        CommittedDrainPoint, DrainClosureObservation, DrainObjectKey, DrainPeer,
        DrainSigningPurpose, FastDrainCommittedState, FastDrainConfig,
    },
    fast_drain_adapters::{
        DrainCommonwareEndpoint, DrainCommonwareRoute, SuccessorBootstrapArtifact,
        sign_authenticated_drain_phase_handoff,
    },
    fast_drain_state::{FastDrainCommitRequest, LocalClosurePayload, NoNewLocksPayload},
    fast_execution::{CanonicalFastExecution, validate_outcome_in_applied_prefix},
    fast_network::{
        AuthenticatedRaftTransport, DrainPhaseSigner, FastNetworkConfig, FastRaftPeerHandler,
        FinalizedPeerIdentity, HandlerFuture, OutcomeSigningRequest, SignedOutcome,
    },
    fast_quorum::{
        AuthenticatedRaftPeer, FastActivation, FastRaftRuntime, FinalizedFastEpoch,
        FinalizedT14Capability, RaftCommit, assemble_fast_raft, verify_committed_certificate,
    },
    fast_raft_state_machine::{
        CommittedProtocolKind, CommittedStateHandle, CommittedTransferRecord,
        DurableStateMachineExecution, inspect_exact_state_image,
    },
    fast_service::{
        CommittedTransferSource, FastServiceConfig, FastServiceError, FastServiceRoute,
        PeerEndpoint, ServiceFuture,
    },
    fast_service_adapters::FastServiceHandle,
};
use zone_evm::same_anchor::SameAnchorOpening;
use zone_fast_transfer::{
    DurableJournal, EpochRoster, InventoryContribution, ProtocolLimits, QuorumVerifier,
    ReplenishmentJob, ReplenishmentWorker,
    admission::{RouteKey, ValueCaps},
    drain::{BarrierInventory, CheckpointImage},
};
use zone_payload::{TempoImport, ZonePayloadAttributes};
use zone_primitives::fast_transfer::{
    CanonicalEncode as _, CertificateBody, MAX_CERTIFICATE_BYTES, OutcomeCertificate,
    SignatureBytes, ZoneDomain, decode_exact,
};
use zone_sequencer::fast_replenishment::{
    AlloyReplenishmentProviderHandles, FilePreparedActionStore, FileReplenishmentNoncePlanner,
    ProviderBackedReplenishmentBridge, ReplenishmentProviderError, ReplenishmentRouteConfig,
};

pub type CertificationFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

const ENROLLMENT_SENTINEL: &str = "fast-epoch-enrollment-v1";
const ENROLLMENT_SENTINEL_DOMAIN: &[u8] = b"tempo.zone.fast-epoch-enrollment.v1";

/// Completion means every locally reconstructed outcome for an entry has a verified, fsynced
/// two-member certificate available to committed RPC reads.
pub trait FastOutcomeCertification: Send + Sync + 'static {
    fn certify_commit(&self, commit: RaftCommit) -> CertificationFuture<'_>;
}

pub struct LoadedFastActivation {
    pub activation: FastActivation,
    pub admission_open: bool,
    pub proof_policy: FinalizedFastProofPolicy,
    pub drain: FinalizedFastDrainCapability,
    /// Initialization is authorized only at the exact finalized L1 block that activates the
    /// epoch. A later restart with missing Raft state is recovery and must fail closed.
    pub allow_initialize: bool,
    pub anchor_timestamp: u64,
    pub anchor_timestamp_millis_part: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FinalizedFastProofPolicy {
    pub mode: u8,
    pub expected_verifier_code_hash: B256,
    pub expected_verifier_config_hash: B256,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FinalizedFastDrainCapability {
    pub closed: bool,
    pub retired: bool,
    pub closure_hash: B256,
    pub final_settlement_height: alloy_primitives::U256,
    pub final_settlement_block_hash: B256,
    pub final_settlement_withdrawal_batch_index: u64,
    pub barriers_hash: B256,
    pub final_settlement_hash: B256,
    pub next_epoch: u64,
    pub next_roster_hash: B256,
    pub checkpoint_log_term: u64,
    pub checkpoint_log_index: u64,
    pub checkpoint_height: alloy_primitives::U256,
    pub checkpoint_block_hash: B256,
    pub checkpoint_state_root: B256,
    pub checkpoint_hash: B256,
}

/// Explicit install-only successor authority. This is committed by the old roster's checkpoint
/// statement before the successor exists as a native voting/token authority. Endpoint manifests
/// are deliberately not part of this value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpectedCandidateRoster {
    pub roster: EpochRoster,
    pub threshold: u8,
    pub proof_policy: FinalizedFastProofPolicy,
    pub peer_portals: [Address; 9],
}

const FAST_EPOCH_CONFIG_WORDS: usize = 27;
const PROOF_MODE_OPERATOR_ATTESTED: u8 = 1;
const PROOF_MODE_REQUIRED: u8 = 2;
const DEVELOPMENT_PROTOTYPE_VERIFIER_CODE_HASH: B256 =
    b256!("c6bc17dc6724fb475ce3c59ec94e01bd733c2996b41bc82281b4d638b928cc33");

#[derive(Clone, Copy, Debug)]
struct FinalizedPortalEpochConfig {
    protocol_version: u32,
    threshold: u8,
    proof_mode: u8,
    closed: bool,
    retired: bool,
    activated_at_tempo_block: u64,
    roster_hash: B256,
    peers_hash: B256,
    expected_verifier_code_hash: B256,
    expected_verifier_config_hash: B256,
    closure_hash: B256,
    final_settlement_height: alloy_primitives::U256,
    final_settlement_block_hash: B256,
    final_settlement_withdrawal_batch_index: u64,
    barriers_hash: B256,
    final_settlement_hash: B256,
    next_epoch: u64,
    next_roster_hash: B256,
    checkpoint_log_term: u64,
    checkpoint_log_index: u64,
    checkpoint_height: alloy_primitives::U256,
    checkpoint_block_hash: B256,
    checkpoint_state_root: B256,
    checkpoint_hash: B256,
}

/// Exact-anchor lifecycle validator installed into the production engine. A changed roster/epoch
/// requires a restart and fresh assembly; this seam only preserves or revokes current authority.
pub struct ExactAnchorActivationRefresh {
    l1: DynProvider<TempoNetwork>,
    portal_address: Address,
    chain_spec: Arc<ZoneChainSpec>,
    zone_id: u32,
    zone_chain_id: u64,
    expected: FastActivation,
    successor_predecessor: Option<(u64, B256)>,
    successor_catchup_target: Option<NumHash>,
    expected_proof_policy: FinalizedFastProofPolicy,
    drain_rosters: Vec<EpochRoster>,
    drain_observer: tokio::sync::watch::Sender<DrainClosureObservation>,
    imported_anchor_observer: tokio::sync::watch::Sender<Option<NumHash>>,
    fast_service: Arc<OnceLock<Arc<FastServiceHandle>>>,
}

impl ExactAnchorActivationRefresh {
    pub fn new(
        l1: DynProvider<TempoNetwork>,
        portal_address: Address,
        chain_spec: Arc<ZoneChainSpec>,
        zone_id: u32,
        zone_chain_id: u64,
        expected: FastActivation,
        successor_predecessor: Option<(u64, B256)>,
        successor_catchup_target: Option<NumHash>,
        expected_proof_policy: FinalizedFastProofPolicy,
        drain_rosters: Vec<EpochRoster>,
        drain_observer: tokio::sync::watch::Sender<DrainClosureObservation>,
        imported_anchor_observer: tokio::sync::watch::Sender<Option<NumHash>>,
        fast_service: Arc<OnceLock<Arc<FastServiceHandle>>>,
    ) -> Self {
        Self {
            l1,
            portal_address,
            chain_spec,
            zone_id,
            zone_chain_id,
            expected,
            successor_predecessor,
            successor_catchup_target,
            expected_proof_policy,
            drain_rosters,
            drain_observer,
            imported_anchor_observer,
            fast_service,
        }
    }
}

impl FastActivationRefresh for ExactAnchorActivationRefresh {
    fn refresh(&self, anchor: NumHash) -> FastActivationRefreshFuture<'_> {
        Box::pin(async move {
            let loaded = match load_fast_activation(
                &self.l1,
                self.portal_address,
                &self.chain_spec,
                anchor,
                self.zone_id,
                self.zone_chain_id,
            )
            .await
            {
                Ok(Some(loaded)) => loaded,
                _result
                    if self
                        .successor_catchup_target
                        .is_some_and(|target| anchor.number < target.number) =>
                {
                    let header = self
                        .l1
                        .get_header_by_hash(anchor.hash)
                        .await
                        .map_err(|error| error.to_string())?
                        .ok_or_else(|| {
                            "successor catch-up anchor is unavailable from Tempo".to_owned()
                        })?;
                    if header.number() != anchor.number || header.hash_slow() != anchor.hash {
                        return Err("successor catch-up anchor number/hash mismatch".to_owned());
                    }
                    self.imported_anchor_observer.send_replace(Some(anchor));
                    return Ok(FastAuthorityRefresh::Drain(SameAnchorOpening::v1(
                        anchor.number,
                        anchor.hash,
                        header.timestamp(),
                        (header.timestamp_millis % 1_000) as u16,
                        self.expected.epoch().epoch,
                    )));
                }
                Ok(None) => {
                    return Err("T14 fast capability is inactive at the imported anchor".to_owned());
                }
                Err(error) => return Err(error.to_string()),
            };
            if loaded.activation.epoch() != self.expected.epoch() {
                if self.successor_predecessor
                    == Some((
                        loaded.activation.epoch().epoch,
                        loaded.activation.epoch().roster_hash,
                    ))
                {
                    self.imported_anchor_observer.send_replace(Some(anchor));
                    return Ok(FastAuthorityRefresh::Drain(SameAnchorOpening::v1(
                        anchor.number,
                        anchor.hash,
                        loaded.anchor_timestamp,
                        loaded.anchor_timestamp_millis_part,
                        self.expected.epoch().epoch,
                    )));
                }
                return Err(
                    "finalized fast epoch or roster changed; restart is required".to_owned(),
                );
            }
            if loaded.proof_policy != self.expected_proof_policy {
                return Err("finalized fast proof policy changed; restart is required".to_owned());
            }
            if let Some(target) = self.successor_catchup_target {
                if anchor.number < target.number {
                    self.imported_anchor_observer.send_replace(Some(anchor));
                    return Ok(FastAuthorityRefresh::Drain(SameAnchorOpening::v1(
                        anchor.number,
                        anchor.hash,
                        loaded.anchor_timestamp,
                        loaded.anchor_timestamp_millis_part,
                        self.expected.epoch().epoch,
                    )));
                }
                if anchor.number != target.number || anchor.hash != target.hash {
                    return Err("successor catch-up diverged from its finalized target".to_owned());
                }
            }
            let closures = load_fast_drain_closures(&self.l1, anchor, &self.drain_rosters)
                .await
                .map_err(|error| error.to_string())?;
            if let Some(service) = self.fast_service.get() {
                service
                    .apply_drain_closure_observation(closures.clone())
                    .await
                    .map_err(|error| error.to_string())?;
            }
            self.imported_anchor_observer.send_replace(Some(anchor));
            self.drain_observer.send_replace(closures);
            let opening = SameAnchorOpening::v1(
                anchor.number,
                anchor.hash,
                loaded.anchor_timestamp,
                loaded.anchor_timestamp_millis_part,
                loaded.activation.epoch().epoch,
            );
            Ok(if loaded.admission_open {
                FastAuthorityRefresh::Open(opening)
            } else {
                FastAuthorityRefresh::Drain(opening)
            })
        })
    }
}

/// Resolve local and remote closure hashes only at the exact imported anchor. The supplied
/// rosters are finalized authority, not routing metadata; any epoch/roster mismatch fails closed.
pub async fn load_fast_drain_closures(
    l1: &DynProvider<TempoNetwork>,
    imported_anchor: NumHash,
    rosters: &[EpochRoster],
) -> Result<DrainClosureObservation, FastRuntimeError> {
    if rosters.len() != 10 {
        return Err(FastRuntimeError::InvalidFinalizedPeers);
    }
    let block = alloy_rpc_types_eth::BlockId::hash_canonical(imported_anchor.hash);
    let local = rosters
        .first()
        .ok_or(FastRuntimeError::InvalidFinalizedRoster)?;
    let mut observation = DrainClosureObservation::default();
    let mut portals = std::collections::BTreeSet::new();
    for roster in rosters {
        if roster.domain.l1_chain_id != local.domain.l1_chain_id
            || roster.domain.protocol_version != local.domain.protocol_version
            || !portals.insert(roster.domain.portal)
        {
            return Err(FastRuntimeError::InvalidFinalizedPeers);
        }
        let config = read_fast_epoch_config(
            l1,
            roster.domain.portal,
            roster.domain.authority_epoch,
            block,
        )
        .await?;
        if config.roster_hash != roster.domain.roster_hash
            || config.protocol_version != u32::from(roster.domain.protocol_version)
        {
            return Err(FastRuntimeError::InvalidFinalizedRoster);
        }
        if config.closed {
            if roster.domain.portal == local.domain.portal {
                observation.local = Some(config.closure_hash);
            } else {
                observation
                    .destinations
                    .insert(roster.domain.portal, config.closure_hash);
            }
        }
    }
    Ok(observation)
}

/// Validate the explicitly expected install-only successor against the old finalized lifecycle.
/// The commitment is derived locally because native successor configuration does not exist until
/// two successor members acknowledge this checkpoint. Routing endpoints are never authority.
pub fn load_finalized_next_roster(
    current: &EpochRoster,
    capability: FinalizedFastDrainCapability,
    candidate: Option<&ExpectedCandidateRoster>,
) -> Result<Option<EpochRoster>, FastRuntimeError> {
    let Some(candidate) = candidate else {
        if capability.next_epoch != 0 || !capability.next_roster_hash.is_zero() {
            return Err(FastRuntimeError::InvalidFinalizedDrainCapability);
        }
        return Ok(None);
    };
    let next = &candidate.roster;
    if !capability.closed
        || capability.retired
        || candidate.threshold != 2
        || !matches!(
            candidate.proof_policy.mode,
            PROOF_MODE_OPERATOR_ATTESTED | PROOF_MODE_REQUIRED
        )
        || candidate.proof_policy.expected_verifier_code_hash.is_zero()
        || candidate
            .proof_policy
            .expected_verifier_config_hash
            .is_zero()
        || next.domain.l1_chain_id != current.domain.l1_chain_id
        || next.domain.zone_id != current.domain.zone_id
        || next.domain.chain_id != current.domain.chain_id
        || next.domain.portal != current.domain.portal
        || next.domain.protocol_version != current.domain.protocol_version
        || next.domain.authority_epoch <= current.domain.authority_epoch
        || candidate.peer_portals.iter().any(|portal| portal.is_zero())
        || candidate
            .peer_portals
            .contains(&candidate.roster.domain.portal)
        || candidate
            .peer_portals
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len()
            != 9
        || candidate
            .roster
            .members
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len()
            != 3
    {
        return Err(FastRuntimeError::InvalidFinalizedRoster);
    }
    if candidate.proof_policy.mode == PROOF_MODE_REQUIRED {
        let prototype = keccak256(&tempo_contracts::zones::T13_ZONE_VERIFIER_RUNTIME);
        if candidate.proof_policy.expected_verifier_code_hash == prototype
            || candidate.proof_policy.expected_verifier_code_hash
                == DEVELOPMENT_PROTOTYPE_VERIFIER_CODE_HASH
        {
            return Err(FastRuntimeError::InvalidFinalizedProofPolicy);
        }
    }
    let expected = candidate_roster_hash(candidate);
    if expected != next.domain.roster_hash {
        return Err(FastRuntimeError::InvalidFinalizedRoster);
    }
    // Before installFastCheckpoint, native nextEpoch/nextRosterHash are intentionally unset. Once
    // present, they must agree exactly; they are never required to bootstrap INSTALL-only mode.
    if (capability.next_epoch != 0 || !capability.next_roster_hash.is_zero())
        && (capability.next_epoch != next.domain.authority_epoch
            || capability.next_roster_hash != next.domain.roster_hash)
    {
        return Err(FastRuntimeError::InvalidFinalizedDrainCapability);
    }
    Ok(Some(next.clone()))
}

fn candidate_roster_hash(candidate: &ExpectedCandidateRoster) -> B256 {
    keccak256(
        (
            keccak256("TEMPO_ZONE_FAST_ROSTER_T14_V1"),
            candidate.roster.domain.portal,
            candidate.roster.domain.authority_epoch,
            u32::from(candidate.roster.domain.protocol_version),
            alloy_primitives::U256::from(candidate.threshold),
            alloy_primitives::U256::from(candidate.proof_policy.mode),
            candidate.proof_policy.expected_verifier_code_hash,
            candidate.proof_policy.expected_verifier_config_hash,
            candidate.roster.members.to_vec(),
            candidate.peer_portals.to_vec(),
        )
            .abi_encode(),
    )
}

pub struct ResolvedFastDrainTopology {
    pub drain: FastDrainConfig,
    pub routes: Vec<DrainCommonwareRoute>,
    pub next_roster_route: Option<DrainCommonwareRoute>,
}

/// Convert the already authenticated C4 authority into C5 routing. ECDSA rosters come from the
/// imported registry; Ed25519 values remain transport endpoints paired to those exact members.
pub fn resolve_fast_drain_topology(
    service: &FastServiceConfig,
    closures: &DrainClosureObservation,
    next_roster: Option<EpochRoster>,
    next_peers: &[FastServicePeerConfig],
) -> Result<ResolvedFastDrainTopology, FastRuntimeError> {
    let mut peers = Vec::with_capacity(9);
    let mut routes = Vec::with_capacity(9);
    for (zone_id, route) in &service.routes {
        peers.push(DrainPeer {
            zone_id: *zone_id,
            closure_hash: closures
                .destinations
                .get(&route.roster.domain.portal)
                .copied()
                .unwrap_or_default(),
            roster: route.roster.clone(),
        });
        routes.push(DrainCommonwareRoute {
            zone_id: *zone_id,
            portal: route.roster.domain.portal,
            endpoints: route
                .endpoints
                .clone()
                .map(|endpoint| DrainCommonwareEndpoint {
                    member: endpoint.member,
                    identity: endpoint.ed25519,
                }),
        });
    }
    let peers: [DrainPeer; 9] = peers
        .try_into()
        .map_err(|_| FastRuntimeError::InvalidFinalizedPeers)?;
    let next_roster_route = match next_roster.as_ref() {
        Some(next)
            if next_peers
                .iter()
                .all(|peer| peer.zone_id == next.domain.zone_id) =>
        {
            Some(DrainCommonwareRoute {
                zone_id: next.domain.zone_id,
                portal: next.domain.portal,
                endpoints: service_endpoints(next_peers.to_vec(), next)?.map(|endpoint| {
                    DrainCommonwareEndpoint {
                        member: endpoint.member,
                        identity: endpoint.ed25519,
                    }
                }),
            })
        }
        Some(_) => return Err(FastRuntimeError::InvalidConfiguration),
        None if next_peers.is_empty() => None,
        None => return Err(FastRuntimeError::InvalidConfiguration),
    };
    let drain = FastDrainConfig {
        local_roster: service.local_roster.clone(),
        local_member: service.local_member,
        peers,
        next_roster,
    };
    drain
        .validate()
        .map_err(|_| FastRuntimeError::InvalidConfiguration)?;
    Ok(ResolvedFastDrainTopology {
        drain,
        routes,
        next_roster_route,
    })
}

/// Materialize closure protocol records on every replica from the same committed full-import
/// entry. This makes remote phase signing independent of which replica first requested a barrier.
pub async fn reconcile_fast_drain_closure_records<P>(
    l1: &DynProvider<TempoNetwork>,
    rosters: &[EpochRoster],
    observation: &DrainClosureObservation,
    execution: &CanonicalFastExecution<P>,
    committed: &CommittedStateHandle<CanonicalFastExecution<P>>,
) -> Result<(), FastRuntimeError>
where
    P: BlockNumReader
        + BlockReader<Block = tempo_primitives::Block>
        + HeaderProvider<Header = tempo_primitives::TempoHeader>
        + ReceiptProvider<Receipt = tempo_primitives::TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    let local = rosters
        .first()
        .ok_or(FastRuntimeError::InvalidFinalizedRoster)?;
    let identity = |kind: CommittedProtocolKind, payload: &[u8]| {
        let mut encoded = Vec::with_capacity(payload.len() + 1);
        encoded.push(kind as u8);
        encoded.extend_from_slice(payload);
        keccak256(encoded)
    };
    let mut required = BTreeMap::<B256, (CommittedProtocolKind, Vec<u8>)>::new();
    if let Some(closure_hash) = observation.local {
        let payload = bincode::serialize(&LocalClosurePayload {
            epoch: local.domain.authority_epoch,
            closure_hash,
        })
        .map_err(|error| FastRuntimeError::Provider(error.to_string()))?;
        required.insert(
            identity(CommittedProtocolKind::ObservedLocalClosure, &payload),
            (CommittedProtocolKind::ObservedLocalClosure, payload),
        );
    }
    for (portal, closure_hash) in &observation.destinations {
        let destination = rosters
            .iter()
            .find(|roster| roster.domain.portal == *portal)
            .ok_or(FastRuntimeError::InvalidFinalizedRoster)?;
        let payload = bincode::serialize(&NoNewLocksPayload {
            source_epoch: local.domain.authority_epoch,
            destination_portal: *portal,
            destination_epoch: destination.domain.authority_epoch,
            closure_hash: *closure_hash,
        })
        .map_err(|error| FastRuntimeError::Provider(error.to_string()))?;
        required.insert(
            identity(CommittedProtocolKind::NoNewLocks, &payload),
            (CommittedProtocolKind::NoNewLocks, payload),
        );
    }
    for existing in committed
        .protocol_records()
        .map_err(|error| FastRuntimeError::Provider(error.to_string()))?
    {
        required.remove(&identity(existing.kind, &existing.canonical_payload));
    }
    if required.is_empty() {
        return Ok(());
    }
    for applied in committed
        .applied_blocks()
        .map_err(|error| FastRuntimeError::Provider(error.to_string()))?
    {
        let attributes: ZonePayloadAttributes = bincode::deserialize(&applied.input.l1_inputs)
            .map_err(|error| FastRuntimeError::Provider(error.to_string()))?;
        let TempoImport::Full(prepared) = attributes.tempo_import else {
            continue;
        };
        let at = load_fast_drain_closures(l1, prepared.header.num_hash(), rosters).await?;
        let mut admitted = Vec::new();
        for (payload_hash, (kind, payload)) in &required {
            let observed = match kind {
                CommittedProtocolKind::ObservedLocalClosure => {
                    let decoded: LocalClosurePayload = bincode::deserialize(payload)
                        .map_err(|error| FastRuntimeError::Provider(error.to_string()))?;
                    at.local == Some(decoded.closure_hash)
                }
                CommittedProtocolKind::NoNewLocks => {
                    let decoded: NoNewLocksPayload = bincode::deserialize(payload)
                        .map_err(|error| FastRuntimeError::Provider(error.to_string()))?;
                    at.destinations.get(&decoded.destination_portal) == Some(&decoded.closure_hash)
                }
                _ => false,
            };
            if observed {
                let record = execution
                    .imported_protocol_record(&applied, *kind, payload.clone())
                    .map_err(|error| FastRuntimeError::Provider(error.to_string()))?;
                committed
                    .persist_protocol_record(record)
                    .map_err(|error| FastRuntimeError::Provider(error.to_string()))?;
                admitted.push(*payload_hash);
            }
        }
        for identity in admitted {
            required.remove(&identity);
        }
        if required.is_empty() {
            return Ok(());
        }
    }
    Err(FastRuntimeError::InvalidFinalizedDrainCapability)
}

/// Consume C5 protocol requests through the actual native submitter and exact committed image.
pub async fn serve_fast_drain_commit_requests<P>(
    mut requests: mpsc::Receiver<FastDrainCommitRequest>,
    service: Arc<OnceLock<Arc<FastServiceHandle>>>,
    committed: CommittedStateHandle<CanonicalFastExecution<P>>,
) -> Result<(), FastRuntimeError>
where
    P: BlockNumReader
        + BlockReader<Block = tempo_primitives::Block>
        + HeaderProvider<Header = tempo_primitives::TempoHeader>
        + ReceiptProvider<Receipt = tempo_primitives::TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    while let Some(request) = requests.recv().await {
        match request {
            FastDrainCommitRequest::NoNewLocks {
                canonical_payload,
                response,
            } => {
                let result = loop {
                    let records = match committed.protocol_records() {
                        Ok(records) => records,
                        Err(error) => break Err(error.to_string()),
                    };
                    let matches = records
                        .into_iter()
                        .filter(|record| {
                            record.kind == CommittedProtocolKind::NoNewLocks
                                && record.canonical_payload == canonical_payload
                        })
                        .collect::<Vec<_>>();
                    match matches.as_slice() {
                        [record] => break Ok(record.clone()),
                        [] => tokio::time::sleep(Duration::from_millis(25)).await,
                        _ => break Err("duplicate canonical no-new-lock records".to_owned()),
                    }
                };
                let _ = response.send(result);
            }
            FastDrainCommitRequest::ImportedBarrier {
                call,
                certificate_digest,
                canonical_inventory,
                response,
            } => {
                let result = async {
                    let inventory = BarrierInventory::decode_durable(&canonical_inventory)
                        .map_err(|error| error.to_string())?;
                    inventory
                        .verify_complete()
                        .map_err(|error| error.to_string())?;
                    let expected_digest =
                        inventory.statement.registry_digest(inventory.l1_chain_id);
                    if certificate_digest != expected_digest
                        || call.barrier_digest != expected_digest
                        || call.complete_lock_root != inventory.statement.complete_lock_root
                    {
                        return Err(
                            "full imported-barrier inventory differs from signed statement"
                                .to_owned(),
                        );
                    }
                    let decoded =
                        tempo_zone_contracts::IFastTransfer::recordImportedBarrierCall::abi_decode(
                            &call.calldata,
                        )
                        .map_err(|error| error.to_string())?;
                    if decoded.sourceBarrierCertificate.len()
                        != tempo_zone_contracts::IMPORTED_BARRIER_CERTIFICATE_BYTES
                    {
                        return Err("invalid compact imported-barrier certificate".to_owned());
                    }
                    let mut signatures = [[0u8; 65]; 2];
                    signatures[0].copy_from_slice(&decoded.sourceBarrierCertificate[33..98]);
                    signatures[1].copy_from_slice(&decoded.sourceBarrierCertificate[98..163]);
                    let expected_call = imported_barrier_call(
                        &inventory.statement,
                        certificate_digest,
                        [SignatureBytes(signatures[0]), SignatureBytes(signatures[1])],
                    );
                    if call != expected_call {
                        return Err(
                            "compact imported-barrier call differs from retained inventory"
                                .to_owned(),
                        );
                    }
                    let service = service
                        .get()
                        .ok_or_else(|| "C4 native submitter is not installed".to_owned())?;
                    let compact_calldata = call.calldata.to_vec();
                    let mut record = service
                        .submit_imported_barrier(
                            &committed,
                            call,
                            certificate_digest,
                            compact_calldata,
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                    // The native transaction is compact, while the protocol record retains the
                    // complete validated object at that exact applied Raft coordinate. Persisting
                    // happens in this consumer before the mpsc response is sent.
                    record.canonical_payload = canonical_inventory;
                    committed
                        .persist_protocol_record(record.clone())
                        .map_err(|error| error.to_string())?;
                    Ok(record)
                };
                let result = result.await;
                let _ = response.send(result);
            }
            FastDrainCommitRequest::InstallCheckpoint { image, response } => {
                let result = (|| {
                    image.validate().map_err(|error| error.to_string())?;
                    let exact = committed
                        .exact_state_image()
                        .map_err(|error| error.to_string())?;
                    let head = exact
                        .blocks
                        .last()
                        .ok_or_else(|| "checkpoint has no committed head".to_owned())?;
                    if image.consensus_snapshot != exact.bytes
                        || image.statement.checkpoint_log_term != exact.last_applied.leader_id.term
                        || image.statement.checkpoint_log_index != exact.last_applied.index
                        || image.statement.checkpoint_height
                            != alloy_primitives::U256::from(head.output.block_height)
                        || image.statement.checkpoint_block_hash != head.output.block_hash
                        || image.statement.checkpoint_state_root != head.output.state_root
                    {
                        return Err("checkpoint differs from exact local OpenRaft image".to_owned());
                    }
                    let mut imported_anchor = None;
                    for applied in exact.blocks.iter().rev() {
                        let attributes: ZonePayloadAttributes =
                            bincode::deserialize(&applied.input.l1_inputs)
                                .map_err(|error| error.to_string())?;
                        match attributes.tempo_import {
                            TempoImport::Full(prepared) => {
                                imported_anchor = Some(prepared.header.num_hash());
                                break;
                            }
                            TempoImport::CheckpointOnly(headers) => {
                                if let Some(header) = headers.last() {
                                    imported_anchor = Some(header.num_hash());
                                    break;
                                }
                            }
                            TempoImport::SameAnchor(_) => {}
                        }
                    }
                    let anchor = imported_anchor
                        .ok_or_else(|| "checkpoint has no imported L1 anchor".to_owned())?;
                    Ok(CommittedDrainPoint {
                        log_term: exact.last_applied.leader_id.term,
                        log_index: exact.last_applied.index,
                        block_height: head.output.block_height,
                        block_hash: head.output.block_hash,
                        state_root: head.output.state_root,
                        imported_anchor_number: anchor.number,
                        imported_anchor_hash: anchor.hash,
                    })
                })();
                let _ = response.send(result);
            }
        }
    }
    Err(FastRuntimeError::Provider(
        "fast drain commit request channel closed".to_owned(),
    ))
}

/// Operator routing and local durable resources. None of these fields activates the protocol;
/// activation is derived exclusively from finalized Portal state by [`load_fast_activation`].
#[derive(Clone, Debug)]
pub struct FastRuntimeConfig {
    pub signer: PrivateKeySigner,
    pub local_transport: P2pPeerId,
    pub member_transports: BTreeMap<Address, P2pPeerId>,
    pub storage: PathBuf,
    pub rpc_timeout: Duration,
    pub service: Option<FastServiceRuntimeConfig>,
    pub replenishment_routes: Vec<FastReplenishmentRuntimeConfig>,
}

impl FastRuntimeConfig {
    pub fn validate(&self) -> Result<(), FastRuntimeError> {
        if self.member_transports.len() != 3
            || self.member_transports.contains_key(&Address::ZERO)
            || self.rpc_timeout.is_zero()
            || self.service.is_none()
            || self
                .service
                .as_ref()
                .is_some_and(|service| !service.is_valid())
            || self.replenishment_routes.iter().any(|route| {
                route.poll_interval.is_zero()
                    || route.storage.as_os_str().is_empty()
                    || route.expected_source_fast_epoch == 0
                    || route.expected_destination_fast_epoch == 0
                    || route.expected_destination_key_x.is_zero()
                    || !matches!(route.expected_destination_key_y_parity, 2 | 3)
            })
        {
            return Err(FastRuntimeError::InvalidConfiguration);
        }
        if self.member_transports.get(&self.signer.address()) != Some(&self.local_transport) {
            return Err(FastRuntimeError::InvalidConfiguration);
        }
        Ok(())
    }
}

/// Explicit C4 operator resources. Finalized Portal reads still supply every roster/domain; these
/// configured peer bindings and endpoints are routing inputs checked against those rosters.
#[derive(Clone, Debug)]
pub struct FastServiceRuntimeConfig {
    pub l1_chain_id: u64,
    pub peers: Vec<FastServicePeerConfig>,
    pub expected_candidate_roster: Option<ExpectedCandidateRoster>,
    pub next_roster_peers: Vec<FastServicePeerConfig>,
    pub routes: Vec<FastServiceRouteConfig>,
    pub limits: ProtocolLimits,
    pub reserved_terminal_bytes: usize,
    pub health_max_age: Duration,
    pub response_timeout: Duration,
    pub native_rpc_endpoint: String,
    pub operator_signer: PrivateKeySigner,
    pub native_chain_id: u64,
    pub fee_token: Address,
    pub commit_timeout: Duration,
    pub drain: FastDrainRuntimeConfig,
    pub exposure: FastExposureRuntimeConfig,
}

/// Explicit provider, asset, durable-storage, and retry resources for the C5 retirement driver.
/// Finalized Portal state remains the authority for both old and successor rosters.
#[derive(Clone, Debug)]
pub struct FastDrainRuntimeConfig {
    pub tempo_l1_rpc_endpoint: String,
    pub factory: Address,
    pub fee_token: Address,
    pub journal_directory: PathBuf,
    pub response_timeout: Duration,
    pub retry_interval: Duration,
}

#[derive(Clone, Debug)]
pub struct FastExposureRuntimeConfig {
    pub tempo_l1_rpc_endpoint: String,
    pub sources: Vec<FastExposureSourceRuntimeConfig>,
    pub poll_interval: Duration,
    pub rpc_timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct FastExposureSourceRuntimeConfig {
    pub zone_id: u32,
    pub chain_id: u64,
    pub portal: Address,
    pub rpc_endpoint: String,
}

impl FastServiceRuntimeConfig {
    fn is_valid(&self) -> bool {
        self.l1_chain_id != 0
            && self.peers.len() == 30
            && self.expected_candidate_roster.is_some() == (self.next_roster_peers.len() == 3)
            && matches!(self.next_roster_peers.len(), 0 | 3)
            && self.routes.len() == 9
            && !self.native_rpc_endpoint.is_empty()
            && self.native_chain_id != 0
            && !self.operator_signer.address().is_zero()
            && self.fee_token != Address::ZERO
            && !self.health_max_age.is_zero()
            && !self.response_timeout.is_zero()
            && !self.commit_timeout.is_zero()
            && !self.drain.tempo_l1_rpc_endpoint.is_empty()
            && self.drain.factory != Address::ZERO
            && self.drain.fee_token != Address::ZERO
            && !self.drain.journal_directory.as_os_str().is_empty()
            && !self.drain.response_timeout.is_zero()
            && !self.drain.retry_interval.is_zero()
            && !self.exposure.tempo_l1_rpc_endpoint.is_empty()
            && self.exposure.sources.len() == 9
            && !self.exposure.poll_interval.is_zero()
            && !self.exposure.rpc_timeout.is_zero()
            && self.exposure.sources.iter().all(|source| {
                source.zone_id != 0
                    && source.chain_id != 0
                    && source.portal != Address::ZERO
                    && !source.rpc_endpoint.is_empty()
            })
    }
}

#[derive(Clone, Debug)]
pub struct FastServicePeerConfig {
    pub zone_id: u32,
    pub certificate_member: Address,
    pub ed25519: P2pPeerId,
    pub endpoint: String,
}

#[derive(Clone, Debug)]
pub struct FastServiceRouteConfig {
    pub zone_id: u32,
    pub chain_id: u64,
    pub portal: Address,
    pub remote_quote: zone_primitives::fast_transfer::QuoteCertificate,
    pub local_quote: zone_primitives::fast_transfer::QuoteCertificate,
    pub outgoing_caps: ValueCaps,
    pub incoming_caps: ValueCaps,
}

async fn read_fast_epoch_config(
    provider: &DynProvider<TempoNetwork>,
    portal: Address,
    epoch: u64,
    block: alloy_rpc_types_eth::BlockId,
) -> Result<FinalizedPortalEpochConfig, FastRuntimeError> {
    let mut calldata = vec![0u8; 36];
    calldata[..4].copy_from_slice(&keccak256("fastEpochConfig(uint64)")[..4]);
    calldata[28..].copy_from_slice(&epoch.to_be_bytes());
    let output = CallBuilder::new_raw(provider, calldata.into())
        .to(portal)
        .block(block)
        .call()
        .await
        .map_err(l1_error)?;
    if output.len() != FAST_EPOCH_CONFIG_WORDS * 32 {
        return Err(FastRuntimeError::InvalidFinalizedEpochEncoding);
    }
    let word = |index: usize| &output[index * 32..(index + 1) * 32];
    let u8_word = |index: usize| -> Result<u8, FastRuntimeError> {
        let value = word(index);
        if value[..31].iter().any(|byte| *byte != 0) {
            return Err(FastRuntimeError::InvalidFinalizedEpochEncoding);
        }
        Ok(value[31])
    };
    let u32_word = |index: usize| -> Result<u32, FastRuntimeError> {
        let value = word(index);
        if value[..28].iter().any(|byte| *byte != 0) {
            return Err(FastRuntimeError::InvalidFinalizedEpochEncoding);
        }
        Ok(u32::from_be_bytes(
            value[28..].try_into().expect("four-byte word suffix"),
        ))
    };
    let u64_word = |index: usize| -> Result<u64, FastRuntimeError> {
        let value = word(index);
        if value[..24].iter().any(|byte| *byte != 0) {
            return Err(FastRuntimeError::InvalidFinalizedEpochEncoding);
        }
        Ok(u64::from_be_bytes(
            value[24..].try_into().expect("eight-byte word suffix"),
        ))
    };
    let bool_word = |index: usize| -> Result<bool, FastRuntimeError> {
        match u8_word(index)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(FastRuntimeError::InvalidFinalizedEpochEncoding),
        }
    };
    let config = FinalizedPortalEpochConfig {
        protocol_version: u32_word(0)?,
        threshold: u8_word(1)?,
        proof_mode: u8_word(2)?,
        closed: bool_word(3)?,
        retired: bool_word(4)?,
        activated_at_tempo_block: u64_word(8)?,
        roster_hash: B256::from_slice(word(9)),
        peers_hash: B256::from_slice(word(10)),
        expected_verifier_code_hash: B256::from_slice(word(11)),
        expected_verifier_config_hash: B256::from_slice(word(12)),
        closure_hash: B256::from_slice(word(13)),
        final_settlement_height: alloy_primitives::U256::from_be_slice(word(14)),
        final_settlement_block_hash: B256::from_slice(word(15)),
        final_settlement_withdrawal_batch_index: u64_word(16)?,
        barriers_hash: B256::from_slice(word(17)),
        final_settlement_hash: B256::from_slice(word(18)),
        next_epoch: u64_word(19)?,
        next_roster_hash: B256::from_slice(word(20)),
        checkpoint_log_term: u64_word(21)?,
        checkpoint_log_index: u64_word(22)?,
        checkpoint_height: alloy_primitives::U256::from_be_slice(word(23)),
        checkpoint_block_hash: B256::from_slice(word(24)),
        checkpoint_state_root: B256::from_slice(word(25)),
        checkpoint_hash: B256::from_slice(word(26)),
    };
    validate_fast_epoch_policy(&config)?;
    Ok(config)
}

async fn validate_enrolled_verifier(
    provider: &DynProvider<TempoNetwork>,
    portal_address: Address,
    block: alloy_rpc_types_eth::BlockId,
    config: &FinalizedPortalEpochConfig,
) -> Result<(), FastRuntimeError> {
    let verifier = ZonePortal::new(portal_address, provider)
        .verifier()
        .block(block)
        .call()
        .await
        .map_err(l1_error)?;
    let code = provider
        .get_code_at(verifier)
        .block_id(block)
        .await
        .map_err(l1_error)?;
    if verifier.is_zero() || keccak256(&code) != config.expected_verifier_code_hash {
        return Err(FastRuntimeError::InvalidFinalizedProofPolicy);
    }
    Ok(())
}

fn validate_fast_epoch_policy(config: &FinalizedPortalEpochConfig) -> Result<(), FastRuntimeError> {
    if !matches!(
        config.proof_mode,
        PROOF_MODE_OPERATOR_ATTESTED | PROOF_MODE_REQUIRED
    ) || config.expected_verifier_code_hash.is_zero()
        || config.expected_verifier_config_hash.is_zero()
    {
        return Err(FastRuntimeError::InvalidFinalizedProofPolicy);
    }
    if config.proof_mode == PROOF_MODE_REQUIRED {
        let prototype = keccak256(&tempo_contracts::zones::T13_ZONE_VERIFIER_RUNTIME);
        if config.expected_verifier_code_hash == prototype
            || config.expected_verifier_code_hash == DEVELOPMENT_PROTOTYPE_VERIFIER_CODE_HASH
        {
            return Err(FastRuntimeError::InvalidFinalizedProofPolicy);
        }
    }
    let has_final_settlement = !config.final_settlement_hash.is_zero();
    let has_checkpoint = !config.checkpoint_hash.is_zero();
    if (!config.closed && !config.closure_hash.is_zero())
        || (config.closed && config.closure_hash.is_zero())
        || config.retired && (!config.closed || !has_final_settlement || !has_checkpoint)
        || has_final_settlement
            != (!config.final_settlement_height.is_zero()
                && !config.final_settlement_block_hash.is_zero()
                && !config.barriers_hash.is_zero())
        || has_checkpoint
            != (config.next_epoch != 0
                && !config.next_roster_hash.is_zero()
                && config.checkpoint_log_term != 0
                && config.checkpoint_log_index != 0
                && !config.checkpoint_height.is_zero()
                && !config.checkpoint_block_hash.is_zero()
                && !config.checkpoint_state_root.is_zero())
    {
        return Err(FastRuntimeError::InvalidFinalizedDrainCapability);
    }
    Ok(())
}

fn finalized_roster_hash(
    portal: Address,
    epoch: u64,
    config: &FinalizedPortalEpochConfig,
    members: [Address; 3],
    peers: [Address; 9],
) -> B256 {
    keccak256(
        (
            keccak256("TEMPO_ZONE_FAST_ROSTER_T14_V1"),
            portal,
            epoch,
            config.protocol_version,
            alloy_primitives::U256::from(config.threshold),
            alloy_primitives::U256::from(config.proof_mode),
            config.expected_verifier_code_hash,
            config.expected_verifier_config_hash,
            members.to_vec(),
            peers.to_vec(),
        )
            .abi_encode(),
    )
}

/// Resolve every C4 authority value from the same imported canonical Tempo anchor used for the
/// local activation. Configured endpoints and Ed25519 keys remain routing data only.
pub async fn load_fast_service_config(
    l1: &DynProvider<TempoNetwork>,
    imported_anchor: NumHash,
    activation: &FastActivation,
    local_member: Address,
    configured: &FastServiceRuntimeConfig,
) -> Result<FastServiceConfig, FastRuntimeError> {
    let epoch = activation.epoch();
    if configured.l1_chain_id != epoch.l1_chain_id
        || configured.native_chain_id != epoch.zone_chain_id
    {
        return Err(FastRuntimeError::InvalidConfiguration);
    }
    let protocol_version = u16::try_from(epoch.protocol_version)
        .map_err(|_| FastRuntimeError::InvalidConfiguration)?;
    let local_roster = EpochRoster::from_finalized_registry(
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
    .map_err(|_| FastRuntimeError::InvalidFinalizedRoster)?;
    let mut configured_peers = configured.peers.iter().cloned().fold(
        BTreeMap::<u32, Vec<FastServicePeerConfig>>::new(),
        |mut peers, peer| {
            peers.entry(peer.zone_id).or_default().push(peer);
            peers
        },
    );
    let local_endpoints = service_endpoints(
        configured_peers
            .remove(&epoch.zone_id)
            .ok_or(FastRuntimeError::InvalidConfiguration)?,
        &local_roster,
    )?;
    let mut routes = BTreeMap::new();
    let mut value_caps = HashMap::new();
    let pinned = alloy_rpc_types_eth::BlockId::hash_canonical(imported_anchor.hash);
    let configured_portals = epoch
        .peer_portals
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>();
    let mut network_portals = configured_portals.clone();
    network_portals.insert(epoch.portal);
    let route_portals = configured
        .routes
        .iter()
        .map(|route| route.portal)
        .collect::<std::collections::BTreeSet<_>>();
    if route_portals != configured_portals {
        return Err(FastRuntimeError::InvalidConfiguration);
    }
    for route in configured.routes.iter().cloned() {
        if !configured_portals.contains(&route.portal) || route.portal == epoch.portal {
            return Err(FastRuntimeError::InvalidConfiguration);
        }
        let roster = load_remote_roster(
            l1,
            pinned,
            configured.l1_chain_id,
            route.zone_id,
            route.chain_id,
            route.portal,
            protocol_version,
            &network_portals,
            imported_anchor.number,
        )
        .await?;
        let endpoints = service_endpoints(
            configured_peers
                .remove(&route.zone_id)
                .ok_or(FastRuntimeError::InvalidConfiguration)?,
            &roster,
        )?;
        let outgoing_key = RouteKey {
            source_zone: epoch.zone_id,
            destination_zone: route.zone_id,
            l1_token: route.remote_quote.quote.asset.l1_token,
        };
        let incoming_key = RouteKey {
            source_zone: route.zone_id,
            destination_zone: epoch.zone_id,
            l1_token: route.local_quote.quote.asset.l1_token,
        };
        if value_caps
            .insert(outgoing_key, route.outgoing_caps)
            .is_some()
            || value_caps
                .insert(incoming_key, route.incoming_caps)
                .is_some()
            || routes
                .insert(
                    route.zone_id,
                    FastServiceRoute {
                        roster,
                        endpoints,
                        remote_quote: route.remote_quote,
                        local_quote: route.local_quote,
                    },
                )
                .is_some()
        {
            return Err(FastRuntimeError::InvalidConfiguration);
        }
    }
    if !configured_peers.is_empty() || routes.len() != 9 {
        return Err(FastRuntimeError::InvalidConfiguration);
    }
    let mut rng = OsRng;
    let stream = loop {
        let candidate = rng.next_u64();
        if candidate != 0 {
            break candidate;
        }
    };
    let service = FastServiceConfig {
        local_roster,
        local_member,
        local_endpoints,
        stream,
        routes,
        limits: configured.limits.clone(),
        value_caps,
        reserved_terminal_bytes: configured.reserved_terminal_bytes,
        health_max_age: configured.health_max_age,
    };
    service
        .validate()
        .map_err(|_| FastRuntimeError::InvalidConfiguration)?;
    Ok(service)
}

fn service_endpoints(
    peers: Vec<FastServicePeerConfig>,
    roster: &EpochRoster,
) -> Result<[PeerEndpoint; 3], FastRuntimeError> {
    let endpoints: Vec<_> = peers
        .into_iter()
        .map(|peer| PeerEndpoint {
            member: peer.certificate_member,
            ed25519: peer.ed25519,
            endpoint: peer.endpoint,
        })
        .collect();
    let endpoints: [PeerEndpoint; 3] = endpoints
        .try_into()
        .map_err(|_| FastRuntimeError::InvalidConfiguration)?;
    let members = endpoints
        .iter()
        .map(|endpoint| endpoint.member)
        .collect::<std::collections::BTreeSet<_>>();
    if members != roster.members.into_iter().collect() {
        return Err(FastRuntimeError::InvalidConfiguration);
    }
    Ok(endpoints)
}

async fn load_remote_roster(
    l1: &DynProvider<TempoNetwork>,
    block: alloy_rpc_types_eth::BlockId,
    l1_chain_id: u64,
    zone_id: u32,
    chain_id: u64,
    portal_address: Address,
    protocol_version: u16,
    network_portals: &std::collections::BTreeSet<Address>,
    imported_anchor_number: u64,
) -> Result<EpochRoster, FastRuntimeError> {
    let portal = ZonePortal::new(portal_address, l1);
    if portal
        .zoneId()
        .block(block)
        .call()
        .await
        .map_err(l1_error)?
        != zone_id
        || zone_primitives::constants::zone_chain_id(l1_chain_id, zone_id)
            .map_err(|_| FastRuntimeError::InvalidFinalizedRoster)?
            != chain_id
    {
        return Err(FastRuntimeError::InvalidFinalizedRoster);
    }
    let epoch = portal
        .fastEpoch()
        .block(block)
        .call()
        .await
        .map_err(l1_error)?;
    if epoch == 0 {
        return Err(FastRuntimeError::InvalidFinalizedRoster);
    }
    let config = read_fast_epoch_config(l1, portal_address, epoch, block).await?;
    if config.next_epoch != 0 && config.next_epoch <= epoch {
        return Err(FastRuntimeError::InvalidFinalizedDrainCapability);
    }
    validate_enrolled_verifier(l1, portal_address, block, &config).await?;
    if config.retired
        || config.protocol_version != u32::from(protocol_version)
        || config.threshold != 2
        || config.activated_at_tempo_block == 0
        || config.activated_at_tempo_block > imported_anchor_number
        || portal
            .fastEpochMemberCount(epoch)
            .block(block)
            .call()
            .await
            .map_err(l1_error)?
            != alloy_primitives::U256::from(3)
    {
        return Err(FastRuntimeError::InvalidFinalizedRoster);
    }
    let mut members = [Address::ZERO; 3];
    for (index, member) in members.iter_mut().enumerate() {
        *member = portal
            .fastEpochMemberAt(epoch, alloy_primitives::U256::from(index))
            .block(block)
            .call()
            .await
            .map_err(l1_error)?;
    }
    if portal
        .fastEpochPeerCount(epoch)
        .block(block)
        .call()
        .await
        .map_err(l1_error)?
        != alloy_primitives::U256::from(9)
    {
        return Err(FastRuntimeError::InvalidFinalizedPeers);
    }
    let mut peer_portals = Vec::with_capacity(9);
    for index in 0..9 {
        peer_portals.push(
            portal
                .fastEpochPeerAt(epoch, alloy_primitives::U256::from(index))
                .block(block)
                .call()
                .await
                .map_err(l1_error)?,
        );
    }
    let peer_portals: [Address; 9] = peer_portals
        .try_into()
        .map_err(|_| FastRuntimeError::InvalidFinalizedPeers)?;
    let peers = peer_portals
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    let expected_peers = network_portals
        .iter()
        .copied()
        .filter(|candidate| *candidate != portal_address)
        .collect::<std::collections::BTreeSet<_>>();
    if peers != expected_peers {
        return Err(FastRuntimeError::InvalidFinalizedPeers);
    }
    if config.peers_hash != keccak256(peer_portals.to_vec().abi_encode()) {
        return Err(FastRuntimeError::InvalidFinalizedPeers);
    }
    if config.roster_hash
        != finalized_roster_hash(portal_address, epoch, &config, members, peer_portals)
    {
        return Err(FastRuntimeError::InvalidFinalizedRoster);
    }
    EpochRoster::from_finalized_registry(
        ZoneDomain {
            l1_chain_id,
            zone_id,
            chain_id,
            portal: portal_address,
            authority_epoch: epoch,
            roster_hash: config.roster_hash,
            protocol_version,
        },
        members,
    )
    .map_err(|_| FastRuntimeError::InvalidFinalizedRoster)
}

/// One explicit, independently preflighted operator-inventory route. Provider handles contain the
/// configured source, destination, and L1 treasury endpoints and wallets; no endpoint or signer is
/// inferred from the public RPC or a peer manifest.
#[derive(Clone)]
pub struct FastReplenishmentRuntimeConfig {
    pub route: ReplenishmentRouteConfig,
    pub providers: AlloyReplenishmentProviderHandles,
    pub expected_source_fast_epoch: u64,
    pub expected_destination_fast_epoch: u64,
    pub expected_token_decimals: u8,
    pub expected_destination_key_index: alloy_primitives::U256,
    pub expected_destination_key_x: B256,
    pub expected_destination_key_y_parity: u8,
    pub storage: PathBuf,
    pub poll_interval: Duration,
}

impl std::fmt::Debug for FastReplenishmentRuntimeConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FastReplenishmentRuntimeConfig")
            .field("route", &self.route)
            .field(
                "expected_source_fast_epoch",
                &self.expected_source_fast_epoch,
            )
            .field(
                "expected_destination_fast_epoch",
                &self.expected_destination_fast_epoch,
            )
            .field("expected_token_decimals", &self.expected_token_decimals)
            .field(
                "expected_destination_key_index",
                &self.expected_destination_key_index,
            )
            .field(
                "expected_destination_key_x",
                &self.expected_destination_key_x,
            )
            .field(
                "expected_destination_key_y_parity",
                &self.expected_destination_key_y_parity,
            )
            .field("storage", &self.storage)
            .field("poll_interval", &self.poll_interval)
            .finish_non_exhaustive()
    }
}

/// Read the complete capability at one finalized L1 block. Calls are explicitly block-pinned so
/// a moving RPC head cannot splice fields from different epochs.
pub async fn load_fast_activation(
    l1: &DynProvider<TempoNetwork>,
    portal_address: Address,
    chain_spec: &ZoneChainSpec,
    imported_anchor: NumHash,
    zone_id: u32,
    zone_chain_id: u64,
) -> Result<Option<LoadedFastActivation>, FastRuntimeError> {
    let finalized = l1
        .get_header_by_number(BlockNumberOrTag::Finalized)
        .await
        .map_err(l1_error)?
        .ok_or(FastRuntimeError::NoFinalizedHeader)?;
    if finalized.number() < imported_anchor.number {
        return Err(FastRuntimeError::FinalizedHeadBehindAnchor {
            finalized: finalized.number(),
            anchor: imported_anchor.number,
        });
    }
    let anchor = l1
        .get_header_by_hash(imported_anchor.hash)
        .await
        .map_err(l1_error)?
        .ok_or(FastRuntimeError::ImportedAnchorUnavailable(imported_anchor))?;
    if anchor.number() != imported_anchor.number || anchor.hash_slow() != imported_anchor.hash {
        return Err(FastRuntimeError::ImportedAnchorMismatch {
            imported: imported_anchor,
            remote: NumHash::new(anchor.number(), anchor.hash_slow()),
        });
    }
    if !chain_spec.supports_same_anchor_at(anchor.timestamp()) {
        return Ok(None);
    }
    // The Zone's imported TempoState is the sole capability anchor. Hash-canonical pinning keeps
    // every registry field on that exact state even if the remote finalized head advances.
    let block = alloy_rpc_types_eth::BlockId::hash_canonical(imported_anchor.hash);
    let portal = ZonePortal::new(portal_address, l1);
    let epoch = portal
        .fastEpoch()
        .block(block)
        .call()
        .await
        .map_err(l1_error)?;
    if epoch == 0 {
        return Ok(None);
    }
    let native_pin = portal
        .FAST_PROTOCOL_NATIVE_PIN()
        .block(block)
        .call()
        .await
        .map_err(l1_error)?;
    let config = read_fast_epoch_config(l1, portal_address, epoch, block).await?;
    if config.next_epoch != 0 && config.next_epoch <= epoch {
        return Err(FastRuntimeError::InvalidFinalizedDrainCapability);
    }
    validate_enrolled_verifier(l1, portal_address, block, &config).await?;
    let member_count = portal
        .fastEpochMemberCount(epoch)
        .block(block)
        .call()
        .await
        .map_err(l1_error)?;
    if member_count != alloy_primitives::U256::from(3) {
        return Err(FastRuntimeError::InvalidFinalizedRoster);
    }
    let mut members = [Address::ZERO; 3];
    for (index, member) in members.iter_mut().enumerate() {
        *member = portal
            .fastEpochMemberAt(epoch, alloy_primitives::U256::from(index))
            .block(block)
            .call()
            .await
            .map_err(l1_error)?;
    }
    let peer_count = portal
        .fastEpochPeerCount(epoch)
        .block(block)
        .call()
        .await
        .map_err(l1_error)?;
    if peer_count != alloy_primitives::U256::from(9) {
        return Err(FastRuntimeError::InvalidFinalizedPeers);
    }
    let mut peer_portals = [Address::ZERO; 9];
    for (index, peer) in peer_portals.iter_mut().enumerate() {
        *peer = portal
            .fastEpochPeerAt(epoch, alloy_primitives::U256::from(index))
            .block(block)
            .call()
            .await
            .map_err(l1_error)?;
    }
    if config.peers_hash != keccak256(peer_portals.to_vec().abi_encode()) {
        return Err(FastRuntimeError::InvalidFinalizedPeers);
    }
    if config.roster_hash
        != finalized_roster_hash(portal_address, epoch, &config, members, peer_portals)
    {
        return Err(FastRuntimeError::InvalidFinalizedRoster);
    }
    let activated_at_l1_block = config.activated_at_tempo_block;
    let activation = FastActivation::from_finalized_epoch(FinalizedT14Capability {
        epoch: FinalizedFastEpoch {
            l1_chain_id: l1.get_chain_id().await.map_err(l1_error)?,
            portal: portal_address,
            zone_id,
            zone_chain_id,
            epoch,
            protocol_version: config.protocol_version,
            threshold: config.threshold,
            proof_mode: config.proof_mode,
            expected_verifier_code_hash: config.expected_verifier_code_hash,
            expected_verifier_config_hash: config.expected_verifier_config_hash,
            members,
            peer_portals,
            roster_hash: config.roster_hash,
            finalized_l1_block: imported_anchor.number,
        },
        native_pin,
        t14_active_at_anchor: true,
        current_epoch: epoch,
        activated_at_l1_block,
        closed: config.closed,
        retired: config.retired,
    })?;
    Ok(Some(LoadedFastActivation {
        activation,
        admission_open: !config.closed,
        proof_policy: FinalizedFastProofPolicy {
            mode: config.proof_mode,
            expected_verifier_code_hash: config.expected_verifier_code_hash,
            expected_verifier_config_hash: config.expected_verifier_config_hash,
        },
        drain: FinalizedFastDrainCapability {
            closed: config.closed,
            retired: config.retired,
            closure_hash: config.closure_hash,
            final_settlement_height: config.final_settlement_height,
            final_settlement_block_hash: config.final_settlement_block_hash,
            final_settlement_withdrawal_batch_index: config.final_settlement_withdrawal_batch_index,
            barriers_hash: config.barriers_hash,
            final_settlement_hash: config.final_settlement_hash,
            next_epoch: config.next_epoch,
            next_roster_hash: config.next_roster_hash,
            checkpoint_log_term: config.checkpoint_log_term,
            checkpoint_log_index: config.checkpoint_log_index,
            checkpoint_height: config.checkpoint_height,
            checkpoint_block_hash: config.checkpoint_block_hash,
            checkpoint_state_root: config.checkpoint_state_root,
            checkpoint_hash: config.checkpoint_hash,
        },
        allow_initialize: activated_at_l1_block == imported_anchor.number,
        anchor_timestamp: anchor.timestamp(),
        anchor_timestamp_millis_part: (anchor.timestamp_millis % 1_000) as u16,
    }))
}

fn enrollment_sentinel(epoch: &FinalizedFastEpoch, proof_policy: FinalizedFastProofPolicy) -> B256 {
    let mut encoded = Vec::with_capacity(256);
    encoded.extend_from_slice(ENROLLMENT_SENTINEL_DOMAIN);
    encoded.extend_from_slice(&epoch.l1_chain_id.to_be_bytes());
    encoded.extend_from_slice(epoch.portal.as_slice());
    encoded.extend_from_slice(&epoch.zone_id.to_be_bytes());
    encoded.extend_from_slice(&epoch.zone_chain_id.to_be_bytes());
    encoded.extend_from_slice(&epoch.epoch.to_be_bytes());
    encoded.extend_from_slice(&epoch.protocol_version.to_be_bytes());
    encoded.push(epoch.threshold);
    for member in epoch.members {
        encoded.extend_from_slice(member.as_slice());
    }
    for portal in epoch.peer_portals {
        encoded.extend_from_slice(portal.as_slice());
    }
    encoded.extend_from_slice(epoch.roster_hash.as_slice());
    encoded.push(proof_policy.mode);
    encoded.extend_from_slice(proof_policy.expected_verifier_code_hash.as_slice());
    encoded.extend_from_slice(proof_policy.expected_verifier_config_hash.as_slice());
    keccak256(encoded)
}

fn persist_or_validate_enrollment(
    storage: &std::path::Path,
    epoch: &FinalizedFastEpoch,
    proof_policy: FinalizedFastProofPolicy,
    may_create: bool,
    state_present: bool,
) -> Result<(), FastRuntimeError> {
    let path = storage.join(ENROLLMENT_SENTINEL);
    let expected = enrollment_sentinel(epoch, proof_policy);
    match std::fs::read(&path) {
        Ok(actual) if actual.as_slice() == expected.as_slice() && state_present => return Ok(()),
        Ok(actual) if actual.as_slice() == expected.as_slice() => {
            return Err(FastRuntimeError::EnrollmentWithoutState(path));
        }
        Ok(_) => return Err(FastRuntimeError::EnrollmentConflict(path)),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        Err(_) if !may_create => return Err(FastRuntimeError::EnrollmentMissing(path)),
        Err(_) => {}
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    file.write_all(expected.as_slice())?;
    file.sync_all()?;
    std::fs::File::open(storage)?.sync_all()?;
    Ok(())
}

/// Verify that a nonempty prefix installed by the disjoint successor process is the exact
/// checkpoint finalized by the predecessor epoch. The current epoch roster and the predecessor
/// checkpoint are read at one hash-canonical imported Tempo anchor; local files supply no
/// authority of their own.
pub async fn verify_finalized_successor_checkpoint(
    l1: &DynProvider<TempoNetwork>,
    imported_anchor: NumHash,
    activation: &FastActivation,
    storage: &std::path::Path,
) -> Result<bool, FastRuntimeError> {
    let checkpoint_path = storage.join("checkpoint-image.bin");
    if !checkpoint_path.is_file() {
        return Ok(false);
    }
    let encoded = std::fs::read(&checkpoint_path)?;
    let image = CheckpointImage::decode_durable(&encoded)
        .map_err(|_| FastRuntimeError::InvalidSuccessorCheckpoint(storage.to_path_buf()))?;
    image
        .validate()
        .map_err(|_| FastRuntimeError::InvalidSuccessorCheckpoint(storage.to_path_buf()))?;
    let current = activation.epoch();
    if image.l1_chain_id != current.l1_chain_id
        || image.statement.portal != current.portal
        || image.statement.next_epoch != current.epoch
        || image.statement.next_roster_hash != current.roster_hash
        || image.statement.old_epoch >= current.epoch
    {
        return Err(FastRuntimeError::InvalidSuccessorCheckpoint(
            storage.to_path_buf(),
        ));
    }
    let pinned = alloy_rpc_types_eth::BlockId::hash_canonical(imported_anchor.hash);
    let predecessor =
        read_fast_epoch_config(l1, current.portal, image.statement.old_epoch, pinned).await?;
    let statement = &image.statement;
    if !predecessor.closed
        || !predecessor.retired
        || predecessor.next_epoch != current.epoch
        || predecessor.next_roster_hash != current.roster_hash
        || predecessor.final_settlement_height != statement.final_zone_height
        || predecessor.final_settlement_block_hash != statement.final_block_hash
        || predecessor.final_settlement_withdrawal_batch_index
            != statement.final_withdrawal_batch_index
        || predecessor.final_settlement_hash != statement.final_settlement_hash
        || predecessor.checkpoint_log_term != statement.checkpoint_log_term
        || predecessor.checkpoint_log_index != statement.checkpoint_log_index
        || predecessor.checkpoint_height != statement.checkpoint_height
        || predecessor.checkpoint_block_hash != statement.checkpoint_block_hash
        || predecessor.checkpoint_state_root != statement.checkpoint_state_root
        || predecessor.checkpoint_hash != statement.registry_digest(image.l1_chain_id)
    {
        return Err(FastRuntimeError::InvalidSuccessorCheckpoint(
            storage.to_path_buf(),
        ));
    }
    let installed = inspect_exact_state_image(&image.consensus_snapshot)
        .map_err(|_| FastRuntimeError::InvalidSuccessorCheckpoint(storage.to_path_buf()))?;
    let installed_head = installed
        .blocks
        .last()
        .ok_or_else(|| FastRuntimeError::InvalidSuccessorCheckpoint(storage.to_path_buf()))?;
    if installed.last_applied.leader_id.term != statement.checkpoint_log_term
        || installed.last_applied.index != statement.checkpoint_log_index
        || alloy_primitives::U256::from(installed_head.output.block_height)
            != statement.checkpoint_height
        || installed_head.output.block_hash != statement.checkpoint_block_hash
        || installed_head.output.state_root != statement.checkpoint_state_root
    {
        return Err(FastRuntimeError::InvalidSuccessorCheckpoint(
            storage.to_path_buf(),
        ));
    }
    let current = inspect_exact_state_image(&std::fs::read(
        storage.join("state-machine/raft-state-machine.bin"),
    )?)
    .map_err(|_| FastRuntimeError::InvalidSuccessorCheckpoint(storage.to_path_buf()))?;
    if current.last_applied.index < installed.last_applied.index
        || !current.blocks.iter().any(|block| {
            block.log_id == installed.last_applied && block.output == installed_head.output
        })
    {
        return Err(FastRuntimeError::InvalidSuccessorCheckpoint(
            storage.to_path_buf(),
        ));
    }
    Ok(true)
}

/// Assemble durable stores, authenticated transport, peer handlers, and exact three-member
/// OpenRaft membership. Existing Zone state may never silently acquire an empty Raft history.
pub async fn assemble_production_fast_runtime<E, P>(
    activation: FastActivation,
    proof_policy: FinalizedFastProofPolicy,
    allow_initialize: bool,
    successor_bootstrap: Option<SuccessorBootstrapArtifact>,
    config: FastRuntimeConfig,
    provider: &P,
    executor: Arc<E>,
    commands: mpsc::Sender<P2pCommand>,
    raft_ports: RaftPorts,
) -> Result<AssembledFastRuntime<E>, FastRuntimeError>
where
    E: DurableStateMachineExecution,
    P: BlockNumReader + BlockReader<Block = tempo_primitives::Block>,
{
    config.validate()?;
    let epoch = activation.epoch();
    let local_index = epoch
        .members
        .iter()
        .position(|member| *member == config.signer.address())
        .ok_or(FastRuntimeError::LocalMemberNotEnrolled)?;
    let log_exists = config.storage.join("log/raft-log.bin").is_file();
    let state_machine_exists = config
        .storage
        .join("state-machine/raft-state-machine.bin")
        .is_file();
    if log_exists != state_machine_exists {
        return Err(FastRuntimeError::IncompleteEnrolledStorage(config.storage));
    }
    let has_state = log_exists && state_machine_exists;
    let best = provider
        .best_block_number()
        .map_err(|error| FastRuntimeError::Provider(error.to_string()))?;
    let already_produced_fast_block = allow_initialize
        && provider
            .block_by_number(best)
            .map_err(|error| FastRuntimeError::Provider(error.to_string()))?
            .is_some_and(|block| {
                block.body.transactions.first().is_some_and(|transaction| {
                    zone_evm::same_anchor::is_same_anchor_opening(transaction.input())
                })
            });
    if (!allow_initialize || already_produced_fast_block) && !has_state {
        return Err(FastRuntimeError::MissingEnrolledStorage(config.storage));
    }
    let members = epoch
        .members
        .iter()
        .enumerate()
        .map(|(index, member)| {
            let node_id = u64::try_from(index + 1).expect("three members fit u64");
            let transport = config
                .member_transports
                .get(member)
                .cloned()
                .ok_or(FastRuntimeError::EndpointMissing(*member))?;
            Ok((
                node_id,
                FinalizedPeerIdentity {
                    member: *member,
                    transport,
                },
            ))
        })
        .collect::<Result<BTreeMap<_, _>, FastRuntimeError>>()?;
    let local_node_id = u64::try_from(local_index + 1).expect("three members fit u64");
    let RaftPorts {
        requests,
        responses,
    } = raft_ports;
    let network_config = FastNetworkConfig {
        epoch: epoch.epoch,
        local_node_id,
        local_member: config.signer.address(),
        local_transport: config.local_transport,
        members,
        rpc_timeout: config.rpc_timeout,
    };
    network_config.validate()?;
    let raft_config = Arc::new(
        Config {
            cluster_name: format!("tempo-zone-{}-fast-epoch-{}", epoch.zone_id, epoch.epoch),
            ..Config::default()
        }
        .validate()?,
    );
    std::fs::create_dir_all(&config.storage)?;
    persist_or_validate_enrollment(
        &config.storage,
        epoch,
        proof_policy,
        !has_state || successor_bootstrap.is_some(),
        has_state,
    )?;
    let transport = AuthenticatedRaftTransport::new(network_config, commands.clone(), responses)?;
    let runtime = Arc::new(
        assemble_fast_raft(
            &activation,
            transport.config().local_member,
            raft_config,
            Arc::new(transport.clone()),
            &config.storage,
            executor,
        )
        .await?,
    );
    let membership = epoch
        .members
        .iter()
        .enumerate()
        .map(|(index, member)| {
            (
                u64::try_from(index + 1).expect("three members fit u64"),
                BasicNode::new(
                    config
                        .member_transports
                        .get(member)
                        .expect("validated endpoint exists")
                        .to_string(),
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    if let Some(successor) = successor_bootstrap.as_ref() {
        let expected = membership.clone();
        if successor.next_epoch != epoch.epoch
            || successor.next_roster_hash != epoch.roster_hash
            || successor.next_membership != expected
        {
            return Err(FastRuntimeError::InvalidSuccessorCheckpoint(
                config.storage.clone(),
            ));
        }
        let exact = runtime
            .committed_handle()
            .exact_state_image()
            .map_err(|error| FastRuntimeError::CommittedState(error.to_string()))?;
        if exact.last_applied.index < successor.accepted_prefix.index
            || !exact
                .blocks
                .iter()
                .any(|block| block.log_id == successor.accepted_prefix)
        {
            return Err(FastRuntimeError::InvalidSuccessorCheckpoint(
                config.storage.clone(),
            ));
        }
        let height = exact
            .blocks
            .last()
            .ok_or_else(|| FastRuntimeError::InvalidSuccessorCheckpoint(config.storage.clone()))?
            .output
            .block_height;
        let canonical = provider
            .block_by_number(height)
            .map_err(|error| FastRuntimeError::Provider(error.to_string()))?
            .ok_or_else(|| FastRuntimeError::InvalidSuccessorCheckpoint(config.storage.clone()))?;
        let expected_head = exact.blocks.last().expect("checked above");
        if canonical.header.hash_slow() != expected_head.output.block_hash
            || canonical.header.state_root() != expected_head.output.state_root
        {
            return Err(FastRuntimeError::InvalidSuccessorCheckpoint(
                config.storage.clone(),
            ));
        }
    }
    let needs_initialize = !runtime
        .raft
        .inner()
        .is_initialized()
        .await
        .map_err(|error| FastRuntimeError::Initialize(error.to_string()))?;
    Ok(AssembledFastRuntime {
        runtime,
        transport,
        commands,
        requests,
        local_node_id,
        epoch: epoch.epoch,
        initial_membership: membership,
        needs_initialize,
        successor_storage: successor_bootstrap.map(|_| config.storage),
    })
}

pub struct AssembledFastRuntime<E: DurableStateMachineExecution> {
    pub runtime: Arc<FastRaftRuntime<E>>,
    pub transport: AuthenticatedRaftTransport,
    pub local_node_id: u64,
    pub epoch: u64,
    pub commands: mpsc::Sender<P2pCommand>,
    pub requests: mpsc::Receiver<RaftRequestFrame>,
    pub initial_membership: BTreeMap<u64, BasicNode>,
    pub needs_initialize: bool,
    pub successor_storage: Option<PathBuf>,
}

/// Bootstrap exactly once, from finalized roster member 1, after all authenticated peer handlers
/// are serving. Followers acquire the initial membership only through AppendEntries.
pub async fn initialize_production_fast_runtime<E: DurableStateMachineExecution>(
    runtime: &FastRaftRuntime<E>,
    local_node_id: u64,
    needs_initialize: bool,
    membership: BTreeMap<u64, BasicNode>,
) -> Result<(), FastRuntimeError> {
    if needs_initialize && local_node_id == 1 {
        runtime
            .raft
            .inner()
            .initialize(membership)
            .await
            .map_err(|error| FastRuntimeError::Initialize(error.to_string()))?;
    }
    Ok(())
}

pub trait LocalOutcomeSigner: Send + Sync + 'static {
    fn sign_local(&self, request: OutcomeSigningRequest) -> Result<SignedOutcome, String>;
}

pub struct ProductionDrainPhaseSigner {
    requester_roster: EpochRoster,
    signing_roster: EpochRoster,
    signer: PrivateKeySigner,
    signing_journal: Arc<DurableJournal>,
    committed: Arc<dyn FastDrainCommittedState>,
}

impl ProductionDrainPhaseSigner {
    pub fn new(
        roster: EpochRoster,
        signer: PrivateKeySigner,
        signing_journal: Arc<DurableJournal>,
        committed: Arc<dyn FastDrainCommittedState>,
    ) -> Self {
        Self::new_handoff(roster.clone(), roster, signer, signing_journal, committed)
    }

    pub fn new_handoff(
        requester_roster: EpochRoster,
        signing_roster: EpochRoster,
        signer: PrivateKeySigner,
        signing_journal: Arc<DurableJournal>,
        committed: Arc<dyn FastDrainCommittedState>,
    ) -> Self {
        Self {
            requester_roster,
            signing_roster,
            signer,
            signing_journal,
            committed,
        }
    }
}

impl DrainPhaseSigner for ProductionDrainPhaseSigner {
    fn sign(
        &self,
        authenticated_requester: Address,
        purpose: DrainSigningPurpose,
        key: &DrainObjectKey,
        digest: B256,
        point: CommittedDrainPoint,
    ) -> Result<SignatureBytes, String> {
        sign_authenticated_drain_phase_handoff(
            authenticated_requester,
            &self.requester_roster,
            &self.signing_roster,
            &self.signer,
            self.signing_journal.as_ref(),
            self.committed.as_ref(),
            purpose,
            key,
            digest,
            point,
        )
        .map_err(|error| error.to_string())
    }
}

/// One production handler owns both consensus RPCs and the local committed-outcome signer.
pub struct ProductionPeerHandler<E: DurableStateMachineExecution> {
    runtime: Arc<FastRaftRuntime<E>>,
    signer: Arc<dyn LocalOutcomeSigner>,
    drain_signer: Arc<OnceLock<Arc<dyn DrainPhaseSigner>>>,
}

impl<E: DurableStateMachineExecution> ProductionPeerHandler<E> {
    pub fn new(
        runtime: Arc<FastRaftRuntime<E>>,
        signer: Arc<dyn LocalOutcomeSigner>,
        drain_signer: Arc<OnceLock<Arc<dyn DrainPhaseSigner>>>,
    ) -> Self {
        Self {
            runtime,
            signer,
            drain_signer,
        }
    }
}

impl<E: DurableStateMachineExecution> FastRaftPeerHandler for ProductionPeerHandler<E> {
    fn append_entries(
        &self,
        peer: AuthenticatedRaftPeer,
        request: openraft::raft::AppendEntriesRequest<crate::fast_quorum::FastRaftConfig>,
    ) -> HandlerFuture<'_, openraft::raft::AppendEntriesResponse<u64>> {
        Box::pin(async move {
            self.runtime
                .handle_append_entries(peer, request)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn vote(
        &self,
        peer: AuthenticatedRaftPeer,
        request: openraft::raft::VoteRequest<u64>,
    ) -> HandlerFuture<'_, openraft::raft::VoteResponse<u64>> {
        Box::pin(async move {
            self.runtime
                .handle_vote(peer, request)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn install_snapshot(
        &self,
        peer: AuthenticatedRaftPeer,
        request: openraft::raft::InstallSnapshotRequest<crate::fast_quorum::FastRaftConfig>,
    ) -> HandlerFuture<'_, openraft::raft::InstallSnapshotResponse<u64>> {
        Box::pin(async move {
            self.runtime
                .handle_install_snapshot(peer, request)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn sign_outcome(
        &self,
        _peer: AuthenticatedRaftPeer,
        request: OutcomeSigningRequest,
    ) -> HandlerFuture<'_, SignedOutcome> {
        Box::pin(async move { self.signer.sign_local(request) })
    }

    fn sign_drain_phase(
        &self,
        peer: AuthenticatedRaftPeer,
        purpose: crate::fast_drain::DrainSigningPurpose,
        key: crate::fast_drain::DrainObjectKey,
        digest: B256,
        point: crate::fast_drain::CommittedDrainPoint,
    ) -> HandlerFuture<'_, zone_primitives::fast_transfer::SignatureBytes> {
        Box::pin(async move {
            self.drain_signer
                .get()
                .ok_or_else(|| "committed drain signing is not installed".to_owned())?
                .sign(peer.member, purpose, &key, digest, point)
        })
    }
}

pub struct ProductionOutcomeCertification<P> {
    execution: Arc<CanonicalFastExecution<P>>,
    committed: CommittedStateHandle<CanonicalFastExecution<P>>,
    activation: FastActivation,
    signer: PrivateKeySigner,
    transport: AuthenticatedRaftTransport,
    local_node_id: u64,
    certification_lock: tokio::sync::Mutex<()>,
    recovered_through: AtomicU64,
}

/// Runtime-owned committed facade used by the C4 service. Every read is fenced by the fsynced
/// state-machine image and certificate recovery uses the production quorum path.
pub struct ProductionCommittedTransferSource<P> {
    committed: CommittedStateHandle<CanonicalFastExecution<P>>,
    certification: Arc<ProductionOutcomeCertification<P>>,
}

impl<P> ProductionCommittedTransferSource<P> {
    pub const fn new(
        committed: CommittedStateHandle<CanonicalFastExecution<P>>,
        certification: Arc<ProductionOutcomeCertification<P>>,
    ) -> Self {
        Self {
            committed,
            certification,
        }
    }
}

impl<P> CommittedTransferSource for ProductionCommittedTransferSource<P>
where
    P: reth_storage_api::BlockNumReader
        + reth_storage_api::BlockReader<Block = tempo_primitives::Block>
        + reth_storage_api::HeaderProvider<Header = tempo_primitives::TempoHeader>
        + reth_storage_api::ReceiptProvider<Receipt = tempo_primitives::TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    fn committed_transfers(&self) -> Result<Vec<CommittedTransferRecord>, FastServiceError> {
        self.committed
            .committed_transfers()
            .map_err(|error| FastServiceError::CommittedState(error.to_string()))
    }

    fn committed_transfer(
        &self,
        transfer_id: B256,
    ) -> Result<Option<CommittedTransferRecord>, FastServiceError> {
        self.committed
            .committed_transfer(transfer_id)
            .map_err(|error| FastServiceError::CommittedState(error.to_string()))
    }

    fn committed_height(&self) -> Result<u64, FastServiceError> {
        self.committed
            .committed_head()
            .map_err(|error| FastServiceError::CommittedState(error.to_string()))?
            .map(|head| head.block.block_height)
            .ok_or_else(|| {
                FastServiceError::CommittedState("committed fast head is unavailable".to_owned())
            })
    }

    fn committed_transaction_result(
        &self,
        transaction_hash: B256,
    ) -> Result<Option<bool>, FastServiceError> {
        self.committed
            .committed_transaction_result(transaction_hash)
            .map_err(|error| FastServiceError::CommittedState(error.to_string()))
    }

    fn ensure_certificate<'a>(
        &'a self,
        record: &'a CommittedTransferRecord,
    ) -> ServiceFuture<'a, Result<OutcomeCertificate, FastServiceError>> {
        Box::pin(async move {
            self.certification
                .ensure_outcome_certificate(OutcomeSigningRequest::from(&record.body))
                .await
                .map_err(FastServiceError::CommittedState)
        })
    }
}

impl<P> ProductionOutcomeCertification<P> {
    pub fn new(
        execution: Arc<CanonicalFastExecution<P>>,
        committed: CommittedStateHandle<CanonicalFastExecution<P>>,
        activation: FastActivation,
        signer: PrivateKeySigner,
        transport: AuthenticatedRaftTransport,
        local_node_id: u64,
    ) -> Self {
        Self {
            execution,
            committed,
            activation,
            signer,
            transport,
            local_node_id,
            certification_lock: tokio::sync::Mutex::new(()),
            recovered_through: AtomicU64::new(0),
        }
    }
}

impl<P> ProductionOutcomeCertification<P>
where
    P: reth_storage_api::BlockNumReader
        + reth_storage_api::BlockReader<Block = tempo_primitives::Block>
        + reth_storage_api::HeaderProvider<Header = tempo_primitives::TempoHeader>
        + reth_storage_api::ReceiptProvider<Receipt = tempo_primitives::TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    /// Return the durable certificate for one committed transfer, recollecting the exact original
    /// `(term, index)` body when a crash interrupted assembly after state-machine application.
    pub async fn ensure_transfer_certificate(
        &self,
        transfer_id: B256,
    ) -> Result<OutcomeCertificate, String> {
        let record = self
            .execution
            .committed_transfer(transfer_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "committed transfer is unavailable".to_owned())?;
        self.ensure_outcome_certificate(OutcomeSigningRequest::from(&record.body))
            .await
    }

    /// Return the certificate for one exact historical outcome identity. Callers that already
    /// read a committed record must not silently switch to a newer body for the same transfer.
    pub async fn ensure_outcome_certificate(
        &self,
        request: OutcomeSigningRequest,
    ) -> Result<OutcomeCertificate, String> {
        let record = self
            .execution
            .outcome_for_signing(request)
            .map_err(|error| error.to_string())?;
        if let Some(certificate) = record.certificate.clone() {
            return Ok(certificate);
        }
        let commit = RaftCommit {
            term: record.body.log_term,
            index: record.body.log_index,
            block: crate::fast_quorum::CommittedBlock {
                input_digest: B256::ZERO,
                block_height: record.body.block_height,
                block_hash: record.body.block_hash,
                state_root: record.body.state_root,
                receipts_root: B256::ZERO,
            },
        };
        self.certify_commit(commit).await?;
        self.execution
            .outcome_for_signing(request)
            .map_err(|error| error.to_string())?
            .certificate
            .ok_or_else(|| "committed transfer certificate was not durably assembled".to_owned())
    }

    /// Recollect any certificate interrupted after commit, preserving the original term/index.
    pub async fn certify_pending(&self) -> Result<(), String> {
        let through = self.recovered_through.load(Ordering::Acquire);
        let applied = self
            .committed
            .applied_blocks()
            .map_err(|error| error.to_string())?;
        for entry in applied
            .into_iter()
            .filter(|entry| entry.log_id.index > through)
        {
            let commit = RaftCommit {
                term: entry.log_id.leader_id.term,
                index: entry.log_id.index,
                block: entry.output,
            };
            self.certify_commit(commit).await?;
            self.recovered_through
                .store(entry.log_id.index, Ordering::Release);
        }
        Ok(())
    }
}

impl<P> LocalOutcomeSigner for ProductionOutcomeCertification<P>
where
    P: reth_storage_api::BlockNumReader
        + reth_storage_api::BlockReader<Block = tempo_primitives::Block>
        + reth_storage_api::HeaderProvider<Header = tempo_primitives::TempoHeader>
        + reth_storage_api::ReceiptProvider<Receipt = tempo_primitives::TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    fn sign_local(&self, request: OutcomeSigningRequest) -> Result<SignedOutcome, String> {
        let record = self
            .execution
            .outcome_for_signing(request)
            .map_err(|error| error.to_string())?;
        let applied = self
            .committed
            .applied_blocks()
            .map_err(|error| error.to_string())?;
        validate_outcome_in_applied_prefix(&record, &applied).map_err(|error| error.to_string())?;
        let (body, signature) = self
            .execution
            .sign_committed_transfer(&self.activation, &self.signer, request)
            .map_err(|error| error.to_string())?;
        Ok(SignedOutcome {
            body: body.canonical_bytes(),
            signature: signature.0.to_vec(),
        })
    }
}

impl<P> FastOutcomeCertification for ProductionOutcomeCertification<P>
where
    P: reth_storage_api::BlockNumReader
        + reth_storage_api::BlockReader<Block = tempo_primitives::Block>
        + reth_storage_api::HeaderProvider<Header = tempo_primitives::TempoHeader>
        + reth_storage_api::ReceiptProvider<Receipt = tempo_primitives::TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    fn certify_commit(&self, commit: RaftCommit) -> CertificationFuture<'_> {
        Box::pin(async move {
            let _guard = self.certification_lock.lock().await;
            let applied = self
                .committed
                .applied_blocks()
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|applied| {
                    applied.log_id.leader_id.term == commit.term
                        && applied.log_id.index == commit.index
                })
                .ok_or_else(|| {
                    "certified outcome is outside the fsynced applied prefix".to_owned()
                })?;
            let records = self
                .execution
                .outcomes_for_commit(commit.term, commit.index)
                .map_err(|error| error.to_string())?;
            let epoch = self.activation.epoch();
            let protocol_version = u16::try_from(epoch.protocol_version)
                .map_err(|_| "invalid finalized protocol version".to_owned())?;
            let verifier = QuorumVerifier::new(
                EpochRoster::from_finalized_registry(
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
                .map_err(|error| error.to_string())?,
            );
            for record in records {
                if let Some(certificate) = record.certificate {
                    let history = self
                        .execution
                        .certified_execution_record(&applied, &record.intent, &certificate)
                        .map_err(|error| error.to_string())?;
                    self.committed
                        .persist_certified_execution(history)
                        .map_err(|error| error.to_string())?;
                    continue;
                }
                let signing_request = OutcomeSigningRequest::from(&record.body);
                let local = self.sign_local(signing_request)?;
                let local_signature = signature_from_wire(&local.signature)?;
                let mut remote_signature = None;
                let remote_nodes = self
                    .transport
                    .config()
                    .members
                    .keys()
                    .copied()
                    .filter(|node_id| *node_id != self.local_node_id)
                    .collect::<Vec<_>>();
                let deadline = tokio::time::Instant::now() + self.transport.config().rpc_timeout;
                while remote_signature.is_none() && tokio::time::Instant::now() < deadline {
                    let (first, second) = tokio::join!(
                        self.transport
                            .request_outcome_signature(remote_nodes[0], signing_request),
                        self.transport
                            .request_outcome_signature(remote_nodes[1], signing_request)
                    );
                    for remote in [first, second].into_iter().flatten() {
                        if decode_exact::<CertificateBody>(&remote.body, MAX_CERTIFICATE_BYTES)
                            .ok()
                            .as_ref()
                            == Some(&record.body)
                            && let Ok(signature) = signature_from_wire(&remote.signature)
                        {
                            let candidate = OutcomeCertificate {
                                body: record.body.clone(),
                                signatures: [local_signature, signature],
                            };
                            if verify_committed_certificate(
                                &verifier,
                                &candidate,
                                &record.intent,
                                &commit,
                            )
                            .is_ok()
                            {
                                remote_signature = Some(signature);
                                break;
                            }
                        }
                    }
                    if remote_signature.is_none() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
                let certificate = OutcomeCertificate {
                    body: record.body.clone(),
                    signatures: [
                        local_signature,
                        remote_signature.ok_or_else(|| {
                            "no second finalized member signed the committed outcome".to_owned()
                        })?,
                    ],
                };
                verify_committed_certificate(&verifier, &certificate, &record.intent, &commit)
                    .map_err(|error| error.to_string())?;
                let history = self
                    .execution
                    .certified_execution_record(&applied, &record.intent, &certificate)
                    .map_err(|error| error.to_string())?;
                self.committed
                    .persist_certified_execution(history)
                    .map_err(|error| error.to_string())?;
                self.execution
                    .persist_certificate(certificate)
                    .map_err(|error| error.to_string())?;
            }
            for history in self
                .execution
                .authenticated_terminal_records(&applied)
                .map_err(|error| error.to_string())?
            {
                self.committed
                    .persist_certified_execution(history)
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        })
    }
}

fn signature_from_wire(encoded: &[u8]) -> Result<SignatureBytes, String> {
    let bytes: [u8; 65] = encoded
        .try_into()
        .map_err(|_| "invalid outcome signature length".to_owned())?;
    Ok(SignatureBytes(bytes))
}

impl<E: DurableStateMachineExecution> FastRaftPeerHandler for FastRaftRuntime<E> {
    fn append_entries(
        &self,
        peer: AuthenticatedRaftPeer,
        request: openraft::raft::AppendEntriesRequest<crate::fast_quorum::FastRaftConfig>,
    ) -> HandlerFuture<'_, openraft::raft::AppendEntriesResponse<u64>> {
        Box::pin(async move {
            self.handle_append_entries(peer, request)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn vote(
        &self,
        peer: AuthenticatedRaftPeer,
        request: openraft::raft::VoteRequest<u64>,
    ) -> HandlerFuture<'_, openraft::raft::VoteResponse<u64>> {
        Box::pin(async move {
            self.handle_vote(peer, request)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn install_snapshot(
        &self,
        peer: AuthenticatedRaftPeer,
        request: openraft::raft::InstallSnapshotRequest<crate::fast_quorum::FastRaftConfig>,
    ) -> HandlerFuture<'_, openraft::raft::InstallSnapshotResponse<u64>> {
        Box::pin(async move {
            self.handle_install_snapshot(peer, request)
                .await
                .map_err(|error| error.to_string())
        })
    }
}

/// Preflight one C6 route, reconstruct all unallocated released inventory from the committed
/// prefix, and continuously drive only real provider-backed bridge actions.
pub async fn run_replenishment_route<P>(
    config: FastReplenishmentRuntimeConfig,
    committed: CommittedStateHandle<CanonicalFastExecution<P>>,
    journal: Arc<DurableJournal>,
    stop: CancellationToken,
) -> Result<(), FastRuntimeError>
where
    P: reth_storage_api::BlockNumReader
        + reth_storage_api::BlockReader<Block = tempo_primitives::Block>
        + reth_storage_api::HeaderProvider<Header = tempo_primitives::TempoHeader>
        + reth_storage_api::ReceiptProvider<Receipt = tempo_primitives::TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    if config.poll_interval.is_zero() || config.storage.as_os_str().is_empty() {
        return Err(FastRuntimeError::InvalidConfiguration);
    }
    let actions = Arc::new(FilePreparedActionStore::open(
        config.storage.join("prepared-actions"),
    )?);
    let nonces = Arc::new(
        FileReplenishmentNoncePlanner::open(
            config.storage.join("nonces.json"),
            &config.providers.source_zone,
            config.providers.source_signing.sender,
            &config.providers.tempo_l1_treasury,
            config.providers.treasury_signing.sender,
        )
        .await?,
    );
    let bridge = ProviderBackedReplenishmentBridge::enable(
        config.route.clone(),
        Arc::new(config.providers),
        actions,
        nonces,
    )
    .await?;
    let finalized = bridge.finalized_route();
    if finalized.source_fast_epoch != config.expected_source_fast_epoch
        || finalized.destination_fast_epoch != config.expected_destination_fast_epoch
        || finalized.l1_token_decimals != config.expected_token_decimals
        || finalized.source_token_decimals != config.expected_token_decimals
        || finalized.destination_token_decimals != config.expected_token_decimals
        || finalized.destination_key_index != config.expected_destination_key_index
        || finalized.destination_key_x != config.expected_destination_key_x
        || finalized.destination_key_y_parity != config.expected_destination_key_y_parity
    {
        return Err(FastRuntimeError::InvalidReplenishmentEnrollment);
    }
    let worker = ReplenishmentWorker::new(journal.clone(), bridge);
    let mut interval = tokio::time::interval(config.poll_interval);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = stop.cancelled() => return Ok(()),
            _ = interval.tick() => {
                reconstruct_replenishment_jobs(
                    &config.route,
                    &committed,
                    journal.as_ref(),
                )?;
                for job in journal.unfinished_replenishment_jobs()? {
                    if job.source_inventory != config.route.source_inventory
                        || job.source_fallback != config.route.source_fallback
                        || job.treasury != config.route.treasury
                        || job.destination_pool != config.route.destination_pool_operator
                    {
                        continue;
                    }
                    // One persist-before-I/O transition per tick keeps every route bounded and
                    // gives canonical observations time to advance.
                    let _ = worker.step(job.job_id).await?;
                }
            }
        }
    }
}

fn reconstruct_replenishment_jobs<P>(
    route: &ReplenishmentRouteConfig,
    committed: &CommittedStateHandle<CanonicalFastExecution<P>>,
    journal: &DurableJournal,
) -> Result<(), FastRuntimeError>
where
    P: reth_storage_api::BlockNumReader
        + reth_storage_api::BlockReader<Block = tempo_primitives::Block>
        + reth_storage_api::HeaderProvider<Header = tempo_primitives::TempoHeader>
        + reth_storage_api::ReceiptProvider<Receipt = tempo_primitives::TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    for record in committed
        .committed_transfers()
        .map_err(|error| FastRuntimeError::CommittedState(error.to_string()))?
    {
        let zone_primitives::fast_transfer::TransferOutcome::Released {
            beneficiary,
            amount,
        } = record.body.outcome
        else {
            continue;
        };
        if record.intent.source.portal != route.source_portal
            || record.intent.destination.portal != route.destination_portal
            || record.intent.asset.l1_token != route.l1_token
            || record.intent.asset.source_token != route.source_token
            || record.intent.asset.destination_token != route.destination_token
            || record.intent.destination_pool != route.destination_pool_operator
            || beneficiary != route.source_inventory
        {
            continue;
        }
        if amount.is_zero() || amount > route.maximum_replenishment_amount {
            return Err(FastRuntimeError::InvalidReplenishmentRecord(
                record.body.transfer_id,
            ));
        }
        let job_id = replenishment_job_id(route, record.body.transfer_id);
        if journal.replenishment_job(job_id)?.is_some() {
            continue;
        }
        let job = ReplenishmentJob::allocate(
            job_id,
            route.source_inventory,
            route.source_fallback,
            route.treasury,
            route.destination_pool_operator,
            record.intent.recipient,
            vec![InventoryContribution {
                transfer_id: record.body.transfer_id,
                amount,
            }],
        )?;
        journal.persist_replenishment_job(job)?;
    }
    Ok(())
}

fn replenishment_job_id(route: &ReplenishmentRouteConfig, transfer_id: B256) -> B256 {
    keccak256(
        (
            keccak256("TEMPO_ZONE_FAST_REPLENISHMENT_JOB_T14_V1"),
            route.l1_chain_id,
            route.source_chain_id,
            route.destination_chain_id,
            route.protocol_version,
            route.source_portal,
            route.destination_portal,
            route.l1_token,
            route.source_token,
            route.destination_token,
            transfer_id,
        )
            .abi_encode(),
    )
}

#[derive(Debug, thiserror::Error)]
pub enum FastRuntimeError {
    #[error(
        "fast runtime configuration must specify exactly three member endpoints including the local listener"
    )]
    InvalidConfiguration,
    #[error("finalized L1 header is unavailable")]
    NoFinalizedHeader,
    #[error(
        "remote finalized L1 head {finalized} is behind the Zone's imported Tempo anchor {anchor}"
    )]
    FinalizedHeadBehindAnchor { finalized: u64, anchor: u64 },
    #[error("the Zone's imported Tempo anchor {0:?} is unavailable from the L1 provider")]
    ImportedAnchorUnavailable(NumHash),
    #[error("L1 provider returned {remote:?} for imported Tempo anchor {imported:?}")]
    ImportedAnchorMismatch { imported: NumHash, remote: NumHash },
    #[error("finalized fast epoch does not have exactly three members")]
    InvalidFinalizedRoster,
    #[error("finalized fast epoch does not have exactly nine peer Portals")]
    InvalidFinalizedPeers,
    #[error("finalized fast epoch config has non-canonical ABI encoding")]
    InvalidFinalizedEpochEncoding,
    #[error("finalized fast epoch has an invalid or unsafe proof policy")]
    InvalidFinalizedProofPolicy,
    #[error("finalized fast epoch has an invalid drain or checkpoint capability")]
    InvalidFinalizedDrainCapability,
    #[error("local signing key is not enrolled in the finalized epoch")]
    LocalMemberNotEnrolled,
    #[error("no authenticated manifest identity is configured for finalized member {0}")]
    EndpointMissing(Address),
    #[error("existing enrolled Zone is missing its fast runtime storage at {0}")]
    MissingEnrolledStorage(PathBuf),
    #[error("fast runtime storage has only one of its durable log/state-machine files at {0}")]
    IncompleteEnrolledStorage(PathBuf),
    #[error("installed successor checkpoint does not match finalized predecessor authority at {0}")]
    InvalidSuccessorCheckpoint(PathBuf),
    #[error("existing fast runtime storage is missing its exact enrollment sentinel at {0}")]
    EnrollmentMissing(PathBuf),
    #[error("fast runtime enrollment sentinel conflicts with the finalized epoch at {0}")]
    EnrollmentConflict(PathBuf),
    #[error("fast runtime enrollment sentinel exists but its durable Raft state is missing at {0}")]
    EnrollmentWithoutState(PathBuf),
    #[error("failed to initialize exact three-member Raft cluster: {0}")]
    Initialize(String),
    #[error(transparent)]
    Activation(#[from] crate::fast_quorum::ActivationError),
    #[error(transparent)]
    Assembly(#[from] crate::fast_quorum::AssembleFastRaftError),
    #[error(transparent)]
    Network(#[from] crate::fast_network::FastNetworkError),
    #[error("finalized L1 fast-epoch read failed: {0}")]
    L1(String),
    #[error("Zone provider error: {0}")]
    Provider(String),
    #[error("committed fast state read failed: {0}")]
    CommittedState(String),
    #[error("committed transfer {0} cannot be represented by the configured replenishment route")]
    InvalidReplenishmentRecord(B256),
    #[error(
        "provider-backed replenishment enrollment does not match the configured epochs, asset decimals, or destination encryption key"
    )]
    InvalidReplenishmentEnrollment,
    #[error(transparent)]
    FastExecution(#[from] crate::fast_execution::FastExecutionError),
    #[error(transparent)]
    Journal(#[from] zone_fast_transfer::JournalError),
    #[error(transparent)]
    Replenishment(#[from] zone_fast_transfer::ReplenishmentError),
    #[error(transparent)]
    ReplenishmentWorker(#[from] zone_fast_transfer::ReplenishmentWorkerError),
    #[error(transparent)]
    ReplenishmentProvider(#[from] ReplenishmentProviderError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    RaftConfig(#[from] openraft::ConfigError),
}

fn l1_error(error: impl std::fmt::Display) -> FastRuntimeError {
    FastRuntimeError::L1(error.to_string())
}

#[cfg(test)]
mod candidate_roster_tests {
    use super::*;
    use zone_primitives::fast_transfer::ZoneDomain;

    fn old_roster() -> EpochRoster {
        EpochRoster::from_finalized_registry(
            ZoneDomain {
                l1_chain_id: 1,
                zone_id: 7,
                chain_id: 7007,
                portal: Address::repeat_byte(0x70),
                authority_epoch: 4,
                roster_hash: B256::repeat_byte(0x44),
                protocol_version: 14,
            },
            [
                Address::repeat_byte(1),
                Address::repeat_byte(2),
                Address::repeat_byte(3),
            ],
        )
        .unwrap()
    }

    fn candidate(old: &EpochRoster) -> ExpectedCandidateRoster {
        let mut candidate = ExpectedCandidateRoster {
            roster: EpochRoster::from_finalized_registry(
                ZoneDomain {
                    authority_epoch: 5,
                    roster_hash: B256::repeat_byte(0xaa),
                    ..old.domain
                },
                [
                    Address::repeat_byte(4),
                    Address::repeat_byte(5),
                    Address::repeat_byte(6),
                ],
            )
            .unwrap(),
            threshold: 2,
            proof_policy: FinalizedFastProofPolicy {
                mode: PROOF_MODE_REQUIRED,
                expected_verifier_code_hash: B256::repeat_byte(0x71),
                expected_verifier_config_hash: B256::repeat_byte(0x72),
            },
            peer_portals: std::array::from_fn(|index| {
                Address::repeat_byte(u8::try_from(index + 0x20).unwrap())
            }),
        };
        candidate.roster.domain.roster_hash = candidate_roster_hash(&candidate);
        candidate
    }

    fn closed_capability() -> FinalizedFastDrainCapability {
        FinalizedFastDrainCapability {
            closed: true,
            retired: false,
            closure_hash: B256::repeat_byte(0x10),
            final_settlement_height: alloy_primitives::U256::from(17),
            final_settlement_block_hash: B256::repeat_byte(0x11),
            final_settlement_withdrawal_batch_index: 9,
            barriers_hash: B256::repeat_byte(0x12),
            final_settlement_hash: B256::repeat_byte(0x13),
            next_epoch: 0,
            next_roster_hash: B256::ZERO,
            checkpoint_log_term: 0,
            checkpoint_log_index: 0,
            checkpoint_height: alloy_primitives::U256::ZERO,
            checkpoint_block_hash: B256::ZERO,
            checkpoint_state_root: B256::ZERO,
            checkpoint_hash: B256::ZERO,
        }
    }

    #[test]
    fn disjoint_candidate_is_installable_before_native_successor_exists() {
        let old = old_roster();
        let next = candidate(&old);
        assert!(
            old.members
                .iter()
                .all(|member| !next.roster.members.contains(member))
        );

        let resolved = load_finalized_next_roster(&old, closed_capability(), Some(&next))
            .unwrap()
            .unwrap();
        assert_eq!(resolved, next.roster);

        let mut installed = closed_capability();
        installed.next_epoch = next.roster.domain.authority_epoch;
        installed.next_roster_hash = next.roster.domain.roster_hash;
        assert_eq!(
            load_finalized_next_roster(&old, installed, Some(&next)).unwrap(),
            Some(next.roster)
        );
    }

    #[test]
    fn candidate_commitment_or_lifecycle_mismatch_fails_closed() {
        let old = old_roster();
        let mut next = candidate(&old);
        next.peer_portals[0] = Address::repeat_byte(0x7f);
        assert!(load_finalized_next_roster(&old, closed_capability(), Some(&next)).is_err());

        let next = candidate(&old);
        let mut capability = closed_capability();
        capability.retired = true;
        assert!(load_finalized_next_roster(&old, capability, Some(&next)).is_err());
    }
}
