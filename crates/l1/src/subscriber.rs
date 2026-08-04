use super::*;
use eyre::WrapErr as _;
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
    latest: Option<NumHash>,
    pruned_through: Option<u64>,
}

#[derive(Debug, Clone)]
struct L1BlockObservation {
    hash: B256,
    portal_events: L1PortalEvents,
}

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

    /// Record an independently validated and applied L1 anchor.
    pub fn record(&self, block: NumHash) -> eyre::Result<()> {
        self.record_with_portal_events(block, L1PortalEvents::default())
    }

    /// Record an L1 anchor together with portal events decoded from its verified receipts.
    pub fn record_with_portal_events(
        &self,
        block: NumHash,
        portal_events: L1PortalEvents,
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
        state.observed.retain(|height, _| *height > number);
        state.pruned_through = Some(state.pruned_through.map_or(number, |old| old.max(number)));
        drop(state);
        self.changed.send_replace(());
    }
}

/// Poll interval for the HTTP block filter fallback (500ms, matching L1 block time).
const HTTP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

type L1ProcessedEvents = (L1PortalEvents, HashSet<Address>);

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
/// An error fences ingestion of the whole L1 block.
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
    /// Shared registry of tokens enabled for this zone.
    pub enabled_tokens: crate::state::EnabledTokenRegistry,
    /// Shared L1 state cache. The subscriber updates the cache anchor on each
    /// finalized block.
    pub l1_state_cache: crate::state::cache::L1StateCache,
    /// Validated and applied L1 anchors shared with follower block import.
    pub block_tracker: L1BlockTracker,
    /// Maximum number of concurrent header and receipt fetches while syncing a
    /// finalized L1 range.
    pub l1_fetch_concurrency: usize,
    /// Interval between L1 connection attempts.
    pub retry_connection_interval: std::time::Duration,
    /// Optional sink that receives leadership transitions before the block that
    /// recorded them is enqueued.
    pub leadership_sink: Option<Arc<dyn LeadershipSink>>,
    /// Private encryption keys bound by finalized Portal rotation events.
    pub encryption_keys: Option<crate::EncryptionKeyRing>,
}

pub(crate) trait LocalTempoCheckpointReader: Send + Sync {
    fn latest_tempo_checkpoint(&self) -> eyre::Result<NumHash>;
}

struct ProviderLocalTempoCheckpointReader<P> {
    provider: P,
}

impl<P> LocalTempoCheckpointReader for ProviderLocalTempoCheckpointReader<P>
where
    P: StateProviderFactory + Clone + Send + Sync + 'static,
{
    fn latest_tempo_checkpoint(&self) -> eyre::Result<NumHash> {
        let state = self.provider.latest()?;
        Ok(state.tempo_num_hash()?)
    }
}

/// L1 chain subscriber that listens for new blocks and extracts deposit events.
#[derive(Clone)]
pub struct L1Subscriber {
    pub(crate) config: L1SubscriberConfig,
    pub(crate) local_state: Arc<dyn LocalTempoCheckpointReader>,
    /// Finalized L1 blocks retained until a Zone consumer processes them.
    pub(crate) deposit_queue: DepositQueue,
    /// L1 subscriber metrics for connection health, backfill, and event ingestion.
    pub(crate) subscriber_metrics: crate::metrics::L1SubscriberMetrics,
}

/// A deterministic failure while applying an already receipt-verified finalized block.
///
/// Reconnecting cannot change these inputs, so retrying would loop forever on the same block.
#[derive(Debug)]
struct FencedIngestionError {
    block_number: u64,
    stage: &'static str,
    source: eyre::Report,
}

impl FencedIngestionError {
    const fn new(block_number: u64, stage: &'static str, source: eyre::Report) -> Self {
        Self {
            block_number,
            stage,
            source,
        }
    }
}

impl std::fmt::Display for FencedIngestionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "L1 ingestion fenced at block {} during {}: {}",
            self.block_number, self.stage, self.source
        )
    }
}

impl std::error::Error for FencedIngestionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

