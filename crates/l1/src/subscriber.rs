use super::*;
use crate::{
    EncryptionKeyRing, L1StateCache, abi::ZonePortal, metrics::L1SubscriberMetrics,
    state::EnabledTokenRegistry,
};
use eyre::{OptionExt as _, WrapErr as _};
use std::collections::{HashSet, VecDeque};
use tempo_contracts::precompiles::{ITIP20::TransferPolicyUpdate, TIP403_REGISTRY_ADDRESS};
use tempo_primitives::is_tip20_prefix;

use std::collections::BTreeMap;

/// Maximum number of authenticated L1 blocks the subscriber may retain ahead of the Zone
/// consumer's imported Tempo checkpoint (approximately one hour at Tempo's 500ms block time).
pub const MAX_L1_LOOKAHEAD_BLOCKS: u64 = 7_200;

/// Maximum ancestry span materialized for historical replay or settlement (about 36 hours).
pub const MAX_L1_REPLAY_BLOCKS: u64 = 262_144;

#[derive(Debug, Default)]
struct L1BlockTrackerState {
    observed: BTreeMap<u64, L1BlockObservation>,
    recent_portal_evidence: BTreeMap<u64, AuthenticatedPortalLogs>,
    latest: Option<NumHash>,
    pruned_through: Option<u64>,
    portal_pause: Option<(NumHash, bool)>,
    control_plane: Option<NumHash>,
    pending: BTreeMap<u64, (SealedHeader<TempoHeader>, L1ProcessedEvents)>,
}

#[derive(Debug, Clone)]
struct L1BlockObservation {
    hash: B256,
    portal_events: L1PortalEvents,
    portal_evidence: Option<AuthenticatedPortalLogs>,
}

/// Receipt-root-authenticated Portal logs for one finalized Tempo block.
#[derive(Debug, Clone)]
pub struct AuthenticatedPortalLogs {
    /// Exact finalized Tempo block containing the logs.
    pub block: NumHash,
    /// Parent hash from the authenticated Tempo header.
    pub parent_hash: B256,
    /// Portal logs in canonical receipt and log order.
    pub logs: Vec<alloy_primitives::Log>,
}

/// One `BatchSubmitted` event from a receipt-verified finalized Tempo block.
#[derive(Debug, Clone)]
pub struct FinalizedBatchSubmission {
    /// Finalized Tempo block containing the accepted submission.
    pub block: NumHash,
    /// Receipt-trie position of the transaction containing the accepted submission.
    pub transaction_index: u64,
    /// Receipt-local log position used to distinguish multiple submissions in one transaction.
    pub log_index: u64,
    /// Event emitted after the Portal accepted the submission.
    pub event: ZonePortal::BatchSubmitted,
}

/// Extract `BatchSubmitted` events using receipt ordering rather than RPC metadata.
///
/// Callers must verify `receipts` against the supplied block's receipt root first.
pub fn extract_finalized_batch_submissions(
    block: NumHash,
    portal_address: Address,
    receipts: &[tempo_alloy::rpc::TempoTransactionReceipt],
) -> Vec<FinalizedBatchSubmission> {
    let mut submissions = Vec::new();
    for (transaction_index, receipt) in receipts.iter().enumerate() {
        for (log_index, log) in receipt.logs().iter().enumerate() {
            if log.address() != portal_address
                || log.topic0() != Some(&ZonePortal::BatchSubmitted::SIGNATURE_HASH)
            {
                continue;
            }
            match ZonePortal::BatchSubmitted::decode_log(&log.inner) {
                Ok(event) => submissions.push(FinalizedBatchSubmission {
                    block,
                    transaction_index: transaction_index as u64,
                    log_index: log_index as u64,
                    event: event.data,
                }),
                Err(err) => warn!(
                    target: "zone::l1::subscriber",
                    block_number = block.number,
                    error = ?err,
                    "Could not decode finalized batch submission for observer"
                ),
            }
        }
    }
    submissions
}

/// Number of consumed Tempo blocks whose authenticated Portal logs remain available to
/// asynchronous observers such as the checker ExEx.
const RECENT_PORTAL_EVIDENCE_BLOCKS: u64 = 256;

/// L1 blocks whose headers and receipts have been independently validated and
/// whose derived state has been applied to the local caches.
///
/// Followers use this to gate zone-block import on the exact L1 anchor embedded in
/// `advanceTempo`. The tracker also provides backpressure for the L1 subscriber: before fetching
/// a block, the subscriber waits for capacity relative to the last checkpoint released by the
/// Zone consumer. Queue-backed subscribers therefore retain observations until block production
/// or follower import calls [`L1BlockTracker::prune_through`].
///
/// This tracker deliberately assumes observed L1 blocks do not reorg: conflicting or
/// non-contiguous observations are errors.
#[derive(Debug, Clone)]
pub struct L1BlockTracker {
    state: Arc<parking_lot::RwLock<L1BlockTrackerState>>,
    changed: tokio::sync::watch::Sender<()>,
}

impl Default for L1BlockTracker {
    fn default() -> Self {
        let (changed, _) = tokio::sync::watch::channel(());
        Self {
            state: Default::default(),
            changed,
        }
    }
}

impl L1BlockTracker {
    /// Initialize the last L1 height already represented by canonical local zone state.
    pub fn initialize_consumed_through(&self, number: u64) {
        let mut state = self.state.write();
        state.pruned_through = Some(state.pruned_through.map_or(number, |old| old.max(number)));
        drop(state);
        self.changed.send_replace(());
    }

    /// Return the independently observed hash at `number`, if it is retained.
    pub fn observed_hash(&self, number: u64) -> Option<B256> {
        self.state
            .read()
            .observed
            .get(&number)
            .map(|observation| observation.hash)
    }

    /// Return the highest L1 anchor admitted to the bounded execution tracker.
    pub fn latest(&self) -> Option<NumHash> {
        self.state.read().latest
    }

    /// Highest finalized block whose governance events and cache invalidations are applied.
    /// This cursor advances independently of the queue consumed by Zone blocks.
    pub fn control_plane_latest(&self) -> Option<NumHash> {
        self.state.read().control_plane
    }

