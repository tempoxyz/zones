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
use tempo_chainspec::spec::TempoHardforks as _;
use tempo_primitives::TempoHeader;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

use zone_chainspec::ZoneChainSpec;
use zone_l1::{DepositQueue, EncryptionKeyRing, FinalizedTarget, L1BlockDeposits, L1BlockTracker};
use zone_p2p::{LeadershipSchedule, P2pPeerId};
use zone_payload::{TempoImport, ZonePayloadAttributes, ZonePayloadTypes};

/// Per-anchor production permit backed by the effective leadership schedule.
///
/// The permit is a single schedule lookup: produce a Zone block only if the portal schedule or a
/// forced-recovery override assigns this node as leader for the block's first imported Tempo
/// header. An optimistic override is open-ended until the next finalized portal transition
/// supplies the ordinary-authority boundary.
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
    fn next_available(&self) -> eyre::Result<Option<Self::Block>>;

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
        let Some(block) = drain.next_available()? else {
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

    /// Advance the chain by one block.
    ///
    /// Wraps the given L1 block into [`ZonePayloadAttributes`], sends FCU
    /// with those attributes, waits for the payload to be built, then submits
    /// via `newPayload`. Only confirms (removes) the L1 block from the
    /// deposit queue after `newPayload` succeeds.
    async fn advance(&mut self, available: AvailableTempoImport) -> eyre::Result<()> {
        let AvailableTempoImport {
            l1_block,
            checkpoint_headers,
            wall_clock_timestamp_millis,
        } = available;
        let checkpoint_only = !checkpoint_headers.is_empty();
        let final_header = checkpoint_headers.last().unwrap_or(&l1_block.header);
        let l1_num_hash = final_header.num_hash();

        let timestamp_millis = zone_timestamp_millis(
            final_header.timestamp_millis(),
            self.last_header.timestamp_millis(),
            wall_clock_timestamp_millis,
        );
        let timestamp_secs = timestamp_millis / 1000;
        let timestamp_millis_part = timestamp_millis % 1000;

        let tempo_import = if checkpoint_only {
            TempoImport::CheckpointOnly(checkpoint_headers)
        } else {
            let portal_work = self
                .deposit_queue
                .operational_work(l1_block.header.num_hash())?;
            TempoImport::Full(Box::new(
                L1BlockDeposits::prepare_many(
                    portal_work,
                    &self.encryption_keys,
                    self.portal_address,
                )
                .await?,
            ))
        };

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
            tempo_import,
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
        if checkpoint_only {
            self.deposit_queue.defer_through(l1_num_hash)?;
        } else {
            self.deposit_queue.confirm_operational(l1_num_hash)?;
        }
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
    type Block = AvailableTempoImport;

    fn next_available(&self) -> eyre::Result<Option<Self::Block>> {
        let Some(l1_block) = self.deposit_queue.peek() else {
            return Ok(None);
        };
        let mut queued_headers = self
            .deposit_queue
            .peek_headers(zone_primitives::constants::MAX_TEMPO_HEADERS_PER_ZONE_BLOCK + 1);
        let latest_l1_header = self
            .deposit_queue
            .latest_header()
            .ok_or_eyre("L1 deposit queue lost its latest header")?;
        let wall_clock_timestamp_millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_millis()
            .try_into()?;
        let decision = tempo_import_decision(
            &self.chain_spec,
            &queued_headers,
            &latest_l1_header,
            self.l1_block_tracker.finalized_target(),
            self.last_header.timestamp_millis(),
            wall_clock_timestamp_millis,
        );
        let checkpoint_headers = match decision {
            TempoImportDecision::WaitForHardforkMatch
            | TempoImportDecision::WaitForFinalizedTarget => return Ok(None),
            TempoImportDecision::ImportFull => Vec::new(),
            TempoImportDecision::ImportCheckpoints(count) => {
                queued_headers.truncate(count);
                queued_headers
            }
        };
        Ok(Some(AvailableTempoImport {
            l1_block,
            checkpoint_headers,
            wall_clock_timestamp_millis,
        }))
    }

    fn permit(&self, block: &Self::Block) -> Option<EngineExit> {
        self.production_permit
            .as_ref()
            .and_then(|permit| permit.check(block.leader_anchor()))
    }

    async fn advance_one(&mut self, block: Self::Block) -> eyre::Result<()> {
        self.advance(block).await
    }
}

#[derive(Debug)]
struct AvailableTempoImport {
    l1_block: L1BlockDeposits,
    checkpoint_headers: Vec<SealedHeader<TempoHeader>>,
    wall_clock_timestamp_millis: u64,
}

impl AvailableTempoImport {
    /// Historical Tempo anchor whose leader must produce this Zone block.
    fn leader_anchor(&self) -> u64 {
        self.checkpoint_headers
            .first()
            .unwrap_or(&self.l1_block.header)
            .number()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TempoImportDecision {
    /// Wait until the queued L1 tip activates the hardfork required by the next Zone block.
    WaitForHardforkMatch,
    /// Wait until the subscriber confirms the queued finalized target did not advance.
    WaitForFinalizedTarget,
    /// Import the full block with `advanceTempo`.
    ImportFull,
    /// Import specified number of checkpoint headers with `advanceTempoHeaders`.
    ImportCheckpoints(usize),
}

fn tempo_import_decision(
    chain_spec: &ZoneChainSpec,
    queued_headers: &[SealedHeader<TempoHeader>],
    latest_l1_header: &SealedHeader<TempoHeader>,
    finalized_target: Option<FinalizedTarget>,
    parent_timestamp_millis: u64,
    wall_clock_timestamp_millis: u64,
) -> TempoImportDecision {
    let Some(first_header) = queued_headers.first() else {
        return TempoImportDecision::ImportFull;
    };
    let first_l1_hardfork = chain_spec.tempo_hardfork_at(first_header.timestamp());
    let next_timestamp_millis = zone_timestamp_millis(
        first_header.timestamp_millis(),
        parent_timestamp_millis,
        wall_clock_timestamp_millis,
    );
    let zone_hardfork = chain_spec.tempo_hardfork_at(next_timestamp_millis / 1000);
    let l1_tip_hardfork = chain_spec.tempo_hardfork_at(latest_l1_header.timestamp());

    if !zone_hardfork.is_t12() {
        return TempoImportDecision::ImportFull;
    }
    // Zone execution must not activate a hardfork before L1. Wait whenever the prospective Zone
    // block is ahead of the latest queued L1 header, but allow L1 to be ahead while the Zone
    // imports the remaining pre-fork prefix under its currently active rules.
    if zone_hardfork > l1_tip_hardfork {
        return TempoImportDecision::WaitForHardforkMatch;
    }

    // During backfill, the subscriber announces its finalized target before filling the queue.
    // Until the announced target is visible, every queued header may be checkpointed because a
    // later header is known to exist for the required full block. Once the target is queued,
    // reserve it while the subscriber checks whether finalized advanced again. Only a stable
    // target may become the operational import.
    let target_is_visible = finalized_target.is_some_and(|target| {
        queued_headers
            .last()
            .is_some_and(|header| header.number() >= target.number)
    });
    let reserve_for_full = finalized_target.is_none() || target_is_visible;
    let checkpoint_count = queued_headers
        .len()
        .saturating_sub(usize::from(reserve_for_full))
        .min(zone_primitives::constants::MAX_TEMPO_HEADERS_PER_ZONE_BLOCK)
        // Never reserve a queue front from an older hardfork for a full import under newer Zone
        // rules. This also closes the small race where the hardfork successor is appended between
        // the bounded queue snapshot and tip read.
        .max(usize::from(first_l1_hardfork < zone_hardfork));
    if checkpoint_count == 0 {
        if target_is_visible && finalized_target.is_some_and(|target| !target.ready) {
            TempoImportDecision::WaitForFinalizedTarget
        } else {
            TempoImportDecision::ImportFull
        }
    } else {
        TempoImportDecision::ImportCheckpoints(checkpoint_count)
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

    fn t12_spec(activation: u64) -> ZoneChainSpec {
        use reth_chainspec::EthChainSpec as _;
        let mut genesis = tempo_chainspec::spec::DEV.genesis().clone();
        genesis.config.chain_id =
            zone_primitives::constants::zone_chain_id(tempo_chainspec::spec::DEV.chain().id(), 1)
                .unwrap();
        genesis
            .config
            .extra_fields
            .insert_value("t12Time".into(), activation)
            .unwrap();
        ZoneChainSpec::from_genesis(genesis).unwrap()
    }

    fn header(number: u64, timestamp: u64) -> SealedHeader<TempoHeader> {
        SealedHeader::seal_slow(TempoHeader {
            inner: alloy_consensus::Header {
                number,
                timestamp,
                ..Default::default()
            },
            ..Default::default()
        })
    }

    fn finalized_target(number: u64, ready: bool) -> Option<FinalizedTarget> {
        Some(FinalizedTarget { number, ready })
    }

    #[test]
    fn checkpoint_import_uses_first_header_as_leader_anchor() {
        let available = AvailableTempoImport {
            l1_block: L1BlockDeposits {
                header: header(90, 90),
                events: Default::default(),
            },
            checkpoint_headers: vec![header(90, 90), header(110, 110)],
            wall_clock_timestamp_millis: 110_000,
        };

        assert_eq!(available.leader_anchor(), 90);
    }

    #[test]
    fn t12_boundary_waits_for_l1_then_checkpoints_the_t11_prefix() {
        let spec = t12_spec(100);
        let t11 = header(1, 99);
        let t12 = header(2, 100);

        assert_eq!(
            tempo_import_decision(
                &spec,
                std::slice::from_ref(&t11),
                &t11,
                finalized_target(1, true),
                98_000,
                100_000
            ),
            TempoImportDecision::WaitForHardforkMatch
        );
        assert_eq!(
            tempo_import_decision(
                &spec,
                std::slice::from_ref(&t11),
                &t12,
                None,
                98_000,
                100_000,
            ),
            TempoImportDecision::ImportCheckpoints(1)
        );
        assert_eq!(
            tempo_import_decision(
                &spec,
                &[t11, t12.clone()],
                &t12,
                finalized_target(2, false),
                98_000,
                100_000,
            ),
            TempoImportDecision::ImportCheckpoints(1)
        );
        assert_eq!(
            tempo_import_decision(
                &spec,
                std::slice::from_ref(&t12),
                &t12,
                finalized_target(2, true),
                99_000,
                100_000
            ),
            TempoImportDecision::ImportFull
        );
    }

    #[test]
    fn pre_t12_zone_block_imports_the_t11_front_normally() {
        let spec = t12_spec(100);
        let t11 = header(1, 99);
        assert_eq!(
            tempo_import_decision(
                &spec,
                std::slice::from_ref(&t11),
                &t11,
                finalized_target(1, true),
                98_000,
                99_000
            ),
            TempoImportDecision::ImportFull
        );
    }

    #[test]
    fn hardfork_gate_does_not_wait_when_l1_is_ahead_of_the_zone() {
        let spec = t12_spec(100);
        let t11 = header(1, 99);
        let t12 = header(2, 100);

        assert_eq!(
            tempo_import_decision(
                &spec,
                &[t11],
                &t12,
                finalized_target(2, true),
                98_000,
                99_000,
            ),
            TempoImportDecision::ImportFull
        );
    }

    #[test]
    fn checkpoint_batching_uses_announced_finalized_target() {
        let spec = t12_spec(100);

        // Backfill has announced 100 missing blocks, but only the first is verified and queued.
        // It must remain a checkpoint-only import instead of becoming a premature full block.
        let first = header(100, 100);
        assert_eq!(
            tempo_import_decision(
                &spec,
                std::slice::from_ref(&first),
                &first,
                finalized_target(199, false),
                99_000,
                100_000,
            ),
            TempoImportDecision::ImportCheckpoints(1)
        );

        // Once all 100 are queued, reserve the target header for the full operational import and
        // fold the preceding 99 headers into one checkpoint-only block.
        let headers = (100..=199)
            .map(|number| header(number, number))
            .collect::<Vec<_>>();
        assert_eq!(
            tempo_import_decision(
                &spec,
                &headers,
                headers.last().unwrap(),
                finalized_target(199, false),
                99_000,
                100_000,
            ),
            TempoImportDecision::ImportCheckpoints(99)
        );

        let target = header(199, 199);
        assert_eq!(
            tempo_import_decision(
                &spec,
                std::slice::from_ref(&target),
                &target,
                finalized_target(199, false),
                99_000,
                100_000,
            ),
            TempoImportDecision::WaitForFinalizedTarget
        );
        assert_eq!(
            tempo_import_decision(
                &spec,
                std::slice::from_ref(&target),
                &target,
                finalized_target(199, true),
                99_000,
                100_000,
            ),
            TempoImportDecision::ImportFull
        );
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

        fn next_available(&self) -> eyre::Result<Option<Self::Block>> {
            Ok(self.pending.front().copied())
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
