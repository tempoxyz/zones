//! Node-side leader block replication and follower import.

use alloy_consensus::BlockHeader as _;
use alloy_eips::NumHash;
use alloy_primitives::B256;
use alloy_provider::DynProvider;
use alloy_rlp::Decodable as _;
use alloy_rpc_types_engine::ForkchoiceState;
use alloy_sol_types::SolCall as _;
use futures::{StreamExt as _, stream::BoxStream};
use reth_chain_state::PersistedBlockSubscriptions;
use reth_node_api::{ConsensusEngineHandle, PayloadTypes as _};
use reth_primitives_traits::{SealedBlock, SealedHeader};
use reth_provider::HeaderProvider;
use reth_storage_api::{BlockNumReader, BlockReader, ReceiptProvider, StateProviderFactory};
use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};
use tempo_alloy::TempoNetwork;
use tempo_primitives::{Block, TempoHeader, TempoTxEnvelope};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync;
use tracing::{debug, info};
use zone_l1::{DepositQueue, L1BlockTracker, L1PortalEvents, TempoStateExt as _};
use zone_p2p::{
    BackfillCommand, BackfillRequest, BackfillResponse, LeadershipSchedule, P2pCommand, P2pEvent,
    P2pPeerId, PeerTip,
};
use zone_payload::{
    ZonePayloadTypes,
    abi::{IZoneInbox, ZONE_INBOX_ADDRESS},
};
use zone_sequencer::{
    BatchAnchorConfig,
    attestation::{
        AttestationDomain, AttestationStore, SettlementAttestation, SignedSettlementAttestation,
    },
};

use alloy_signer_local::PrivateKeySigner;
use eyre::{OptionExt as _, WrapErr as _};

use crate::settlement_attestation::build_settlement_attestation;

/// Shared signing and L1-validation context for settlement attestations.
#[derive(Clone)]
pub(crate) struct AttestationContext {
    pub(crate) domain: AttestationDomain,
    /// Portal sequencer-set version validated against the manifest at startup.
    pub(crate) pinned_sequencer_set_version: Option<u64>,
    /// `None` on an rpc-only member: it holds no individual key and never signs.
    pub(crate) signer: Option<PrivateKeySigner>,
    pub(crate) addresses: HashMap<zone_p2p::P2pPeerId, alloy_primitives::Address>,
    pub(crate) store: AttestationStore,
    pub(crate) l1_provider: DynProvider<TempoNetwork>,
    pub(crate) anchor_config: BatchAnchorConfig,
}

impl AttestationContext {
    pub(crate) fn new(
        domain: AttestationDomain,
        pinned_sequencer_set_version: Option<u64>,
        signer: Option<PrivateKeySigner>,
        addresses: HashMap<zone_p2p::P2pPeerId, alloy_primitives::Address>,
        store: AttestationStore,
        l1_provider: DynProvider<TempoNetwork>,
        anchor_config: BatchAnchorConfig,
    ) -> Self {
        Self {
            domain,
            pinned_sequencer_set_version,
            signer,
            addresses,
            store,
            l1_provider,
            anchor_config,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PersistedTip {
    number: u64,
    hash: B256,
}

pub(crate) struct EncodedPersistedBlock {
    number: u64,
    hash: B256,
    encoded: Vec<u8>,
}

/// The one shutdown decision the role controller makes for a leader's block broadcaster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BroadcasterShutdown {
    /// The engine has stopped, so wait for its frozen canonical tail to become durable.
    Drain,
    /// The engine stop was not proven; flush only the durable prefix and abandon the rest.
    Stop,
}

/// Interface used by the replication task to keep track of blocks that are persisted vs broadcast
pub(crate) trait PersistedBlockSource: Clone + Send + Sync + 'static {
    fn last_block_number(&self) -> eyre::Result<u64>;
    /// The canonical head, which may be ahead of the persisted head (reth persists lazily).
    ///
    /// This is a drain *target*, never a broadcast height: a canonical-only block lives in
    /// reth's volatile in-memory state and would vanish from this node on restart. Read it only
    /// after the engine has stopped, and publish the blocks it names through the persisted
    /// stream.
    fn canonical_block_number(&self) -> eyre::Result<u64>;
    fn persisted_block_stream(&self) -> BoxStream<'static, PersistedTip>;
    fn encoded_block_by_number(&self, number: u64) -> eyre::Result<EncodedPersistedBlock>;
}

impl<P> PersistedBlockSource for P
where
    P: PersistedBlockSubscriptions + BlockReader<Block = Block> + Clone + Send + Sync + 'static,
{
    fn last_block_number(&self) -> eyre::Result<u64> {
        Ok(BlockNumReader::last_block_number(self)?)
    }

    fn canonical_block_number(&self) -> eyre::Result<u64> {
        Ok(BlockNumReader::best_block_number(self)?)
    }

    fn persisted_block_stream(&self) -> BoxStream<'static, PersistedTip> {
        PersistedBlockSubscriptions::persisted_block_stream(self)
            .map(|tip| PersistedTip {
                number: tip.number,
                hash: tip.hash,
            })
            .boxed()
    }

    fn encoded_block_by_number(&self, number: u64) -> eyre::Result<EncodedPersistedBlock> {
        let block = self
            .block_by_number(number)?
            .ok_or_else(|| eyre::eyre!("persisted zone block {number} is missing"))?;
        let sealed = SealedBlock::seal_slow(block);
        Ok(EncodedPersistedBlock {
            number: sealed.number(),
            hash: sealed.hash(),
            encoded: alloy_rlp::encode(sealed.into_block()),
        })
    }
}

/// Broadcast every newly persisted leader block in canonical order until cancelled.
///
/// A single shutdown command decides how to handle the leader's final block:
///
/// * [`BroadcasterShutdown::Drain`] is the graceful path. The role controller sends it only
///   after the engine task has returned, which freezes the canonical head and lets this task wait
///   for the final block to persist before publishing it.
/// * [`BroadcasterShutdown::Stop`] is the abrupt path. It flushes what is already durable and
///   abandons the rest.
///
/// Neither path ever broadcasts a canonical-only block.
pub(crate) async fn broadcast_persisted_blocks<P>(
    provider: P,
    commands: mpsc::Sender<P2pCommand>,
    mut shutdown: oneshot::Receiver<BroadcasterShutdown>,
) where
    P: PersistedBlockSource,
{
    // Handle race conditions carefully at startup. Read before subscribing, then reconcile after subscribing.
    // This closes both startup windows: a block persisted before the subscription is found by the
    // second read, while a block persisted after the subscription is retained by the stream.
    let mut last_broadcast = match provider.last_block_number() {
        Ok(number) => number,
        Err(err) => {
            tracing::error!(target: "zone::p2p", %err, "Failed reading persisted zone head");
            return;
        }
    };
    let mut persisted = provider.persisted_block_stream();
    let startup_tip = match provider.last_block_number() {
        Ok(number) => number,
        Err(err) => {
            tracing::error!(target: "zone::p2p", %err, "Failed reconciling persisted zone head");
            return;
        }
    };

    if let Err(err) =
        broadcast_persisted_range(&provider, &commands, &mut last_broadcast, startup_tip, None)
            .await
    {
        tracing::error!(target: "zone::p2p", %err, "Failed broadcasting persisted zone blocks");
        return;
    }

    loop {
        let persisted_tip = tokio::select! {
            biased;
            command = &mut shutdown => {
                match command {
                    Ok(BroadcasterShutdown::Drain) => {
                        if let Err(err) = drain_persisted_blocks_after_engine_stop(
                            &provider,
                            &commands,
                            &mut last_broadcast,
                            &mut persisted,
                        )
                        .await
                        {
                            tracing::error!(target: "zone::p2p", %err, "Failed draining persisted zone blocks after the leader engine stopped");
                        }
                    }
                    Ok(BroadcasterShutdown::Stop) => {
                        // Stopped without an engine-complete drain, so the canonical head is
                        // still moving and cannot be a flush target. Publish what is already
                        // durable and abandon the rest: a canonical-only block would disappear
                        // from this node on restart, leaving followers on a height no replica can
                        // serve.
                        match provider.last_block_number() {
                            Ok(persisted_head) => {
                                if let Err(err) = broadcast_persisted_range(
                                    &provider,
                                    &commands,
                                    &mut last_broadcast,
                                    persisted_head,
                                    None,
                                )
                                .await
                                {
                                    tracing::error!(target: "zone::p2p", %err, "Failed flushing persisted zone blocks on stop");
                                }
                            }
                            Err(err) => {
                                tracing::error!(target: "zone::p2p", %err, "Failed reading the persisted zone head for the stop flush");
                            }
                        }
                        debug!(target: "zone::p2p", "Persisted block broadcaster stopped");
                    }
                    Err(_) => {
                        debug!(target: "zone::p2p", "Persisted block broadcaster shutdown control dropped");
                    }
                }
                return;
            }
            tip = persisted.next() => match tip {
                Some(tip) => tip,
                None => break,
            },
        };
        if persisted_tip.number < last_broadcast {
            tracing::error!(
                target: "zone::p2p",
                persisted = persisted_tip.number,
                last_broadcast,
                "Persisted zone head moved backwards"
            );
            return;
        }

        if let Err(err) = broadcast_persisted_range(
            &provider,
            &commands,
            &mut last_broadcast,
            persisted_tip.number,
            Some(persisted_tip.hash),
        )
        .await
        {
            tracing::error!(target: "zone::p2p", %err, "Failed broadcasting persisted zone blocks");
            return;
        }
    }
    debug!(target: "zone::p2p", "Persisted block stream closed");
}

