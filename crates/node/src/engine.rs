//! Zone Engine — L1-event-driven block production for zone nodes.
//!
//! Advances the zone chain whenever new L1 blocks arrive in the deposit
//! queue, enabling full-speed sync during catch-up and instant reaction in
//! steady state.
//!
//! ## Block production flow
//!
//! ```text
//! L1Subscriber ──enqueue──► DepositQueue ──notify──► ZoneEngine
//!                                │                       │
//!                                │                   1. peek queue → L1 block
//!                                │                   2. build ZonePayloadAttributes
//!                                │                      (inner attrs + l1_block)
//!                                │                   3. FCU w/ payload attributes
//!                                │                       │
//!                                │                       ▼
//!                                │               reth payload service
//!                                │                       │
//!                                │               4. build payload
//!                                │                  (L1 data from attributes)
//!                                │                       │
//!                                │                       ▼
//!                                │                  ZoneEngine
//!                                │               5. resolve payload
//!                                │               6. newPayload
//!                                │               7. FCU (update head)
//!                                │                       │
//!                                ◄── confirm ◄───────────┘
//! ```
//!
//! The deposit queue uses a **peek / confirm** pattern: the engine peeks at
//! the next L1 block, wraps it into [`ZonePayloadAttributes`], and only
//! confirms (removes) the block after `newPayload` succeeds. A failed build
//! leaves the block in the queue for retry.
//!
//! The zone assumes **instant finality** — head, safe, and finalized all point
//! to the same block.

use alloy_consensus::BlockHeader as _;
use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::{ForkchoiceState, PayloadAttributes as EthPayloadAttributes};
use eyre::OptionExt;
use reth_chainspec::EthereumHardforks;
use reth_node_builder::ConsensusEngineHandle;
use reth_payload_builder::PayloadBuilderHandle;
use reth_payload_primitives::{BuiltPayload, PayloadKind};
use reth_primitives_traits::SealedHeader;
use std::{sync::Arc, time::Duration};
use tempo_primitives::TempoHeader;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use zone_chainspec::ZoneChainSpec;
use zone_l1::{DepositQueue, L1BlockDeposits, L1BlockTracker, PreparedL1Block};
use zone_p2p::{LeadershipSchedule, P2pPeerId};
use zone_payload::{ZonePayloadAttributes, ZonePayloadTypes};

/// Per-anchor production permit backed by the leadership schedule.
///
/// The permit is a single schedule lookup: produce anchor `N` only if
/// `schedule.leader_for(N)` is this node.
#[derive(Debug, Clone)]
pub struct ProductionPermit {
    schedule: LeadershipSchedule,
    local_ed25519_public_key: P2pPeerId,
}

impl ProductionPermit {
    /// Create a permit for this node based on the shared leadership schedule.
    pub fn new(schedule: LeadershipSchedule, local_ed25519_public_key: P2pPeerId) -> Self {
        Self {
            schedule,
            local_ed25519_public_key,
        }
    }

    /// Decide whether this node may produce the zone block embedding `tempo_anchor`.
    pub fn check(&self, tempo_anchor: u64) -> PermitDecision {
        match self.schedule.leader_for(tempo_anchor) {
            None => PermitDecision::Fenced,
            Some(record) if record.leader == self.local_ed25519_public_key => {
                PermitDecision::Produce
            }
            Some(record) => PermitDecision::Demoted {
                epoch: record.epoch,
            },
        }
    }

    /// Record that the zone block embedding `tempo_anchor` is locally canonical.
    pub fn record_applied_anchor(&self, tempo_anchor: u64) {
        self.schedule.record_applied_anchor(tempo_anchor);
    }
}

/// Outcome of a per-anchor production permit check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermitDecision {
    /// This node is the scheduled leader for the anchor.
    Produce,
    /// A retained transition assigns the anchor to another node.
    Demoted { epoch: u64 },
    /// Something bad happened. No leadership record is available.
    Fenced,
}

/// Why the engine loop returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineExit {
    /// The cancellation token fired at a block boundary.
    Cancelled,
    /// The leadership permit assigns the next anchor to another node.
    Demoted { tempo_anchor: u64, epoch: u64 },
    /// No leadership record governs the anchor; production is fenced.
    Fenced { tempo_anchor: u64 },
}

