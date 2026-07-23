//! Node-side leader block replication and follower import.

use alloy_consensus::BlockHeader as _;
use alloy_primitives::B256;
use alloy_provider::DynProvider;
use alloy_rlp::Decodable as _;
use alloy_rpc_types_engine::ForkchoiceState;
use futures::{StreamExt as _, stream::BoxStream};
use reth_chain_state::PersistedBlockSubscriptions;
use reth_node_api::{ConsensusEngineHandle, PayloadTypes as _};
use reth_primitives_traits::SealedBlock;
use reth_provider::HeaderProvider;
use reth_storage_api::{BlockNumReader, BlockReader, ReceiptProvider};
use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};
use tempo_alloy::TempoNetwork;
use tempo_primitives::{Block, TempoHeader};
use tokio::sync::mpsc;
use tracing::{debug, info};
use zone_p2p::{P2pCommand, P2pEvent, Role};
use zone_payload::ZonePayloadTypes;
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
    pub(crate) signer: PrivateKeySigner,
    pub(crate) addresses: HashMap<zone_p2p::P2pPeerId, alloy_primitives::Address>,
    pub(crate) store: Option<AttestationStore>,
    pub(crate) l1_provider: DynProvider<TempoNetwork>,
    pub(crate) anchor_config: BatchAnchorConfig,
}

