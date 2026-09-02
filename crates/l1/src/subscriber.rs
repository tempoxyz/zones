use super::*;
use crate::{
    EncryptionKeyRing, L1StateCache, metrics::L1SubscriberMetrics, state::EnabledTokenRegistry,
};
use eyre::{OptionExt as _, WrapErr as _};
use std::collections::HashSet;
use tempo_contracts::precompiles::{ITIP20::TransferPolicyUpdate, TIP403_REGISTRY_ADDRESS};
use tempo_primitives::is_tip20_prefix;

use std::collections::BTreeMap;

/// Maximum number of authenticated L1 blocks the subscriber may retain ahead of the Zone
/// consumer's imported Tempo checkpoint (approximately one hour at Tempo's 500ms block time).
pub const MAX_L1_LOOKAHEAD_BLOCKS: u64 = 7_200;

#[derive(Debug, Default)]
struct L1BlockTrackerState {
    observed: BTreeMap<u64, L1BlockObservation>,
    recent_portal_evidence: BTreeMap<u64, AuthenticatedPortalLogs>,
    latest: Option<NumHash>,
    pruned_through: Option<u64>,
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

    /// Return the highest independently observed L1 anchor.
    pub fn latest(&self) -> Option<NumHash> {
        self.state.read().latest
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

type L1ProcessedEvents = (
    L1PortalEvents,
    HashSet<Address>,
    Option<Vec<alloy_primitives::Log>>,
);

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
#[derive(Clone)]
pub struct L1Subscriber<P> {
    pub(crate) config: L1SubscriberConfig,
    pub(crate) provider: P,
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
        provider: P,
        deposit_queue: DepositQueue,
        enabled_tokens: EnabledTokenRegistry,
        l1_state_cache: L1StateCache,
        block_tracker: L1BlockTracker,
        leadership_sink: Option<Arc<dyn LeadershipSink>>,
        encryption_keys: Option<EncryptionKeyRing>,
    ) -> Self {
        Self {
            config,
            provider,
            deposit_queue,
            enabled_tokens,
            l1_state_cache,
            block_tracker,
            leadership_sink,
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
        let state = self.provider.latest().map_err(eyre::Report::from)?;
        let local_checkpoint = state.tempo_num_hash().map_err(eyre::Report::from)?;
        if local_checkpoint.hash == B256::ZERO {
            return Err(eyre::eyre!("zone genesis is not anchored to an L1 block").into());
        }
        let local_tempo_block_number = local_checkpoint.number;
        info!(local_tempo_block_number, "Resuming from local zone state");
        Ok(local_tempo_block_number + 1)
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
    /// The cursor advances after each block is fully applied.
    pub(crate) async fn sync_finalized_once(
        &self,
        l1_provider: &impl Provider<TempoNetwork>,
        next_block: &mut u64,
    ) -> Result<(), L1SubscriberError> {
        let finalized = self.finalized_block_number(l1_provider).await?;
        if *next_block > finalized {
            self.record_seen_block(finalized, 0);
            return Ok(());
        }

        let blocks = finalized - *next_block + 1;
        self.record_seen_block(finalized, blocks);
        info!(
            from = *next_block,
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
        Ok(())
    }

    /// Backfill L1 blocks from `from..=to` with pipelined RPC fetching.
    ///
    /// Fetches headers and receipts for up to `l1_fetch_concurrency` blocks in
    /// parallel, then processes them sequentially (event extraction and enqueue).
    /// Receipts are fetched by the corresponding block
    /// hash and validated against the header's receipts root before processing.
    #[instrument(skip(self, l1_provider, next_block), fields(from = *next_block, to))]
    async fn backfill(
        &self,
        l1_provider: &impl Provider<TempoNetwork>,
        next_block: &mut u64,
        to: u64,
    ) -> Result<(), L1SubscriberError> {
        use futures::stream;

        let from = *next_block;
        let concurrency = self.config.l1_fetch_concurrency.max(1);
        let subscriber_metrics = self.subscriber_metrics.clone();
        let block_tracker = self.block_tracker.clone();

        let mut fetched = stream::iter(from..=to)
            .map(move |block_number| {
                let provider = l1_provider;
                let subscriber_metrics = subscriber_metrics.clone();
                let block_tracker = block_tracker.clone();
                async move {
                    block_tracker.wait_for_capacity(block_number).await?;
                    let start = std::time::Instant::now();
                    let fetch_failures = &subscriber_metrics.fetch_failures;
                    let header_resp =
                        async {
                            let header = provider.get_header_by_number(block_number.into()).await?;
                            Ok::<_, L1SubscriberError>(header.ok_or_eyre(format!(
                                "L1 header not found for block {block_number}"
                            ))?)
                        }
                        .await
                        .inspect_err(|_| {
                            fetch_failures.increment(1);
                        })?;
                    let block_hash = header_resp.hash();
                    let block = NumHash::new(block_number, block_hash);
                    let expected_receipts_root = header_resp.receipts_root();
                    let expected_logs_bloom = header_resp.logs_bloom();
                    let receipts = fetch_and_verify_receipts_for_header(
                        provider,
                        block,
                        expected_receipts_root,
                        expected_logs_bloom,
                    )
                    .await
                    .inspect_err(|_| {
                        fetch_failures.increment(1);
                    })?;
                    let elapsed = start.elapsed();
                    debug!(
                        block_number,
                        %block_hash,
                        elapsed_ms = elapsed.as_millis() as u64,
                        receipts = receipts.len(),
                        "Fetched and validated L1 block data"
                    );
                    let header = header_resp.inner.inner;
                    Ok::<_, L1SubscriberError>((header, receipts))
                }
            })
            .buffered(concurrency);

        let mut processed = 0u64;
        let backfill_start = std::time::Instant::now();

        while let Some((header, receipts)) = fetched.try_next().await? {
            let block_number = header.number();
            // Decoding fails closed: a decode failure of a recognized portal log aborts this
            // block before anything is enqueued or any cache advances.
            let processed_events = self
                .extract_events(block_number, &receipts)
                .inspect_err(|_| {
                    self.subscriber_metrics.decode_fence_failures.increment(1);
                })
                .map_err(L1SubscriberError::fatal_from_err(
                    block_number,
                    "portal event decoding",
                ))?;
            let (events, invalidated, portal_logs) = processed_events;
            self.record_seen_block(block_number, to.saturating_sub(block_number));

            let sealed = SealedHeader::seal_slow(header);
            let anchor = sealed.num_hash();
            let portal_evidence = portal_logs.map(|logs| (sealed.parent_hash(), logs));
            // Publish the leadership transition _before_ the activation block becomes
            // consumable.
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
            let appended = self
                .deposit_queue
                .try_enqueue_sealed(sealed, events.clone())
                .wrap_err_with(|| {
                    format!("unexpected discontinuity while enqueueing L1 block {block_number}")
                })?;
            if let Some((parent_hash, logs)) = portal_evidence {
                self.block_tracker.record_with_portal_evidence(
                    anchor,
                    parent_hash,
                    events.clone(),
                    logs,
                )?;
            } else {
                self.block_tracker
                    .record_with_portal_events(anchor, events.clone())?;
            }
            // Publish derived L1 state only after the header has been admitted to every
            // configured retention sink and the contiguous observation tracker.
            self.apply_enabled_token_events(&events);
            self.update_l1_state_anchor(block_number, &invalidated);
            *next_block = block_number.saturating_add(1);
            if appended {
                self.subscriber_metrics.blocks_enqueued.increment(1);
            }
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

    /// Run the L1 subscriber, reconnecting after transient RPC failures.
    ///
    /// The subscriber follows only the L1 `finalized` tag. WebSocket
    /// `newHeads` or HTTP block-filter updates are used as wakeups; each
    /// notification ingests the missing finalized range in order.
    ///
    /// Transport and ordinary failures reconnect after the configured retry interval.
    /// Deterministic failures while applying a receipt-verified finalized block are fatal.
    pub async fn run(self) -> Result<(), L1SubscriberError> {
        let mut next_block = self.resolve_start_block()?;
        self.block_tracker
            .initialize_consumed_through(next_block.saturating_sub(1));

        loop {
            let result: Result<(), L1SubscriberError> = async {
                let provider = self.connect().await?;
                let mut header_stream = self.subscribe_block_headers(&provider).await?;
                info!(
                    portal = %self.config.portal_address,
                    "Following finalized L1 blocks"
                );

                // Subscribe before the initial sync so a head published while catching
                // up remains queued in the stream.
                self.sync_finalized_once(&provider, &mut next_block).await?;
                while let Some(_) = header_stream.next().await {
                    self.sync_finalized_once(&provider, &mut next_block).await?;
                }

                Err(eyre::eyre!("L1 head notification stream ended").into())
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
                    return Err(error);
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
        block_number: u64,
        receipts: &[tempo_alloy::rpc::TempoTransactionReceipt],
    ) -> eyre::Result<L1ProcessedEvents> {
        let portal_address = self.config.portal_address;
        let mut portal_events = L1PortalEvents::default();
        let mut invalidated = HashSet::new();
        let mut portal_logs = self.config.retain_portal_evidence.then(Vec::new);

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
                    portal_events
                        .push_log(log, block_number)
                        .wrap_err_with(|| {
                            format!("failed to decode a portal event in L1 block {block_number}")
                        })?;
                } else if let Some(address) = cache_invalidation_address(address, log.topic0()) {
                    invalidated.extend([address, log.address()]);
                }
            }
        }

        // Enabling may migrate token-local policy storage into TIP-403.
        for event in &portal_events.enabled_tokens {
            invalidated.extend([event.token, TIP403_REGISTRY_ADDRESS]);
        }
        self.record_portal_event_metrics(&portal_events);
        Ok((portal_events, invalidated, portal_logs))
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