fn fenced_ingestion_error(error: &eyre::Report) -> Option<&FencedIngestionError> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<FencedIngestionError>())
}

#[cfg(test)]
pub(crate) fn is_fenced_ingestion_error(error: &eyre::Report) -> bool {
    fenced_ingestion_error(error).is_some()
}

impl L1Subscriber {
    /// Create and spawn the L1 subscriber as a critical background task.
    ///
    /// Transient failures reconnect after the configured retry interval. Deterministic failures
    /// while applying a receipt-verified finalized block fail the critical task instead.
    pub fn spawn<P>(
        config: L1SubscriberConfig,
        local_state_provider: P,
        deposit_queue: DepositQueue,
        task_executor: reth_tasks::Runtime,
    ) where
        P: StateProviderFactory + Clone + Send + Sync + 'static,
    {
        let subscriber = Self {
            config,
            local_state: Arc::new(ProviderLocalTempoCheckpointReader {
                provider: local_state_provider,
            }),
            deposit_queue,
            subscriber_metrics: Default::default(),
        };

        task_executor.spawn_critical_task(
            "l1-block-subscriber",
            Box::pin(async move {
                loop {
                    if let Err(e) = subscriber.run().await {
                        if let Some(fenced) = fenced_ingestion_error(&e) {
                            error!(
                                block_number = fenced.block_number,
                                stage = fenced.stage,
                                error = %e,
                                "Fatal L1 ingestion fence; refusing to retry an immutable finalized block"
                            );
                            // This is a critical task: panicking notifies the task manager and
                            // shuts the node down instead of leaving it alive with frozen L1 state.
                            panic!("{fenced}");
                        }
                        let retry_interval = subscriber.config.retry_connection_interval;
                        subscriber.subscriber_metrics.reconnects.increment(1);
                        error!(
                            error = %e,
                            retry_secs = retry_interval.as_secs_f32(),
                            "L1 subscriber failed, reconnecting after retry interval"
                        );
                        tokio::time::sleep(retry_interval).await;
                    }
                }
            }),
        );
    }

