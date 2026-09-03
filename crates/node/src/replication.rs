//! Node-side leader replication and role-neutral backfill serving.

use alloy_consensus::BlockHeader as _;
use alloy_primitives::B256;
use alloy_provider::DynProvider;
use futures::{StreamExt as _, stream::BoxStream};
use reth_chain_state::PersistedBlockSubscriptions;
use reth_primitives_traits::SealedBlock;
use reth_provider::HeaderProvider;
use reth_storage_api::{BlockNumReader, BlockReader, ReceiptProvider, StateProviderFactory};
use std::collections::HashMap;
use tempo_alloy::TempoNetwork;
use tempo_primitives::{Block, TempoHeader};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync;
use tracing::{debug, info};
use zone_l1::TempoStateExt as _;
use zone_p2p::{BackfillCommand, BackfillRequest, P2pCommand, P2pEvent, P2pPeerId, PeerTip};
use zone_sequencer::{
    BatchAnchorConfig,
    attestation::{AttestationDomain, AttestationStore, SignedSettlementAttestation},
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

const BACKFILL_PAGE_SIZE: u64 = 64;
/// Bounds queued requests at the process-lifetime backfill server. Requesters keep at most
/// one request outstanding per peer, so manifest size bounds the live queue depth; the
/// headroom absorbs requests arriving before the server task starts.
pub(crate) const BACKFILL_SERVE_QUEUE_CAPACITY: usize = 128;

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
        Some((
            signed.attestation.anchorBlockNumber,
            signed.attestation.anchorBlockHash,
        )),
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

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    };

    use futures::{StreamExt as _, stream};
    use tokio::sync::{oneshot, watch};

    use super::{
        BroadcasterShutdown, EncodedPersistedBlock, PersistedBlockSource, PersistedTip,
        broadcast_persisted_blocks,
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
}