/// A queue-backed block consumer that can drain all work currently available.
///
/// Kept separate from [`ZoneEngine`] so cancellation at the boundary between two advances can
/// be tested deterministically without mocking the Engine API and payload builder.
trait AvailableBlockDrain {
    type Block;

    /// Returns the next available block without consuming it.
    fn next_available(&self) -> Option<Self::Block>;

    /// Checks the leadership permit for one available block.
    fn permit(&self, block: &Self::Block) -> (u64, PermitDecision);

    /// Completes and consumes one block.
    async fn advance_one(&mut self, block: Self::Block) -> eyre::Result<()>;
}

/// Drain available blocks until the queue is empty, cancellation is observed, or the
/// leadership permit halts production.
///
/// Cancellation and the permit are checked only before starting a new advance. An advance
/// already in flight is always allowed to finish so its queue confirmation and canonical head
/// remain consistent.
async fn drain_all_available<D>(
    drain: &mut D,
    stop: &CancellationToken,
) -> eyre::Result<Option<EngineExit>>
where
    D: AvailableBlockDrain,
{
    loop {
        if stop.is_cancelled() {
            return Ok(Some(EngineExit::Cancelled));
        }
        let Some(block) = drain.next_available() else {
            return Ok(None);
        };
        match drain.permit(&block) {
            (_, PermitDecision::Produce) => {}
            (tempo_anchor, PermitDecision::Demoted { epoch }) => {
                return Ok(Some(EngineExit::Demoted {
                    tempo_anchor,
                    epoch,
                }));
            }
            (tempo_anchor, PermitDecision::Fenced) => {
                return Ok(Some(EngineExit::Fenced { tempo_anchor }));
            }
        }
        drain.advance_one(block).await?;
    }
}

/// Engine that drives L2 block production from L1 events.
///
/// Waits for L1 blocks in the [`DepositQueue`], then for each block:
/// 1. Peeks the L1 block from the queue
/// 2. Builds [`ZonePayloadAttributes`] wrapping inner Tempo attrs + L1 data
/// 3. Sends FCU with payload attributes to start a build
/// 4. Resolves the built payload
/// 5. Submits via `newPayload`
/// 6. Confirms the L1 block in the queue (removes it)
///
/// On failure the L1 block stays in the queue and is retried.
#[derive(Debug)]
pub struct ZoneEngine {
    /// Chain spec for hardfork checks when building attributes.
    chain_spec: Arc<ZoneChainSpec>,
    /// Engine API handle for FCU and newPayload.
    to_engine: ConsensusEngineHandle<ZonePayloadTypes>,
    /// Payload builder handle.
    payload_builder: PayloadBuilderHandle<ZonePayloadTypes>,
    /// Queue of L1 blocks with their deposits.
    deposit_queue: DepositQueue,
    /// Independently observed L1 blocks retained until their zone payload is accepted.
    l1_block_tracker: L1BlockTracker,
    /// Latest block header — used as parent for the next payload and as the
    /// head/safe/finalized hash in FCU (instant finality).
    last_header: SealedHeader<TempoHeader>,
    /// Address that receives block fees.
    fee_recipient: Address,
    /// Sequencer's secp256k1 secret key for ECIES decryption of encrypted deposits.
    sequencer_key: k256::SecretKey,
    /// ZonePortal address on L1 — used as context in HKDF key derivation.
    portal_address: Address,
    /// Optional per-anchor leadership permit. `None` runs the legacy single-sequencer mode.
    production_permit: Option<ProductionPermit>,
}

impl ZoneEngine {
    pub fn new(
        chain_spec: Arc<ZoneChainSpec>,
        to_engine: ConsensusEngineHandle<ZonePayloadTypes>,
        payload_builder: PayloadBuilderHandle<ZonePayloadTypes>,
        deposit_queue: DepositQueue,
        l1_block_tracker: L1BlockTracker,
        last_header: SealedHeader<TempoHeader>,
        fee_recipient: Address,
        sequencer_key: k256::SecretKey,
        portal_address: Address,
    ) -> Self {
        Self {
            chain_spec,
            to_engine,
            payload_builder,
            deposit_queue,
            l1_block_tracker,
            last_header,
            fee_recipient,
            sequencer_key,
            portal_address,
            production_permit: None,
        }
    }