    /// Connect to the L1 node.
    ///
    /// The transport (HTTP or WebSocket) is auto-detected from the URL scheme.
    #[instrument(skip(self), fields(l1_rpc_url = %self.config.l1_rpc_url))]
    async fn connect(&self) -> eyre::Result<DynProvider<TempoNetwork>> {
        info!(url = %self.config.l1_rpc_url, "Connecting to L1 node");

        let url: url::Url = self.config.l1_rpc_url.parse()?;
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

    /// Return transport-appropriate L1 head notifications as unit triggers.
    ///
    /// WebSocket connections use `newHeads`. When pubsub is unavailable, HTTP
    /// connections fall back to `eth_newBlockFilter` / `eth_getFilterChanges`.
    /// Trigger payloads are ignored because block selection always comes from
    /// the L1 `finalized` tag.
    pub(crate) async fn head_triggers<'a>(
        &self,
        provider: &'a DynProvider<TempoNetwork>,
    ) -> eyre::Result<Pin<Box<dyn Stream<Item = eyre::Result<()>> + Send + 'a>>> {
        match provider.subscribe_blocks().await {
            Ok(subscription) => {
                info!("Using WebSocket newHeads notifications");
                Ok(Box::pin(subscription.into_stream().map(|_| Ok(()))))
            }
            Err(err)
                if err
                    .as_transport_err()
                    .is_some_and(|transport| transport.is_pubsub_unavailable()) =>
            {
                info!("Pubsub unavailable, using HTTP block filter polling");
                let mut watcher = provider.watch_blocks().await?;
                watcher.set_poll_interval(HTTP_POLL_INTERVAL);
                Ok(Box::pin(
                    watcher.into_stream().filter_map(|hashes| async move {
                        (!hashes.is_empty()).then_some(Ok::<(), eyre::Report>(()))
                    }),
                ))
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Determine the starting block number for backfill.
    ///
    /// The zone's persisted Tempo checkpoint is the authoritative source for
    /// where ingestion resumes. A non-zero hash distinguishes an L1-anchored
    /// block-zero genesis from the unanchored template.
    pub(crate) fn resolve_start_block(&self) -> eyre::Result<u64> {
        let local_checkpoint = self.local_state.latest_tempo_checkpoint()?;
        eyre::ensure!(
            local_checkpoint.hash != B256::ZERO,
            "zone genesis is not anchored to an L1 block"
        );
        let local_tempo_block_number = local_checkpoint.number;
        info!(local_tempo_block_number, "Resuming from local zone state");
        Ok(local_tempo_block_number + 1)
    }

    /// Resolve the first L1 block that has not already been ingested.
    pub(crate) fn next_block_to_sync(&self) -> eyre::Result<u64> {
        let resolved = self.resolve_start_block()?;
        let queued = self
            .deposit_queue
            .last_enqueued()
            .map(|last| last.number.saturating_add(1));
        let observed = self
            .config
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
        self.config
            .block_tracker
            .initialize_consumed_through(resolved.saturating_sub(1));
        Ok(next)
    }

    /// Return the block number referenced by the L1 `finalized` tag.
    async fn finalized_block_number(
        &self,
        l1_provider: &impl Provider<TempoNetwork>,
    ) -> eyre::Result<u64> {
        l1_provider
            .get_header_by_number(BlockNumberOrTag::Finalized)
            .await
            .inspect_err(|_| self.subscriber_metrics.fetch_failures.increment(1))?
            .map(|header| header.number())
            .ok_or_else(|| eyre::eyre!("L1 finalized block is not available"))
    }

    /// Synchronize all missing blocks through the current finalized L1 head.
    ///
    /// Callers provide the next block number and receive the next cursor after
    /// a successful sync.
    pub(crate) async fn sync_finalized_once(
        &self,
        l1_provider: &impl Provider<TempoNetwork>,
        next_block: u64,
    ) -> eyre::Result<u64> {
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
    pub(crate) async fn follow_finalized<S>(
        &self,
        l1_provider: &impl Provider<TempoNetwork>,
        triggers: S,
    ) -> eyre::Result<()>
    where
        S: Stream<Item = eyre::Result<()>> + Send,
    {
        let mut triggers = Box::pin(triggers);
        let mut next_block = self.next_block_to_sync()?;

        // Subscribe before the initial sync so a head published while catching
        // up remains queued as another trigger.
        next_block = self.sync_finalized_once(l1_provider, next_block).await?;

        while let Some(trigger) = triggers.next().await {
            trigger?;
            next_block = self.sync_finalized_once(l1_provider, next_block).await?;
        }

        Err(eyre::eyre!("L1 head notification stream ended"))
    }

    /// Backfill L1 blocks from `from..=to` with pipelined RPC fetching.
    ///
    /// Fetches headers and receipts for up to `l1_fetch_concurrency` blocks in
    /// parallel, then processes them sequentially (event extraction and enqueue).
    /// Receipts are fetched by the corresponding block
    /// hash and validated against the header's receipts root before processing.
    #[instrument(skip(self, l1_provider), fields(from, to))]
    async fn backfill(
        &self,
        l1_provider: &impl Provider<TempoNetwork>,
        from: u64,
        to: u64,
    ) -> eyre::Result<()> {
        use futures::stream;

        let concurrency = self.config.l1_fetch_concurrency.max(1);
        let subscriber_metrics = self.subscriber_metrics.clone();
        let block_tracker = self.config.block_tracker.clone();

        let mut fetched = stream::iter(from..=to)
            .map(move |block_number| {
                let provider = l1_provider;
                let subscriber_metrics = subscriber_metrics.clone();
                let block_tracker = block_tracker.clone();
                async move {
                    block_tracker.wait_for_capacity(block_number).await?;
                    let start = std::time::Instant::now();
                    let fetch_failures = &subscriber_metrics.fetch_failures;
                    let header_resp = async {
                        provider
                            .get_header_by_number(block_number.into())
                            .await?
                            .ok_or_else(|| {
                                eyre::eyre!("L1 header not found for block {block_number}")
                            })
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
                    Ok::<_, eyre::Report>((header, receipts))
                }
            })
            .buffered(concurrency);

        let mut processed = 0u64;
        let backfill_start = std::time::Instant::now();

        while let Some((header, receipts)) = fetched.try_next().await? {
            let block_number = header.number();
            // Decoding fails closed: a decode failure of a recognized portal log aborts this
            // block before anything is enqueued or any cache advances. Ingestion halts here (fenced)
            // instead of continuing under a partial event view.
            let (events, invalidated) =
                self.extract_events(block_number, &receipts)
                    .map_err(|err| {
                        self.subscriber_metrics.decode_fence_failures.increment(1);
                        FencedIngestionError::new(block_number, "portal event decoding", err)
                    })?;
            self.record_seen_block(block_number, to.saturating_sub(block_number));

            let sealed = SealedHeader::seal_slow(header);
            let anchor = sealed.num_hash();
            // Publish the leadership transition _before_ the activation block becomes
            // consumable.
            if let Some(sink) = &self.config.leadership_sink {
                let transition = events.final_leader_transition().map_err(|err| {
                    FencedIngestionError::new(block_number, "leadership event validation", err)
                })?;
                if let Some(transition) = transition {
                    sink.apply_leader_transition(transition)
                        .wrap_err_with(|| {
                            format!(
                                "cannot apply the leadership transition from block {block_number}"
                            )
                        })
                        .map_err(|err| {
                            FencedIngestionError::new(
                                block_number,
                                "leadership transition application",
                                err,
                            )
                        })?;
                }
            }
            if let Some(keys) = &self.config.encryption_keys {
                for rotation in &events.encryption_key_rotations {
                    keys.apply_rotation(rotation).map_err(|err| {
                        FencedIngestionError::new(
                            block_number,
                            "encryption key rotation application",
                            err,
                        )
                    })?;
                }
            }
            let appended = self
                .deposit_queue
                .try_enqueue_sealed(sealed, events.clone())
                .wrap_err_with(|| {
                    format!("unexpected discontinuity while enqueueing L1 block {block_number}")
                })?;
            self.config
                .block_tracker
                .record_with_portal_events(anchor, events.clone())?;
            // Publish derived L1 state only after the header has been admitted to every
            // configured retention sink and the contiguous observation tracker.
            self.apply_enabled_token_events(&events);
            self.update_l1_state_anchor(block_number, &invalidated);
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

    /// Run the L1 subscriber until an RPC operation fails.
    ///
    /// The subscriber follows only the L1 `finalized` tag. WebSocket
    /// `newHeads` or HTTP block-filter updates are used as wakeups; each
    /// notification ingests the missing finalized range in order.
    ///
    /// [`Self::spawn`] retries transient errors and treats deterministic finalized-block
    /// ingestion failures as fatal.
    pub async fn run(&self) -> eyre::Result<()> {
        let provider = self.connect().await?;
        let triggers = self.head_triggers(&provider).await?;
        info!(
            portal = %self.config.portal_address,
            "Following finalized L1 blocks"
        );
        self.follow_finalized(&provider, triggers).await
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

        for receipt in receipts {
            for log in receipt.logs() {
                let address = log.address();

                if address == portal_address {
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
        Ok((portal_events, invalidated))
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

        let mut enabled_tokens = self.config.enabled_tokens.write();
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
        self.config
            .l1_state_cache
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
) -> eyre::Result<Vec<tempo_alloy::rpc::TempoTransactionReceipt>> {
    let block_number = block.number;
    let block_hash = block.hash;
    let receipts = provider
        .get_block_receipts(BlockId::hash(block_hash))
        .await?
        .ok_or_else(|| eyre::eyre!("no receipts for block {block_number} ({block_hash})"))?;
    verify_receipts(
        block,
        expected_receipts_root,
        expected_logs_bloom,
        &receipts,
    )?;
    Ok(receipts)
}

pub(crate) fn verify_receipts(
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