impl AttestationContext {
    pub(crate) fn new(
        domain: AttestationDomain,
        signer: PrivateKeySigner,
        addresses: HashMap<zone_p2p::P2pPeerId, alloy_primitives::Address>,
        store: Option<AttestationStore>,
        l1_provider: DynProvider<TempoNetwork>,
        anchor_config: BatchAnchorConfig,
    ) -> Self {
        Self {
            domain,
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

/// Interface used by the replication task to keep track of blocks that are persisted vs broadcast
pub(crate) trait PersistedBlockSource: Clone + Send + Sync + 'static {
    fn last_block_number(&self) -> eyre::Result<u64>;
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

/// Broadcast every newly persisted leader block in canonical order.
pub(crate) async fn broadcast_persisted_blocks<P>(provider: P, commands: mpsc::Sender<P2pCommand>)
where
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

    while let Some(persisted_tip) = persisted.next().await {
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
const MAX_PENDING_BLOCKS: usize = 128;
const BACKFILL_PAGE_SIZE: u64 = 64;
const BACKFILL_SERVE_QUEUE_CAPACITY: usize = 8;

struct BackfillRequest {
    peer: zone_p2p::P2pPeerId,
    request_id: u64,
    start: u64,
}

struct BackfillServerTask(tokio::task::JoinHandle<()>);

impl Drop for BackfillServerTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

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

    fn request(&self, best: u64) -> Option<P2pCommand> {
        self.needed.then(|| P2pCommand::RequestBackfill {
            start: best.saturating_add(1),
        })
    }

    fn probe_after_inactivity(&mut self, best: u64) -> P2pCommand {
        self.needed = true;
        P2pCommand::RequestBackfill {
            start: best.saturating_add(1),
        }
    }
}

fn buffer_pending_block(
    pending: &mut BTreeMap<u64, Vec<u8>>,
    number: u64,
    block: Vec<u8>,
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
    commands: &mpsc::Sender<P2pCommand>,
    peer: zone_p2p::P2pPeerId,
    request_id: u64,
    start: u64,
) -> eyre::Result<()>
where
    P: BlockNumReader + BlockReader<Block = Block>,
{
    let tip = provider.best_block_number()?;
    let end = tip.min(start.saturating_add(BACKFILL_PAGE_SIZE.saturating_sub(1)));
    for number in start..=end {
        let block = provider.block_by_number(number)?.ok_or_else(|| {
            eyre::eyre!("canonical block {number} is missing while serving backfill")
        })?;
        commands
            .blocking_send(P2pCommand::SendBackfillBlock {
                peer: peer.clone(),
                request_id,
                block: alloy_rlp::encode(block),
            })
            .map_err(|_| eyre::eyre!("P2P command channel closed"))?;
    }
    commands
        .blocking_send(P2pCommand::CompleteBackfill {
            peer,
            request_id,
            tip,
        })
        .map_err(|_| eyre::eyre!("P2P command channel closed"))?;
    Ok(())
}

async fn serve_backfill_requests<P>(
    provider: P,
    commands: mpsc::Sender<P2pCommand>,
    mut requests: mpsc::Receiver<BackfillRequest>,
) where
    P: BlockNumReader + BlockReader<Block = Block> + Clone + Send + Sync + 'static,
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

/// Run role-appropriate block replication.
///
/// Leaders are deliberately serve-only: [`ZoneEngine`](crate::ZoneEngine) is their sole
/// chain-head writer. Leader recovery is deferred to a future startup recovery phase, before the
/// zone engine starts. Followers serve catch-up requests and import live/backfilled blocks in
/// canonical order.
pub(crate) async fn run_block_sync<P>(
    role: Role,
    provider: P,
    engine: ConsensusEngineHandle<ZonePayloadTypes>,
    events: mpsc::Receiver<P2pEvent>,
    commands: mpsc::Sender<P2pCommand>,
    attestation: AttestationContext,
) where
    P: BlockNumReader
        + BlockReader<Block = Block>
        + HeaderProvider<Header = TempoHeader>
        + ReceiptProvider
        + Clone
        + Send
        + Sync
        + 'static,
{
    match role {
        Role::Leader => run_leader_backfill_server(provider, events, commands, attestation).await,
        Role::Follower => {
            run_follower_block_sync(provider, engine, events, commands, attestation).await
        }
    }
}

/// Leader will serve follower backfill requests (without ever requesting or importing peer blocks).
async fn run_leader_backfill_server<P>(
    provider: P,
    mut events: mpsc::Receiver<P2pEvent>,
    commands: mpsc::Sender<P2pCommand>,
    attestation: AttestationContext,
) where
    P: BlockNumReader
        + BlockReader<Block = Block>
        + HeaderProvider<Header = TempoHeader>
        + ReceiptProvider
        + Clone
        + Send
        + Sync
        + 'static,
{
    let (backfill_requests, backfill_request_rx) = mpsc::channel(BACKFILL_SERVE_QUEUE_CAPACITY);
    let mut backfill_server = BackfillServerTask(tokio::spawn(serve_backfill_requests(
        provider.clone(),
        commands,
        backfill_request_rx,
    )));

    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else {
                    debug!(target: "zone::p2p", "P2P event channel closed");
                    return;
                };
                match event {
                    P2pEvent::Started { .. } => {}
                    P2pEvent::BackfillRequested { peer, request_id, start } => {
                        if let Err(err) = backfill_requests.try_send(BackfillRequest { peer, request_id, start }) {
                            tracing::warn!(target: "zone::p2p", %err, start, queue_capacity = BACKFILL_SERVE_QUEUE_CAPACITY, "Dropped block backfill request because the serving queue is unavailable");
                        }
                    }
                    P2pEvent::SettlementSignatureReceived { follower, signature } => {
                        let result = async {
                            let signed = SignedSettlementAttestation::decode(&signature)?;
                            let signer = signed.recover_signer(attestation.domain)?;
                            let expected_signer = attestation.addresses.get(&follower).copied()
                                .ok_or_eyre("unknown follower identity")?;
                            eyre::ensure!(signer == expected_signer, "settlement signer does not match authenticated peer");
                            let height: u64 = signed
                                .attestation
                                .zoneHeight
                                .try_into()
                                .wrap_err("settlement height does not fit in u64")?;
                            let expected = build_settlement_attestation(
                                &provider,
                                height,
                                &attestation,
                                Some((signed.attestation.anchorBlockNumber, signed.attestation.anchorBlockHash)),
                            ).await?.ok_or_eyre("signed block is not a batch boundary")?;
                            eyre::ensure!(signed.attestation == expected, "settlement signature does not match leader state");
                            let (_, signatures) = attestation.store.as_ref()
                                .expect("leader must have an attestation store")
                                .insert_settlement(attestation.domain, signer, signed);
                            Ok::<_, eyre::Report>((height, signer, signatures))
                        }.await;
                        match result {
                            Ok((height, signer, signatures)) => info!(target: "zone::p2p", %follower, %signer, height, signatures, "Stored follower settlement signature"),
                            Err(err) => tracing::warn!(target: "zone::p2p", %follower, %err, "Rejected follower settlement signature"),
                        }
                    }
                    P2pEvent::BlockReceived { .. }
                    | P2pEvent::BackfillBlockReceived { .. }
                    | P2pEvent::BackfillCompleted { .. }
                    | P2pEvent::TransactionReceived { .. }
                    | P2pEvent::SettlementProposalReceived { .. } => {
                        // A leader never follows peer chain heads. Keeping this explicit ensures
                        // ZoneEngine remains the sole writer of the leader's canonical head.
                        debug!(target: "zone::p2p", "Ignoring peer block sync event on serve-only leader");
                    }
                }
            }
            result = &mut backfill_server.0 => {
                match result {
                    Ok(()) => tracing::error!(target: "zone::p2p", "Block backfill server stopped unexpectedly"),
                    Err(err) => tracing::error!(target: "zone::p2p", %err, "Block backfill server task failed"),
                }
                return;
            }
        }
    }
}

