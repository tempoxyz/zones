//! Per-anchor sequencing decisions. Leadership changes update state in one persistent loop.

use std::{sync::Arc, time::Duration};

use alloy_consensus::BlockHeader as _;
use alloy_eips::NumHash;
use alloy_primitives::Address;
use eyre::WrapErr as _;
use reth_chain_state::PersistedBlockSubscriptions;
use reth_node_api::ConsensusEngineHandle;
use reth_payload_builder::PayloadBuilderHandle;
use reth_primitives_traits::SealedHeader;
use reth_provider::HeaderProvider;
use reth_storage_api::{BlockNumReader, BlockReader, ReceiptProvider, StateProviderFactory};
use reth_transaction_pool::TransactionPool;
use tempo_primitives::{Block, TempoHeader};
use tokio::{
    sync::{mpsc, watch},
    task::JoinSet,
};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};
use tracing::error;
use zone_chainspec::ZoneChainSpec;
use zone_l1::{DepositQueue, EncryptionKeyRing, L1BlockTracker, TempoStateExt as _};
use zone_p2p::{BackfillCommand, LeadershipSchedule, P2pCommand, P2pPeerId};
use zone_payload::ZonePayloadTypes;
use zone_sequencer::{
    ShadowProverConfig, ZoneSequencerConfig, ZoneSequencerProvider, resolve_portal_zone_anchor,
    spawn_zone_sequencer,
};
use zone_transaction_pool_alias::TempoPooledTransaction;

mod zone_transaction_pool_alias {
    pub(super) use tempo_transaction_pool::transaction::TempoPooledTransaction;
}

use crate::{
    ProductionPermit, ZoneEngine, ZoneSequencerAddOnsConfig,
    follower::{BlockSyncP2p, FollowerBlockSync, FollowerBlockSyncContext, PeerTipRegistry},
    p2p_engine::{P2pEventRoutes, ZoneP2pEngine},
    replication::collect_follower_settlement_signatures,
    settlement_attestation::{AttestationContext, collect_leader_settlements},
    tx_forwarding::{forward_new_transactions, insert_forwarded_transactions},
};

/// Resources shared by the persistent sequencer and its background services.
pub(crate) struct SequencerContext<P, Pool> {
    pub local_ed25519_public_key: P2pPeerId,
    pub schedule: LeadershipSchedule,
    pub provider: P,
    pub pool: Pool,
    pub engine_handle: ConsensusEngineHandle<ZonePayloadTypes>,
    pub payload_builder: PayloadBuilderHandle<ZonePayloadTypes>,
    pub chain_spec: Arc<ZoneChainSpec>,
    pub deposit_queue: DepositQueue,
    pub l1_block_tracker: L1BlockTracker,
    pub encryption_keys: EncryptionKeyRing,
    pub commands: mpsc::Sender<P2pCommand>,
    pub backfill_commands: mpsc::Sender<BackfillCommand>,
    pub attestation: AttestationContext,
    pub portal_address: Address,
    /// `None` means this node can never lead.
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
    /// Monotonic role transition counter (retains the existing RPC field name).
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
    pub prover_config: Option<ShadowProverConfig>,
}

/// Sequencing mode derived from the next canonical anchor.
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
    /// Stable name for status, metric labels, and logs.
    const fn name(self) -> &'static str {
        match self {
            Self::Leader { .. } => "leader",
            Self::Follower { .. } => "follower",
            Self::Fenced => "fenced",
        }
    }

    /// Value for the `zone_leadership_role` gauge.
    const fn gauge(self) -> f64 {
        match self {
            Self::Leader { .. } => 2.0,
            Self::Follower { .. } => 1.0,
            Self::Fenced => 0.0,
        }
    }

    /// Epoch of the governing record, when one governs.
    const fn epoch(self) -> Option<u64> {
        match self {
            Self::Leader { epoch, .. } | Self::Follower { epoch } => Some(epoch),
            Self::Fenced => None,
        }
    }
}

/// Derive the desired role for the next anchor from local canonical state and the schedule.
///
/// `DesiredRole::Leader` carries the next anchor it was derived from, so the promotion
/// barrier never re-reads the checkpoint under a second, possibly divergent, error policy.
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

/// Promotion-readiness verdict from the evidence required by the active transition mode.
#[derive(Debug)]
enum Readiness {
    Ready,
    Conflicted(String),
}