/// Publish the outgoing leader's final blocks once they are durable.
///
/// The caller must have observed the engine task return before signalling this, which is what
/// makes the loop terminate: cancelling the generation token is not a block boundary, because an
/// in-flight advance still completes before the engine yields. Only a stopped engine pins the
/// canonical head, and only a pinned target can be waited for.
///
/// Blocks still go out through [`broadcast_persisted_range`] with the hash the persisted stream
/// reported, so the durable-source invariant holds on this path too.
async fn drain_persisted_blocks_after_engine_stop<P>(
    provider: &P,
    commands: &mpsc::Sender<P2pCommand>,
    last_broadcast: &mut u64,
    persisted: &mut BoxStream<'static, PersistedTip>,
) -> eyre::Result<()>
where
    P: PersistedBlockSource,
{
    let canonical = provider.canonical_block_number()?;
    let persisted_head = provider.last_block_number()?;
    broadcast_persisted_range(provider, commands, last_broadcast, persisted_head, None).await?;

    while *last_broadcast < canonical {
        let persisted_tip = persisted.next().await.ok_or_else(|| {
            eyre::eyre!("persisted zone block stream closed before the canonical tail persisted")
        })?;
        if persisted_tip.number < *last_broadcast {
            eyre::bail!(
                "persisted zone head moved backwards while draining after the leader engine stopped: persisted={}, last_broadcast={}",
                persisted_tip.number,
                *last_broadcast,
            );
        }
        broadcast_persisted_range(
            provider,
            commands,
            last_broadcast,
            persisted_tip.number,
            Some(persisted_tip.hash),
        )
        .await?;
    }

    debug!(target: "zone::p2p", canonical, broadcast = *last_broadcast, "Drained the canonical tail after the leader engine stopped");
    Ok(())
}

async fn broadcast_persisted_range<P>(
    provider: &P,
    commands: &mpsc::Sender<P2pCommand>,
    last_broadcast: &mut u64,
    tip_number: u64,
    expected_tip_hash: Option<B256>,
) -> eyre::Result<()>
where
    P: PersistedBlockSource,
{
    for number in last_broadcast.saturating_add(1)..=tip_number {
        let block = provider.encoded_block_by_number(number)?;
        let number = block.number;
        let hash = block.hash;
        if number == tip_number
            && let Some(expected) = expected_tip_hash
            && hash != expected
        {
            eyre::bail!(
                "persisted zone block hash does not match notification at height {number}: expected={expected}, actual={hash}"
            );
        }
        commands
            .send(P2pCommand::BroadcastBlock(block.encoded))
            .await
            .map_err(|_| eyre::eyre!("P2P command channel closed"))?;
        debug!(target: "zone::p2p", number, ?hash, "Queued persisted block for followers");
        *last_broadcast = number;
    }
    Ok(())
}

const BACKFILL_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const BLOCK_INACTIVITY_TIMEOUT: Duration = Duration::from_secs(30);
const PEER_ANCHOR_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PENDING_BLOCKS: usize = 128;
const BACKFILL_PAGE_SIZE: u64 = 64;
/// Bounds queued requests at the process-lifetime backfill server. Requesters keep at most
/// one request outstanding per peer, so manifest size bounds the live queue depth; the
/// headroom absorbs requests arriving before the server task starts.
pub(crate) const BACKFILL_SERVE_QUEUE_CAPACITY: usize = 128;

/// Keep track of the backfill exactly. We'll buffer any live blocks received
/// during backfill.
struct BackfillProgress {
    target_tip: Option<u64>,
    received_completion: bool,
    needed: bool,
}

impl BackfillProgress {
    const fn new() -> Self {
        Self {
            target_tip: None,
            received_completion: false,
            needed: true,
        }
    }

    fn observe_block(&mut self, number: u64, best: u64) {
        self.target_tip = Some(self.target_tip.map_or(number, |tip| tip.max(number)));
        if number > best.saturating_add(1) {
            self.needed = true;
        }
    }

    fn refresh_after_import(&mut self, best: u64, first_pending: Option<u64>) {
        self.needed = !self.received_completion
            || self.target_tip.is_some_and(|tip| best < tip)
            || first_pending.is_some_and(|number| number > best.saturating_add(1));
    }

    fn complete(&mut self, tip: u64, best: u64, first_pending: Option<u64>) {
        self.received_completion = true;
        self.target_tip = Some(self.target_tip.map_or(tip, |target| target.max(tip)));
        self.needed = best < self.target_tip.unwrap_or(tip)
            || first_pending.is_some_and(|number| number > best.saturating_add(1));
    }

    fn request(&self, best: u64) -> Option<BackfillCommand> {
        self.needed.then(|| BackfillCommand::Request {
            start: best.saturating_add(1),
        })
    }

    fn probe_after_inactivity(&mut self, best: u64) -> BackfillCommand {
        self.needed = true;
        BackfillCommand::Request {
            start: best.saturating_add(1),
        }
    }
}

/// A peer block awaiting import, with its provenance.
///
/// `live_sender` is the broadcast sender for live blocks and `None` for
/// backfilled blocks.
#[derive(Debug, Clone)]
pub(crate) struct PendingPeerBlock {
    encoded: Vec<u8>,
    live_sender: Option<P2pPeerId>,
}

/// Latest tip evidence advertised by each peer, with observation time.
///
/// Fed by backfill completions on the follower sync loop and consumed by the status RPC.
#[derive(Debug, Clone, Default)]
pub(crate) struct PeerTipRegistry {
    inner: std::sync::Arc<std::sync::Mutex<HashMap<P2pPeerId, (PeerTip, std::time::Instant)>>>,
}

impl PeerTipRegistry {
    pub(crate) fn record(&self, peer: P2pPeerId, tip: PeerTip) {
        self.inner
            .lock()
            .expect("poisoned")
            .insert(peer, (tip, std::time::Instant::now()));
    }

    pub(crate) fn snapshot(&self) -> Vec<(P2pPeerId, PeerTip, std::time::Instant)> {
        self.inner
            .lock()
            .expect("poisoned")
            .iter()
            .map(|(peer, (tip, at))| (peer.clone(), *tip, *at))
            .collect()
    }
}

fn buffer_pending_block(
    pending: &mut BTreeMap<u64, PendingPeerBlock>,
    number: u64,
    block: PendingPeerBlock,
) -> Option<u64> {
    if pending.contains_key(&number) {
        return None;
    }
    if pending.len() < MAX_PENDING_BLOCKS {
        pending.insert(number, block);
        return None;
    }

    let Some((&farthest, _)) = pending.last_key_value() else {
        pending.insert(number, block);
        return None;
    };
    if number < farthest {
        pending.pop_last();
        pending.insert(number, block);
        Some(farthest)
    } else {
        Some(number)
    }
}

fn encoded_block_number(encoded: &[u8]) -> eyre::Result<u64> {
    let mut input = encoded;
    let block = Block::decode(&mut input)
        .map_err(|err| eyre::eyre!("invalid RLP-encoded zone block: {err}"))?;
    eyre::ensure!(
        input.is_empty(),
        "encoded zone block has {} trailing bytes",
        input.len()
    );
    Ok(block.header.number())
}

fn serve_backfill_page<P>(
    provider: &P,
    commands: &mpsc::Sender<BackfillCommand>,
    peer: zone_p2p::P2pPeerId,
    request_id: u64,
    start: u64,
) -> eyre::Result<()>
where
    P: BlockNumReader
        + BlockReader<Block = Block>
        + HeaderProvider<Header = TempoHeader>
        + StateProviderFactory,
{
    let tip = provider.last_block_number()?;
    let end = tip.min(start.saturating_add(BACKFILL_PAGE_SIZE.saturating_sub(1)));
    for number in start..=end {
        let block = provider.block_by_number(number)?.ok_or_else(|| {
            eyre::eyre!("persisted canonical block {number} is missing while serving backfill")
        })?;
        commands
            .blocking_send(BackfillCommand::SendBlock {
                peer: peer.clone(),
                request_id,
                block: alloy_rlp::encode(block),
            })
            .map_err(|_| eyre::eyre!("P2P command channel closed"))?;
    }
    // Advertise the persisted tip.
    let tip_header = provider.sealed_header(tip)?.ok_or_else(|| {
        eyre::eyre!("persisted canonical head {tip} is missing while serving backfill")
    })?;
    let tempo = provider
        .state_by_block_hash(tip_header.hash())?
        .tempo_num_hash()?;
    commands
        .blocking_send(BackfillCommand::Complete {
            peer,
            request_id,
            tip: PeerTip {
                zone_height: tip,
                zone_hash: tip_header.hash(),
                tempo_block_number: tempo.number,
                tempo_block_hash: tempo.hash,
            },
        })
        .map_err(|_| eyre::eyre!("P2P command channel closed"))?;
    Ok(())
}