/// Serve catch-up requests and import live/backfilled blocks in canonical order on a follower.
async fn run_follower_block_sync<P>(
    provider: P,
    engine: ConsensusEngineHandle<ZonePayloadTypes>,
    mut events: mpsc::Receiver<P2pEvent>,
    commands: mpsc::Sender<P2pCommand>,
    attestation: AttestationContext,
) where
    P: BlockNumReader
        + BlockReader<Block = Block>
        + HeaderProvider<Header = TempoHeader>
        + ReceiptProvider
        + Clone
        + Send
        + Sync
        + 'static,
{
    // Keep track of all live blocks if we end up needing a backfill, so we can immediately catchup
    // This is capped to `MAX_PENDING_BLOCKS`.
    let mut pending = BTreeMap::<u64, Vec<u8>>::new();
    let mut backfill = BackfillProgress::new();
    let (backfill_requests, backfill_request_rx) = mpsc::channel(BACKFILL_SERVE_QUEUE_CAPACITY);

    // Serve backfill requests in a separate task to avoid competing the live blocks
    let mut backfill_server = BackfillServerTask(tokio::spawn(serve_backfill_requests(
        provider.clone(),
        commands.clone(),
        backfill_request_rx,
    )));

    // Always probe on startup to see if we're behind
    let mut retry = tokio::time::interval(BACKFILL_RETRY_INTERVAL);
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // If we don't get blocks after 30s, probe to get the next block
    let inactivity = tokio::time::sleep(BLOCK_INACTIVITY_TIMEOUT);
    tokio::pin!(inactivity);

    loop {
        tokio::select! {
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
                            let expected = build_settlement_attestation(
                                &provider,
                                height,
                                &attestation,
                                Some((proposal.anchorBlockNumber, proposal.anchorBlockHash)),
                            ).await?.ok_or_eyre("proposed block is not a batch boundary")?;
                            eyre::ensure!(proposal == expected, "settlement proposal does not match follower state");

                            let signed = SignedSettlementAttestation::sign(
                                proposal,
                                attestation.domain,
                                &attestation.signer,
                            )?;

                            // Return the signed settlement attestation back to the leader
                            commands.send(P2pCommand::SendSettlementSignature(signed.encode()))
                                .await
                                .wrap_err("P2P command channel closed")?;
                            Ok::<_, eyre::Report>(height)
                        }.await;
                        match result {
                            Ok(height) => info!(target: "zone::p2p", %leader, height, "Signed settlement proposal"),
                            Err(err) => tracing::warn!(target: "zone::p2p", %leader, %err, "Rejected settlement proposal"),
                        }
                    }
                    P2pEvent::BackfillRequested { peer, request_id, start } => {
                        if let Err(err) = backfill_requests.try_send(BackfillRequest { peer, request_id, start }) {
                            tracing::warn!(target: "zone::p2p", %err, start, queue_capacity = BACKFILL_SERVE_QUEUE_CAPACITY, "Dropped block backfill request because the serving queue is unavailable");
                        }
                    }
                    P2pEvent::BlockReceived { block, .. }
                    | P2pEvent::BackfillBlockReceived { block, .. } => {
                        match encoded_block_number(&block) {
                            Ok(number) => {
                                inactivity
                                    .as_mut()
                                    .reset(tokio::time::Instant::now() + BLOCK_INACTIVITY_TIMEOUT);
                                let best = match provider.best_block_number() {
                                    Ok(best) => best,
                                    Err(err) => {
                                        tracing::error!(target: "zone::p2p", %err, "Failed reading local head");
                                        continue;
                                    }
                                };
                                if number <= best {
                                    if let Err(err) = import_peer_block(&provider, &engine, &block).await {
                                        tracing::error!(target: "zone::p2p", %err, "Rejected duplicate or conflicting peer block");
                                    }
                                    continue;
                                }
                                backfill.observe_block(number, best);
                                if let Some(dropped) = buffer_pending_block(&mut pending, number, block) {
                                    tracing::warn!(target: "zone::p2p", dropped, pending_limit = MAX_PENDING_BLOCKS, "Dropped far-future peer block because the pending block buffer is full");
                                }
                                if number > best.saturating_add(1) {
                                    info!(target: "zone::p2p", local_head = best, received = number, "Detected zone block gap; requesting backfill");
                                }
                                if let Err(err) = drain_pending_blocks(&provider, &engine, &mut pending).await {
                                    tracing::error!(target: "zone::p2p", %err, "Rejected peer block while draining backfill");
                                    backfill.needed = true;
                                } else {
                                    let best = match provider.best_block_number() {
                                        Ok(best) => best,
                                        Err(err) => {
                                            tracing::error!(target: "zone::p2p", %err, "Failed reading local head after importing peer blocks");
                                            continue;
                                        }
                                    };
                                    backfill.refresh_after_import(
                                        best,
                                        pending.first_key_value().map(|(&number, _)| number),
                                    );
                                }
                            }
                            Err(err) => tracing::error!(target: "zone::p2p", %err, "Rejected malformed peer block"),
                        }
                    }
                    P2pEvent::BackfillCompleted { peer, tip } => {
                        let best = match provider.best_block_number() {
                            Ok(best) => best,
                            Err(err) => {
                                tracing::error!(target: "zone::p2p", %err, "Failed reading local head after backfill response");
                                continue;
                            }
                        };
                        backfill.complete(
                            tip,
                            best,
                            pending.first_key_value().map(|(&number, _)| number),
                        );
                        debug!(target: "zone::p2p", %peer, best, tip, backfill_needed = backfill.needed, "Completed block backfill response page");
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
                    && commands.send(command).await.is_err()
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
                if commands.send(command).await.is_err() {
                    debug!(target: "zone::p2p", "P2P command channel closed");
                    return;
                }
            }
            result = &mut backfill_server.0 => {
                match result {
                    Ok(()) => tracing::error!(target: "zone::p2p", "Block backfill server stopped unexpectedly"),
                    Err(err) => tracing::error!(target: "zone::p2p", %err, "Block backfill server task failed"),
                }
                return;
            }
        }
    }
}