    /// Enforce the per-anchor leadership permit before every advance.
    pub fn with_production_permit(mut self, permit: ProductionPermit) -> Self {
        self.production_permit = Some(permit);
        self
    }

    /// Runs the main Zone engine loop until cancelled or halted by the leadership permit.
    ///
    /// Without a permit this method only returns on cancellation. It:
    /// 1. Waits for L1 blocks to arrive in the deposit queue
    /// 2. Advances the zone chain for each available L1 block (no delay between blocks)
    /// 3. Sends periodic FCU heartbeats
    pub async fn run_until(mut self, stop: CancellationToken) -> EngineExit {
        let mut fcu_interval = tokio::time::interval(Duration::from_secs(1));
        fcu_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Send initial FCU to establish head
        if let Err(e) = self.update_forkchoice_state().await {
            error!(target: "zone::engine", "Error sending initial FCU: {:?}", e);
        }

        loop {
            tokio::select! {
                biased;
                () = stop.cancelled() => {
                    info!(target: "zone::engine", "ZoneEngine stopped at a block boundary");
                    return EngineExit::Cancelled;
                }
                // Wait for new L1 blocks in the deposit queue
                _ = self.deposit_queue.notified() => {
                    if let Some(exit) = self.advance_all_available(&stop).await {
                        return exit;
                    }
                }
                // Periodic FCU heartbeat — also drains any blocks we missed
                _ = fcu_interval.tick() => {
                    if let Some(exit) = self.advance_all_available(&stop).await {
                        return exit;
                    }
                    if let Err(e) = self.update_forkchoice_state().await {
                        error!(target: "zone::engine", "Error updating fork choice: {:?}", e);
                    }
                }
            }
        }
    }

    /// Runs the engine loop forever (legacy single-sequencer mode).
    pub async fn run(self) {
        self.run_until(CancellationToken::new()).await;
    }

    /// Returns the current forkchoice state.
    ///
    /// The zone has instant finality so head = safe = finalized.
    fn forkchoice_state(&self) -> ForkchoiceState {
        ForkchoiceState::same_hash(self.last_header.hash())
    }

    /// Send an FCU without payload attributes (heartbeat).
    async fn update_forkchoice_state(&self) -> eyre::Result<()> {
        let state = self.forkchoice_state();
        let res = self.to_engine.fork_choice_updated(state, None).await?;

        if !res.is_valid() {
            eyre::bail!("Invalid fork choice update {state:?}: {res:?}");
        }

        Ok(())
    }

    /// Advance the chain for all available L1 blocks in the queue.
    ///
    /// During catch-up this processes blocks as fast as the EVM can execute
    /// them, with no timer delays between blocks.
    ///
    /// Returns `Some` when the loop must stop: cancellation, demotion, or fenced leadership.
    async fn advance_all_available(&mut self, stop: &CancellationToken) -> Option<EngineExit> {
        match drain_all_available(self, stop).await {
            Ok(Some(EngineExit::Cancelled)) | Ok(None) => None,
            Ok(Some(exit @ EngineExit::Demoted { .. })) => {
                info!(target: "zone::engine", ?exit, "Leadership permit revoked; stopping block production at the boundary");
                Some(exit)
            }
            Ok(Some(exit @ EngineExit::Fenced { .. })) => {
                error!(target: "zone::engine", ?exit, "No leadership record governs the next anchor; fencing block production");
                Some(exit)
            }
            Err(e) => {
                error!(target: "zone::engine", "Error advancing the chain: {:?}", e);
                tokio::time::sleep(Duration::from_millis(100)).await;
                None
            }
        }
    }

    /// Decrypt encrypted deposits and ABI-encode them into a [`PreparedL1Block`] ready for
    /// the payload builder. Mint-recipient policy is enforced during upstream TIP-20 execution
    /// against the finalized L1 anchor.
    async fn prepare_l1_block(&self, l1_block: L1BlockDeposits) -> eyre::Result<PreparedL1Block> {
        l1_block
            .prepare(&self.sequencer_key, self.portal_address)
            .await
    }