/// Serve block backfill requests for the process lifetime.
///
/// Backfill serving is role-neutral: leaders, followers, and fenced nodes all serve the
/// same canonical provider. Running one server outside the role generations means a
/// generation switch can never drop or abandon an accepted request, which would suppress
/// the requesting peer until its response timeout and stall a leadership handoff.
/// Exits when the request channel closes.
pub(crate) async fn serve_backfill_requests<P>(
    provider: P,
    commands: mpsc::Sender<BackfillCommand>,
    mut requests: mpsc::Receiver<BackfillRequest>,
) where
    P: BlockNumReader
        + BlockReader<Block = Block>
        + HeaderProvider<Header = TempoHeader>
        + StateProviderFactory
        + Clone
        + Send
        + Sync
        + 'static,
{
    // One worker deliberately serializes page construction and sending, bounding serving
    // concurrency independently of the block import loop.
    while let Some(BackfillRequest {
        peer,
        request_id,
        start,
    }) = requests.recv().await
    {
        let page_provider = provider.clone();
        let page_commands = commands.clone();
        let page = tokio::task::spawn_blocking(move || {
            serve_backfill_page(&page_provider, &page_commands, peer, request_id, start)
        })
        .await;
        let result = match page {
            Ok(result) => result,
            Err(err) => Err(eyre::eyre!("backfill page worker failed: {err}")),
        };
        if let Err(err) = result {
            tracing::error!(target: "zone::p2p", %err, start, "Failed serving block backfill");
        }
    }
}

/// Collect and verify follower settlement signatures on the leader, without ever
/// requesting or importing peer blocks.
///
/// While this runs, [`ZoneEngine`](crate::ZoneEngine) is the sole chain-head
/// writer. Backfill requests are served by the process-lifetime
/// [`serve_backfill_requests`] task, never by role generations. The loop exits
/// when `stop` fires.
async fn store_follower_settlement_signature<P>(
    provider: &P,
    follower: &P2pPeerId,
    signature: &[u8],
    attestation: &AttestationContext,
    store: &AttestationStore,
) -> eyre::Result<(u64, alloy_primitives::Address, usize)>
where
    P: HeaderProvider<Header = TempoHeader> + ReceiptProvider,
{
    let signed = SignedSettlementAttestation::decode(signature)?;
    let signer = signed.recover_signer(attestation.domain)?;
    let expected_signer = attestation
        .addresses
        .get(follower)
        .copied()
        .ok_or_eyre("unknown follower identity")?;
    eyre::ensure!(
        signer == expected_signer,
        "settlement signer does not match authenticated peer"
    );
    let height: u64 = signed
        .attestation
        .zoneHeight
        .try_into()
        .wrap_err("settlement height does not fit in u64")?;
    let digest = attestation.domain.settlement_digest(&signed.attestation);
    let leader = attestation
        .signer
        .as_ref()
        .ok_or_eyre("leader has no settlement signer")?
        .address();
    store.precheck_follower_settlement(height, digest, leader, signer)?;

    let expected = build_settlement_attestation(
        provider,
        height,
        attestation,
        (
            signed.attestation.anchorBlockNumber,
            signed.attestation.anchorBlockHash,
        ),
    )
    .await?
    .ok_or_eyre("signed block is not a batch boundary")?;
    eyre::ensure!(
        signed.attestation == expected,
        "settlement signature does not match leader state"
    );
    let signatures =
        store.insert_follower_settlement(attestation.domain, leader, signer, signed)?;
    Ok((height, signer, signatures))
}

pub(crate) async fn collect_follower_settlement_signatures<P>(
    provider: P,
    mut events: mpsc::Receiver<P2pEvent>,
    attestation: AttestationContext,
    stop: sync::CancellationToken,
) where
    P: BlockNumReader
        + BlockReader<Block = Block>
        + HeaderProvider<Header = TempoHeader>
        + StateProviderFactory
        + ReceiptProvider
        + Clone
        + Send
        + Sync
        + 'static,
{
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => {
                debug!(target: "zone::p2p", "Leader settlement signature collection stopped");
                return;
            }
            event = events.recv() => {
                let Some(event) = event else {
                    debug!(target: "zone::p2p", "P2P event channel closed");
                    return;
                };
                match event {
                    P2pEvent::Started { .. } => {}
                    P2pEvent::SettlementSignatureReceived { follower, signature } => {
                        let result = async {
                            store_follower_settlement_signature(
                                &provider,
                                &follower,
                                &signature,
                                &attestation,
                                &attestation.store,
                            ).await
                        }.await;
                        match result {
                            Ok((height, signer, signatures)) => info!(target: "zone::p2p", %follower, %signer, height, signatures, "Stored follower settlement signature"),
                            Err(err) => tracing::warn!(target: "zone::p2p", %follower, %err, "Rejected follower settlement signature"),
                        }
                    }
                    P2pEvent::BlockReceived { .. }
                    | P2pEvent::TransactionReceived { .. }
                    | P2pEvent::SettlementProposalReceived { .. } => {
                        // A leader never follows peer chain heads. Keeping this explicit ensures
                        // ZoneEngine remains the sole writer of the leader's canonical head.
                        debug!(target: "zone::p2p", "Ignoring peer block sync event on serve-only leader");
                    }
                }
            }
        }
    }
}