    fn validate_control_plane_block(&self, block: NumHash, parent_hash: B256) -> eyre::Result<()> {
        let state = self.state.read();
        if let Some(observed) = state.observed.get(&block.number) {
            eyre::ensure!(
                observed.hash == block.hash,
                "control-plane block conflicts with retained L1 anchor {}",
                block.number
            );
        }
        if let Some(previous) = state.control_plane {
            eyre::ensure!(
                block.number == previous.number.saturating_add(1) && parent_hash == previous.hash,
                "non-contiguous finalized control-plane block: previous {previous:?}, new {block:?}"
            );
        }
        Ok(())
    }

    /// Return whether finalized Portal state currently pauses block production.
    pub fn portal_paused(&self) -> bool {
        self.state
            .read()
            .portal_pause
            .is_some_and(|(_, paused)| paused)
    }

    /// Reject missing code once the Portal has been observed on finalized L1.
    pub fn validate_portal_absence(&self) -> eyre::Result<()> {
        eyre::ensure!(
            self.state.read().portal_pause.is_none(),
            "finalized Portal code disappeared after deployment; preserving the previous pause state"
        );
        Ok(())
    }

    /// Apply an exact finalized Portal snapshot, rejecting contradictory observations.
    /// Returns whether the effective pause value changed. Older observations are ignored.
    pub fn observe_portal_pause(&self, block: NumHash, paused: bool) -> eyre::Result<bool> {
        let mut state = self.state.write();
        if let Some((current, current_paused)) = state.portal_pause {
            if current.number > block.number {
                return Ok(false);
            }
            if current.number == block.number {
                eyre::ensure!(
                    current.hash == block.hash,
                    "conflicting finalized Portal block hashes at height {}",
                    block.number
                );
                eyre::ensure!(
                    current_paused == paused,
                    "contradictory Portal pause observations for block {}",
                    block.hash
                );
                return Ok(false);
            }
        }
        if let Some(observation) = state.observed.get(&block.number) {
            eyre::ensure!(
                observation.hash == block.hash,
                "Portal snapshot conflicts with authenticated L1 block {}",
                block.number
            );
        }
        let changed = state
            .portal_pause
            .map_or(paused, |(_, current)| current != paused);
        state.portal_pause = Some((block, paused));
        drop(state);
        if changed {
            self.changed.send_replace(());
        }
        Ok(changed)
    }

    /// Subscribe to validated L1-state changes, including portal pause transitions.
    pub fn subscribe_changes(&self) -> tokio::sync::watch::Receiver<()> {
        self.changed.subscribe()
    }

    /// Return whether `number` fits inside the bounded subscriber lookahead window.
    pub fn has_capacity_for(&self, number: u64) -> bool {
        self.state
            .read()
            .pruned_through
            .is_none_or(|consumed| number <= consumed.saturating_add(MAX_L1_LOOKAHEAD_BLOCKS))
    }

    /// Wait until the Zone consumer advances enough for the subscriber to retain `number`.
    pub async fn wait_for_capacity(&self, number: u64) -> eyre::Result<()> {
        let mut changed = self.changed.subscribe();
        while !self.has_capacity_for(number) {
            changed
                .changed()
                .await
                .map_err(|_| eyre::eyre!("L1 block tracker closed"))?;
        }
        Ok(())
    }

    /// Return the next L1 height the subscriber needs to retain.
    pub fn next_observation_number(&self) -> Option<u64> {
        let state = self.state.read();
        state
            .latest
            .map(|latest| latest.number.saturating_add(1))
            .or_else(|| {
                state
                    .pruned_through
                    .map(|consumed| consumed.saturating_add(1))
            })
    }

    /// Wait until the exact L1 block has been validated and applied locally.
    pub async fn wait_for(&self, block: NumHash) -> eyre::Result<()> {
        self.wait_for_portal_events(block).await.map(|_| ())
    }

    /// Wait for an exact L1 block and return its receipt-authenticated portal events.
    pub async fn wait_for_portal_events(&self, block: NumHash) -> eyre::Result<L1PortalEvents> {
        let mut changed = self.changed.subscribe();
        loop {
            {
                let state = self.state.read();
                match state.observed.get(&block.number) {
                    Some(observation) if observation.hash == block.hash => {
                        return Ok(observation.portal_events.clone());
                    }
                    Some(observation) => {
                        eyre::bail!(
                            "observed different L1 hash at block {}: expected {}, got {}",
                            block.number,
                            block.hash,
                            observation.hash
                        )
                    }
                    None if state
                        .pruned_through
                        .is_some_and(|height| height >= block.number) =>
                    {
                        eyre::bail!(
                            "L1 block {} was already consumed and pruned from the tracker",
                            block.number
                        )
                    }
                    None if state
                        .latest
                        .is_some_and(|latest| latest.number >= block.number) =>
                    {
                        eyre::bail!(
                            "L1 block {} is missing below the latest observed height {}",
                            block.number,
                            state.latest.expect("checked above").number
                        )
                    }
                    None => {}
                }
            }
            changed
                .changed()
                .await
                .map_err(|_| eyre::eyre!("L1 block tracker closed"))?;
        }
    }

    /// Return retained receipt-authenticated Portal logs for an exact Tempo block.
    ///
    /// This is intentionally non-blocking. Callers replaying older history can fall back to an
    /// archival provider when the bounded recent cache no longer contains the block.
    pub fn authenticated_portal_logs(
        &self,
        block: NumHash,
    ) -> eyre::Result<Option<AuthenticatedPortalLogs>> {
        let state = self.state.read();
        if let Some(observation) = state.observed.get(&block.number) {
            eyre::ensure!(
                observation.hash == block.hash,
                "observed different L1 hash at block {}: expected {}, got {}",
                block.number,
                block.hash,
                observation.hash
            );
            return Ok(observation.portal_evidence.clone());
        }
        if let Some(evidence) = state.recent_portal_evidence.get(&block.number) {
            eyre::ensure!(
                evidence.block.hash == block.hash,
                "retained different L1 hash at block {}: expected {}, got {}",
                block.number,
                block.hash,
                evidence.block.hash
            );
            return Ok(Some(evidence.clone()));
        }
        Ok(None)
    }