    /// Advance the chain by one block.
    ///
    /// Wraps the given L1 block into [`ZonePayloadAttributes`], sends FCU
    /// with those attributes, waits for the payload to be built, then submits
    /// via `newPayload`. Only confirms (removes) the L1 block from the
    /// deposit queue after `newPayload` succeeds.
    async fn advance(&mut self, l1_block: L1BlockDeposits) -> eyre::Result<()> {
        let l1_num_hash = l1_block.header.num_hash();

        // Zone block timestamp is locked to the L1 block's timestamp so the
        // two chains stay in lockstep.
        let timestamp_secs = l1_block.header.timestamp();
        let timestamp_millis_part = l1_block.header.timestamp_millis_part;

        let l1_block = self.prepare_l1_block(l1_block).await?;

        let attributes = ZonePayloadAttributes {
            inner: EthPayloadAttributes {
                timestamp: timestamp_secs,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: self.fee_recipient,
                withdrawals: self
                    .chain_spec
                    .is_shanghai_active_at_timestamp(timestamp_secs)
                    .then(Default::default),
                parent_beacon_block_root: self
                    .chain_spec
                    .is_cancun_active_at_timestamp(timestamp_secs)
                    .then_some(B256::ZERO),
                slot_number: None,
                target_gas_limit: None,
            },
            timestamp_millis_part,
            l1_block,
        };

        // Send FCU with payload attributes through the engine API to trigger
        // payload building. The forkchoice state points at the current head;
        // the attributes carry the L1 block data for the new zone block.
        let res = self
            .to_engine
            .fork_choice_updated(self.forkchoice_state(), Some(attributes))
            .await?;

        if res.is_invalid() {
            eyre::bail!("Invalid payload status");
        }

        let payload_id = res.payload_id.ok_or_eyre("No payload id")?;

        let Some(Ok(payload)) = self
            .payload_builder
            .resolve_kind(payload_id, PayloadKind::WaitForPending)
            .await
        else {
            eyre::bail!("No payload");
        };

        let header = payload.block().sealed_header().clone();
        let block_number = header.number();
        let res = self.to_engine.new_payload(payload.into()).await?;

        if !res.is_valid() {
            eyre::bail!("Invalid payload for block {block_number}");
        }

        // newPayload succeeded — remove the exact finalized L1 block that
        // produced it. A mismatch indicates an internal consumer-ordering bug.
        self.deposit_queue.confirm(l1_num_hash)?;
        self.l1_block_tracker.prune_through(l1_num_hash.number);
        if let Some(permit) = &self.production_permit {
            permit.record_applied_anchor(l1_num_hash.number);
        }

        self.last_header = header;

        // Canonicalize the new head — FCU-with-attrs above only set the
        // *previous* head as canonical; this bare FCU makes the just-built
        // block the EL's canonical head.
        if let Err(e) = self.update_forkchoice_state().await {
            error!(target: "zone::engine", "Error sending post-newPayload FCU: {:?}", e);
        }

        Ok(())
    }
}

impl AvailableBlockDrain for ZoneEngine {
    type Block = L1BlockDeposits;

    fn next_available(&self) -> Option<Self::Block> {
        self.deposit_queue.peek()
    }

    fn permit(&self, block: &Self::Block) -> (u64, PermitDecision) {
        let tempo_anchor = block.header.number();
        let decision = self
            .production_permit
            .as_ref()
            .map_or(PermitDecision::Produce, |permit| permit.check(tempo_anchor));
        (tempo_anchor, decision)
    }