/// Import live/backfilled blocks in canonical order on a follower.
///
/// Live blocks are only imported from the leader when the sender equals
/// `schedule.leader_for(the block's embedded anchor)`. We do this because if there are accidentally
/// two leaders (split brain) for a block, we need to decide to import the correct one.
///
/// Backfilled blocks carry no producer claim and are judged by
/// parent/anchor/execution/conflict validation alone. The loop exits when `stop` fires.
pub(crate) async fn run_follower_block_sync<P>(
    provider: P,
    engine: ConsensusEngineHandle<ZonePayloadTypes>,
    mut events: mpsc::Receiver<P2pEvent>,
    commands: mpsc::Sender<P2pCommand>,
    backfill_commands: mpsc::Sender<BackfillCommand>,
    mut backfill_responses: mpsc::Receiver<BackfillResponse>,
    l1_block_tracker: L1BlockTracker,
    deposit_queue: DepositQueue,
    attestation: AttestationContext,
    schedule: LeadershipSchedule,
    peer_tips: PeerTipRegistry,
    stop: sync::CancellationToken,
) where
    P: BlockNumReader
        + BlockReader<Block = Block>
        + HeaderProvider<Header = TempoHeader>
        + StateProviderFactory
        + ReceiptProvider
        + Clone
        + Send
        + Sync
        + 'static,
{
    // Keep track of all live blocks if we end up needing a backfill, so we can immediately catchup
    // This is capped to `MAX_PENDING_BLOCKS`.
    let mut pending = BTreeMap::<u64, PendingPeerBlock>::new();
    let mut backfill = BackfillProgress::new();

    // Always probe on startup to see if we're behind
    let mut retry = tokio::time::interval(BACKFILL_RETRY_INTERVAL);
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // If we don't get blocks after 30s, probe to get the next block
    let inactivity = tokio::time::sleep(BLOCK_INACTIVITY_TIMEOUT);
    tokio::pin!(inactivity);

    loop {
        tokio::select! {
        biased;
            () = stop.cancelled() => {
                debug!(target: "zone::p2p", "Follower block sync stopped");
                return;
            }
            response = backfill_responses.recv() => {
                let Some(response) = response else {
                    debug!(target: "zone::p2p", "Backfill response channel closed");
                    return;
                };
                match response {
                    BackfillResponse::Block { block, .. } => {
                        let number = match encoded_block_number(&block) {
                            Ok(number) => number,
                            Err(err) => {
                                tracing::error!(target: "zone::p2p", %err, "Rejected malformed peer block");
                                continue;
                            }
                        };
                        inactivity
                            .as_mut()
                            .reset(tokio::time::Instant::now() + BLOCK_INACTIVITY_TIMEOUT);
                        if !process_follower_block(
                            &provider,
                            &engine,
                            &l1_block_tracker,
                            &deposit_queue,
                            &schedule,
                            &mut pending,
                            &mut backfill,
                            number,
                            block,
                            None,
                            &stop,
                        )
                        .await
                        {
                            return;
                        }
                    }
                    BackfillResponse::Completed { peer, tip } => {
                        peer_tips.record(peer.clone(), tip);
                        let best = match provider.best_block_number() {
                            Ok(best) => best,
                            Err(err) => {
                                tracing::error!(target: "zone::p2p", %err, "Failed reading local head after backfill response");
                                continue;
                            }
                        };
                        backfill.complete(
                            tip.zone_height,
                            best,
                            pending.first_key_value().map(|(&number, _)| number),
                        );
                        debug!(target: "zone::p2p", %peer, best, tip_height = tip.zone_height, tip_hash = ?tip.zone_hash, backfill_needed = backfill.needed, "Completed block backfill response page");
                    }
                }
            }
            event = events.recv() => {
                let Some(event) = event else {
                    debug!(target: "zone::p2p", "P2P event channel closed");
                    return;
                };
                match event {
                    P2pEvent::Started { .. } => {}
                    P2pEvent::SettlementProposalReceived { leader, proposal } => {
                        let result = async {
                            let proposal = SettlementAttestation::decode(&proposal)?;
                            let height: u64 = proposal
                                .zoneHeight
                                .try_into()
                                .wrap_err("settlement height does not fit in u64")?;
                            let persisted_head = provider
                                .last_block_number()
                                .wrap_err("failed reading persisted zone head")?;
                            eyre::ensure!(
                                height <= persisted_head,
                                "settlement proposal at height {height} is not durable; persisted head is {persisted_head}"
                            );
                            let expected = build_settlement_attestation(
                                &provider,
                                height,
                                &attestation,
                                (proposal.anchorBlockNumber, proposal.anchorBlockHash),
                            ).await?.ok_or_eyre("proposed block is not a batch boundary")?;
                            eyre::ensure!(proposal == expected, "settlement proposal does not match follower state");

                            // Unreachable on an rpc-only member: the P2P layer never routes a
                            // proposal to one. Fails closed rather than panicking if it ever does.
                            let signer = attestation.signer.as_ref().ok_or_eyre(
                                "this node holds no individual secp256k1 key, so it cannot sign a settlement attestation",
                            )?;
                            let signed = SignedSettlementAttestation::sign(
                                proposal,
                                attestation.domain,
                                signer,
                            )?;

                            // Return the signed settlement attestation to the peer that
                            // proposed it. During a scheduled handoff that is the outgoing
                            // leader, not the most recently observed one.
                            commands.send(P2pCommand::SendSettlementSignature {
                                leader: leader.clone(),
                                signature: signed.encode(),
                            })
                                .await
                                .wrap_err("P2P command channel closed")?;
                            Ok::<_, eyre::Report>(height)
                        }.await;
                        match result {
                            Ok(height) => info!(target: "zone::p2p", %leader, height, "Signed settlement proposal"),
                            Err(err) => tracing::warn!(target: "zone::p2p", %leader, %err, "Rejected settlement proposal"),
                        }
                    }
                    P2pEvent::BlockReceived { .. } => {
                        let (block, live_sender) = match event {
                            P2pEvent::BlockReceived { leader_ed25519_public_key, block } => {
                                (block, Some(leader_ed25519_public_key))
                            }
                            _ => unreachable!("outer match arm restricts the event kind"),
                        };
                        let number = match encoded_block_number(&block) {
                            Ok(number) => number,
                            Err(err) => {
                                tracing::error!(target: "zone::p2p", %err, "Rejected malformed peer block");
                                continue;
                            }
                        };
                        inactivity
                            .as_mut()
                            .reset(tokio::time::Instant::now() + BLOCK_INACTIVITY_TIMEOUT);
                        if !process_follower_block(
                            &provider,
                            &engine,
                            &l1_block_tracker,
                            &deposit_queue,
                            &schedule,
                            &mut pending,
                            &mut backfill,
                            number,
                            block,
                            live_sender,
                            &stop,
                        )
                        .await
                        {
                            return;
                        }
                    }
                    P2pEvent::TransactionReceived { .. } => {
                        debug!(target: "zone::p2p", "Ignoring unexpected transaction event in follower block sync");
                    }
                    P2pEvent::SettlementSignatureReceived { .. } => {
                        debug!(target: "zone::p2p", "Ignoring leader-only attestation event on follower");
                    }
                }
            }
            _ = retry.tick(), if backfill.needed => {
                let best = match provider.best_block_number() {
                    Ok(best) => best,
                    Err(err) => {
                        tracing::error!(target: "zone::p2p", %err, "Failed reading local head for backfill request");
                        continue;
                    }
                };

                // retry the backfill
                if let Some(command) = backfill.request(best)
                    && backfill_commands.send(command).await.is_err()
                {
                    debug!(target: "zone::p2p", "P2P command channel closed");
                    return;
                }
            }
            _ = &mut inactivity, if !backfill.needed => {
                // Reset before reading the provider so a transient provider error cannot leave an
                // elapsed sleep continuously ready and spin this loop.
                inactivity
                    .as_mut()
                    .reset(tokio::time::Instant::now() + BLOCK_INACTIVITY_TIMEOUT);
                let best = match provider.best_block_number() {
                    Ok(best) => best,
                    Err(err) => {
                        tracing::error!(target: "zone::p2p", %err, "Failed reading local head for inactivity backfill probe");
                        continue;
                    }
                };
                let command = backfill.probe_after_inactivity(best);
                info!(target: "zone::p2p", best, "No peer block received recently; probing for backfill");
                if backfill_commands.send(command).await.is_err() {
                    debug!(target: "zone::p2p", "P2P command channel closed");
                    return;
                }
            }
        }
    }
}

async fn process_follower_block<P>(
    provider: &P,
    engine: &ConsensusEngineHandle<ZonePayloadTypes>,
    l1_block_tracker: &L1BlockTracker,
    deposit_queue: &DepositQueue,
    schedule: &LeadershipSchedule,
    pending: &mut BTreeMap<u64, PendingPeerBlock>,
    backfill: &mut BackfillProgress,
    number: u64,
    block: Vec<u8>,
    live_sender: Option<P2pPeerId>,
    stop: &sync::CancellationToken,
) -> bool
where
    P: BlockNumReader
        + BlockReader<Block = Block>
        + HeaderProvider<Header = TempoHeader>
        + StateProviderFactory
        + ReceiptProvider
        + Clone
        + Send
        + Sync
        + 'static,
{
    let best = match provider.best_block_number() {
        Ok(best) => best,
        Err(err) => {
            tracing::error!(target: "zone::p2p", %err, "Failed reading local head");
            return true;
        }
    };
    let peer_block = PendingPeerBlock {
        encoded: block,
        live_sender,
    };
    if number <= best {
        match import_peer_block(
            provider,
            engine,
            l1_block_tracker,
            deposit_queue,
            schedule,
            &peer_block,
            stop,
        )
        .await
        {
            Ok(PeerBlockImportOutcome::Cancelled) => return false,
            Ok(PeerBlockImportOutcome::TimedOut { .. }) => {
                tracing::warn!(target: "zone::p2p", "Dropping peer block whose L1 anchor was not observed before the import deadline");
            }
            Ok(PeerBlockImportOutcome::Imported) => {}
            Err(err) => {
                tracing::error!(target: "zone::p2p", %err, "Rejected duplicate or conflicting peer block");
            }
        }
        return true;
    }
    backfill.observe_block(number, best);
    if let Some(dropped) = buffer_pending_block(pending, number, peer_block) {
        tracing::warn!(target: "zone::p2p", dropped, pending_limit = MAX_PENDING_BLOCKS, "Dropped far-future peer block because the pending block buffer is full");
    }
    if number > best.saturating_add(1) {
        info!(target: "zone::p2p", local_head = best, received = number, "Detected zone block gap; requesting backfill");
    }
    match drain_pending_blocks(
        provider,
        engine,
        l1_block_tracker,
        deposit_queue,
        schedule,
        pending,
        stop,
    )
    .await
    {
        Ok(PeerBlockImportOutcome::Cancelled) => return false,
        Ok(PeerBlockImportOutcome::TimedOut {
            block_number,
            anchor,
        }) => {
            tracing::warn!(target: "zone::p2p", block_number, anchor_number = anchor.number, anchor_hash = ?anchor.hash, "Dropping peer block whose L1 anchor was not observed before the import deadline");
            backfill.needed = true;
        }
        Ok(PeerBlockImportOutcome::Imported) => {
            let best = match provider.best_block_number() {
                Ok(best) => best,
                Err(err) => {
                    tracing::error!(target: "zone::p2p", %err, "Failed reading local head after importing peer blocks");
                    return true;
                }
            };
            backfill
                .refresh_after_import(best, pending.first_key_value().map(|(&number, _)| number));
        }
        Err(err) => {
            tracing::error!(target: "zone::p2p", %err, "Rejected peer block while draining backfill");
            backfill.needed = true;
        }
    }
    true
}

async fn drain_pending_blocks<P>(
    provider: &P,
    engine: &ConsensusEngineHandle<ZonePayloadTypes>,
    l1_block_tracker: &L1BlockTracker,
    deposit_queue: &DepositQueue,
    schedule: &LeadershipSchedule,
    pending: &mut BTreeMap<u64, PendingPeerBlock>,
    stop: &sync::CancellationToken,
) -> eyre::Result<PeerBlockImportOutcome>
where
    P: reth_storage_api::BlockNumReader
        + reth_storage_api::HeaderProvider<Header = TempoHeader>
        + StateProviderFactory
        + Clone
        + Send
        + Sync
        + 'static,
{
    loop {
        let next = provider.best_block_number()?.saturating_add(1);
        let Some(block) = pending.remove(&next) else {
            return Ok(PeerBlockImportOutcome::Imported);
        };
        import_peer_block(
            provider,
            engine,
            l1_block_tracker,
            deposit_queue,
            schedule,
            &block,
            stop,
        )
        .await?;
    }
}

