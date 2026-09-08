//! Node-side leader replication and role-neutral backfill serving.

use alloy_primitives::B256;
use futures::{StreamExt as _, stream::BoxStream};
use reth_chain_state::PersistedBlockSubscriptions;
use reth_primitives_traits::SealedBlock;
use reth_provider::HeaderProvider;
use reth_storage_api::{BlockNumReader, BlockReader, ReceiptProvider, StateProviderFactory};
use tempo_primitives::{Block, TempoHeader};
use tokio::sync::mpsc;
use tokio_util::sync;
use tracing::{debug, info};
use zone_l1::TempoStateExt as _;
use zone_p2p::{BackfillCommand, BackfillRequest, P2pCommand, P2pEvent, P2pPeerId, PeerTip};
use zone_sequencer::attestation::{AttestationStore, SignedSettlementAttestation};

use eyre::{OptionExt as _, WrapErr as _};

use crate::settlement_attestation::{AttestationContext, build_settlement_attestation};

pub(crate) struct EncodedPersistedBlock {
    hash: B256,
    encoded: Vec<u8>,
}

/// Interface used by the replication task to keep track of blocks that are persisted vs broadcast
pub(crate) trait PersistedBlockSource: Clone + Send + Sync + 'static {
    fn last_block_number(&self) -> eyre::Result<u64>;
    fn persisted_block_stream(&self) -> BoxStream<'static, ()>;
    fn encoded_block_by_number(&self, number: u64) -> eyre::Result<EncodedPersistedBlock>;
}

impl<P> PersistedBlockSource for P
where
    P: PersistedBlockSubscriptions + BlockReader<Block = Block> + Clone + Send + Sync + 'static,
{
    fn last_block_number(&self) -> eyre::Result<u64> {
        Ok(BlockNumReader::last_block_number(self)?)
    }

    fn persisted_block_stream(&self) -> BoxStream<'static, ()> {
        PersistedBlockSubscriptions::persisted_block_stream(self)
            .map(|_| ())
            .boxed()
    }

    fn encoded_block_by_number(&self, number: u64) -> eyre::Result<EncodedPersistedBlock> {
        let block = self
            .block_by_number(number)?
            .ok_or_else(|| eyre::eyre!("persisted zone block {number} is missing"))?;
        let sealed = SealedBlock::seal_slow(block);
        Ok(EncodedPersistedBlock {
            hash: sealed.hash(),
            encoded: alloy_rlp::encode(sealed.into_block()),
        })
    }
}

/// Publish exactly the locally produced blocks, after they are durable. Peer imports never
/// enter this channel, and a role change cannot truncate the previous leader's durable tail.
pub(crate) async fn broadcast_finalized_blocks<P: PersistedBlockSource>(
    provider: P,
    commands: mpsc::Sender<P2pCommand>,
    mut blocks: mpsc::Receiver<alloy_eips::NumHash>,
) -> eyre::Result<()> {
    let mut persisted = provider.persisted_block_stream();
    while let Some(block) = blocks.recv().await {
        while provider.last_block_number()? < block.number {
            persisted
                .next()
                .await
                .ok_or_eyre("persisted block stream closed")?;
        }
        let encoded = provider.encoded_block_by_number(block.number)?;
        eyre::ensure!(
            encoded.hash == block.hash,
            "finalized block changed before propagation"
        );
        commands
            .send(P2pCommand::BroadcastBlock(encoded.encoded))
            .await
            .wrap_err("P2P command channel closed")?;
    }
    Ok(())
}

/// Bounded queue for role-neutral canonical block serving.
pub(crate) const BACKFILL_SERVE_QUEUE_CAPACITY: usize = 128;
const BACKFILL_PAGE_SIZE: u64 = 64;

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
/// same canonical provider. The process-lifetime server retains accepted requests across
/// leadership changes, so peers can complete their backfill during a handoff.
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

/// Validate a follower signature against its authenticated identity and canonical settlement.
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
    use super::*;
    use alloy_eips::NumHash;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };
    use tokio::sync::watch;

    #[derive(Clone)]
    struct Source {
        persisted: Arc<AtomicU64>,
        updates: watch::Receiver<()>,
    }

    impl PersistedBlockSource for Source {
        fn last_block_number(&self) -> eyre::Result<u64> {
            Ok(self.persisted.load(Ordering::SeqCst))
        }
        fn persisted_block_stream(&self) -> BoxStream<'static, ()> {
            futures::stream::unfold(self.updates.clone(), |mut updates| async move {
                updates.changed().await.ok()?;
                Some(((), updates))
            })
            .boxed()
        }
        fn encoded_block_by_number(&self, number: u64) -> eyre::Result<EncodedPersistedBlock> {
            assert!(number <= self.persisted.load(Ordering::SeqCst));
            Ok(EncodedPersistedBlock {
                hash: B256::repeat_byte(number as u8),
                encoded: vec![number as u8],
            })
        }
    }

    #[tokio::test]
    async fn propagation_waits_for_persistence_and_drains_after_producer_closes() {
        let persisted = Arc::new(AtomicU64::new(1));
        let (updates, update_rx) = watch::channel(());
        let source = Source {
            persisted: persisted.clone(),
            updates: update_rx,
        };
        let (commands, mut received) = mpsc::channel(4);
        let (blocks, block_rx) = mpsc::channel(4);
        blocks
            .send(NumHash::new(2, B256::repeat_byte(2)))
            .await
            .unwrap();
        drop(blocks);
        let task = tokio::spawn(broadcast_finalized_blocks(source, commands, block_rx));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), received.recv())
                .await
                .is_err()
        );
        assert!(!task.is_finished());
        persisted.store(2, Ordering::SeqCst);
        updates.send(()).unwrap();
        task.await.unwrap().unwrap();
        assert_eq!(
            received.recv().await,
            Some(P2pCommand::BroadcastBlock(vec![2]))
        );
        assert_eq!(received.recv().await, None);
    }

    #[tokio::test]
    async fn propagation_sends_only_named_blocks_and_rejects_hash_mismatch() {
        let (_updates, update_rx) = watch::channel(());
        let source = Source {
            persisted: Arc::new(AtomicU64::new(4)),
            updates: update_rx,
        };
        let (commands, mut received) = mpsc::channel(4);
        let (blocks, block_rx) = mpsc::channel(4);
        blocks
            .send(NumHash::new(2, B256::repeat_byte(2)))
            .await
            .unwrap();
        blocks.send(NumHash::new(4, B256::ZERO)).await.unwrap();
        drop(blocks);
        let error = broadcast_finalized_blocks(source, commands, block_rx)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("changed before propagation"));
        assert_eq!(
            received.recv().await,
            Some(P2pCommand::BroadcastBlock(vec![2]))
        );
        assert_eq!(received.recv().await, None);
    }
}