    /// Record an independently validated and applied L1 anchor.
    pub fn record(&self, block: NumHash) -> eyre::Result<()> {
        self.record_observation(block, L1PortalEvents::default(), None)
    }

    /// Record an L1 anchor together with portal events decoded from its verified receipts.
    pub fn record_with_portal_events(
        &self,
        block: NumHash,
        portal_events: L1PortalEvents,
    ) -> eyre::Result<()> {
        self.record_observation(block, portal_events, None)
    }

    /// Record an L1 anchor together with decoded events and authenticated raw Portal logs.
    pub fn record_with_portal_evidence(
        &self,
        block: NumHash,
        parent_hash: B256,
        portal_events: L1PortalEvents,
        logs: Vec<alloy_primitives::Log>,
    ) -> eyre::Result<()> {
        let evidence = AuthenticatedPortalLogs {
            block,
            parent_hash,
            logs,
        };
        self.record_observation(block, portal_events, Some(evidence))
    }

    fn record_observation(
        &self,
        block: NumHash,
        portal_events: L1PortalEvents,
        portal_evidence: Option<AuthenticatedPortalLogs>,
    ) -> eyre::Result<()> {
        let mut state = self.state.write();
        if let Some((snapshot, _)) = state.portal_pause {
            eyre::ensure!(
                snapshot.number != block.number || snapshot.hash == block.hash,
                "authenticated L1 block {} conflicts with the finalized Portal snapshot",
                block.number
            );
        }
        if let Some(observation) = state.observed.get(&block.number) {
            eyre::ensure!(
                observation.hash == block.hash,
                "conflicting L1 hash at observed height {}: existing {}, new {}",
                block.number,
                observation.hash,
                block.hash
            );
            return Ok(());
        }
        if state.latest == Some(block) {
            // The exact latest observation may already have been pruned on a
            // leader after it was handed to the deposit queue.
            return Ok(());
        }
        let consumed = *state
            .pruned_through
            .get_or_insert_with(|| block.number.saturating_sub(1));
        eyre::ensure!(
            block.number <= consumed.saturating_add(MAX_L1_LOOKAHEAD_BLOCKS),
            "L1 observation {} exceeds subscriber lookahead through {}",
            block.number,
            consumed.saturating_add(MAX_L1_LOOKAHEAD_BLOCKS)
        );
        if let Some(latest) = state.latest {
            eyre::ensure!(
                block.number == latest.number.saturating_add(1),
                "non-contiguous L1 observation: latest {}, new {}",
                latest.number,
                block.number
            );
        } else {
            eyre::ensure!(
                block.number == consumed.saturating_add(1),
                "non-contiguous first L1 observation: consumed through {}, new {}",
                consumed,
                block.number
            );
        }
        state.observed.insert(
            block.number,
            L1BlockObservation {
                hash: block.hash,
                portal_events,
                portal_evidence,
            },
        );
        state.latest = Some(block);
        drop(state);
        self.changed.send_replace(());
        Ok(())
    }

    /// Drop observations through `number` after the corresponding Zone checkpoint is canonical.
    ///
    /// Advancing this watermark releases L1 subscriber capacity, so queue consumers must call it
    /// only after successfully consuming the matching finalized L1 work.
    pub fn prune_through(&self, number: u64) {
        let mut state = self.state.write();
        let consumed = state
            .observed
            .range(..=number)
            .filter_map(|(_, observation)| {
                observation
                    .portal_evidence
                    .clone()
                    .map(|evidence| (evidence.block.number, evidence))
            })
            .collect::<Vec<_>>();
        state.recent_portal_evidence.extend(consumed);
        state.observed.retain(|height, _| *height > number);
        let retain_after = number.saturating_sub(RECENT_PORTAL_EVIDENCE_BLOCKS);
        state
            .recent_portal_evidence
            .retain(|height, _| *height > retain_after);
        state.pruned_through = Some(state.pruned_through.map_or(number, |old| old.max(number)));
        drop(state);
        self.changed.send_replace(());
    }
}

/// Poll interval for the HTTP block filter fallback (500ms, matching L1 block time).
const HTTP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Debug, Clone)]
pub(crate) struct L1ProcessedEvents {
    pub(crate) portal_events: L1PortalEvents,
    pub(crate) invalidated: HashSet<Address>,
    pub(crate) portal_logs: Option<Vec<alloy_primitives::Log>>,
    pub(crate) finalized_batches: VecDeque<FinalizedBatchSubmission>,
    pub(crate) portal_pause: Option<bool>,
}

fn cache_invalidation_address(address: Address, topic0: Option<&B256>) -> Option<Address> {
    (address == TIP403_REGISTRY_ADDRESS
        || (is_tip20_prefix(address) && topic0 == Some(&TransferPolicyUpdate::SIGNATURE_HASH)))
    .then_some(TIP403_REGISTRY_ADDRESS)
}

fn portal_event_cache_invalidation_address(topic0: Option<&B256>) -> Option<Address> {
    use tempo_contracts::precompiles::TIP403_REGISTRY_ADDRESS;

    (topic0 == Some(&TokenEnabled::SIGNATURE_HASH)).then_some(TIP403_REGISTRY_ADDRESS)
}

/// Sink for leadership transitions decoded from verified finalized receipts.
///
/// Implemented by the node over its `LeadershipSchedule`. The subscriber applies every
/// transition **before** enqueueing the block that recorded it — enqueueing notifies the
/// engine internally, so append-after-enqueue could race a producer waking on that notify.
/// An error makes ingestion of the finalized L1 block fail.
pub trait LeadershipSink: Send + Sync + std::fmt::Debug {
    /// Apply one decoded leadership transition.
    fn apply_leader_transition(&self, transition: &crate::LeaderTransition) -> eyre::Result<()>;
}

