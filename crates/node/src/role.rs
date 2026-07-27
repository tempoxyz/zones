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
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use zone_chainspec::ZoneChainSpec;
use zone_l1::{DepositQueue, L1BlockTracker, TempoStateExt as _};
use zone_p2p::{LeadershipSchedule, P2pCommand, P2pEvent, P2pPeerId};
use zone_payload::ZonePayloadTypes;
use zone_sequencer::{ZoneSequencerConfig, spawn_zone_sequencer};
use zone_transaction_pool_alias::TempoPooledTransaction;

mod zone_transaction_pool_alias {
    pub(super) use tempo_transaction_pool::transaction::TempoPooledTransaction;
}

use crate::{
    EngineExit, ProductionPermit, ZoneEngine, ZoneSequencerAddOnsConfig,
    replication::{
        AttestationContext, PeerTipRegistry, broadcast_persisted_blocks, run_follower_block_sync,
        run_leader_backfill_server,
    },
    settlement_attestation::collect_leader_settlements,
    tx_forwarding::{forward_new_transactions, insert_forwarded_transactions},
};

/// How often the controller re-derives the desired role when nothing else wakes it.
const ROLE_RECHECK_INTERVAL: Duration = Duration::from_millis(500);
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
        *self.inner.lock().expect("poisoned") = GenerationSinks {
            sync: Some(sync),
            transactions,
        };
    }

    fn clear(&self) {
        *self.inner.lock().expect("poisoned") = GenerationSinks::default();
    }
}

/// Long-lived P2P event demultiplexer.
///
/// Owns the single event receiver for the process lifetime and forwards each event to the
/// active generation's substream. Events arriving between generations are dropped — every
/// generation re-derives its state from the provider and the schedule on start, so a dropped
/// wakeup is recovered by the follower's backfill probes or the leader's queue heartbeat.
pub(crate) async fn route_events_to_generations(
    mut events: mpsc::Receiver<P2pEvent>,
    sinks: EventSinks,
) {
    while let Some(event) = events.recv().await {
        let sink = {
            let sinks = sinks.inner.lock().expect("poisoned");
            if matches!(event, P2pEvent::TransactionReceived { .. }) {
                sinks.transactions.clone()
            } else {
                sinks.sync.clone()
            }
        };
        let Some(sink) = sink else {
            metrics::counter!("zone_leadership_events_dropped_between_generations_total")
                .increment(1);
            debug!(target: "zone::role", "Dropping P2P event with no active generation consumer");
            continue;
        };
        if sink.send(event).await.is_err() {
            // The generation stopped after we snapshotted its sink; the next generation
            // installs fresh channels, so this event is dropped by design (late events must
            // never reach a newer generation).
            metrics::counter!("zone_leadership_events_dropped_between_generations_total")
                .increment(1);
        }
    }
    error!(target: "zone::role", "P2P event channel closed");
}

