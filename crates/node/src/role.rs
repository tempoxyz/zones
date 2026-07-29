//! Dynamic role supervision for multi-sequencer zone nodes.
//!
//! The [`RoleController`] switches complete role generations — block production or import,
//! broadcast, transaction flow, settlement, and sequencer background tasks — as one fenced
//! unit, driven by the finalized [`LeadershipSchedule`].
//!
//! The control rule is about the **next anchor**, never the last applied one: for the next
//! Tempo anchor `N` to be consumed, if `schedule.leader_for(N)` is this node and the zone
//! block embedding `N − 1` is locally canonical, the controller runs the leader generation
//! and the engine produces `N`; otherwise it runs the follower generation and imports. The
//! per-anchor [`ProductionPermit`](crate::ProductionPermit) inside the engine and the
//! anchor-aware sender fence inside follower import are the protocol fences; generation
//! switching is lifecycle management.

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use reth_chain_state::PersistedBlockSubscriptions;
use reth_node_api::ConsensusEngineHandle;
use reth_payload_builder::PayloadBuilderHandle;
use reth_primitives_traits::SealedHeader;
use reth_provider::HeaderProvider;
use reth_storage_api::{BlockNumReader, BlockReader, ReceiptProvider, StateProviderFactory};
use reth_transaction_pool::TransactionPool;
use tempo_primitives::{Block, TempoHeader};
use tokio::{sync::mpsc, task::JoinSet};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
use tracing::{debug, error, info, warn};
use zone_chainspec::ZoneChainSpec;
use zone_l1::{DepositQueue, L1BlockTracker, TempoStateExt as _};
use zone_p2p::{LeadershipSchedule, P2pCommand, P2pEvent, P2pPeerId};
use zone_payload::ZonePayloadTypes;
use zone_sequencer::{
    ZoneSequencerConfig, ZoneSequencerHandle, ZoneSequencerProvider, spawn_zone_sequencer,
};
use zone_transaction_pool_alias::TempoPooledTransaction;

mod zone_transaction_pool_alias {
    pub(super) use tempo_transaction_pool::transaction::TempoPooledTransaction;
}

use crate::{
    EngineExit, ProductionPermit, ZoneEngine, ZoneSequencerAddOnsConfig,
    replication::{
        AttestationContext, BackfillRequest, PeerTipRegistry, broadcast_persisted_blocks,
        collect_follower_settlement_signatures, run_follower_block_sync,
    },
    settlement_attestation::collect_leader_settlements,
    tx_forwarding::{forward_new_transactions, insert_forwarded_transactions},
};

/// Backoff after a transient role or promotion-readiness derivation failure.
const ROLE_DECISION_RETRY_BACKOFF: Duration = Duration::from_millis(500);
/// How long a stopping generation may take before its remaining tasks are aborted.
const GENERATION_STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Backoff after a generation task fails unexpectedly.
const GENERATION_RESTART_BACKOFF: Duration = Duration::from_millis(500);
/// Buffer size for per-generation event channels.
const GENERATION_EVENT_BACKLOG: usize = 128;
/// Tip evidence older than this is stale for promotion decisions.
const TIP_EVIDENCE_TTL: Duration = Duration::from_secs(30);

/// Everything a role generation needs to start its tasks.
pub(crate) struct RoleControllerContext<P, Pool> {
    pub local_ed25519_public_key: P2pPeerId,
    pub schedule: LeadershipSchedule,
    pub provider: P,
    pub pool: Pool,
    pub engine_handle: ConsensusEngineHandle<ZonePayloadTypes>,
    pub payload_builder: PayloadBuilderHandle<ZonePayloadTypes>,
    pub chain_spec: Arc<ZoneChainSpec>,
    pub deposit_queue: DepositQueue,
    pub l1_block_tracker: L1BlockTracker,
    pub commands: mpsc::Sender<P2pCommand>,
    pub attestation: AttestationContext,
    pub portal_address: Address,
    /// Sequencer resources constructed unconditionally at startup; activation is gated by
    /// the leader generation. `None` means this node can never lead.
    pub sequencer: Option<LeaderSequencerDeps>,
    /// Tip evidence advertised by peers via backfill completions.
    pub peer_tips: PeerTipRegistry,
    /// Live role/readiness snapshot shared with the status RPC.
    pub status: SharedRoleStatus,
}

/// Live role and promotion-readiness snapshot for observability and the status RPC.
#[derive(Debug, Clone, Default)]
pub struct RoleStatus {
    /// `"leader"`, `"follower"`, or `"fenced"`.
    pub role: &'static str,
    /// Monotonic generation counter.
    pub generation: u64,
    /// Epoch of the record governing the next anchor, when known.
    pub epoch: Option<u64>,
    /// Whether the promotion barrier is currently satisfied.
    pub ready_for_promotion: bool,
    /// Unsatisfied promotion-readiness reasons (empty when ready).
    pub promotion_reasons: Vec<String>,
}

/// Shared handle to the live [`RoleStatus`].
pub type SharedRoleStatus = Arc<std::sync::Mutex<RoleStatus>>;

/// Leader-only background task dependencies (batch submission, withdrawal processing).
pub(crate) struct LeaderSequencerDeps {
    pub config: ZoneSequencerAddOnsConfig,
    pub sequencer_config: ZoneSequencerConfig,
}