async fn import_peer_block<P>(
    provider: &P,
    engine: &ConsensusEngineHandle<ZonePayloadTypes>,
    l1_block_tracker: &L1BlockTracker,
    deposit_queue: &DepositQueue,
    schedule: &LeadershipSchedule,
    peer_block: &PendingPeerBlock,
    stop: &sync::CancellationToken,
) -> eyre::Result<PeerBlockImportOutcome>
where
    P: BlockNumReader
        + HeaderProvider<Header = TempoHeader>
        + StateProviderFactory
        + Clone
        + Send
        + Sync
        + 'static,
{
    // Check the received block
    let mut input = peer_block.encoded.as_slice();
    let block = Block::decode(&mut input)
        .map_err(|err| eyre::eyre!("invalid RLP-encoded zone block: {err}"))?;
    if !input.is_empty() {
        eyre::bail!("encoded zone block has {} trailing bytes", input.len());
    }

    let block = SealedBlock::seal_slow(block);
    let block_number = block.number();
    let hash = block.hash();
    let best_block = provider.best_block_number()?;

    // 1. Block number is correct
    if block_number <= best_block {
        let existing = provider.sealed_header(block_number)?.ok_or_else(|| {
            eyre::eyre!("missing local canonical header at height {block_number}")
        })?;
        if existing.hash() == hash {
            debug!(target: "zone::p2p", block_number, ?hash, "Ignoring duplicate peer block");
            return Ok(PeerBlockImportOutcome::Imported);
        }
        eyre::bail!(
            "peer block conflicts with canonical block at height {block_number}: local={}, received={hash}",
            existing.hash()
        );
    }

    let expected_number = best_block.saturating_add(1);
    if block_number != expected_number {
        eyre::bail!(
            "peer block gap: local head is {best_block}, received height {block_number}, expected {expected_number}"
        );
    }

    // 2. Block's parent hash is correct
    let parent = provider
        .sealed_header(best_block)?
        .ok_or_else(|| eyre::eyre!("missing local canonical head at height {best_block}"))?;
    if block.parent_hash() != parent.hash() {
        eyre::bail!(
            "peer block parent mismatch at height {block_number}: local={}, received={}",
            parent.hash(),
            block.parent_hash()
        );
    }

    // 3. Require the block to advance the local Tempo checkpoint by exactly
    // one independently observed L1 block.
    let (l1_header, portal_inputs) = decode_advance_tempo(&block)?;
    let local = provider
        .state_by_block_hash(parent.hash())?
        .tempo_num_hash()?;
    validate_l1_checkpoint_transition(&l1_header, local.number, local.hash, block_number)?;
    let anchor = l1_header.num_hash();

    // Anchor-aware fence for live blocks: the sender must
    // be the scheduled leader of the block's anchor. This stops an honest stale
    // leader's broadcast from splitting followers.
    // Backfilled blocks carry no producer claim and are judged by parent/anchor/execution/conflict
    // validation only.
    //
    // Check once before waiting to reject an already-known invalid sender without blocking the
    // import loop. `wait_for_validated_peer_anchor` checks again after observing the anchor:
    // the anchor itself may finalize a leadership transition that changes its assigned producer.
    validate_live_block_sender(
        schedule,
        peer_block.live_sender.as_ref(),
        anchor.number,
        block_number,
    )?;
    let observed = match wait_for_validated_peer_anchor(
        l1_block_tracker,
        schedule,
        &portal_inputs,
        peer_block.live_sender.as_ref(),
        anchor,
        block_number,
        stop,
        PEER_ANCHOR_WAIT_TIMEOUT,
    )
    .await
    {
        Ok(observed) => observed,
        Err(PeerAnchorWaitError::Cancelled) => return Ok(PeerBlockImportOutcome::Cancelled),
        Err(PeerAnchorWaitError::TimedOut {
            block_number,
            anchor,
        }) => {
            return Ok(PeerBlockImportOutcome::TimedOut {
                block_number,
                anchor,
            });
        }
        Err(PeerAnchorWaitError::Other(error)) => return Err(error),
    };

    // The subscriber normally enqueues immediately after recording this observation. Enqueueing
    // here as well closes that small scheduling window and makes follower import self-contained;
    // the queue treats the subscriber's later enqueue as a duplicate. This is peer-driven, so a
    // gap must surface as a rejected block rather than aborting the node.
    deposit_queue
        .try_enqueue_sealed(l1_header, observed)
        .wrap_err_with(|| format!("cannot queue the anchor of block {block_number}"))?;

    // 4. All txns in the block execute properly
    let payload = ZonePayloadTypes::block_to_payload(block, None);
    let status = engine.new_payload(payload).await?;
    if !status.is_valid() {
        eyre::bail!("execution engine rejected peer block {block_number} ({hash}): {status:?}");
    }

    // 5. Forkchoice
    let forkchoice = ForkchoiceState::same_hash(hash);
    let result = engine.fork_choice_updated(forkchoice, None).await?;
    if !result.is_valid() {
        eyre::bail!(
            "execution engine rejected forkchoice for block {block_number} ({hash}): {result:?}"
        );
    }

    // Mirror the leader engine only after the block is canonical locally. The block cannot be
    // un-imported at this point, so the observation must be released unconditionally — leaving it
    // behind would stall the subscriber once the lookahead window fills. Advancing the queue is
    // likewise tolerant of drift; it fails only on a genuine hash conflict.
    l1_block_tracker.prune_through(anchor.number);
    deposit_queue
        .confirm_through(anchor)
        .wrap_err_with(|| format!("cannot advance the deposit queue past block {block_number}"))?;
    schedule.record_applied_anchor(anchor.number);

    info!(target: "zone::p2p", block_number, ?hash, "Imported canonical leader block");
    Ok(PeerBlockImportOutcome::Imported)
}

fn validate_live_block_sender(
    schedule: &LeadershipSchedule,
    live_sender: Option<&P2pPeerId>,
    anchor_number: u64,
    block_number: u64,
) -> eyre::Result<()> {
    let Some(sender) = live_sender else {
        return Ok(());
    };
    match schedule.leader_for(anchor_number) {
        Some(record) if &record.leader == sender => Ok(()),
        Some(record) => eyre::bail!(
            "live block {block_number} for anchor {anchor_number} was broadcast by {sender}, but \
             the schedule assigns that anchor to {} (epoch {})",
            record.leader,
            record.epoch,
        ),
        None => eyre::bail!(
            "live block {block_number} embeds anchor {anchor_number} which no retained leadership \
             record governs",
        ),
    }
}

async fn wait_for_validated_peer_anchor(
    l1_block_tracker: &L1BlockTracker,
    schedule: &LeadershipSchedule,
    portal_inputs: &AdvanceTempoPortalInputs,
    live_sender: Option<&P2pPeerId>,
    anchor: NumHash,
    block_number: u64,
    stop: &sync::CancellationToken,
    wait_timeout: Duration,
) -> Result<L1PortalEvents, PeerAnchorWaitError> {
    let observed = tokio::select! {
        biased;
        () = stop.cancelled() => return Err(PeerAnchorWaitError::Cancelled),
        observed = tokio::time::timeout(
            wait_timeout,
            l1_block_tracker.wait_for_portal_events(anchor),
        ) => match observed {
            Ok(observed) => observed.map_err(PeerAnchorWaitError::Other)?,
            Err(_) => return Err(PeerAnchorWaitError::TimedOut {
                block_number,
                anchor,
            }),
        },
    };
    portal_inputs
        .validate(&observed)
        .map_err(PeerAnchorWaitError::Other)?;

    // The L1 subscriber publishes any transition finalized by this anchor before recording the
    // anchor in the tracker. Re-read the schedule now so the pre-wait decision cannot authorize a
    // sender that this anchor demoted.
    validate_live_block_sender(schedule, live_sender, anchor.number, block_number)
        .map_err(PeerAnchorWaitError::Other)?;
    Ok(observed)
}

#[derive(Debug)]
enum PeerBlockImportOutcome {
    Imported,
    Cancelled,
    TimedOut { block_number: u64, anchor: NumHash },
}

#[derive(Debug)]
enum PeerAnchorWaitError {
    Cancelled,
    TimedOut { block_number: u64, anchor: NumHash },
    Other(eyre::Report),
}

struct AdvanceTempoPortalInputs {
    deposits: Vec<zone_payload::abi::QueuedDeposit>,
    enabled_tokens: Vec<zone_payload::abi::EnabledToken>,
}

impl AdvanceTempoPortalInputs {
    fn validate(&self, observed: &L1PortalEvents) -> eyre::Result<()> {
        observed.validate_advance_tempo_inputs(&self.deposits, &self.enabled_tokens)
    }
}