async fn drain_pending_blocks<P>(
    provider: &P,
    engine: &ConsensusEngineHandle<ZonePayloadTypes>,
    pending: &mut BTreeMap<u64, Vec<u8>>,
) -> eyre::Result<()>
where
    P: reth_storage_api::BlockNumReader
        + reth_storage_api::HeaderProvider<Header = TempoHeader>
        + Clone
        + Send
        + Sync
        + 'static,
{
    loop {
        let next = provider.best_block_number()?.saturating_add(1);
        let Some(block) = pending.remove(&next) else {
            return Ok(());
        };
        import_peer_block(provider, engine, &block).await?;
    }
}

async fn import_peer_block<P>(
    provider: &P,
    engine: &ConsensusEngineHandle<ZonePayloadTypes>,
    encoded: &[u8],
) -> eyre::Result<()>
where
    P: BlockNumReader + HeaderProvider<Header = TempoHeader> + Clone + Send + Sync + 'static,
{
    // Check the received block
    let mut input = encoded;
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
            return Ok(());
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

    // 3. All txns in the block execute properly
    let payload = ZonePayloadTypes::block_to_payload(block, None);
    let status = engine.new_payload(payload).await?;
    if !status.is_valid() {
        eyre::bail!("execution engine rejected peer block {block_number} ({hash}): {status:?}");
    }

    // 4. Forkchoice
    let forkchoice = ForkchoiceState::same_hash(hash);
    let result = engine.fork_choice_updated(forkchoice, None).await?;
    if !result.is_valid() {
        eyre::bail!(
            "execution engine rejected forkchoice for block {block_number} ({hash}): {result:?}"
        );
    }

    info!(target: "zone::p2p", block_number, ?hash, "Imported canonical leader block");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use futures::{StreamExt as _, stream};

    use super::{
        BackfillProgress, EncodedPersistedBlock, MAX_PENDING_BLOCKS, PersistedBlockSource,
        PersistedTip, broadcast_persisted_blocks, buffer_pending_block,
    };
    use alloy_primitives::B256;
    use zone_p2p::P2pCommand;

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

        broadcast_persisted_blocks(source, commands).await;

        assert_eq!(
            command_rx.recv().await,
            Some(P2pCommand::BroadcastBlock(vec![1]))
        );
        assert_eq!(command_rx.recv().await, None);
    }

    #[test]
    fn requests_backfill_again_when_live_block_reveals_gap_after_completion() {
        const LOCAL_HEAD: u64 = 10;

        let mut backfill = BackfillProgress::new();
        assert_eq!(
            backfill.request(LOCAL_HEAD),
            Some(P2pCommand::RequestBackfill {
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
            Some(P2pCommand::RequestBackfill {
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
            P2pCommand::RequestBackfill {
                start: LOCAL_HEAD + 1,
            }
        );
        assert_eq!(
            backfill.request(LOCAL_HEAD),
            Some(P2pCommand::RequestBackfill {
                start: LOCAL_HEAD + 1,
            })
        );
    }

    #[test]
    fn pending_block_limit_keeps_blocks_closest_to_local_head() {
        let mut pending = std::collections::BTreeMap::new();
        for number in 100..100 + MAX_PENDING_BLOCKS as u64 {
            assert_eq!(
                buffer_pending_block(&mut pending, number, vec![number as u8]),
                None
            );
        }
        assert_eq!(pending.len(), MAX_PENDING_BLOCKS);

        let farthest = 100 + MAX_PENDING_BLOCKS as u64 - 1;
        assert_eq!(
            buffer_pending_block(&mut pending, 99, vec![99]),
            Some(farthest)
        );
        assert_eq!(pending.len(), MAX_PENDING_BLOCKS);
        assert!(pending.contains_key(&99));
        assert!(!pending.contains_key(&farthest));

        let farther = farthest + 1;
        assert_eq!(
            buffer_pending_block(&mut pending, farther, vec![farther as u8]),
            Some(farther)
        );
        assert_eq!(pending.len(), MAX_PENDING_BLOCKS);
        assert!(!pending.contains_key(&farther));
    }
}