/// Sinks for the long-lived P2P event demultiplexer.
///
/// Role generations receive typed substreams, so dropping one generation never drops the
/// underlying network event stream.
#[derive(Clone, Default)]
pub(crate) struct EventSinks {
    inner: Arc<std::sync::Mutex<GenerationSinks>>,
}

#[derive(Default)]
struct GenerationSinks {
    sync: Option<mpsc::Sender<P2pEvent>>,
    transactions: Option<mpsc::Sender<P2pEvent>>,
}

impl EventSinks {
    fn install(&self, sync: mpsc::Sender<P2pEvent>, transactions: Option<mpsc::Sender<P2pEvent>>) {
        let mut sinks = self.inner.lock().expect("poisoned");
        sinks.sync = Some(sync);
        sinks.transactions = transactions;
    }

    fn clear(&self) {
        let mut sinks = self.inner.lock().expect("poisoned");
        sinks.sync = None;
        sinks.transactions = None;
    }

    fn sink_for(&self, event: &P2pEvent) -> Option<mpsc::Sender<P2pEvent>> {
        let sinks = self.inner.lock().expect("poisoned");
        if matches!(event, P2pEvent::TransactionReceived { .. }) {
            sinks.transactions.clone()
        } else {
            sinks.sync.clone()
        }
    }
}

/// Long-lived P2P event demultiplexer.
///
/// Owns the single event receiver for the process lifetime. Backfill requests are
/// generation-independent — every role serves the same canonical provider — so they go to
/// the process-lifetime backfill server, never to a role generation; dropping one would
/// suppress the requesting peer until its response timeout and stall a leadership handoff.
/// Every other event is forwarded to the active generation's substream, and events arriving
/// between generations are dropped because the successor re-derives its state from the
/// provider and schedule. Generation delivery is deliberately non-blocking: when a generation
/// falls behind, dropping recoverable events is preferable to blocking process-lifetime backfill
/// serving behind generation-local backpressure.
pub(crate) async fn route_events_to_generations(
    mut events: mpsc::Receiver<P2pEvent>,
    sinks: EventSinks,
    backfill: mpsc::Sender<BackfillRequest>,
) {
    while let Some(event) = events.recv().await {
        if let P2pEvent::BackfillRequested {
            peer,
            request_id,
            start,
        } = event
        {
            if let Err(err) = backfill.try_send(BackfillRequest {
                peer,
                request_id,
                start,
            }) {
                metrics::counter!("zone_leadership_backfill_requests_dropped_total").increment(1);
                warn!(target: "zone::role", %err, start, "Dropped a block backfill request because the serving queue is unavailable");
            }
            continue;
        }
        let Some(sink) = sinks.sink_for(&event) else {
            metrics::counter!("zone_leadership_events_dropped_between_generations_total")
                .increment(1);
            debug!(target: "zone::role", "Dropping P2P event with no active generation consumer");
            continue;
        };
        match sink.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Live blocks and backfill responses are recovered by the follower's periodic
                // backfill probe; transaction forwarding reconciles from the pool. Never let a
                // slow generation block the process-lifetime backfill request path.
                metrics::counter!("zone_leadership_events_dropped_backpressure_total").increment(1);
                debug!(
                    target: "zone::role",
                    "Dropping generation event because its bounded sink is full"
                );
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // The generation stopped after we snapshotted its sink; the next generation
                // installs fresh channels and re-derives its state, so late generation-specific
                // events must not leak into the successor.
                metrics::counter!("zone_leadership_events_dropped_between_generations_total")
                    .increment(1);
            }
        }
    }
    error!(target: "zone::role", "P2P event channel closed");
}

/// The role a generation runs, derived from the next-anchor rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesiredRole {
    Leader {
        epoch: u64,
        /// The next anchor, read while deriving this role decision.
        next_anchor: u64,
    },
    Follower {
        epoch: u64,
    },
    /// No retained leadership record governs the next anchor, or this node cannot lead.
    Fenced,
}