/// Configuration for the L1 subscriber.
#[derive(Debug, Clone)]
pub struct L1SubscriberConfig {
    /// RPC URL of the L1 node (HTTP or WebSocket).
    pub l1_rpc_url: String,
    /// ZonePortal contract address on L1.
    pub portal_address: Address,
    /// Maximum number of concurrent header and receipt fetches while syncing a
    /// finalized L1 range.
    pub l1_fetch_concurrency: usize,
    /// Interval between L1 connection attempts.
    pub retry_connection_interval: std::time::Duration,
    /// Whether to retain authenticated Portal logs for external observers.
    pub retain_portal_evidence: bool,
}

/// L1 chain subscriber that listens for new blocks and extracts deposit events.
pub struct L1Subscriber<P> {
    pub(crate) config: L1SubscriberConfig,
    pub(crate) zone_provider: P,
    /// Finalized L1 blocks retained until a Zone consumer processes them.
    pub(crate) deposit_queue: DepositQueue,
    /// Shared registry of tokens enabled for this zone.
    pub(crate) enabled_tokens: EnabledTokenRegistry,
    /// Shared L1 state cache updated after each finalized block.
    pub(crate) l1_state_cache: L1StateCache,
    /// Validated and applied L1 anchors shared with follower block import.
    pub(crate) block_tracker: L1BlockTracker,
    /// Optional sink for leadership transitions.
    pub(crate) leadership_sink: Option<Arc<dyn LeadershipSink>>,
    /// Optional execution-side channel for finalized accepted batch submissions.
    /// Its backpressure must never stop finalized governance observation.
    pub(crate) finalized_batch_submissions:
        Option<tokio::sync::mpsc::Sender<FinalizedBatchSubmission>>,
    /// Private encryption keys bound by finalized Portal rotation events.
    pub(crate) encryption_keys: Option<EncryptionKeyRing>,
    /// L1 subscriber metrics for connection health, backfill, and event ingestion.
    pub(crate) subscriber_metrics: L1SubscriberMetrics,
}

