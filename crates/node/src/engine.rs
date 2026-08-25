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
use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tempo_primitives::TempoHeader;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use zone_chainspec::ZoneChainSpec;
use zone_l1::{DepositQueue, EncryptionKeyRing, L1BlockDeposits, L1BlockTracker, PreparedL1Block};
use zone_p2p::{LeadershipSchedule, P2pPeerId};
use zone_payload::{ZonePayloadAttributes, ZonePayloadTypes};

/// Per-anchor production permit backed by the effective leadership schedule.
///
/// The permit is a single schedule lookup: produce anchor `N` only if the portal schedule or a
/// forced-recovery override assigns `N` to this node. An optimistic override is open-ended until
/// the next finalized portal transition supplies the ordinary-authority boundary.
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
    ///
    /// `None` authorizes production; `Some(exit)` is the reason the engine must stop.
    pub fn check(&self, tempo_anchor: u64) -> Option<EngineExit> {
        // NOTE: jtcn 111: Before building a Zone block, checks this node still owns its L1 block.
        // This stops the old leader exactly where a handoff begins.
        match self.schedule.leader_for(tempo_anchor) {
            None => Some(EngineExit::Fenced { tempo_anchor }),
            Some(record) if record.leader == self.local_ed25519_public_key => None,
            Some(record) => Some(EngineExit::Demoted {
                tempo_anchor,
                epoch: record.epoch,
            }),
        }
    }

    /// Record that the zone block embedding `tempo_anchor` is locally canonical.
    pub fn record_applied_anchor(&self, tempo_anchor: u64) {
        self.schedule.record_applied_anchor(tempo_anchor);
    }
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
    ///
    /// `None` authorizes production; `Some(exit)` halts the drain with that reason.
    fn permit(&self, block: &Self::Block) -> Option<EngineExit>;

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
        if let Some(exit) = drain.permit(&block) {
            return Ok(Some(exit));
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
    /// Private keys bound to the Portal indexes used by deposits.
    encryption_keys: EncryptionKeyRing,
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
        encryption_keys: EncryptionKeyRing,
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
            encryption_keys,
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
        // NOTE: jtcn 25: A queued L1 block wakes the engine immediately. The timer only retries if
        // a wake up was missed.
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
        // NOTE: jtcn 26: Drains every finalized L1 block already waiting in the queue. It does not
        // sleep between blocks when the Zone is catching up.
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

    /// Decrypt deposits and ABI-encode them into a [`PreparedL1Block`] ready for
    /// the payload builder. Mint-recipient policy is enforced during upstream TIP-20 execution
    /// against the finalized L1 anchor.
    async fn prepare_l1_block(&self, l1_block: L1BlockDeposits) -> eyre::Result<PreparedL1Block> {
        l1_block
            .prepare(&self.encryption_keys, self.portal_address)
            .await
    }

    /// Advance the chain by one block.
    ///
    /// Wraps the given L1 block into [`ZonePayloadAttributes`], sends FCU
    /// with those attributes, waits for the payload to be built, then submits
    /// via `newPayload`. Only confirms (removes) the L1 block from the
    /// deposit queue after `newPayload` succeeds.
    async fn advance(&mut self, l1_block: L1BlockDeposits) -> eyre::Result<()> {
        // NOTE: jtcn 27: Starts exactly one Zone block for this finalized L1 block. The Zone and L1
        // advance together one block at a time.
        let l1_num_hash = l1_block.header.num_hash();

        // The L1 timestamp is a lower bound so a Zone block anchored after an L1 timestamp-based
        // fork cannot predate it. Use wall-clock time to avoid backdating transactions during
        // catch-up, and advance by at least one millisecond to keep consecutive blocks monotonic.
        let wall_clock_timestamp_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_millis()
            .try_into()?;
        let timestamp_millis = zone_timestamp_millis(
            l1_block.header.timestamp_millis(),
            self.last_header.timestamp_millis(),
            wall_clock_timestamp_millis,
        );
        let timestamp_secs = timestamp_millis / 1000;
        let timestamp_millis_part = timestamp_millis % 1000;

        // NOTE: jtcn 28: Decrypts this L1 block's deposits and packages its header, portal events,
        // and token changes for Zone execution.
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

        // NOTE: jtcn 29: Gives Reth the finalized L1 input for the next Zone block. Reth calls
        // `ZonePayloadBuilder::try_build` to execute and assemble it.

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
        // NOTE: jtcn 31: Sends the completed candidate back through Reth validation. A bad block
        // cannot advance the Zone or consume its queued L1 input.
        let res = self.to_engine.new_payload(payload.into()).await?;

        if !res.is_valid() {
            eyre::bail!("Invalid payload for block {block_number}");
        }

        // NOTE: jtcn 32: After validation, Reth writes the block and its EVM state to the Zone DB.
        // Only then does the node consume the matching L1 item and make the block canonical.

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

        // NOTE: jtcn 33: Checkpoint: One finalized L1 block produced one validated Zone block. The
        // block, receipts, and EVM state are now saved and ready for settlement.
        Ok(())
    }
}