/// Evaluate the promotion barrier
///
/// Forced-recovery promotion requires the operator-selected block to remain in the local canonical
/// chain. The node may have advanced beyond that checkpoint before restarting, so requiring it to
/// remain the head would make every in-progress recovery restart fatal. Normal transitions need no
/// additional evidence: the next-anchor rule and one-to-one zone/L1 block mapping ensure all
/// earlier leaders' blocks are already local.
fn promotion_readiness<P>(
    provider: &P,
    schedule: &LeadershipSchedule,
    local: &P2pPeerId,
    next_anchor: u64,
) -> Readiness
where
    P: BlockNumReader + HeaderProvider<Header = TempoHeader>,
{
    if let Some(recovery) = schedule.forced_recovery().filter(|recovery| {
        &recovery.leader == local
            && next_anchor >= recovery.recovery_start_tempo_block
            && recovery
                .portal_activation_tempo_block
                .is_none_or(|activation| next_anchor < activation)
    }) {
        if let Err(err) = canonical_recovery_height(provider, recovery.recovery_block_hash) {
            return Readiness::Conflicted(format!(
                "forced recovery checkpoint is not canonical: {err}"
            ));
        }
        return Readiness::Ready;
    }

    Readiness::Ready
}

/// Resolve an operator-selected recovery hash to its canonical header.
///
/// The hash-to-number index can contain a known non-canonical block, so the header at the resolved
/// height is read through the canonical number index and compared again before the checkpoint is
/// trusted. This proves local ancestry only: restart safety additionally assumes that participating
/// nodes advanced on the same non-equivocating recovery-leader chain.
pub(crate) fn canonical_recovery_height<P>(
    provider: &P,
    recovery_block_hash: alloy_primitives::B256,
) -> eyre::Result<u64>
where
    P: BlockNumReader + HeaderProvider<Header = TempoHeader>,
{
    let recovery_height = provider
        .block_number(recovery_block_hash)?
        .ok_or_else(|| eyre::eyre!("recovery block {recovery_block_hash} is unknown"))?;
    let header = provider.sealed_header(recovery_height)?.ok_or_else(|| {
        eyre::eyre!("canonical header at recovery height {recovery_height} is missing")
    })?;
    eyre::ensure!(
        header.hash() == recovery_block_hash,
        "recovery block {recovery_block_hash} is not canonical at height {recovery_height}; \
         canonical hash is {}",
        header.hash(),
    );
    Ok(recovery_height)
}

/// Block production state owned by the same loop that imports peer blocks.
/// There is only one canonical head writer, including across leadership transitions.
pub(crate) struct Sequencing<P> {
    provider: P,
    schedule: LeadershipSchedule,
    local: P2pPeerId,
    engine: Option<ZoneEngine>,
    status: SharedRoleStatus,
    active: watch::Sender<bool>,
    finalized: mpsc::Sender<NumHash>,
    observed_through: Option<u64>,
}

impl<P> Sequencing<P>
where
    P: StateProviderFactory + BlockNumReader + HeaderProvider<Header = TempoHeader>,
{
    pub(crate) fn observed(&mut self, block: zone_l1::L1BlockDeposits) {
        self.observed_through = Some(block.header.number());
    }

    pub(crate) fn is_leader(&self) -> bool {
        *self.active.borrow()
    }

    /// Produce at most one block, then return to the channel loop before the next boundary.
    pub(crate) async fn advance(&mut self) -> eyre::Result<bool> {
        let can_lead = self.engine.is_some() && self.schedule.is_quorum_member(&self.local);
        let mut reasons = Vec::new();
        let mut desired = match desired_role(&self.provider, &self.schedule, &self.local, can_lead)
        {
            Ok(role) => role,
            Err(error) => {
                reasons.push(format!("cannot read the canonical L1 checkpoint: {error}"));
                DesiredRole::Fenced
            }
        };
        if let DesiredRole::Leader { next_anchor, .. } = desired
            && let Readiness::Conflicted(reason) =
                promotion_readiness(&self.provider, &self.schedule, &self.local, next_anchor)
        {
            reasons.push(reason);
            desired = DesiredRole::Fenced;
        }
        if !can_lead {
            reasons.push("rpc-only nodes cannot get promoted".to_owned());
        }
        let leader = matches!(desired, DesiredRole::Leader { .. });
        self.active.send_if_modified(|active| {
            if *active == leader {
                false
            } else {
                *active = leader;
                true
            }
        });
        {
            let mut status = self.status.lock().expect("poisoned");
            if status.role != desired.name() {
                status.generation += 1;
                metrics::counter!("zone_leadership_transitions_total", "to" => desired.name())
                    .increment(1);
            }
            status.role = desired.name();
            status.epoch = desired.epoch();
            status.ready_for_promotion = reasons.is_empty();
            status.promotion_reasons = reasons;
            metrics::gauge!("zone_leadership_ready_for_promotion")
                .set(if status.ready_for_promotion { 1.0 } else { 0.0 });
        }
        metrics::gauge!("zone_leadership_role").set(desired.gauge());
        if let Some(epoch) = desired.epoch() {
            metrics::gauge!("zone_leadership_epoch").set(epoch as f64);
        }
        let DesiredRole::Leader { next_anchor, .. } = desired else {
            return Ok(false);
        };
        if self
            .observed_through
            .is_none_or(|observed| observed < next_anchor)
        {
            return Ok(false);
        }
        let engine = self.engine.as_mut().expect("leader requires an engine");
        // Imports may have advanced the head since this node last produced a block.
        let parent = latest_sealed_header(&self.provider)?;
        let Some(block) = engine.produce_next(parent).await? else {
            return Ok(false);
        };
        self.finalized
            .send(block)
            .await
            .wrap_err("P2P finalized block channel closed")?;
        Ok(true)
    }
}