fn validate_l1_checkpoint_transition(
    l1_header: &SealedHeader<TempoHeader>,
    local_number: u64,
    local_hash: B256,
    zone_block_number: u64,
) -> eyre::Result<()> {
    if l1_header.number() != local_number.saturating_add(1) {
        eyre::bail!(
            "peer block {zone_block_number} advances Tempo to L1 block {}, but local checkpoint is {}; expected {}",
            l1_header.number(),
            local_number,
            local_number.saturating_add(1)
        );
    }
    if l1_header.parent_hash() != local_hash {
        eyre::bail!(
            "advanceTempo L1 header {} does not extend the local Tempo checkpoint: embedded parent {}, local hash {}",
            l1_header.number(),
            l1_header.parent_hash(),
            local_hash
        );
    }
    Ok(())
}

/// Decode the L1 header embedded in the first `IZoneInbox.advanceTempo` system transaction.
#[cfg(test)]
fn decode_advance_tempo_header(
    block: &SealedBlock<Block>,
) -> eyre::Result<SealedHeader<TempoHeader>> {
    decode_advance_tempo(block).map(|(header, _)| header)
}

fn decode_advance_tempo(
    block: &SealedBlock<Block>,
) -> eyre::Result<(SealedHeader<TempoHeader>, AdvanceTempoPortalInputs)> {
    // Do some basic checks

    // 1. `advanceTempo` is the first tx
    let first_tx = block.body().transactions().next().ok_or_else(|| {
        eyre::eyre!("peer block has no transactions; expected an advanceTempo system tx")
    })?;
    let TempoTxEnvelope::Legacy(signed) = first_tx else {
        eyre::bail!("first transaction in peer block is not a legacy system transaction")
    };
    if !first_tx.is_system_tx() {
        eyre::bail!("first transaction in peer block is not a Tempo system transaction")
    }

    // 2. Address is correct
    if signed.tx().to != ZONE_INBOX_ADDRESS.into() {
        eyre::bail!("first Tempo system transaction is not sent to IZoneInbox")
    }
    let call = IZoneInbox::advanceTempoCall::abi_decode(signed.tx().input.as_ref())
        .map_err(|err| eyre::eyre!("first transaction does not decode as advanceTempo: {err}"))?;

    // 3. the system tx is valid.
    let mut header_rlp = call.header.as_ref();
    let header = TempoHeader::decode(&mut header_rlp)
        .map_err(|err| eyre::eyre!("invalid RLP-encoded L1 header in advanceTempo: {err}"))?;
    if !header_rlp.is_empty() {
        eyre::bail!(
            "advanceTempo L1 header has {} trailing bytes",
            header_rlp.len()
        )
    }
    Ok((
        SealedHeader::seal_slow(header),
        AdvanceTempoPortalInputs {
            deposits: call.deposits,
            enabled_tokens: call.enabledTokens,
        },
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicU64, AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use alloy_eips::NumHash;
    use futures::{StreamExt as _, stream};
    use tokio::sync::{oneshot, watch};
    use tokio_util::sync;

    use super::{
        AdvanceTempoPortalInputs, BackfillProgress, BroadcasterShutdown, EncodedPersistedBlock,
        MAX_PENDING_BLOCKS, PEER_ANCHOR_WAIT_TIMEOUT, PersistedBlockSource, PersistedTip,
        broadcast_persisted_blocks, buffer_pending_block, validate_live_block_sender,
        wait_for_validated_peer_anchor,
    };
    use alloy_primitives::B256;
    use zone_l1::{L1BlockTracker, L1PortalEvents};
    use zone_p2p::{BackfillCommand, LeadershipSchedule, LeadershipState, P2pCommand};

    #[derive(Clone)]
    struct StartupRaceSource {
        reads: Arc<AtomicUsize>,
        tip: PersistedTip,
    }

    impl PersistedBlockSource for StartupRaceSource {
        fn last_block_number(&self) -> eyre::Result<u64> {
            let read = self.reads.fetch_add(1, Ordering::SeqCst);
            Ok(if read == 0 {
                self.tip.number - 1
            } else {
                self.tip.number
            })
        }

        fn canonical_block_number(&self) -> eyre::Result<u64> {
            Ok(self.tip.number)
        }

        fn persisted_block_stream(&self) -> futures::stream::BoxStream<'static, PersistedTip> {
            stream::iter([self.tip]).boxed()
        }

        fn encoded_block_by_number(&self, number: u64) -> eyre::Result<EncodedPersistedBlock> {
            assert_eq!(number, self.tip.number);
            Ok(EncodedPersistedBlock {
                number,
                hash: self.tip.hash,
                encoded: vec![number as u8],
            })
        }
    }

    /// A provider whose canonical head sits above its persisted head, which is the window the
    /// stop flush and the demotion drain both have to get right.
    #[derive(Clone)]
    struct DivergentHeadSource {
        canonical: u64,
        persisted: Arc<AtomicU64>,
        /// The persisted head observed while the broadcaster subscribes. Subsequent reads see
        /// `persisted`, allowing tests to model a block that becomes durable during shutdown.
        startup_persisted: u64,
        startup_reads: Arc<AtomicUsize>,
        updates: watch::Receiver<PersistedTip>,
    }

    impl PersistedBlockSource for DivergentHeadSource {
        fn last_block_number(&self) -> eyre::Result<u64> {
            let read = self.startup_reads.fetch_add(1, Ordering::SeqCst);
            Ok(if read < 2 {
                self.startup_persisted
            } else {
                self.persisted.load(Ordering::SeqCst)
            })
        }

        fn canonical_block_number(&self) -> eyre::Result<u64> {
            Ok(self.canonical)
        }

        fn persisted_block_stream(&self) -> futures::stream::BoxStream<'static, PersistedTip> {
            stream::unfold(self.updates.clone(), |mut updates| async move {
                updates.changed().await.ok()?;
                let tip = *updates.borrow_and_update();
                Some((tip, updates))
            })
            .boxed()
        }

        fn encoded_block_by_number(&self, number: u64) -> eyre::Result<EncodedPersistedBlock> {
            assert!(
                number <= self.persisted.load(Ordering::SeqCst),
                "encoded a block that is not durable yet: {number}"
            );
            Ok(EncodedPersistedBlock {
                number,
                hash: B256::repeat_byte(number as u8),
                encoded: vec![number as u8],
            })
        }
    }

    #[test]
    fn decodes_advance_tempo_header_from_first_system_tx() {
        use alloy_consensus::BlockHeader as _;
        use reth_primitives_traits::{SealedBlock, SealedHeader};
        use tempo_primitives::{Block, TempoHeader};

        let l1_header = TempoHeader {
            inner: alloy_consensus::Header {
                number: 7,
                parent_hash: B256::repeat_byte(0x42),
                ..Default::default()
            },
            ..Default::default()
        };
        let prepared = zone_l1::PreparedL1Block {
            header: SealedHeader::seal_slow(l1_header),
            queued_deposits: vec![],
            decryptions: vec![],
            enabled_tokens: vec![],
            follows_checkpoint_blocks: false,
        };
        let tx = zone_payload::build_advance_tempo_tx(&prepared, 1337);
        let block = SealedBlock::seal_slow(Block {
            header: TempoHeader::default(),
            body: alloy_consensus::BlockBody {
                transactions: vec![tx.into_inner()],
                ommers: vec![],
                withdrawals: None,
            },
        });

        let decoded = super::decode_advance_tempo_header(&block).unwrap();
        assert_eq!(decoded.number(), 7);
        assert_eq!(decoded.parent_hash(), B256::repeat_byte(0x42));
        assert_eq!(decoded.hash(), prepared.header.hash());
    }

    #[test]
    fn rejects_advance_tempo_sent_to_wrong_contract() {
        use alloy_consensus::{Signed, TxLegacy};
        use alloy_primitives::{Address, Bytes, U256};
        use alloy_rlp::Encodable as _;
        use alloy_sol_types::SolCall as _;
        use reth_primitives_traits::SealedBlock;
        use tempo_primitives::{
            Block, TempoHeader, TempoTxEnvelope, transaction::envelope::TEMPO_SYSTEM_TX_SIGNATURE,
        };

        let mut header_rlp = Vec::new();
        TempoHeader::default().encode(&mut header_rlp);
        let calldata = zone_payload::abi::IZoneInbox::advanceTempoCall {
            header: Bytes::from(header_rlp),
            deposits: vec![],
            decryptions: vec![],
            enabledTokens: vec![],
        }
        .abi_encode();
        let tx = TxLegacy {
            chain_id: None,
            nonce: 0,
            gas_price: 0,
            gas_limit: 100_000,
            to: Address::repeat_byte(0x99).into(),
            value: U256::ZERO,
            input: calldata.into(),
        };
        let block = SealedBlock::seal_slow(Block {
            header: TempoHeader::default(),
            body: alloy_consensus::BlockBody {
                transactions: vec![TempoTxEnvelope::Legacy(Signed::new_unhashed(
                    tx,
                    TEMPO_SYSTEM_TX_SIGNATURE,
                ))],
                ommers: vec![],
                withdrawals: None,
            },
        });

        let error = super::decode_advance_tempo_header(&block).unwrap_err();
        assert!(error.to_string().contains("IZoneInbox"));
    }

    #[test]
    fn rejects_block_without_advance_tempo() {
        use reth_primitives_traits::SealedBlock;
        use tempo_primitives::{Block, TempoHeader};

        let block = SealedBlock::seal_slow(Block {
            header: TempoHeader::default(),
            body: alloy_consensus::BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: None,
            },
        });

        let error = super::decode_advance_tempo_header(&block).unwrap_err();
        assert!(error.to_string().contains("no transactions"));
    }

    #[test]
    fn rejects_non_system_advance_tempo_transaction() {
        use alloy_consensus::{Signed, TxLegacy};
        use alloy_primitives::Signature;
        use reth_primitives_traits::{SealedBlock, SealedHeader};
        use tempo_primitives::{Block, TempoHeader, TempoTxEnvelope};

        let prepared = zone_l1::PreparedL1Block {
            header: SealedHeader::seal_slow(TempoHeader::default()),
            queued_deposits: vec![],
            decryptions: vec![],
            enabled_tokens: vec![],
            follows_checkpoint_blocks: false,
        };
        let TempoTxEnvelope::Legacy(system_tx) =
            zone_payload::build_advance_tempo_tx(&prepared, 1337).into_inner()
        else {
            unreachable!("advanceTempo builder must produce a legacy transaction")
        };
        let block = SealedBlock::seal_slow(Block {
            header: TempoHeader::default(),
            body: alloy_consensus::BlockBody {
                transactions: vec![TempoTxEnvelope::Legacy(Signed::<TxLegacy>::new_unhashed(
                    system_tx.tx().clone(),
                    Signature::test_signature(),
                ))],
                ommers: vec![],
                withdrawals: None,
            },
        });

        let error = super::decode_advance_tempo_header(&block).unwrap_err();
        assert!(error.to_string().contains("not a Tempo system transaction"));
    }

    #[test]
    fn rejects_malformed_advance_tempo_calldata() {
        use alloy_consensus::{Signed, TxLegacy};
        use alloy_primitives::{Bytes, U256};
        use reth_primitives_traits::SealedBlock;
        use tempo_primitives::{
            Block, TempoHeader, TempoTxEnvelope, transaction::envelope::TEMPO_SYSTEM_TX_SIGNATURE,
        };

        let tx = TxLegacy {
            to: zone_payload::abi::ZONE_INBOX_ADDRESS.into(),
            value: U256::ZERO,
            input: Bytes::from_static(b"not advanceTempo calldata"),
            ..Default::default()
        };
        let block = SealedBlock::seal_slow(Block {
            header: TempoHeader::default(),
            body: alloy_consensus::BlockBody {
                transactions: vec![TempoTxEnvelope::Legacy(Signed::new_unhashed(
                    tx,
                    TEMPO_SYSTEM_TX_SIGNATURE,
                ))],
                ommers: vec![],
                withdrawals: None,
            },
        });

        let error = super::decode_advance_tempo_header(&block).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not decode as advanceTempo")
        );
    }

    #[test]
    fn rejects_malformed_or_trailing_advance_tempo_header_rlp() {
        use alloy_consensus::{Signed, TxLegacy};
        use alloy_primitives::{Bytes, U256};
        use alloy_sol_types::SolCall as _;
        use reth_primitives_traits::SealedBlock;
        use tempo_primitives::{
            Block, TempoHeader, TempoTxEnvelope, transaction::envelope::TEMPO_SYSTEM_TX_SIGNATURE,
        };

        let make_block = |header: Vec<u8>| {
            let calldata = zone_payload::abi::IZoneInbox::advanceTempoCall {
                header: Bytes::from(header),
                deposits: vec![],
                decryptions: vec![],
                enabledTokens: vec![],
            }
            .abi_encode();
            let tx = TxLegacy {
                to: zone_payload::abi::ZONE_INBOX_ADDRESS.into(),
                value: U256::ZERO,
                input: calldata.into(),
                ..Default::default()
            };
            SealedBlock::seal_slow(Block {
                header: TempoHeader::default(),
                body: alloy_consensus::BlockBody {
                    transactions: vec![TempoTxEnvelope::Legacy(Signed::new_unhashed(
                        tx,
                        TEMPO_SYSTEM_TX_SIGNATURE,
                    ))],
                    ommers: vec![],
                    withdrawals: None,
                },
            })
        };

        let malformed = make_block(vec![0xff]);
        let error = super::decode_advance_tempo_header(&malformed).unwrap_err();
        assert!(error.to_string().contains("invalid RLP-encoded L1 header"));

        let mut trailing = alloy_rlp::encode(TempoHeader::default());
        trailing.push(0x00);
        let trailing = make_block(trailing);
        let error = super::decode_advance_tempo_header(&trailing).unwrap_err();
        assert!(error.to_string().contains("trailing bytes"));
    }

    #[test]
    fn validates_embedded_l1_checkpoint_continuity() {
        use reth_primitives_traits::SealedHeader;
        use tempo_primitives::TempoHeader;

        let local_hash = B256::repeat_byte(0x42);
        let header = SealedHeader::seal_slow(TempoHeader {
            inner: alloy_consensus::Header {
                number: 11,
                parent_hash: local_hash,
                ..Default::default()
            },
            ..Default::default()
        });
        super::validate_l1_checkpoint_transition(&header, 10, local_hash, 7).unwrap();

        let skipped =
            super::validate_l1_checkpoint_transition(&header, 9, local_hash, 7).unwrap_err();
        assert!(skipped.to_string().contains("expected 10"));

        let wrong_parent =
            super::validate_l1_checkpoint_transition(&header, 10, B256::repeat_byte(0x99), 7)
                .unwrap_err();
        assert!(wrong_parent.to_string().contains("does not extend"));
    }

    #[tokio::test]
    async fn revalidates_live_sender_after_anchor_observation() {
        use commonware_cryptography::{Signer as _, ed25519::PrivateKey};

        const ANCHOR_NUMBER: u64 = 10;
        const ZONE_BLOCK_NUMBER: u64 = 7;

        let outgoing = PrivateKey::from_seed(1).public_key();
        let incoming = PrivateKey::from_seed(2).public_key();
        let schedule = LeadershipSchedule::seeded(LeadershipState::new(1, outgoing.clone(), 0));
        let tracker = L1BlockTracker::default();
        let anchor = NumHash::new(ANCHOR_NUMBER, B256::repeat_byte(0x10));

        // The sender is valid under the pre-observation schedule.
        validate_live_block_sender(&schedule, Some(&outgoing), ANCHOR_NUMBER, ZONE_BLOCK_NUMBER)
            .unwrap();

        let waiter = {
            let schedule = schedule.clone();
            let tracker = tracker.clone();
            let outgoing = outgoing.clone();
            tokio::spawn(async move {
                let portal_inputs = AdvanceTempoPortalInputs {
                    deposits: vec![],
                    enabled_tokens: vec![],
                };
                wait_for_validated_peer_anchor(
                    &tracker,
                    &schedule,
                    &portal_inputs,
                    Some(&outgoing),
                    anchor,
                    ZONE_BLOCK_NUMBER,
                    &sync::CancellationToken::new(),
                    PEER_ANCHOR_WAIT_TIMEOUT,
                )
                .await
            })
        };

        // Match subscriber ordering: publish the transition finalized by H before H becomes
        // observable to follower import.
        schedule
            .publish(LeadershipState::new(2, incoming.clone(), ANCHOR_NUMBER))
            .unwrap();
        tracker
            .record_with_portal_events(anchor, L1PortalEvents::default())
            .unwrap();

        let error = waiter
            .await
            .expect("anchor waiter must not panic")
            .expect_err("the post-observation sender check must reject the outgoing leader");
        let super::PeerAnchorWaitError::Other(error) = error else {
            panic!("sender validation must fail as an operational error");
        };
        let message = error.to_string();
        assert!(message.contains(&outgoing.to_string()));
        assert!(message.contains(&incoming.to_string()));
        assert!(message.contains("schedule assigns that anchor"));
    }

    #[tokio::test]
    async fn peer_anchor_wait_stops_promptly_on_generation_cancellation() {
        use commonware_cryptography::{Signer as _, ed25519::PrivateKey};

        let leader = PrivateKey::from_seed(1).public_key();
        let schedule = LeadershipSchedule::seeded(LeadershipState::new(1, leader, 0));
        let tracker = L1BlockTracker::default();
        let stop = sync::CancellationToken::new();
        stop.cancel();

        let error = wait_for_validated_peer_anchor(
            &tracker,
            &schedule,
            &AdvanceTempoPortalInputs {
                deposits: vec![],
                enabled_tokens: vec![],
            },
            None,
            NumHash::new(10, B256::repeat_byte(0x10)),
            7,
            &stop,
            PEER_ANCHOR_WAIT_TIMEOUT,
        )
        .await
        .expect_err("cancelled generation must stop waiting for its peer anchor");

        assert!(matches!(error, super::PeerAnchorWaitError::Cancelled));
    }

    #[tokio::test]
    async fn peer_anchor_wait_has_one_finite_deadline() {
        use commonware_cryptography::{Signer as _, ed25519::PrivateKey};

        let leader = PrivateKey::from_seed(1).public_key();
        let schedule = LeadershipSchedule::seeded(LeadershipState::new(1, leader, 0));
        let tracker = L1BlockTracker::default();

        let error = wait_for_validated_peer_anchor(
            &tracker,
            &schedule,
            &AdvanceTempoPortalInputs {
                deposits: vec![],
                enabled_tokens: vec![],
            },
            None,
            NumHash::new(10, B256::repeat_byte(0x10)),
            7,
            &sync::CancellationToken::new(),
            Duration::from_millis(1),
        )
        .await
        .expect_err("missing peer anchor must reach its import deadline");

        assert!(matches!(error, super::PeerAnchorWaitError::TimedOut { .. }));
    }

    #[test]
    fn forced_recovery_reassigns_live_sender_for_missing_anchors() {
        use commonware_cryptography::{Signer as _, ed25519::PrivateKey};

        let outgoing = PrivateKey::from_seed(1).public_key();
        let incoming = PrivateKey::from_seed(2).public_key();
        let schedule = LeadershipSchedule::seeded(LeadershipState::new(7, outgoing.clone(), 0));
        schedule
            .install_forced_recovery(8, incoming.clone(), B256::repeat_byte(0x11), 51)
            .unwrap();
        schedule
            .publish(LeadershipState::new(8, incoming.clone(), 60))
            .unwrap();

        validate_live_block_sender(&schedule, Some(&incoming), 51, 11).unwrap();
        let error = validate_live_block_sender(&schedule, Some(&outgoing), 51, 11)
            .expect_err("the crashed leader must not remain authoritative in the recovery window");
        assert!(error.to_string().contains(&incoming.to_string()));
    }

    #[tokio::test]
    async fn broadcasts_block_persisted_during_startup_reconciliation_once() {
        let source = StartupRaceSource {
            reads: Arc::new(AtomicUsize::new(0)),
            tip: PersistedTip {
                number: 1,
                hash: B256::repeat_byte(0x11),
            },
        };
        let (commands, mut command_rx) = tokio::sync::mpsc::channel(4);
        let (_shutdown, shutdown_rx) = oneshot::channel();

        broadcast_persisted_blocks(source, commands, shutdown_rx).await;

        assert_eq!(
            command_rx.recv().await,
            Some(P2pCommand::BroadcastBlock(vec![1]))
        );
        assert_eq!(command_rx.recv().await, None);
    }

    #[tokio::test]
    async fn stop_flush_never_broadcasts_a_canonical_only_block() {
        // Exactly the reported window: block N is canonical but only N-1 is durable.
        let source = DivergentHeadSource {
            canonical: 2,
            persisted: Arc::new(AtomicU64::new(1)),
            startup_persisted: 0,
            startup_reads: Arc::new(AtomicUsize::new(0)),
            updates: watch::channel(PersistedTip {
                number: 1,
                hash: B256::repeat_byte(1),
            })
            .1,
        };
        let (commands, mut command_rx) = tokio::sync::mpsc::channel(4);
        let (shutdown, shutdown_rx) = oneshot::channel();
        shutdown
            .send(BroadcasterShutdown::Stop)
            .expect("the broadcaster must retain the shutdown receiver");

        broadcast_persisted_blocks(source, commands, shutdown_rx).await;

        // Block 1 is durable and must be flushed; block 2 exists only in memory and would be
        // lost on restart, stranding any follower that imported it.
        assert_eq!(
            command_rx.recv().await,
            Some(P2pCommand::BroadcastBlock(vec![1]))
        );
        assert_eq!(
            command_rx.recv().await,
            None,
            "the stop flush broadcast a canonical-but-unpersisted block"
        );
    }

    #[tokio::test]
    async fn drain_after_engine_stop_waits_for_the_canonical_tail_to_persist() {
        let persisted = Arc::new(AtomicU64::new(1));
        let (updates, update_rx) = watch::channel(PersistedTip {
            number: 1,
            hash: B256::repeat_byte(1),
        });
        let source = DivergentHeadSource {
            canonical: 2,
            persisted: persisted.clone(),
            startup_persisted: 0,
            startup_reads: Arc::new(AtomicUsize::new(0)),
            updates: update_rx,
        };
        let (commands, mut command_rx) = tokio::sync::mpsc::channel(4);
        let (shutdown, shutdown_rx) = oneshot::channel();
        let task = tokio::spawn(broadcast_persisted_blocks(source, commands, shutdown_rx));

        shutdown
            .send(BroadcasterShutdown::Drain)
            .expect("the broadcaster must retain the shutdown receiver");

        // The durable prefix goes out immediately.
        assert_eq!(
            command_rx.recv().await,
            Some(P2pCommand::BroadcastBlock(vec![1]))
        );
        // The canonical tail must not, until it persists.
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), command_rx.recv())
                .await
                .is_err(),
            "the drain broadcast the canonical tail before it was durable"
        );

        persisted.store(2, Ordering::SeqCst);
        updates
            .send(PersistedTip {
                number: 2,
                hash: B256::repeat_byte(2),
            })
            .expect("the broadcaster must retain the persisted-block stream");

        assert_eq!(
            command_rx.recv().await,
            Some(P2pCommand::BroadcastBlock(vec![2]))
        );
        task.await.expect("the broadcaster task must not panic");
        assert_eq!(command_rx.recv().await, None);
    }

    #[test]
    fn requests_backfill_again_when_live_block_reveals_gap_after_completion() {
        const LOCAL_HEAD: u64 = 10;

        let mut backfill = BackfillProgress::new();
        assert_eq!(
            backfill.request(LOCAL_HEAD),
            Some(BackfillCommand::Request {
                start: LOCAL_HEAD + 1,
            })
        );

        // The first response catches the follower up to the responder's snapshot tip.
        backfill.complete(LOCAL_HEAD, LOCAL_HEAD, None);
        assert_eq!(backfill.request(LOCAL_HEAD), None);

        // N+1's live broadcast is missed, then N+2 arrives and remains buffered behind the gap.
        let live_block = LOCAL_HEAD + 2;
        backfill.observe_block(live_block, LOCAL_HEAD);
        backfill.refresh_after_import(LOCAL_HEAD, Some(live_block));

        // The retry starts at the missing N+1 rather than skipping to the received N+2.
        assert_eq!(
            backfill.request(LOCAL_HEAD),
            Some(BackfillCommand::Request {
                start: LOCAL_HEAD + 1,
            })
        );
    }

    #[test]
    fn inactivity_probe_restarts_backfill_retries() {
        const LOCAL_HEAD: u64 = 10;

        let mut backfill = BackfillProgress::new();
        backfill.complete(LOCAL_HEAD, LOCAL_HEAD, None);
        assert_eq!(backfill.request(LOCAL_HEAD), None);

        assert_eq!(
            backfill.probe_after_inactivity(LOCAL_HEAD),
            BackfillCommand::Request {
                start: LOCAL_HEAD + 1,
            }
        );
        assert_eq!(
            backfill.request(LOCAL_HEAD),
            Some(BackfillCommand::Request {
                start: LOCAL_HEAD + 1,
            })
        );
    }

    #[test]
    fn peer_tip_registry_records_latest_tip() {
        use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
        use zone_p2p::PeerTip;

        let registry = super::PeerTipRegistry::default();

        let peer = PrivateKey::from_seed(1).public_key();
        let tip = PeerTip {
            zone_height: 7,
            zone_hash: B256::repeat_byte(0x07),
            tempo_block_number: 11,
            tempo_block_hash: B256::repeat_byte(0x11),
        };
        registry.record(peer.clone(), tip);

        // Re-advertising the same tip refreshes its observation timestamp.
        registry.record(peer.clone(), tip);
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].0, peer);
        assert_eq!(snapshot[0].1, tip);
    }

    fn pending_block(payload: u64) -> super::PendingPeerBlock {
        super::PendingPeerBlock {
            encoded: vec![payload as u8],
            live_sender: None,
        }
    }

    #[test]
    fn pending_block_limit_keeps_blocks_closest_to_local_head() {
        let mut pending = std::collections::BTreeMap::new();
        for number in 100..100 + MAX_PENDING_BLOCKS as u64 {
            assert_eq!(
                buffer_pending_block(&mut pending, number, pending_block(number)),
                None
            );
        }
        assert_eq!(pending.len(), MAX_PENDING_BLOCKS);

        let farthest = 100 + MAX_PENDING_BLOCKS as u64 - 1;
        assert_eq!(
            buffer_pending_block(&mut pending, 99, pending_block(99)),
            Some(farthest)
        );
        assert_eq!(pending.len(), MAX_PENDING_BLOCKS);
        assert!(pending.contains_key(&99));
        assert!(!pending.contains_key(&farthest));

        let farther = farthest + 1;
        assert_eq!(
            buffer_pending_block(&mut pending, farther, pending_block(farther)),
            Some(farther)
        );
        assert_eq!(pending.len(), MAX_PENDING_BLOCKS);
        assert!(!pending.contains_key(&farther));
    }
}