impl DesiredRole {
    const fn kind(self) -> GenerationKind {
        match self {
            Self::Leader { .. } => GenerationKind::Leader,
            Self::Follower { .. } => GenerationKind::Follower,
            Self::Fenced => GenerationKind::Fenced,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GenerationKind {
    Leader,
    Follower,
    Fenced,
}

/// Outcome of one generation task, tagged for supervision decisions.
#[derive(Debug)]
enum TaskEnd {
    Engine(EngineExit),
    SequencerStopped,
    Ended(&'static str),
}

/// Supervise the two long-running sequencer children as one role-generation task.
///
/// An unexpected child exit must restart the whole generation immediately. During an intentional
/// generation stop, however, both children retain the graceful shutdown window needed to finish
/// in-flight L1 transactions.
async fn supervise_sequencer_tasks(
    handle: ZoneSequencerHandle,
    stop: CancellationToken,
) -> TaskEnd {
    let mut withdrawal = AbortOnDropHandle::new(handle.withdrawal_handle);
    let mut monitor = AbortOnDropHandle::new(handle.monitor_handle);

    tokio::select! {
        biased;
        () = stop.cancelled() => {
            // Both children observe the same token at their poll boundaries. Keep both handles
            // alive until they finish; the outer generation timeout will abort this supervisor
            // and AbortOnDropHandle will then abort either child that is still stuck.
            let (withdrawal_result, monitor_result) =
                tokio::join!(&mut withdrawal, &mut monitor);
            if let Err(err) = withdrawal_result {
                warn!(target: "zone::role", %err, "Withdrawal processor task failed during shutdown");
            }
            if let Err(err) = monitor_result {
                warn!(target: "zone::role", %err, "Zone monitor task failed during shutdown");
            }
            TaskEnd::SequencerStopped
        }
        result = &mut withdrawal => {
            match result {
                Ok(()) => warn!(target: "zone::role", "Withdrawal processor task stopped unexpectedly"),
                Err(err) => warn!(target: "zone::role", %err, "Withdrawal processor task failed"),
            }
            // Returning drops and aborts the monitor handle. The role controller observes
            // TaskEnd::Ended and restarts the complete generation.
            TaskEnd::Ended("withdrawal-processor")
        }
        result = &mut monitor => {
            match result {
                Ok(()) => warn!(target: "zone::role", "Zone monitor task stopped unexpectedly"),
                Err(err) => warn!(target: "zone::role", %err, "Zone monitor task failed"),
            }
            // Returning drops and aborts the withdrawal handle before the generation restarts.
            TaskEnd::Ended("zone-monitor")
        }
    }
}

struct RunningGeneration {
    id: u64,
    kind: GenerationKind,
    token: CancellationToken,
    tasks: JoinSet<TaskEnd>,
}

impl RunningGeneration {
    async fn stop(mut self, sinks: &EventSinks) {
        sinks.clear();
        self.token.cancel();
        let deadline = tokio::time::Instant::now() + GENERATION_STOP_TIMEOUT;
        loop {
            match tokio::time::timeout_at(deadline, self.tasks.join_next()).await {
                Ok(Some(result)) => {
                    if let Err(err) = result
                        && !err.is_cancelled()
                    {
                        warn!(target: "zone::role", generation = self.id, %err, "Generation task panicked during stop");
                    }
                }
                Ok(None) => break,
                Err(_) => {
                    warn!(
                        target: "zone::role",
                        generation = self.id,
                        "Generation did not stop within the timeout; aborting remaining tasks"
                    );
                    self.tasks.abort_all();
                    while self.tasks.join_next().await.is_some() {}
                    break;
                }
            }
        }
        info!(target: "zone::role", generation = self.id, kind = ?self.kind, "Role generation stopped");
    }
}

/// Derive the desired role for the next anchor from local canonical state and the schedule.
fn desired_role<P>(
    provider: &P,
    schedule: &LeadershipSchedule,
    local: &P2pPeerId,
    can_lead: bool,
) -> eyre::Result<DesiredRole>
where
    P: StateProviderFactory,
{
    let checkpoint = provider
        .latest()
        .map_err(eyre::Report::from)
        .and_then(|state| state.tempo_block_number().map_err(eyre::Report::from))?;
    let next_anchor = checkpoint.saturating_add(1);
    Ok(match schedule.leader_for(next_anchor) {
        None => DesiredRole::Fenced,
        Some(record) if &record.leader == local => {
            if can_lead {
                DesiredRole::Leader {
                    epoch: record.epoch,
                    next_anchor,
                }
            } else {
                error!(
                    target: "zone::role",
                    epoch = record.epoch,
                    next_anchor,
                    "This node is the scheduled leader but has no sequencer resources; fencing"
                );
                DesiredRole::Fenced
            }
        }
        Some(record) => DesiredRole::Follower {
            epoch: record.epoch,
        },
    })
}

/// Promotion-readiness verdict from hash-carrying peer tip evidence.
#[derive(Debug)]
enum Readiness {
    Ready,
    NotReady(Vec<String>),
    /// A peer holds a different block at a height we consider canonical: fence.
    Conflicted(String),
}

/// Evaluate the promotion barrier
///
/// Quorum depends on the mode: a **planned handoff** (the outgoing leader is alive and authoritative)
/// requires the outgoing leader's fresh evidence to match our head;
/// **same-identity recovery** (we already governed the previous anchor, or nothing did) requires fresh
/// evidence from every other quorum peer, because an unreachable follower may hold replicated blocks
/// the others lack.
fn promotion_readiness<P>(
    provider: &P,
    schedule: &LeadershipSchedule,
    local: &P2pPeerId,
    quorum_peers: &[P2pPeerId],
    registry: &PeerTipRegistry,
    next_anchor: u64,
) -> Readiness
where
    P: BlockNumReader + HeaderProvider<Header = TempoHeader>,
{
    let local_head = match provider.best_block_number() {
        Ok(head) => head,
        Err(err) => return Readiness::NotReady(vec![format!("cannot read the local head: {err}")]),
    };

    let now = tokio::time::Instant::now().into_std();
    let mut fresh = std::collections::HashMap::new();
    for (peer, tip, at) in registry.snapshot() {
        if now.duration_since(at) <= TIP_EVIDENCE_TTL {
            fresh.insert(peer, tip);
        }
    }

    let mut reasons = Vec::new();
    for (peer, tip) in &fresh {
        if tip.zone_height > local_head {
            reasons.push(format!(
                "peer {peer} advertises tip {} above the local head {local_head}",
                tip.zone_height
            ));
            continue;
        }
        match provider.sealed_header(tip.zone_height) {
            Ok(Some(header)) if header.hash() == tip.zone_hash => {}
            Ok(Some(header)) => {
                return Readiness::Conflicted(format!(
                    "peer {peer} holds a conflicting block at height {}: local {}, peer {}",
                    tip.zone_height,
                    header.hash(),
                    tip.zone_hash,
                ));
            }
            Ok(None) => reasons.push(format!(
                "local canonical header {} is missing while validating peer {peer} evidence",
                tip.zone_height
            )),
            Err(err) => reasons.push(format!(
                "cannot read local header {}: {err}",
                tip.zone_height
            )),
        }
    }

    // A planned handoff has a distinct outgoing leader governing the previous anchor.
    let outgoing = schedule
        .leader_for(next_anchor.saturating_sub(1))
        .filter(|record| &record.leader != local)
        .map(|record| record.leader);
    match outgoing {
        Some(outgoing) => {
            if !fresh.contains_key(&outgoing) {
                reasons.push(format!(
                    "no fresh tip evidence from the outgoing leader {outgoing}"
                ));
            }
        }
        None => {
            for peer in quorum_peers {
                if peer != local && !fresh.contains_key(peer) {
                    reasons.push(format!(
                        "no fresh tip evidence from peer {peer} (required for same-identity \
                         recovery; an unreachable follower may hold replicated blocks)"
                    ));
                }
            }
        }
    }

    if reasons.is_empty() {
        Readiness::Ready
    } else {
        Readiness::NotReady(reasons)
    }
}

/// Run the role controller until the process shuts down.
///
/// `sinks` must be the same [`EventSinks`] handed to [`route_events_to_generations`] — the
/// router is long-lived while generations come and go.
///
/// Subscribes to schedule and peer-tip notifications plus generation task completion,
/// re-derives the desired role by the next-anchor rule, and switches role generations.
/// Observation alone never switches roles — the trigger is consumption progress reaching
/// the boundary, which this loop tracks by re-reading the local Tempo checkpoint.
pub(crate) async fn run_role_controller<P, Pool>(
    context: RoleControllerContext<P, Pool>,
    sinks: EventSinks,
) where
    P: BlockNumReader
        + BlockReader<Block = Block>
        + HeaderProvider<Header = TempoHeader>
        + StateProviderFactory
        + ReceiptProvider
        + PersistedBlockSubscriptions
        + ZoneSequencerProvider
        + Clone
        + Send
        + Sync
        + 'static,
    Pool: TransactionPool<Transaction = TempoPooledTransaction> + Clone + 'static,
{
    let mut schedule_changes = context.schedule.subscribe();
    let mut peer_tip_changes = context.peer_tips.subscribe();

    let mut generation_id: u64 = 0;
    let mut current: Option<RunningGeneration> = None;
    // Standbys are excluded: one is never a catch-up source for a quorum node, so it never
    // returns the backfill completion that carries tip evidence, and requiring evidence from one
    // would block same-identity recovery forever.
    let manifest_peers: Vec<P2pPeerId> = context.attestation.addresses.keys().cloned().collect();
    let quorum_peers = context.schedule.quorum_peers(&manifest_peers);

    loop {
        // An rpc-only member is not registered with `ZonePortal`, so it can never be named
        // leader by a finalized transition. Fencing it explicitly means a corrupt or wrongly
        // provisioned record cannot start a producer whose blocks nobody could settle.
        let can_lead = context.sequencer.is_some()
            && context
                .schedule
                .is_quorum_member(&context.local_ed25519_public_key);
        let mut retry_decision = false;
        let mut desired = match desired_role(
            &context.provider,
            &context.schedule,
            &context.local_ed25519_public_key,
            can_lead,
        ) {
            Ok(desired) => desired,
            Err(err) => {
                error!(target: "zone::role", %err, "Failed reading the local Tempo checkpoint; fencing");
                retry_decision = true;
                DesiredRole::Fenced
            }
        };

        // Promotion barrier: switching the head writer to this node requires hash-carrying
        // tip evidence. Until the barrier is satisfied the node keeps importing as a
        // follower and actively probes peers for evidence.
        let mut ready_for_promotion = true;
        let mut promotion_reasons = Vec::new();
        if let DesiredRole::Leader { epoch, next_anchor } = desired
            && current.as_ref().map(|generation| generation.kind) != Some(GenerationKind::Leader)
        {
            match promotion_readiness(
                &context.provider,
                &context.schedule,
                &context.local_ed25519_public_key,
                &quorum_peers,
                &context.peer_tips,
                next_anchor,
            ) {
                Readiness::Ready => {
                    info!(target: "zone::role", epoch, next_anchor, "Promotion barrier satisfied");
                }
                Readiness::NotReady(reasons) => {
                    retry_decision = true;
                    ready_for_promotion = false;
                    debug!(target: "zone::role", epoch, next_anchor, ?reasons, "Promotion pending readiness");
                    promotion_reasons = reasons;
                    // Probe peers so evidence (and any missing blocks) arrive promptly.
                    if let Ok(best) = context.provider.best_block_number() {
                        let _ = context.commands.try_send(P2pCommand::RequestBackfill {
                            start: best.saturating_add(1),
                        });
                    }
                    desired = DesiredRole::Follower { epoch };
                }
                Readiness::Conflicted(detail) => {
                    ready_for_promotion = false;
                    error!(
                        target: "zone::role",
                        epoch,
                        %detail,
                        "Conflicting peer tip evidence; fencing instead of promoting"
                    );
                    promotion_reasons = vec![detail];
                    desired = DesiredRole::Fenced;
                }
            }
        }
        let switch = current.as_ref().map(|generation| generation.kind) != Some(desired.kind());
        if switch {
            if let Some(generation) = current.take() {
                generation.stop(&sinks).await;
            }
            generation_id += 1;
            match start_generation(&context, &sinks, desired, generation_id).await {
                Ok(generation) => {
                    current = Some(generation);
                }
                Err(err) => {
                    // Starting a role generation is atomic: none of its task graph remains
                    // active after a startup failure. Leave the node fenced and retry the
                    // complete desired generation after the decision backoff.
                    sinks.clear();
                    retry_decision = true;
                    ready_for_promotion = false;
                    promotion_reasons = vec![format!(
                        "failed to start {:?} generation: {err:#}",
                        desired.kind()
                    )];
                    error!(
                        target: "zone::role",
                        generation = generation_id,
                        kind = ?desired.kind(),
                        %err,
                        "Role generation failed to start; fencing before retry"
                    );
                }
            }

            let active_kind = current
                .as_ref()
                .map(|generation| generation.kind)
                .unwrap_or(GenerationKind::Fenced);
            metrics::counter!(
                "zone_leadership_transitions_total",
                "to" => match active_kind {
                    GenerationKind::Leader => "leader",
                    GenerationKind::Follower => "follower",
                    GenerationKind::Fenced => "fenced",
                },
            )
            .increment(1);
            metrics::gauge!("zone_leadership_role").set(match active_kind {
                GenerationKind::Leader => 2.0,
                GenerationKind::Follower => 1.0,
                GenerationKind::Fenced => 0.0,
            });
            if active_kind == desired.kind()
                && let Some(epoch) = match desired {
                    DesiredRole::Leader { epoch, .. } | DesiredRole::Follower { epoch } => {
                        Some(epoch)
                    }
                    DesiredRole::Fenced => None,
                }
            {
                metrics::gauge!("zone_leadership_epoch").set(epoch as f64);
            }
        }

        let active_kind = current
            .as_ref()
            .map(|generation| generation.kind)
            .unwrap_or(GenerationKind::Fenced);
        {
            let mut status = context.status.lock().expect("poisoned");
            status.role = match active_kind {
                GenerationKind::Leader => "leader",
                GenerationKind::Follower => "follower",
                GenerationKind::Fenced => "fenced",
            };
            status.epoch = if active_kind == desired.kind() {
                match desired {
                    DesiredRole::Leader { epoch, .. } | DesiredRole::Follower { epoch } => {
                        Some(epoch)
                    }
                    DesiredRole::Fenced => None,
                }
            } else {
                None
            };
            status.generation = generation_id;
            status.ready_for_promotion = ready_for_promotion;
            status.promotion_reasons = promotion_reasons;
        }
        metrics::gauge!("zone_leadership_ready_for_promotion").set(if ready_for_promotion {
            1.0
        } else {
            0.0
        });

        // A fenced generation has no tasks; an empty JoinSet's join_next() is immediately
        // ready with None, so polling it would spin this loop hot. Stay pending instead and
        // wake only on an explicit change or a retry for a failed/pending decision.
        let task_end = async {
            match current.as_mut() {
                Some(generation) if !generation.tasks.is_empty() => {
                    generation.tasks.join_next().await
                }
                _ => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            changed = schedule_changes.changed() => {
                if changed.is_err() {
                    error!(target: "zone::role", "Leadership schedule notifier closed");
                    return;
                }
            }
            changed = peer_tip_changes.changed() => {
                if changed.is_err() {
                    error!(target: "zone::role", "Peer tip notifier closed");
                    return;
                }
            }
            result = task_end => {
                match result {
                    Some(Ok(TaskEnd::Engine(EngineExit::Demoted { tempo_anchor, epoch }))) => {
                        info!(
                            target: "zone::role",
                            tempo_anchor,
                            epoch,
                            "Engine halted at the activation boundary; demoting"
                        );
                        // The next loop iteration derives Follower from the same schedule.
                    }
                    Some(Ok(TaskEnd::Engine(EngineExit::Fenced { tempo_anchor }))) => {
                        error!(
                            target: "zone::role",
                            tempo_anchor,
                            "Engine fenced on an ungoverned anchor"
                        );
                        if let Some(generation) = current.take() {
                            generation.stop(&sinks).await;
                        }
                        tokio::time::sleep(GENERATION_RESTART_BACKOFF).await;
                    }
                    Some(Ok(TaskEnd::Engine(EngineExit::Cancelled)))
                    | Some(Ok(TaskEnd::SequencerStopped)) => {}
                    Some(Ok(TaskEnd::Ended(name))) => {
                        // A generation task ended while its generation is still desired:
                        // restart the whole generation to keep the task graph coherent.
                        error!(target: "zone::role", task = name, "Generation task ended unexpectedly; restarting generation");
                        if let Some(generation) = current.take() {
                            generation.stop(&sinks).await;
                        }
                        tokio::time::sleep(GENERATION_RESTART_BACKOFF).await;
                    }
                    Some(Err(err)) => {
                        error!(target: "zone::role", %err, "Generation task panicked; restarting generation");
                        if let Some(generation) = current.take() {
                            generation.stop(&sinks).await;
                        }
                        tokio::time::sleep(GENERATION_RESTART_BACKOFF).await;
                    }
                    // Unreachable: join_next() is only polled while the set is non-empty.
                    None => {}
                }
            }
            _ = async {
                if retry_decision {
                    tokio::time::sleep(ROLE_DECISION_RETRY_BACKOFF).await;
                } else {
                    std::future::pending().await
                }
            } => {}
        }
    }
}

async fn start_generation<P, Pool>(
    context: &RoleControllerContext<P, Pool>,
    sinks: &EventSinks,
    desired: DesiredRole,
    id: u64,
) -> eyre::Result<RunningGeneration>
where
    P: BlockNumReader
        + BlockReader<Block = Block>
        + HeaderProvider<Header = TempoHeader>
        + StateProviderFactory
        + ReceiptProvider
        + PersistedBlockSubscriptions
        + ZoneSequencerProvider
        + Clone
        + Send
        + Sync
        + 'static,
    Pool: TransactionPool<Transaction = TempoPooledTransaction> + Clone + 'static,
{
    let token = CancellationToken::new();
    let mut tasks = JoinSet::new();

    match desired {
        DesiredRole::Fenced => {
            sinks.clear();
            warn!(
                target: "zone::role",
                generation = id,
                "Leadership is uninitialized or this node cannot lead; all role tasks fenced"
            );
        }
        DesiredRole::Follower { epoch } => {
            let (sync_tx, sync_rx) = mpsc::channel(GENERATION_EVENT_BACKLOG);
            sinks.install(sync_tx, None);

            let follower_token = token.clone();
            let provider = context.provider.clone();
            let engine = context.engine_handle.clone();
            let commands = context.commands.clone();
            let tracker = context.l1_block_tracker.clone();
            let queue = context.deposit_queue.clone();
            let attestation = context.attestation.clone();
            let schedule = context.schedule.clone();
            let peer_tips = context.peer_tips.clone();
            tasks.spawn(async move {
                run_follower_block_sync(
                    provider,
                    engine,
                    sync_rx,
                    commands,
                    tracker,
                    queue,
                    attestation,
                    schedule,
                    peer_tips,
                    follower_token.clone(),
                )
                .await;
                if follower_token.is_cancelled() {
                    TaskEnd::Ended("follower-block-sync (cancelled)")
                } else {
                    TaskEnd::Ended("follower-block-sync")
                }
            });

            // Restarting this task performs a full txpool reconciliation on its first tick,
            // so pending transactions converge on the replacement leader.
            let pool = context.pool.clone();
            let listener = pool.new_transactions_listener();
            let commands = context.commands.clone();
            let forward_token = token.clone();
            tasks.spawn(async move {
                tokio::select! {
                    () = forward_token.cancelled() => TaskEnd::Ended("transaction-forwarding (cancelled)"),
                    () = forward_new_transactions(pool, listener, commands) => {
                        TaskEnd::Ended("transaction-forwarding")
                    }
                }
            });
            info!(target: "zone::role", generation = id, epoch, "Follower generation started");
        }
        DesiredRole::Leader { epoch, .. } => {
            let sequencer = context
                .sequencer
                .as_ref()
                .expect("leader generation requires sequencer resources");

            // Acquire every fallible prerequisite before installing sinks or spawning any
            // leader task. Otherwise a transient head-read failure could leave a partial
            // generation classified as Leader without its canonical head writer.
            let last_header = latest_sealed_header(&context.provider)?;

            let (sync_tx, sync_rx) = mpsc::channel(GENERATION_EVENT_BACKLOG);
            let (transactions_tx, transactions_rx) = mpsc::channel(GENERATION_EVENT_BACKLOG);
            sinks.install(sync_tx, Some(transactions_tx));

            // Canonical head writer: the engine with the per-anchor production permit.
            let engine = build_engine(context, sequencer, last_header);
            let engine_token = token.clone();
            tasks.spawn(async move { TaskEnd::Engine(engine.run_until(engine_token).await) });

            let broadcast_token = token.clone();
            let provider = context.provider.clone();
            let commands = context.commands.clone();
            tasks.spawn(async move {
                broadcast_persisted_blocks(provider, commands, broadcast_token).await;
                TaskEnd::Ended("block-broadcast")
            });

            let server_token = token.clone();
            let provider = context.provider.clone();
            let attestation = context.attestation.clone();
            tasks.spawn(async move {
                collect_follower_settlement_signatures(
                    provider,
                    sync_rx,
                    attestation,
                    server_token,
                )
                .await;
                TaskEnd::Ended("leader-settlement-signatures")
            });

            let pool = context.pool.clone();
            let import_token = token.clone();
            tasks.spawn(async move {
                tokio::select! {
                    () = import_token.cancelled() => TaskEnd::Ended("transaction-import (cancelled)"),
                    () = insert_forwarded_transactions(pool, transactions_rx) => {
                        TaskEnd::Ended("transaction-import")
                    }
                }
            });

            let provider = context.provider.clone();
            let commands = context.commands.clone();
            let attestation = context.attestation.clone();
            let settlement_token = token.clone();
            tasks.spawn(async move {
                tokio::select! {
                    () = settlement_token.cancelled() => TaskEnd::Ended("settlement-collection (cancelled)"),
                    () = collect_leader_settlements(provider, commands, attestation) => {
                        TaskEnd::Ended("settlement-collection")
                    }
                }
            });

            // Sequencer background tasks (batch submission + withdrawal processing) stop
            // gracefully: they observe the token at their poll boundaries, letting in-flight
            // L1 transactions resolve before teardown.
            let sequencer_config = sequencer.sequencer_config.clone();
            let signer = sequencer
                .config
                .l1_transaction_signer
                .clone()
                .unwrap_or_else(|| sequencer.config.sequencer_signer.clone());
            let zone_provider = context.provider.clone();
            let sequencer_token = token.clone();
            tasks.spawn(async move {
                let handle = spawn_zone_sequencer(
                    sequencer_config,
                    signer,
                    zone_provider,
                    sequencer_token.clone(),
                )
                .await;
                supervise_sequencer_tasks(handle, sequencer_token).await
            });

            info!(target: "zone::role", generation = id, epoch, "Leader generation started");
        }
    }

    Ok(RunningGeneration {
        id,
        kind: desired.kind(),
        token,
        tasks,
    })
}

fn latest_sealed_header<P>(provider: &P) -> eyre::Result<SealedHeader<TempoHeader>>
where
    P: BlockNumReader + HeaderProvider<Header = TempoHeader>,
{
    let number = provider.best_block_number().map_err(eyre::Report::from)?;
    provider
        .sealed_header(number)
        .map_err(eyre::Report::from)?
        .ok_or_else(|| eyre::eyre!("no latest block header"))
}

fn build_engine<P, Pool>(
    context: &RoleControllerContext<P, Pool>,
    sequencer: &LeaderSequencerDeps,
    last_header: SealedHeader<TempoHeader>,
) -> ZoneEngine
where
    P: Clone,
{
    let sequencer_key = k256::SecretKey::from(sequencer.config.sequencer_signer.credential());
    ZoneEngine::new(
        context.chain_spec.clone(),
        context.engine_handle.clone(),
        context.payload_builder.clone(),
        context.deposit_queue.clone(),
        context.l1_block_tracker.clone(),
        last_header,
        sequencer.config.sequencer_signer.address(),
        sequencer_key,
        context.portal_address,
    )
    .with_production_permit(ProductionPermit::new(
        context.schedule.clone(),
        context.local_ed25519_public_key.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use std::{future::pending, net::SocketAddr, time::Duration};

    use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
    use reth_provider::test_utils::MockEthProvider;
    use tempo_primitives::TempoPrimitives;
    use tokio::sync::{mpsc, oneshot};
    use tokio_util::sync::CancellationToken;
    use zone_p2p::P2pEvent;
    use zone_sequencer::ZoneSequencerHandle;

    use super::{
        EventSinks, TaskEnd, latest_sealed_header, route_events_to_generations,
        supervise_sequencer_tasks,
    };

    struct DropSignal(Option<oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(signal) = self.0.take() {
                let _ = signal.send(());
            }
        }
    }

    #[test]
    fn unavailable_canonical_head_fails_leader_prerequisite() {
        let provider = MockEthProvider::<TempoPrimitives>::new();

        assert!(
            latest_sealed_header(&provider).is_err(),
            "leader startup must fail when its canonical head cannot be read"
        );
    }

    #[tokio::test]
    async fn sequencer_supervisor_reports_panicking_child_without_waiting_for_sibling() {
        let (monitor_started_tx, monitor_started_rx) = oneshot::channel();
        let (monitor_dropped_tx, monitor_dropped_rx) = oneshot::channel();
        let monitor_handle = tokio::spawn(async move {
            let _drop_signal = DropSignal(Some(monitor_dropped_tx));
            let _ = monitor_started_tx.send(());
            pending::<()>().await;
        });
        monitor_started_rx
            .await
            .expect("monitor task must start before the panic");

        let withdrawal_handle = tokio::spawn(async move {
            panic!("simulated withdrawal processor panic");
        });
        let handle = ZoneSequencerHandle {
            withdrawal_handle,
            monitor_handle,
        };

        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            supervise_sequencer_tasks(handle, CancellationToken::new()),
        )
        .await
        .expect("supervisor waited for the healthy long-running sibling");
        assert!(matches!(outcome, TaskEnd::Ended("withdrawal-processor")));
        tokio::time::timeout(Duration::from_secs(1), monitor_dropped_rx)
            .await
            .expect("sibling was not aborted when the supervisor returned")
            .expect("monitor drop signal was lost");
    }

    #[tokio::test]
    async fn sequencer_supervisor_waits_for_both_children_during_graceful_stop() {
        let stop = CancellationToken::new();
        let (ready_tx, mut ready_rx) = mpsc::channel(2);

        let withdrawal_stop = stop.clone();
        let withdrawal_ready = ready_tx.clone();
        let withdrawal_handle = tokio::spawn(async move {
            withdrawal_ready.send(()).await.unwrap();
            withdrawal_stop.cancelled().await;
        });

        let monitor_stop = stop.clone();
        let (monitor_stopping_tx, monitor_stopping_rx) = oneshot::channel();
        let (release_monitor_tx, release_monitor_rx) = oneshot::channel();
        let monitor_handle = tokio::spawn(async move {
            ready_tx.send(()).await.unwrap();
            monitor_stop.cancelled().await;
            let _ = monitor_stopping_tx.send(());
            let _ = release_monitor_rx.await;
        });

        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(1), ready_rx.recv())
                .await
                .expect("sequencer child did not start")
                .expect("sequencer child readiness channel closed");
        }

        let supervisor = tokio::spawn(supervise_sequencer_tasks(
            ZoneSequencerHandle {
                withdrawal_handle,
                monitor_handle,
            },
            stop.clone(),
        ));
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(1), monitor_stopping_rx)
            .await
            .expect("monitor did not observe graceful shutdown")
            .expect("monitor stopping signal was lost");
        tokio::task::yield_now().await;
        assert!(
            !supervisor.is_finished(),
            "supervisor aborted the slower child during graceful shutdown"
        );

        let _ = release_monitor_tx.send(());
        let outcome = tokio::time::timeout(Duration::from_secs(1), supervisor)
            .await
            .expect("supervisor did not finish after both children stopped")
            .expect("supervisor task panicked");
        assert!(matches!(outcome, TaskEnd::SequencerStopped));
    }

    /// Reproduces the startup ordering from the native stress test: the P2P router is live
    /// and receives a peer's backfill request before the role controller installs its first
    /// generation sink. The request must reach the process-lifetime backfill server anyway —
    /// losing it leaves the requester blocked behind the P2P backfill response timeout.
    #[tokio::test]
    async fn backfill_request_bypasses_generation_sinks() {
        let sinks = EventSinks::default();
        let (network_events, event_rx) = mpsc::channel(1);
        let (backfill_tx, mut backfill_rx) = mpsc::channel(1);
        let router = tokio::spawn(route_events_to_generations(
            event_rx,
            sinks.clone(),
            backfill_tx,
        ));
        let peer = PrivateKey::from_seed(1).public_key();

        network_events
            .send(P2pEvent::BackfillRequested {
                peer: peer.clone(),
                request_id: 7,
                start: 42,
            })
            .await
            .unwrap();
        // A generation-specific event with no installed sink is dropped, not deferred.
        network_events
            .send(P2pEvent::Started {
                ed25519_public_key: peer.clone(),
                listen: SocketAddr::from(([127, 0, 0, 1], 9000)),
            })
            .await
            .unwrap();

        let request = tokio::time::timeout(Duration::from_secs(1), backfill_rx.recv())
            .await
            .expect("backfill request never reached the serving channel")
            .expect("backfill serving channel closed");
        assert_eq!(request.peer, peer);
        assert_eq!(request.request_id, 7);
        assert_eq!(request.start, 42);

        drop(network_events);
        tokio::time::timeout(Duration::from_secs(1), router)
            .await
            .expect("event router did not stop")
            .expect("event router task panicked");
    }

    #[tokio::test]
    async fn backfill_request_bypasses_saturated_generation_sink() {
        let sinks = EventSinks::default();
        let (generation_tx, mut generation_rx) = mpsc::channel(1);
        sinks.install(generation_tx, None);

        let (network_events, event_rx) = mpsc::channel(4);
        let (backfill_tx, mut backfill_rx) = mpsc::channel(1);
        let router = tokio::spawn(route_events_to_generations(event_rx, sinks, backfill_tx));
        let peer = PrivateKey::from_seed(1).public_key();

        // The first event fills the generation sink. The second must be dropped instead of
        // parking the router ahead of the process-lifetime backfill request.
        for port in [9000, 9001] {
            network_events
                .send(P2pEvent::Started {
                    ed25519_public_key: peer.clone(),
                    listen: SocketAddr::from(([127, 0, 0, 1], port)),
                })
                .await
                .unwrap();
        }
        network_events
            .send(P2pEvent::BackfillRequested {
                peer: peer.clone(),
                request_id: 8,
                start: 43,
            })
            .await
            .unwrap();

        let request = tokio::time::timeout(Duration::from_secs(1), backfill_rx.recv())
            .await
            .expect("generation backpressure blocked the backfill request")
            .expect("backfill serving channel closed");
        assert_eq!(request.peer, peer);
        assert_eq!(request.request_id, 8);
        assert_eq!(request.start, 43);

        drop(network_events);
        tokio::time::timeout(Duration::from_secs(1), router)
            .await
            .expect("event router did not stop")
            .expect("event router task panicked");

        let forwarded = generation_rx
            .recv()
            .await
            .expect("the first generation event should have been forwarded");
        assert!(matches!(
            forwarded,
            P2pEvent::Started { listen, .. } if listen.port() == 9000
        ));
        assert!(
            generation_rx.try_recv().is_err(),
            "the event that encountered backpressure should have been dropped"
        );
    }
}