/// The role a generation runs, derived from the next-anchor rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesiredRole {
    Leader {
        epoch: u64,
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
) -> DesiredRole
where
    P: StateProviderFactory,
{
    let checkpoint = match provider
        .latest()
        .map_err(eyre::Report::from)
        .and_then(|state| state.tempo_block_number().map_err(eyre::Report::from))
    {
        Ok(checkpoint) => checkpoint,
        Err(err) => {
            error!(target: "zone::role", %err, "Failed reading the local Tempo checkpoint; fencing");
            return DesiredRole::Fenced;
        }
    };
    let next_anchor = checkpoint.saturating_add(1);
    match schedule.leader_for(next_anchor) {
        None => DesiredRole::Fenced,
        Some(record) if &record.leader == local => {
            if can_lead {
                DesiredRole::Leader {
                    epoch: record.epoch,
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
    }
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
/// evidence from every other manifest peer, because an unreachable follower may hold replicated blocks
/// the others lack.
fn promotion_readiness<P>(
    provider: &P,
    schedule: &LeadershipSchedule,
    local: &P2pPeerId,
    manifest_peers: &[P2pPeerId],
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
            for peer in manifest_peers {
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
/// Subscribes to the schedule notifier plus generation task completion, re-derives the
/// desired role by the next-anchor rule, and switches role generations. Observation alone
/// never switches roles — the trigger is consumption progress reaching the boundary, which
/// this loop tracks by re-reading the local Tempo checkpoint.
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
        + Clone
        + Send
        + Sync
        + 'static,
    Pool: TransactionPool<Transaction = TempoPooledTransaction> + Clone + 'static,
{
    let mut schedule_changes = context.schedule.subscribe();
    let mut recheck = tokio::time::interval(ROLE_RECHECK_INTERVAL);
    recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut generation_id: u64 = 0;
    let mut current: Option<RunningGeneration> = None;
    let manifest_peers: Vec<P2pPeerId> = context.attestation.addresses.keys().cloned().collect();

    loop {
        let can_lead = context.sequencer.is_some();
        let mut desired = desired_role(
            &context.provider,
            &context.schedule,
            &context.local_ed25519_public_key,
            can_lead,
        );

        // Promotion barrier: switching the head writer to this node requires hash-carrying
        // tip evidence. Until the barrier is satisfied the node keeps importing as a
        // follower and actively probes peers for evidence.
        let mut ready_for_promotion = true;
        let mut promotion_reasons = Vec::new();
        if let DesiredRole::Leader { epoch } = desired
            && current.as_ref().map(|generation| generation.kind) != Some(GenerationKind::Leader)
        {
            let next_anchor = match context
                .provider
                .latest()
                .map_err(eyre::Report::from)
                .and_then(|state| state.tempo_block_number().map_err(eyre::Report::from))
            {
                Ok(checkpoint) => checkpoint.saturating_add(1),
                Err(_) => 0,
            };
            match promotion_readiness(
                &context.provider,
                &context.schedule,
                &context.local_ed25519_public_key,
                &manifest_peers,
                &context.peer_tips,
                next_anchor,
            ) {
                Readiness::Ready => {
                    info!(target: "zone::role", epoch, next_anchor, "Promotion barrier satisfied");
                }
                Readiness::NotReady(reasons) => {
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
        {
            let mut status = context.status.lock().expect("poisoned");
            status.role = match desired.kind() {
                GenerationKind::Leader => "leader",
                GenerationKind::Follower => "follower",
                GenerationKind::Fenced => "fenced",
            };
            status.epoch = match desired {
                DesiredRole::Leader { epoch } | DesiredRole::Follower { epoch } => Some(epoch),
                DesiredRole::Fenced => None,
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

        let switch = current.as_ref().map(|generation| generation.kind) != Some(desired.kind());
        if switch {
            if let Some(generation) = current.take() {
                generation.stop(&sinks).await;
            }
            generation_id += 1;
            metrics::counter!(
                "zone_leadership_transitions_total",
                "to" => match desired.kind() {
                    GenerationKind::Leader => "leader",
                    GenerationKind::Follower => "follower",
                    GenerationKind::Fenced => "fenced",
                },
            )
            .increment(1);
            metrics::gauge!("zone_leadership_role").set(match desired.kind() {
                GenerationKind::Leader => 2.0,
                GenerationKind::Follower => 1.0,
                GenerationKind::Fenced => 0.0,
            });
            if let Some(epoch) = match desired {
                DesiredRole::Leader { epoch } | DesiredRole::Follower { epoch } => Some(epoch),
                DesiredRole::Fenced => None,
            } {
                metrics::gauge!("zone_leadership_epoch").set(epoch as f64);
            }
            current = Some(start_generation(&context, &sinks, desired, generation_id).await);
        }

        let task_end = async {
            match current.as_mut() {
                Some(generation) => generation.tasks.join_next().await,
                None => std::future::pending().await,
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
                    None => {}
                }
            }
            _ = recheck.tick() => {}
        }
    }
}

async fn start_generation<P, Pool>(
    context: &RoleControllerContext<P, Pool>,
    sinks: &EventSinks,
    desired: DesiredRole,
    id: u64,
) -> RunningGeneration
where
    P: BlockNumReader
        + BlockReader<Block = Block>
        + HeaderProvider<Header = TempoHeader>
        + StateProviderFactory
        + ReceiptProvider
        + PersistedBlockSubscriptions
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
        DesiredRole::Leader { epoch } => {
            let (sync_tx, sync_rx) = mpsc::channel(GENERATION_EVENT_BACKLOG);
            let (transactions_tx, transactions_rx) = mpsc::channel(GENERATION_EVENT_BACKLOG);
            sinks.install(sync_tx, Some(transactions_tx));

            let sequencer = context
                .sequencer
                .as_ref()
                .expect("leader generation requires sequencer resources");

            // Canonical head writer: the engine with the per-anchor production permit.
            match context
                .provider
                .best_block_number()
                .map_err(eyre::Report::from)
                .and_then(|number| {
                    context
                        .provider
                        .sealed_header(number)?
                        .ok_or_else(|| eyre::eyre!("no latest block header"))
                }) {
                Ok(last_header) => {
                    let engine = build_engine(context, sequencer, last_header);
                    let engine_token = token.clone();
                    tasks.spawn(
                        async move { TaskEnd::Engine(engine.run_until(engine_token).await) },
                    );
                }
                Err(err) => {
                    error!(target: "zone::role", %err, "Failed reading the local head; leader generation degenerates to fenced");
                }
            }

            let broadcast_token = token.clone();
            let provider = context.provider.clone();
            let commands = context.commands.clone();
            tasks.spawn(async move {
                broadcast_persisted_blocks(provider, commands, broadcast_token).await;
                TaskEnd::Ended("block-broadcast")
            });

            let server_token = token.clone();
            let provider = context.provider.clone();
            let commands = context.commands.clone();
            let attestation = context.attestation.clone();
            tasks.spawn(async move {
                run_leader_backfill_server(provider, sync_rx, commands, attestation, server_token)
                    .await;
                TaskEnd::Ended("leader-backfill-server")
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
            let sequencer_token = token.clone();
            tasks.spawn(async move {
                let handle =
                    spawn_zone_sequencer(sequencer_config, signer, sequencer_token.clone()).await;
                let (withdrawal, monitor) =
                    tokio::join!(handle.withdrawal_handle, handle.monitor_handle);
                if let Err(err) = withdrawal {
                    warn!(target: "zone::role", %err, "Withdrawal processor task failed");
                }
                if let Err(err) = monitor {
                    warn!(target: "zone::role", %err, "Zone monitor task failed");
                }
                if sequencer_token.is_cancelled() {
                    TaskEnd::SequencerStopped
                } else {
                    TaskEnd::Ended("sequencer-tasks")
                }
            });

            info!(target: "zone::role", generation = id, epoch, "Leader generation started");
        }
    }

    RunningGeneration {
        id,
        kind: desired.kind(),
        token,
        tasks,
    }
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