pub(crate) async fn run_sequencer<P, Pool>(
    context: SequencerContext<P, Pool>,
    p2p: ZoneP2pEngine,
    l1_blocks: mpsc::Receiver<zone_l1::L1BlockDeposits>,
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
    let stop = CancellationToken::new();
    let _cancel = stop.clone().drop_guard();
    let (active_tx, active_rx) = watch::channel(false);
    let (finalized_tx, finalized_rx) = mpsc::channel(128);
    let engine = context.sequencer.as_ref().map(|sequencer| {
        build_engine(
            &context,
            sequencer,
            latest_sealed_header(&context.provider)
                .expect("canonical head is required to start the sequencer"),
        )
    });
    let sequencing = Sequencing {
        provider: context.provider.clone(),
        schedule: context.schedule.clone(),
        local: context.local_ed25519_public_key.clone(),
        engine,
        status: context.status.clone(),
        active: active_tx,
        finalized: finalized_tx,
        // Programmatic dev nodes with a zero Portal inject already-applied L1 fixtures.
        observed_through: context.portal_address.is_zero().then_some(u64::MAX),
    };
    let (blocks_tx, blocks_rx) = mpsc::channel(128);
    let (transactions_tx, transactions_rx) = mpsc::channel(128);
    let (signatures_tx, signatures_rx) = mpsc::channel(128);
    let (backfill_tx, backfill_rx) = mpsc::channel(128);
    let sync = FollowerBlockSync::new(
        FollowerBlockSyncContext {
            provider: context.provider.clone(),
            engine: context.engine_handle.clone(),
            l1_block_tracker: context.l1_block_tracker.clone(),
            deposit_queue: context.deposit_queue.clone(),
            attestation: context.attestation.clone(),
            schedule: context.schedule.clone(),
            peer_tips: context.peer_tips.clone(),
        },
        BlockSyncP2p {
            events: blocks_rx,
            commands: context.commands.clone(),
            backfill_responses: backfill_rx,
            backfill_commands: context.backfill_commands.clone(),
        },
        stop.clone(),
    );
    let mut tasks = JoinSet::new();
    tasks.spawn(sync.run(sequencing, l1_blocks));
    tasks.spawn(p2p.run(
        context.provider.clone(),
        context.commands.clone(),
        finalized_rx,
        P2pEventRoutes {
            blocks: blocks_tx,
            transactions: transactions_tx,
            signatures: signatures_tx,
            backfill: backfill_tx,
        },
    ));
    tasks.spawn(collect_follower_settlement_signatures(
        context.provider.clone(),
        signatures_rx,
        context.attestation.clone(),
        stop.clone(),
    ));
    let pool = context.pool.clone();
    tasks.spawn(forward_new_transactions(
        pool.clone(),
        pool.new_transactions_listener(),
        context.commands.clone(),
    ));
    tasks.spawn(insert_forwarded_transactions(
        context.pool.clone(),
        transactions_rx,
    ));
    if context.sequencer.is_some() {
        tasks.spawn(run_leader_services(context, active_rx, stop));
    }
    let result = tasks.join_next().await;
    panic!("zone sequencer task stopped unexpectedly: {result:?}");
}