#[derive(Debug, thiserror::Error)]
pub enum L1SubscriberError {
    #[error(transparent)]
    TransportError(#[from] alloy_transport::TransportError),
    #[error(transparent)]
    Other(#[from] eyre::Report),
    #[error("fatal L1 ingestion error at block {block_number} during {stage}: {source}")]
    Fatal {
        block_number: u64,
        stage: &'static str,
        #[source]
        source: eyre::Report,
    },
}

impl L1SubscriberError {
    fn fatal_from_err(block_number: u64, stage: &'static str) -> impl FnOnce(eyre::Report) -> Self {
        move |source| Self::Fatal {
            block_number,
            stage,
            source,
        }
    }

    pub(crate) fn should_retry(&self) -> bool {
        !matches!(self, Self::Fatal { .. })
    }
}

type HeaderStream = Pin<Box<dyn Stream<Item = ()> + Send>>;

impl<P> L1Subscriber<P>
where
    P: StateProviderFactory + Sync,
{
    /// Create an L1 subscriber.
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        config: L1SubscriberConfig,
        zone_provider: P,
        deposit_queue: DepositQueue,
        enabled_tokens: EnabledTokenRegistry,
        l1_state_cache: L1StateCache,
        block_tracker: L1BlockTracker,
        leadership_sink: Option<Arc<dyn LeadershipSink>>,
        finalized_batch_submissions: Option<tokio::sync::mpsc::Sender<FinalizedBatchSubmission>>,
        encryption_keys: Option<EncryptionKeyRing>,
    ) -> Self {
        Self {
            config,
            zone_provider,
            deposit_queue,
            enabled_tokens,
            l1_state_cache,
            block_tracker,
            leadership_sink,
            finalized_batch_submissions,
            encryption_keys,
            subscriber_metrics: Default::default(),
        }
    }

    /// Connect to the L1 node.
    ///
    /// The transport (HTTP or WebSocket) is auto-detected from the URL scheme.
    #[instrument(skip(self), fields(l1_rpc_url = %self.config.l1_rpc_url))]
    async fn connect(&self) -> Result<DynProvider<TempoNetwork>, L1SubscriberError> {
        info!(url = %self.config.l1_rpc_url, "Connecting to L1 node");

        let url: url::Url = self.config.l1_rpc_url.parse().map_err(eyre::Report::from)?;
        let mut conn_config =
            crate::rpc::rpc_connection_config(self.config.retry_connection_interval);

        if !url.username().is_empty() {
            let auth = Authorization::basic(url.username(), url.password().unwrap_or_default());
            conn_config = conn_config.with_auth(auth);
        }

        let client = RpcClient::builder()
            .connect_with_config(&self.config.l1_rpc_url, conn_config)
            .await?;

        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_client(client)
            .erased();
        info!("Connected to L1 node");
        Ok(provider)
    }

    /// Subscribe to transport-appropriate L1 head notifications.
    ///
    /// WebSocket connections use `newHeads`. When pubsub is unavailable, HTTP
    /// connections fall back to `eth_newBlockFilter` / `eth_getFilterChanges`.
    /// Header payloads are ignored because block selection always comes from
    /// the L1 `finalized` tag.
    pub(crate) async fn subscribe_block_headers(
        &self,
        provider: &DynProvider<TempoNetwork>,
    ) -> Result<HeaderStream, L1SubscriberError> {
        match provider.subscribe_blocks().await {
            Ok(subscription) => {
                info!("Using WebSocket newHeads notifications");
                Ok(Box::pin(subscription.into_stream().map(|_| ())))
            }
            Err(err)
                if err
                    .as_transport_err()
                    .is_some_and(|transport| transport.is_pubsub_unavailable()) =>
            {
                info!("Pubsub unavailable, using HTTP block filter polling");
                let mut watcher = provider.watch_blocks().await?;
                watcher.set_poll_interval(HTTP_POLL_INTERVAL);
                Ok(Box::pin(watcher.into_stream().filter_map(
                    |hashes| async move { (!hashes.is_empty()).then_some(()) },
                )))
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Determine the starting block number for backfill.
    ///
    /// The zone's persisted Tempo checkpoint is the authoritative source for
    /// where ingestion resumes. A non-zero hash distinguishes an L1-anchored
    /// block-zero genesis from the unanchored template.
    pub(crate) fn resolve_start_block(&self) -> Result<u64, L1SubscriberError> {
        let state = self.zone_provider.latest().map_err(eyre::Report::from)?;
        let local_checkpoint = state.tempo_num_hash().map_err(eyre::Report::from)?;
        if local_checkpoint.hash == B256::ZERO {
            return Err(eyre::eyre!("zone genesis is not anchored to an L1 block").into());
        }
        let local_tempo_block_number = local_checkpoint.number;
        info!(local_tempo_block_number, "Resuming from local zone state");
        Ok(local_tempo_block_number + 1)
    }

    /// Resolve the first L1 block that has not already been ingested.
    pub(crate) fn next_block_to_sync(&self) -> Result<u64, L1SubscriberError> {
        let resolved = self.resolve_start_block()?;
        let queued = self
            .deposit_queue
            .last_enqueued()
            .map(|last| last.number.saturating_add(1));
        let observed = self
            .block_tracker
            .latest()
            .map(|last| last.number.saturating_add(1));

        let next = [Some(resolved), queued, observed]
            .into_iter()
            .flatten()
            .max()
            .expect("resolved checkpoint is always present");

        // Only the persisted zone checkpoint proves consumption.
        // Queue and observation cursors are fetch high-water marks.
        self.block_tracker
            .initialize_consumed_through(resolved.saturating_sub(1));
        Ok(next)
    }

    /// Return the block number referenced by the L1 `finalized` tag.
    async fn finalized_block_number(
        &self,
        l1_provider: &impl Provider<TempoNetwork>,
    ) -> Result<u64, L1SubscriberError> {
        Ok(l1_provider
            .get_header_by_number(BlockNumberOrTag::Finalized)
            .await
            .inspect_err(|_| self.subscriber_metrics.fetch_failures.increment(1))?
            .map(|header| header.number())
            .ok_or_eyre("L1 finalized block is not available")?)
    }

    /// Synchronize all missing blocks through the current finalized L1 head.
    ///
    /// Callers provide the next block number and receive the next cursor after
    /// a successful sync.
    pub(crate) async fn sync_finalized_once(
        &self,
        l1_provider: &impl Provider<TempoNetwork>,
        next_block: u64,
    ) -> Result<u64, L1SubscriberError> {
        let finalized = self.finalized_block_number(l1_provider).await?;
        if next_block > finalized {
            self.record_seen_block(finalized, 0);
            return Ok(next_block);
        }

        let blocks = finalized - next_block + 1;
        self.record_seen_block(finalized, blocks);
        info!(
            from = next_block,
            to = finalized,
            blocks,
            "Synchronizing finalized L1 blocks"
        );

        let start = std::time::Instant::now();
        self.backfill(l1_provider, next_block, finalized).await?;
        self.subscriber_metrics
            .backfill_duration_seconds
            .record(start.elapsed().as_secs_f64());
        self.subscriber_metrics.current_l1_lag_blocks.set(0.0);
        Ok(finalized.saturating_add(1))
    }

    /// Follow finalized L1 using transport-specific head notifications as wakeups.
    ///
    /// Header contents are intentionally ignored. Canonical block selection is
    /// always based on the `finalized` tag read by [`Self::sync_finalized_once`].
    pub(crate) async fn follow_finalized(
        &self,
        l1_provider: &impl Provider<TempoNetwork>,
        mut stream: HeaderStream,
    ) -> Result<(), L1SubscriberError> {
        let checkpoint = self
            .zone_provider
            .latest()
            .map_err(eyre::Report::from)?
            .tempo_num_hash()
            .map_err(eyre::Report::from)?;
        if checkpoint.hash == B256::ZERO {
            return Err(eyre::eyre!("zone genesis is not anchored to an L1 block").into());
        }
        let mut next_block = self
            .block_tracker
            .state
            .write()
            .control_plane
            .get_or_insert(checkpoint)
            .number
            .saturating_add(1);

        // Subscribe before the initial sync so a head published while catching
        // up remains queued in the stream.
        next_block = self.sync_finalized_once(l1_provider, next_block).await?;

        while stream.next().await.is_some() {
            next_block = self.sync_finalized_once(l1_provider, next_block).await?;
        }

        Err(eyre::eyre!("L1 head notification stream ended").into())
    }

    /// Fetch and authenticate one finalized block before either reader uses its events.
    async fn fetch_block(
        &self,
        provider: &impl Provider<TempoNetwork>,
        block_number: u64,
    ) -> Result<(SealedHeader<TempoHeader>, L1ProcessedEvents), L1SubscriberError> {
        let start = std::time::Instant::now();
        let header_resp = provider
            .get_header_by_number(block_number.into())
            .await
            .inspect_err(|_| self.subscriber_metrics.fetch_failures.increment(1))?
            .ok_or_else(|| {
                self.subscriber_metrics.fetch_failures.increment(1);
                eyre::eyre!("L1 header not found for block {block_number}")
            })?;
        if header_resp.number() != block_number {
            return Err(L1SubscriberError::Fatal {
                block_number,
                stage: "L1 header number validation",
                source: eyre::eyre!(
                    "requested L1 block {block_number}, received {}",
                    header_resp.number()
                ),
            });
        }
        let header = SealedHeader::seal_slow(header_resp.inner.inner);
        let events = self.fetch_events(provider, &header).await?;
        debug!(
            block_number,
            elapsed_ms = start.elapsed().as_millis() as u64,
            "Fetched and validated L1 block data"
        );
        Ok((header, events))
    }

    async fn fetch_events(
        &self,
        provider: &impl Provider<TempoNetwork>,
        header: &SealedHeader<TempoHeader>,
    ) -> Result<L1ProcessedEvents, L1SubscriberError> {
        let block_number = header.number();
        let receipts = fetch_and_verify_receipts_for_header(
            provider,
            header.num_hash(),
            header.receipts_root(),
            header.logs_bloom(),
        )
        .await
        .inspect_err(|_| self.subscriber_metrics.fetch_failures.increment(1))?;
        // A recognized log that cannot be decoded must fail before any state is published.
        self.extract_events(header.num_hash(), &receipts)
            .inspect_err(|_| self.subscriber_metrics.decode_fence_failures.increment(1))
            .map_err(L1SubscriberError::fatal_from_err(
                block_number,
                "portal event decoding",
            ))
    }

    /// Apply finalized governance continuously, independent of execution backpressure.
    #[instrument(skip(self, l1_provider), fields(from, to))]
    async fn backfill(
        &self,
        l1_provider: &impl Provider<TempoNetwork>,
        from: u64,
        to: u64,
    ) -> Result<(), L1SubscriberError> {
        let mut fetched = futures::stream::iter(from..=to)
            .map(|number| self.fetch_block(l1_provider, number))
            .buffered(self.config.l1_fetch_concurrency.max(1));
        let mut processed = 0u64;
        let backfill_start = std::time::Instant::now();
        while let Some((sealed, processed_events)) = fetched.try_next().await? {
            let block_number = sealed.number();
            let events = &processed_events.portal_events;
            self.record_seen_block(block_number, to.saturating_sub(block_number));

            let anchor = sealed.num_hash();
            self.block_tracker
                .validate_control_plane_block(anchor, sealed.parent_hash())
                .map_err(L1SubscriberError::fatal_from_err(
                    block_number,
                    "control-plane continuity",
                ))?;
            if let Some(sink) = &self.leadership_sink {
                let transition =
                    events
                        .final_leader_transition()
                        .map_err(L1SubscriberError::fatal_from_err(
                            block_number,
                            "leadership event validation",
                        ))?;
                if let Some(transition) = transition {
                    sink.apply_leader_transition(transition)
                        .wrap_err_with(|| {
                            format!(
                                "cannot apply the leadership transition from block {block_number}"
                            )
                        })
                        .map_err(L1SubscriberError::fatal_from_err(
                            block_number,
                            "leadership transition application",
                        ))?;
                }
            }
            if let Some(keys) = &self.encryption_keys {
                for rotation in &events.encryption_key_rotations {
                    keys.apply_rotation(rotation)
                        .map_err(L1SubscriberError::fatal_from_err(
                            block_number,
                            "encryption key rotation application",
                        ))?;
                }
            }
            if let Some(paused) = processed_events.portal_pause {
                self.block_tracker
                    .observe_portal_pause(anchor, paused)
                    .map_err(L1SubscriberError::fatal_from_err(
                        block_number,
                        "portal pause observation",
                    ))?;
            }
            self.apply_enabled_token_events(events);
            self.update_l1_state_anchor(block_number, &processed_events.invalidated);
            self.record_portal_event_metrics(events);
            // Wake the queue reader only after all governance state is applied.
            {
                let mut state = self.block_tracker.state.write();
                // Preserve the execution window without making governance wait for its consumer.
                let consumed = *state.pruned_through.get_or_insert(from.saturating_sub(1));
                if block_number <= consumed.saturating_add(MAX_L1_LOOKAHEAD_BLOCKS) {
                    state
                        .pending
                        .insert(block_number, (sealed, processed_events));
                }
                state.control_plane = Some(anchor);
            }
            self.block_tracker.changed.send_replace(());
            processed += 1;

            if processed.is_multiple_of(100) {
                let elapsed = backfill_start.elapsed();
                let blocks_per_sec = processed as f64 / elapsed.as_secs_f64().max(0.001);
                info!(
                    processed,
                    current_block = block_number,
                    target = to,
                    remaining = to - block_number,
                    blocks_per_sec = format!("{blocks_per_sec:.1}"),
                    "Backfill progress"
                );
            }
        }

        let elapsed = backfill_start.elapsed();
        info!(
            from,
            to,
            blocks = to - from + 1,
            elapsed_ms = elapsed.as_millis() as u64,
            "Backfill complete"
        );
        Ok(())
    }

    /// Fill execution history up to the observer's cursor. A full queue only blocks this loop.
    pub(crate) async fn follow_anchors(
        &self,
        provider: &impl Provider<TempoNetwork>,
    ) -> Result<(), L1SubscriberError> {
        let mut changed = self.block_tracker.changed.subscribe();
        let mut next = self.next_block_to_sync()?;
        loop {
            if let Some(tip) = self.block_tracker.control_plane_latest()
                && next <= tip.number
            {
                if let Err(error) = self.backfill_anchors(provider, next, tip.number).await {
                    if !error.should_retry() {
                        return Err(error);
                    }
                    // A missing historical block must not cancel live L1 observation.
                    warn!(%error, next, "L1 execution history unavailable, retrying");
                    tokio::time::sleep(self.config.retry_connection_interval).await;
                    next = self.next_block_to_sync()?;
                    continue;
                }
                next = tip.number.saturating_add(1);
                continue;
            }
            changed
                .changed()
                .await
                .map_err(|_| eyre::eyre!("L1 block tracker closed"))?;
        }
    }

    pub(crate) async fn backfill_anchors(
        &self,
        provider: &impl Provider<TempoNetwork>,
        from: u64,
        to: u64,
    ) -> Result<(), L1SubscriberError> {
        let mut changed = self.block_tracker.changed.subscribe();
        let mut replay = VecDeque::new();
        for number in from..=to {
            while self
                .block_tracker
                .control_plane_latest()
                .is_none_or(|tip| tip.number < number)
            {
                changed
                    .changed()
                    .await
                    .map_err(|_| eyre::eyre!("L1 block tracker closed"))?;
            }
            self.block_tracker.wait_for_capacity(number).await?;
            // Take a clone until delivery completes, so reconnects cannot lose observer events.
            let cached = self
                .block_tracker
                .state
                .read()
                .pending
                .get(&number)
                .cloned();
            let (header, events) = if let Some(cached) = cached {
                replay.pop_front();
                cached
            } else {
                if replay.is_empty() {
                    let tip = self
                        .block_tracker
                        .control_plane_latest()
                        .expect("governance reached this block");
                    replay = self.replay_headers(provider, number, tip).await?;
                }
                let header = replay
                    .pop_front()
                    .ok_or_eyre("missing authenticated replay header")?;
                let events = self.fetch_events(provider, &header).await?;
                // Keep the in-flight replay block too: reconnects must retain observer progress.
                self.block_tracker
                    .state
                    .write()
                    .pending
                    .insert(number, (header.clone(), events.clone()));
                (header, events)
            };
            let anchor = header.num_hash();
            let parent_hash = header.parent_hash();
            if let Some(sender) = &self.finalized_batch_submissions {
                for submission in events.finalized_batches {
                    sender
                        .send(submission)
                        .await
                        .map_err(|_| L1SubscriberError::Fatal {
                            block_number: number,
                            stage: "finalized batch observer delivery",
                            source: eyre::eyre!(
                                "finalized batch submission observer is unavailable"
                            ),
                        })?;
                    // No await between delivery and removal: cancellation can only leave the
                    // undelivered suffix for the next connection.
                    self.block_tracker
                        .state
                        .write()
                        .pending
                        .get_mut(&number)
                        .expect("in-flight block retained")
                        .1
                        .finalized_batches
                        .pop_front();
                }
            }
            let appended = self
                .deposit_queue
                .try_enqueue_sealed(header, events.portal_events.clone())
                .wrap_err_with(|| {
                    format!(
                        "unexpected discontinuity while enqueueing L1 block {}",
                        anchor.number
                    )
                })?;
            if let Some(logs) = events.portal_logs {
                self.block_tracker.record_with_portal_evidence(
                    anchor,
                    parent_hash,
                    events.portal_events,
                    logs,
                )?;
            } else {
                self.block_tracker
                    .record_with_portal_events(anchor, events.portal_events)?;
            }
            self.block_tracker.state.write().pending.remove(&number);
            if appended {
                self.subscriber_metrics.blocks_enqueued.increment(1);
            }
        }
        Ok(())
    }

    /// Authenticate discarded history backwards from the exact governance tip before enqueueing.
    /// This path is used only after execution has fallen outside the retained window.
    /// Each parent hash depends on the preceding response, so this bounded walk is serial and
    /// must finish before execution advances. Large gaps can take substantial time to authenticate.
    pub(crate) async fn replay_headers(
        &self,
        provider: &impl Provider<TempoNetwork>,
        from: u64,
        tip: NumHash,
    ) -> Result<VecDeque<SealedHeader<TempoHeader>>, L1SubscriberError> {
        if from > tip.number {
            return Err(eyre::eyre!("replay start is above its governance anchor").into());
        }
        if tip.number - from >= MAX_L1_REPLAY_BLOCKS {
            return Err(eyre::eyre!("L1 replay gap {from}..={} exceeds the {MAX_L1_REPLAY_BLOCKS}-block recovery limit; operator recovery required", tip.number).into());
        }
        let mut expected_hash = tip.hash;
        let mut headers = VecDeque::new();
        for number in (from..=tip.number).rev() {
            let response = provider
                .get_header_by_hash(expected_hash)
                .await?
                .ok_or_eyre(format!("missing historical L1 header {expected_hash}"))?;
            let header = SealedHeader::seal_slow(response.inner.inner);
            if header.number() != number || header.hash() != expected_hash {
                return Err(L1SubscriberError::Fatal {
                    block_number: number,
                    stage: "historical L1 ancestry validation",
                    source: eyre::eyre!(
                        "historical header does not match governance ancestry at {number}"
                    ),
                });
            }
            expected_hash = header.parent_hash();
            headers.push_front(header);
        }
        Ok(headers)
    }

    /// Run the L1 subscriber, reconnecting after transient RPC failures.
    ///
    /// The subscriber follows only the L1 `finalized` tag. WebSocket
    /// `newHeads` or HTTP block-filter updates are used as wakeups; each
    /// notification ingests the missing finalized range in order.
    ///
    /// Transport and ordinary failures reconnect after the configured retry interval.
    /// Deterministic failures while applying a receipt-verified finalized block are fatal.
    pub async fn run(self) {
        loop {
            let result = async {
                let provider = self.connect().await?;
                let header_stream = self.subscribe_block_headers(&provider).await?;
                info!(
                    portal = %self.config.portal_address,
                    "Following finalized L1 blocks"
                );
                tokio::try_join!(
                    self.follow_finalized(&provider, header_stream),
                    self.follow_anchors(&provider),
                )
            }
            .await;

            if let Err(error) = result {
                // Retry connection and sync errors, finalized event errors are fatal.
                if error.should_retry() {
                    let retry_interval = self.config.retry_connection_interval;
                    self.subscriber_metrics.reconnects.increment(1);
                    error!(
                        error = %error,
                        retry_secs = retry_interval.as_secs_f32(),
                        "L1 subscriber failed, reconnecting after retry interval"
                    );
                    tokio::time::sleep(retry_interval).await;
                } else {
                    panic!("{error}");
                }
            }
        }
    }

    /// Extract portal events and raw-cache mutation barriers from fetched receipts.
    ///
    /// A decode failure of a portal log is an error for the whole block. A silently dropped
    /// event would diverge this node from its peers.
    pub(crate) fn extract_events(
        &self,
        block: NumHash,
        receipts: &[tempo_alloy::rpc::TempoTransactionReceipt],
    ) -> eyre::Result<L1ProcessedEvents> {
        let block_number = block.number;
        let portal_address = self.config.portal_address;
        let mut portal_events = L1PortalEvents::default();
        let mut invalidated = HashSet::new();
        let mut portal_logs = self.config.retain_portal_evidence.then(Vec::new);
        let finalized_batches = if self.finalized_batch_submissions.is_some() {
            extract_finalized_batch_submissions(block, portal_address, receipts)
        } else {
            Vec::new()
        };
        let mut portal_pause = None;

        for receipt in receipts {
            let retain_receipt_logs = portal_logs.is_some() && receipt.status();
            for log in receipt.logs() {
                let address = log.address();

                if address == portal_address {
                    if retain_receipt_logs && let Some(logs) = &mut portal_logs {
                        logs.push(log.inner.clone());
                    }
                    invalidated.insert(address);
                    if let Some(address) =
                        portal_event_cache_invalidation_address(log.topics().first())
                    {
                        invalidated.insert(address);
                    }
                    if let Some(transition) = portal_events
                        .push_log(log, block_number)
                        .wrap_err_with(|| {
                            format!("failed to decode a portal event in L1 block {block_number}")
                        })?
                    {
                        // The last transition in canonical log order is the block's final state.
                        portal_pause = Some(transition);
                    }
                } else if let Some(address) = cache_invalidation_address(address, log.topic0()) {
                    invalidated.extend([address, log.address()]);
                }
            }
        }

        // Enabling may migrate token-local policy storage into TIP-403.
        for event in &portal_events.enabled_tokens {
            invalidated.extend([event.token, TIP403_REGISTRY_ADDRESS]);
        }
        Ok(L1ProcessedEvents {
            portal_events,
            invalidated,
            portal_logs,
            finalized_batches: finalized_batches.into(),
            portal_pause,
        })
    }

    fn record_seen_block(&self, block_number: u64, lag_blocks: u64) {
        self.subscriber_metrics
            .latest_l1_block_seen
            .set(block_number as f64);
        self.subscriber_metrics
            .current_l1_lag_blocks
            .set(lag_blocks as f64);
    }

    fn record_portal_event_metrics(&self, portal_events: &L1PortalEvents) {
        let mut withdrawal_bounce_backs = 0u64;
        let mut deposits = 0u64;
        for deposit in &portal_events.deposits {
            match deposit {
                L1Deposit::WithdrawalBounceBack(_) => withdrawal_bounce_backs += 1,
                L1Deposit::Deposit(_) => deposits += 1,
            }
        }
        if withdrawal_bounce_backs > 0 {
            self.subscriber_metrics
                .withdrawal_bounce_back_events
                .increment(withdrawal_bounce_backs);
        }
        if deposits > 0 {
            self.subscriber_metrics.deposit_events.increment(deposits);
        }
        if !portal_events.enabled_tokens.is_empty() {
            self.subscriber_metrics
                .token_enabled_events
                .increment(portal_events.enabled_tokens.len() as u64);
        }
        if !portal_events.leader_transitions.is_empty() {
            self.subscriber_metrics
                .leader_updated_events
                .increment(portal_events.leader_transitions.len() as u64);
        }
    }

    /// Add tokens discovered in finalized portal events to the shared registry.
    pub(crate) fn apply_enabled_token_events(&self, portal_events: &L1PortalEvents) {
        if portal_events.enabled_tokens.is_empty() {
            return;
        }

        let mut enabled_tokens = self.enabled_tokens.write();
        for enabled in &portal_events.enabled_tokens {
            if enabled_tokens.insert(enabled.token) {
                info!(token = %enabled.token, "New token enabled");
            }
        }
    }

    /// Update the L1 state cache anchor to the latest ingested finalized block.
    pub(crate) fn update_l1_state_anchor(
        &self,
        number: u64,
        invalidated_accounts: &HashSet<Address>,
    ) {
        self.l1_state_cache
            .lock()
            .invalidate_and_set_anchor(number, invalidated_accounts.iter().copied());
    }
}

/// Fetch receipts for the L1 header by block hash and verify they match the
/// header's receipts root and logs bloom before returning them.
async fn fetch_and_verify_receipts_for_header(
    provider: &impl Provider<TempoNetwork>,
    block: NumHash,
    expected_receipts_root: B256,
    expected_logs_bloom: Bloom,
) -> Result<Vec<tempo_alloy::rpc::TempoTransactionReceipt>, L1SubscriberError> {
    let block_number = block.number;
    let block_hash = block.hash;
    let receipts = provider
        .get_block_receipts(BlockId::hash(block_hash))
        .await?
        .ok_or_eyre(format!(
            "no receipts for block {block_number} ({block_hash})"
        ))?;
    verify_receipts_against_header(
        block,
        expected_receipts_root,
        expected_logs_bloom,
        &receipts,
    )?;
    Ok(receipts)
}

/// Verify that RPC receipts reproduce an authenticated Tempo header's receipt root and log bloom.
pub fn verify_receipts_against_header(
    block: NumHash,
    expected_receipts_root: B256,
    expected_logs_bloom: Bloom,
    receipts: &[tempo_alloy::rpc::TempoTransactionReceipt],
) -> eyre::Result<()> {
    let block_number = block.number;
    let block_hash = block.hash;
    let receipts = receipts
        .iter()
        .map(|receipt| {
            receipt
                .inner
                .inner
                .clone()
                .map_receipt(|receipt| receipt.map_logs(Into::into))
        })
        .collect::<Vec<_>>();
    let computed_receipts_root = alloy_consensus::proofs::calculate_receipt_root(&receipts);
    if computed_receipts_root != expected_receipts_root {
        eyre::bail!(
            "receipt root mismatch for L1 block {block_number} ({block_hash}): expected {expected_receipts_root}, got {computed_receipts_root}"
        );
    }
    let computed_logs_bloom = receipts
        .iter()
        .fold(Bloom::ZERO, |bloom, receipt| bloom | receipt.bloom_ref());
    if computed_logs_bloom != expected_logs_bloom {
        eyre::bail!(
            "logs bloom mismatch for L1 block {block_number} ({block_hash}): expected {expected_logs_bloom}, got {computed_logs_bloom}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn token_policy_updates_invalidate_the_registry() {
        let token = address!("20C0000000000000000000000000000000000999");

        assert_eq!(
            cache_invalidation_address(token, Some(&TransferPolicyUpdate::SIGNATURE_HASH)),
            Some(TIP403_REGISTRY_ADDRESS)
        );
    }

    #[test]
    fn token_enabled_events_invalidate_the_registry() {
        assert_eq!(
            portal_event_cache_invalidation_address(Some(&TokenEnabled::SIGNATURE_HASH)),
            Some(TIP403_REGISTRY_ADDRESS)
        );
        assert_eq!(portal_event_cache_invalidation_address(None), None);
    }
}