impl AvailableBlockDrain for ZoneEngine {
    type Block = L1BlockDeposits;

    fn next_available(&self) -> Option<Self::Block> {
        self.deposit_queue.peek()
    }

    fn permit(&self, block: &Self::Block) -> Option<EngineExit> {
        // No permit is legacy single-sequencer mode: production is always authorized.
        self.production_permit
            .as_ref()
            .and_then(|permit| permit.check(block.header.number()))
    }

    async fn advance_one(&mut self, block: Self::Block) -> eyre::Result<()> {
        self.advance(block).await
    }
}

/// Select a Zone timestamp that preserves the L1 lower bound without backdating user activity.
fn zone_timestamp_millis(
    l1_timestamp_millis: u64,
    parent_timestamp_millis: u64,
    wall_clock_timestamp_millis: u64,
) -> u64 {
    l1_timestamp_millis
        .max(wall_clock_timestamp_millis)
        .max(parent_timestamp_millis.saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tokio::sync::oneshot;

    #[test]
    fn zone_timestamp_uses_l1_timestamp_as_a_lower_bound() {
        assert_eq!(zone_timestamp_millis(2_000, 999, 1_500), 2_000);
    }

    #[test]
    fn zone_timestamp_uses_wall_clock_during_catch_up() {
        assert_eq!(zone_timestamp_millis(1_000, 999, 2_000), 2_000);
    }

    #[test]
    fn zone_timestamp_advances_past_parent_when_catching_up_in_same_millisecond() {
        assert_eq!(zone_timestamp_millis(1_000, 2_000, 2_000), 2_001);
    }

    struct PausedDrain {
        pending: VecDeque<u64>,
        advanced: Vec<u64>,
        first_started: Option<oneshot::Sender<()>>,
        release_first: Option<oneshot::Receiver<()>>,
        /// Blocks (by value) the permit rejects, with the exit it produces.
        denied: Vec<(u64, EngineExit)>,
    }

    impl AvailableBlockDrain for PausedDrain {
        type Block = u64;

        fn next_available(&self) -> Option<Self::Block> {
            self.pending.front().copied()
        }

        fn permit(&self, block: &Self::Block) -> Option<EngineExit> {
            self.denied
                .iter()
                .find(|(denied, _)| denied == block)
                .map(|(_, exit)| exit.clone())
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
            denied: vec![(
                3,
                EngineExit::Demoted {
                    tempo_anchor: 3,
                    epoch: 7,
                },
            )],
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
            denied: vec![(5, EngineExit::Fenced { tempo_anchor: 5 })],
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
        use alloy_primitives::B256;
        use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
        use zone_p2p::LeadershipState;

        let me = PrivateKey::from_seed(1).public_key();
        let other = PrivateKey::from_seed(2).public_key();
        let schedule = LeadershipSchedule::seeded(LeadershipState::new(1, me.clone(), 0));
        schedule
            .publish(LeadershipState::new(2, other.clone(), 100))
            .unwrap();
        let permit = ProductionPermit::new(schedule, me.clone());

        assert_eq!(permit.check(0), None);
        assert_eq!(permit.check(99), None);
        assert_eq!(
            permit.check(100),
            Some(EngineExit::Demoted {
                tempo_anchor: 100,
                epoch: 2
            })
        );
        assert_eq!(
            permit.check(u64::MAX),
            Some(EngineExit::Demoted {
                tempo_anchor: u64::MAX,
                epoch: 2
            })
        );

        let recovery_schedule = LeadershipSchedule::seeded(LeadershipState::new(7, me, 0));
        recovery_schedule
            .install_forced_recovery(8, other.clone(), B256::repeat_byte(0x11), 51)
            .unwrap();
        recovery_schedule
            .publish(LeadershipState::new(8, other.clone(), 60))
            .unwrap();
        let recovery_permit = ProductionPermit::new(recovery_schedule, other);
        assert_eq!(
            recovery_permit.check(50),
            Some(EngineExit::Demoted {
                tempo_anchor: 50,
                epoch: 7
            })
        );
        assert_eq!(recovery_permit.check(51), None);
        assert_eq!(recovery_permit.check(59), None);
        assert_eq!(recovery_permit.check(60), None);

        let uninitialized = ProductionPermit::new(
            LeadershipSchedule::uninitialized(),
            PrivateKey::from_seed(1).public_key(),
        );
        assert_eq!(
            uninitialized.check(0),
            Some(EngineExit::Fenced { tempo_anchor: 0 })
        );
    }
}