/// Keep settlement services alive across role changes, draining L1 transactions on demotion.
async fn run_leader_services<P, Pool>(
    context: SequencerContext<P, Pool>,
    mut active: watch::Receiver<bool>,
    stop: CancellationToken,
) where
    P: ZoneSequencerProvider + PersistedBlockSubscriptions,
    Pool: Send + 'static,
{
    let deps = context
        .sequencer
        .as_ref()
        .expect("sequencer resources required");
    loop {
        if active.wait_for(|active| *active).await.is_err() {
            return;
        }
        let token = stop.child_token();
        let portal_anchor = match resolve_portal_zone_anchor(
            &context.provider,
            context.portal_address,
            &context.attestation.l1_provider,
        )
        .await
        {
            Ok(anchor) => anchor,
            Err(error) => {
                error!(%error, "Cannot restore settlement anchor");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        context
            .attestation
            .store
            .remove_submitted(portal_anchor.block_number);
        let proposals = collect_leader_settlements(
            context.provider.clone(),
            context.commands.clone(),
            context.attestation.clone(),
            portal_anchor.block_number,
        );
        let signer = deps
            .config
            .l1_transaction_signer
            .clone()
            .unwrap_or_else(|| deps.config.sequencer_signer.clone());
        let handle = spawn_zone_sequencer(
            deps.sequencer_config.clone(),
            signer,
            context.provider.clone(),
            deps.prover_config.clone(),
            token.clone(),
        )
        .await;
        let mut withdrawal = AbortOnDropHandle::new(handle.withdrawal_handle);
        let mut monitor = AbortOnDropHandle::new(handle.monitor_handle);
        tokio::pin!(proposals);
        tokio::select! {
            _ = active.wait_for(|active| !*active) => {}
            () = stop.cancelled() => {}
            () = &mut proposals => panic!("settlement proposal collector stopped"),
            result = &mut withdrawal => panic!("withdrawal worker stopped: {result:?}"),
            result = &mut monitor => panic!("batch submission worker stopped: {result:?}"),
        }
        token.cancel();
        // Never drop an in-flight L1 receipt wait at a leadership boundary.
        let (withdrawal, monitor) = tokio::join!(withdrawal, monitor);
        withdrawal.expect("withdrawal worker failed during demotion");
        monitor.expect("batch worker failed during demotion");
        if stop.is_cancelled() {
            return;
        }
    }
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
    context: &SequencerContext<P, Pool>,
    sequencer: &LeaderSequencerDeps,
    last_header: SealedHeader<TempoHeader>,
) -> ZoneEngine
where
    P: Clone,
{
    ZoneEngine::new(
        context.chain_spec.clone(),
        context.engine_handle.clone(),
        context.payload_builder.clone(),
        context.deposit_queue.clone(),
        context.l1_block_tracker.clone(),
        last_header,
        sequencer.config.sequencer_signer.address(),
        context.encryption_keys.clone(),
        context.portal_address,
    )
    .with_production_permit(ProductionPermit::new(
        context.schedule.clone(),
        context.local_ed25519_public_key.clone(),
    ))
}

#[cfg(test)]
mod tests {

    use reth_primitives_traits::SealedHeader;
    use reth_provider::test_utils::MockEthProvider;
    use tempo_primitives::{TempoHeader, TempoPrimitives};

    use super::{canonical_recovery_height, latest_sealed_header};

    #[test]
    fn unavailable_canonical_head_fails_leader_prerequisite() {
        let provider = MockEthProvider::<TempoPrimitives>::new();

        assert!(
            latest_sealed_header(&provider).is_err(),
            "leader startup must fail when its canonical head cannot be read"
        );
    }

    #[test]
    fn recovery_checkpoint_may_be_a_canonical_ancestor() {
        let provider = MockEthProvider::<TempoPrimitives>::new();
        let mut recovery_header = TempoHeader::default();
        recovery_header.inner.number = 7;
        let recovery_hash = SealedHeader::seal_slow(recovery_header.clone()).hash();
        provider.add_header(recovery_hash, recovery_header);

        let mut head = TempoHeader::default();
        head.inner.number = 9;
        let head_hash = SealedHeader::seal_slow(head.clone()).hash();
        provider.add_header(head_hash, head);

        assert_eq!(
            canonical_recovery_height(&provider, recovery_hash).unwrap(),
            7
        );
    }

    #[test]
    fn recovery_checkpoint_rejects_a_known_noncanonical_hash() {
        let provider = MockEthProvider::<TempoPrimitives>::new();
        let mut header = TempoHeader::default();
        header.inner.number = 7;
        let noncanonical_hash = alloy_primitives::B256::repeat_byte(0x42);
        provider.add_header(noncanonical_hash, header);

        let error = canonical_recovery_height(&provider, noncanonical_hash).unwrap_err();
        assert!(error.to_string().contains("is not canonical at height 7"));
    }
}