    async fn advance_one(&mut self, block: Self::Block) -> eyre::Result<()> {
        self.advance(block).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tokio::sync::oneshot;

    struct PausedDrain {
        pending: VecDeque<u64>,
        advanced: Vec<u64>,
        first_started: Option<oneshot::Sender<()>>,
        release_first: Option<oneshot::Receiver<()>>,
        /// Blocks (by value) the permit rejects, with the simulated decision.
        denied: Vec<(u64, PermitDecision)>,
    }

    impl AvailableBlockDrain for PausedDrain {
        type Block = u64;

        fn next_available(&self) -> Option<Self::Block> {
            self.pending.front().copied()
        }

        fn permit(&self, block: &Self::Block) -> (u64, PermitDecision) {
            let decision = self
                .denied
                .iter()
                .find(|(denied, _)| denied == block)
                .map_or(PermitDecision::Produce, |(_, decision)| *decision);
            (*block, decision)
        }

        async fn advance_one(&mut self, block: Self::Block) -> eyre::Result<()> {
            if let Some(started) = self.first_started.take() {
                let _ = started.send(());
                self.release_first
                    .take()
                    .expect("first advance has a release signal")
                    .await
                    .expect("test releases the first advance");
            }

            assert_eq!(self.pending.pop_front(), Some(block));
            self.advanced.push(block);
            Ok(())
        }
    }

    #[tokio::test]
    async fn cancellation_finishes_the_in_flight_block_without_draining_the_backlog() {
        let stop = CancellationToken::new();
        let task_stop = stop.clone();
        let (first_started, started) = oneshot::channel();
        let (release, release_first) = oneshot::channel();
        let mut drain = PausedDrain {
            pending: VecDeque::from([1, 2, 3]),
            advanced: Vec::new(),
            first_started: Some(first_started),
            release_first: Some(release_first),
            denied: Vec::new(),
        };

        let task = tokio::spawn(async move {
            let exit = drain_all_available(&mut drain, &task_stop)
                .await
                .expect("drain succeeds");
            (drain, exit)
        });

        started.await.expect("the first block starts");
        stop.cancel();
        release
            .send(())
            .expect("the first block is still in flight");

        let (drain, exit) = task.await.expect("drain task succeeds");
        assert_eq!(exit, Some(EngineExit::Cancelled));
        assert_eq!(drain.advanced, [1]);
        assert_eq!(drain.pending, [2, 3]);
    }

    #[tokio::test]
    async fn demotion_halts_production_exactly_at_the_activation_boundary() {
        let stop = CancellationToken::new();
        let mut drain = PausedDrain {
            pending: VecDeque::from([1, 2, 3]),
            advanced: Vec::new(),
            first_started: None,
            release_first: None,
            denied: vec![(3, PermitDecision::Demoted { epoch: 7 })],
        };

        let exit = drain_all_available(&mut drain, &stop)
            .await
            .expect("drain succeeds");
        assert_eq!(
            exit,
            Some(EngineExit::Demoted {
                tempo_anchor: 3,
                epoch: 7,
            })
        );
        // The permitted prefix is produced; the demoted anchor is never consumed.
        assert_eq!(drain.advanced, [1, 2]);
        assert_eq!(drain.pending, [3]);
    }

    #[tokio::test]
    async fn ungoverned_anchor_fences_production() {
        let stop = CancellationToken::new();
        let mut drain = PausedDrain {
            pending: VecDeque::from([5]),
            advanced: Vec::new(),
            first_started: None,
            release_first: None,
            denied: vec![(5, PermitDecision::Fenced)],
        };

        let exit = drain_all_available(&mut drain, &stop)
            .await
            .expect("drain succeeds");
        assert_eq!(exit, Some(EngineExit::Fenced { tempo_anchor: 5 }));
        assert!(drain.advanced.is_empty());
        assert_eq!(drain.pending, [5]);
    }

    #[test]
    fn production_permit_is_a_single_schedule_lookup() {
        use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
        use zone_p2p::LeadershipState;

        let me = PrivateKey::from_seed(1).public_key();
        let other = PrivateKey::from_seed(2).public_key();
        let schedule = LeadershipSchedule::seeded(LeadershipState::new(1, me.clone(), 0));
        schedule
            .publish(LeadershipState::new(2, other, 100))
            .unwrap();
        let permit = ProductionPermit::new(schedule, me);

        assert_eq!(permit.check(0), PermitDecision::Produce);
        assert_eq!(permit.check(99), PermitDecision::Produce);
        assert_eq!(permit.check(100), PermitDecision::Demoted { epoch: 2 });
        assert_eq!(permit.check(u64::MAX), PermitDecision::Demoted { epoch: 2 });

        let uninitialized = ProductionPermit::new(
            LeadershipSchedule::uninitialized(),
            PrivateKey::from_seed(1).public_key(),
        );
        assert_eq!(uninitialized.check(0), PermitDecision::Fenced);
    }
}
