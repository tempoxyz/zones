use super::*;

/// Poll interval for the HTTP block filter fallback (500ms, matching L1 block time).
const HTTP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Configuration for the L1 subscriber.
#[derive(Debug, Clone)]
pub struct L1SubscriberConfig {
    /// RPC URL of the L1 node (HTTP or WebSocket).
    pub l1_rpc_url: String,
    /// ZonePortal contract address on L1.
    pub portal_address: Address,
    /// Optional genesis Tempo block number override. When set, used instead of
    /// the portal's on-chain `genesisTempoBlockNumber` (which may be 0 for
    /// portals not created via ZoneFactory).
    pub genesis_tempo_block_number: Option<u64>,
    /// Shared TIP-403 policy cache. The subscriber applies policy events
    /// extracted from L1 receipts directly into this cache before enqueuing
    /// blocks.
    pub policy_cache: crate::state::tip403::PolicyCache,
    /// Shared L1 state cache. The subscriber updates the cache anchor on each
    /// confirmed block and clears it on reorgs.
    pub l1_state_cache: crate::state::cache::L1StateCache,
    /// Maximum number of concurrent L1 RPC receipt fetches. Used directly for
    /// the live stream and halved for backfill (which sends 2 requests per block).
    pub l1_fetch_concurrency: usize,
    /// Interval between WebSocket reconnection attempts.
    pub retry_connection_interval: std::time::Duration,
}

pub(crate) trait LocalTempoStateReader: Send + Sync {
    fn latest_tempo_block_number(&self) -> eyre::Result<u64>;
}

struct ProviderLocalTempoStateReader<P> {
    provider: P,
}

impl<P> LocalTempoStateReader for ProviderLocalTempoStateReader<P>
where
    P: StateProviderFactory + Clone + Send + Sync + 'static,
{
    fn latest_tempo_block_number(&self) -> eyre::Result<u64> {
        let state = self.provider.latest()?;
        Ok(state.tempo_block_number()?)
    }
}

/// L1 chain subscriber that listens for new blocks and extracts deposit events.
#[derive(Clone)]
pub struct L1Subscriber {
    pub(crate) config: L1SubscriberConfig,
    pub(crate) local_state: Arc<dyn LocalTempoStateReader>,
    pub(crate) deposit_queue: DepositQueue,
    /// Mutable set of token addresses tracked for TIP-403 policy events.
    /// Initialized from config, grows dynamically when `TokenEnabled` events are seen.
    pub(crate) tracked_tokens: Vec<Address>,
    /// TIP-403 metrics (cache sizes, events applied).
    pub(crate) tip403_metrics: crate::state::tip403::Tip403Metrics,
    /// L1 subscriber metrics for connection health, backfill, and event ingestion.
    pub(crate) subscriber_metrics: crate::metrics::L1SubscriberMetrics,
}

impl L1Subscriber {
    /// Create and spawn the L1 subscriber as a critical background task.
    ///
    /// The subscriber runs in a retry loop — if the connection drops or
    /// [`Self::run`] returns an error, it reconnects after the configured retry
    /// interval.
    pub fn spawn<P>(
        config: L1SubscriberConfig,
        local_state_provider: P,
        deposit_queue: DepositQueue,
        task_executor: reth_tasks::Runtime,
    ) where
        P: StateProviderFactory + Clone + Send + Sync + 'static,
    {
        let tracked_tokens = config.policy_cache.read().tracked_tokens();
        let subscriber = Self {
            config,
            local_state: Arc::new(ProviderLocalTempoStateReader {
                provider: local_state_provider,
            }),
            deposit_queue,
            tracked_tokens,
            tip403_metrics: Default::default(),
            subscriber_metrics: Default::default(),
        };

        task_executor.spawn_critical_task(
            "l1-deposit-subscriber",
            Box::pin(async move {
                loop {
                    if let Err(e) = subscriber.clone().run().await {
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

    /// Returns a stream of new L1 block headers, abstracting over the transport.
    ///
    /// - **WebSocket**: uses `subscribe_blocks` for push-based delivery.
    /// - **HTTP**: falls back to `watch_full_blocks` (filter-based polling via
    ///   `eth_newBlockFilter` + `eth_getFilterChanges`), extracting the header
    ///   from each block. The fallback is selected when `subscribe_blocks`
    ///   returns `PubsubUnavailable`.
    ///
    /// Both paths produce the same header payloads; transport-specific polling
    /// failures are surfaced as stream errors so [`run`](Self::run) can
    /// reconnect and resync.
    async fn header_stream<'a>(
        &self,
        provider: &'a DynProvider<TempoNetwork>,
    ) -> eyre::Result<
        Pin<
            Box<
                dyn Stream<
                        Item = eyre::Result<
                            <TempoNetwork as alloy_network::Network>::HeaderResponse,
                        >,
                    > + Send
                    + 'a,
            >,
        >,
    > {
        match provider.subscribe_blocks().await {
            Ok(sub) => {
                info!("Using WebSocket block subscription");
                Ok(Box::pin(sub.into_stream().map(Ok)))
            }
            Err(e) => {
                if e.as_transport_err()
                    .is_some_and(|t| t.is_pubsub_unavailable())
                {
                    info!("Pubsub unavailable, falling back to HTTP polling");
                    let mut watcher = provider.watch_full_blocks().await?;
                    watcher.set_poll_interval(HTTP_POLL_INTERVAL);
                    let stream = watcher
                        .into_stream()
                        .map(|res| res.map(|block| block.header).map_err(Into::into));
                    Ok(Box::pin(stream))
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Build the live L1 block stream, fetching receipts for each new header
    /// and buffering requests ahead of processing.
    async fn l1_block_stream<'a>(
        &self,
        provider: &'a DynProvider<TempoNetwork>,
    ) -> eyre::Result<
        Pin<
            Box<
                dyn Stream<
                        Item = eyre::Result<(
                            <TempoNetwork as alloy_network::Network>::HeaderResponse,
                            Vec<<TempoNetwork as alloy_network::Network>::ReceiptResponse>,
                        )>,
                    > + Send
                    + 'a,
            >,
        >,
    > {
        let header_stream = self.header_stream(provider).await?;
        let concurrency = self.config.l1_fetch_concurrency.max(1);
        let subscriber_metrics = self.subscriber_metrics.clone();
        let stream = header_stream
            .map_ok(move |header| {
                let provider = provider;
                let subscriber_metrics = subscriber_metrics.clone();
                async move {
                    let block_number = header.number();
                    let start = std::time::Instant::now();
                    let fetch_failures = &subscriber_metrics.fetch_failures;
                    let receipts = provider
                        .get_block_receipts(BlockId::number(block_number))
                        .await
                        .map_err(eyre::Report::from)
                        .and_then(|receipts| {
                            receipts
                                .ok_or_else(|| eyre::eyre!("no receipts for block {block_number}"))
                        })
                        .inspect_err(|_| {
                            fetch_failures.increment(1);
                        })?;
                    let elapsed = start.elapsed();
                    debug!(
                        block_number,
                        elapsed_ms = elapsed.as_millis() as u64,
                        receipts = receipts.len(),
                        "Fetched live block receipts"
                    );
                    Ok::<_, eyre::Report>((header, receipts))
                }
            })
            .try_buffered(concurrency);
        Ok(Box::pin(stream))
    }

    /// Determine the starting block number for backfill.
    ///
    /// Uses the zone's local `tempoBlockNumber` as the primary starting point —
    /// this is the authoritative source for where the zone left off. Falls back
    /// to the CLI genesis override or the portal's `genesisTempoBlockNumber`
    /// when the zone hasn't processed any blocks yet.
    pub(crate) async fn resolve_start_block(
        &self,
        l1_provider: &impl Provider<TempoNetwork>,
    ) -> eyre::Result<Option<u64>> {
        // The zone's local state is the authoritative source for where to
        // resume. This avoids the bug where the portal's
        // lastSyncedTempoBlockNumber runs ahead of local zone state.
        let local_tempo_block_number = self.local_state.latest_tempo_block_number()?;
        if local_tempo_block_number > 0 {
            info!(local_tempo_block_number, "Resuming from local zone state");
            return Ok(Some(local_tempo_block_number + 1));
        }

        if let Some(genesis) = self.config.genesis_tempo_block_number {
            info!(genesis, "Using CLI genesis block number override");
            return Ok(Some(genesis + 1));
        }

        let portal = ZonePortal::new(self.config.portal_address, l1_provider);
        let on_chain = portal.genesisTempoBlockNumber().call().await?;
        if on_chain == 0 {
            warn!(
                "Portal genesisTempoBlockNumber is 0 — skipping backfill. \
                 Set --l1.genesis-block-number to backfill from the correct block."
            );
            return Ok(None);
        }

        info!(genesis = on_chain, "Using portal's genesisTempoBlockNumber");
        Ok(Some(on_chain + 1))
    }

    /// Backfill deposit events from the starting block to the current L1 tip.
    #[instrument(skip(self, l1_provider))]
    async fn sync_to_l1_tip(
        &mut self,
        l1_provider: &impl Provider<TempoNetwork>,
    ) -> eyre::Result<()> {
        let Some(mut from) = self.resolve_start_block(l1_provider).await? else {
            self.subscriber_metrics.current_l1_lag_blocks.set(0.0);
            return Ok(());
        };

        // Skip past blocks already in the queue from a previous `run()`.
        if let Some(last) = self.deposit_queue.last_enqueued() {
            let adjusted = last.number + 1;
            if adjusted > from {
                info!(
                    portal_from = from,
                    queue_last = last.number,
                    adjusted_from = adjusted,
                    "Skipping blocks already in deposit queue"
                );
            }
            from = from.max(adjusted);
        }

        let tip = l1_provider.get_block_number().await?;
        self.record_seen_block(tip, 0);
        if from > tip {
            info!(from, tip, "Already synced to L1 tip");
            self.subscriber_metrics.current_l1_lag_blocks.set(0.0);
            return Ok(());
        }

        info!(
            from,
            tip,
            blocks = tip - from + 1,
            "Backfilling deposit events"
        );
        let start = std::time::Instant::now();
        let result = self.backfill(l1_provider, from, tip).await;
        self.subscriber_metrics
            .backfill_duration_seconds
            .record(start.elapsed().as_secs_f64());
        if result.is_ok() {
            self.subscriber_metrics.current_l1_lag_blocks.set(0.0);
        }
        result
    }

    /// Backfill L1 blocks from `from..=to` with pipelined RPC fetching.
    ///
    /// Fetches receipts and headers for up to `BACKFILL_CONCURRENCY` blocks in
    /// parallel, then processes them sequentially (event extraction, policy
    /// application, enqueue). This avoids the round-trip latency of fetching
    /// one block at a time.
    #[instrument(skip(self, l1_provider), fields(from, to))]
    async fn backfill(
        &mut self,
        l1_provider: &impl Provider<TempoNetwork>,
        from: u64,
        to: u64,
    ) -> eyre::Result<()> {
        use futures::stream;

        // Backfill sends 2 requests per block (receipts + header), so halve
        // the concurrency to stay within the configured fetch budget.
        let concurrency = (self.config.l1_fetch_concurrency / 2).max(1);
        let subscriber_metrics = self.subscriber_metrics.clone();

        let mut fetched = stream::iter(from..=to)
            .map(move |block_number| {
                let provider = l1_provider;
                let subscriber_metrics = subscriber_metrics.clone();
                async move {
                    let start = std::time::Instant::now();
                    let fetch_failures = &subscriber_metrics.fetch_failures;
                    let (receipts, header_resp) = tokio::try_join!(
                        async {
                            provider
                                .get_block_receipts(BlockId::number(block_number))
                                .await
                                .map_err(eyre::Report::from)?
                                .ok_or_else(|| eyre::eyre!("no receipts for block {block_number}"))
                        },
                        async {
                            provider
                                .get_header_by_number(block_number.into())
                                .await?
                                .ok_or_else(|| {
                                    eyre::eyre!("L1 header not found for block {block_number}")
                                })
                        },
                    )
                    .inspect_err(|_| {
                        fetch_failures.increment(1);
                    })?;
                    let elapsed = start.elapsed();
                    debug!(
                        block_number,
                        elapsed_ms = elapsed.as_millis() as u64,
                        receipts = receipts.len(),
                        "Fetched L1 block data"
                    );
                    let header = header_resp.inner.inner;
                    Ok::<_, eyre::Report>((header, receipts))
                }
            })
            .buffered(concurrency);

        let mut enqueued = 0u64;
        let backfill_start = std::time::Instant::now();

        while let Some((header, receipts)) = fetched.try_next().await? {
            let block_number = header.number();
            let (events, policy_events) = self.extract_events(block_number, &receipts);
            self.record_seen_block(block_number, to.saturating_sub(block_number));

            let sealed = SealedHeader::seal_slow(header);
            self.update_l1_state_anchor(block_number, sealed.hash(), sealed.parent_hash());
            self.apply_policy_events(block_number, &policy_events);
            self.apply_portal_state_events(block_number, &events);
            self.deposit_queue
                .enqueue_sealed(sealed, events, policy_events);
            enqueued += 1;
            self.subscriber_metrics.blocks_enqueued.increment(1);

            if enqueued.is_multiple_of(100) {
                let elapsed = backfill_start.elapsed();
                let blocks_per_sec = enqueued as f64 / elapsed.as_secs_f64().max(0.001);
                info!(
                    enqueued,
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

    /// Run the L1 subscriber until the stream ends or an error occurs.
    ///
    /// Connects to the L1 node (HTTP or WebSocket), backfills deposit events
    /// to the current L1 tip, then listens for new block headers. Each block —
    /// with or without deposits — is enqueued so the zone engine sees a strict
    /// sequential chain.
    ///
    /// Live-streamed blocks are buffered one block behind: a block is only
    /// flushed to the deposit queue once the next block arrives with a
    /// matching parent hash, proving the buffered block is canonical. This
    /// prevents the zone from committing to an L1 tip that gets reorged away.
    ///
    /// Callers should retry on error (see [`Self::spawn`]).
    pub async fn run(mut self) -> eyre::Result<()> {
        // Re-read tracked tokens from the policy cache so we pick up any
        // tokens discovered during a previous run (before a reconnect).
        self.tracked_tokens = self.config.policy_cache.read().tracked_tokens();

        let provider = self.connect().await?;

        // Backfill to the current tip before subscribing.
        // Backfilled blocks are historical and considered confirmed.
        self.sync_to_l1_tip(&provider).await?;

        info!(portal = %self.config.portal_address, "Listening for L1 blocks");
        let mut stream = self.l1_block_stream(&provider).await?;

        // Confirmation buffer: holds the latest unconfirmed L1 block.
        // A block is only flushed to the deposit queue once the NEXT block
        // arrives with a matching parent hash, proving the buffered block
        // is on the canonical chain.
        let mut unconfirmed_tip: Option<(
            SealedHeader<TempoHeader>,
            L1PortalEvents,
            Vec<PolicyEvent>,
        )> = None;

        loop {
            let stream_wait_start = std::time::Instant::now();
            let next = stream.try_next().await?;
            self.subscriber_metrics
                .stream_try_next_duration_seconds
                .record(stream_wait_start.elapsed().as_secs_f64());
            let Some((header, receipts)) = next else {
                break;
            };
            let block_number = header.number();
            let sealed = SealedHeader::seal_slow(header.inner.into_consensus());
            let (events, policy_events) = self.extract_events(block_number, &receipts);
            self.record_seen_block(block_number, 0);

            // If we have a buffered tip, check if the new block confirms it.
            if let Some((tip_header, tip_events, tip_policy_events)) = unconfirmed_tip.take() {
                if sealed.parent_hash() == tip_header.hash() {
                    // Confirmed — update the L1 state anchor, apply events, and
                    // flush to the queue.
                    let tip_number = tip_header.number();
                    let tip_hash = tip_header.hash();
                    let tip_parent = tip_header.parent_hash();
                    self.update_l1_state_anchor(tip_number, tip_hash, tip_parent);
                    self.apply_policy_events(tip_number, &tip_policy_events);
                    self.apply_portal_state_events(tip_number, &tip_events);
                    match self
                        .deposit_queue
                        .try_enqueue(tip_header, tip_events, tip_policy_events)
                    {
                        EnqueueOutcome::Accepted => {
                            self.subscriber_metrics.blocks_enqueued.increment(1);
                        }
                        EnqueueOutcome::Duplicate => {}
                        EnqueueOutcome::NeedBackfill { from, to } => {
                            // Gap between queue head and confirmed tip — backfill
                            // the missing range including the tip (re-fetched from
                            // the provider since try_enqueue consumed ownership).
                            warn!(
                                from,
                                to,
                                tip = tip_number,
                                "Backfilling gap before confirmed tip"
                            );
                            self.backfill(&provider, from, tip_number).await?;
                        }
                    }
                } else {
                    // Reorg — discard the buffered tip and clear L1 state and
                    // policy caches.
                    self.subscriber_metrics.reorgs_detected.increment(1);
                    warn!(
                        discarded_block = tip_header.number(),
                        discarded_hash = %tip_header.hash(),
                        new_block = block_number,
                        new_parent = %sealed.parent_hash(),
                        "Discarding unconfirmed L1 block (reorg)"
                    );
                    self.config.l1_state_cache.write().clear();
                    self.config.policy_cache.write().clear();
                }
            }

            // Buffer the new block as unconfirmed tip.
            unconfirmed_tip = Some((sealed, events, policy_events));
        }

        warn!("L1 block subscription stream ended");
        Ok(())
    }

    /// Extract portal and policy events from pre-fetched receipts (no RPC).
    fn extract_events(
        &mut self,
        block_number: u64,
        receipts: &[tempo_alloy::rpc::TempoTransactionReceipt],
    ) -> (L1PortalEvents, Vec<PolicyEvent>) {
        use tempo_contracts::precompiles::{ITIP20::TransferPolicyUpdate, TIP403_REGISTRY_ADDRESS};

        let portal_address = self.config.portal_address;
        let mut portal_events = L1PortalEvents::default();
        let mut policy_events = Vec::new();

        for receipt in receipts {
            for log in receipt.logs() {
                let addr = log.address();

                if addr == portal_address {
                    let prev_len = portal_events.enabled_tokens.len();
                    if let Err(e) = portal_events.push_log(log, block_number) {
                        warn!(block_number, %e, "Failed to decode portal event from receipt");
                    }
                    if let Some(enabled) = portal_events.enabled_tokens.get(prev_len) {
                        let token = enabled.token;
                        if !self.tracked_tokens.contains(&token) {
                            info!(%token, "New token enabled, adding to tracked tokens");
                            self.tracked_tokens.push(token);
                        }
                    }
                } else if addr == TIP403_REGISTRY_ADDRESS {
                    if let Some(event) = PolicyEvent::decode_registry(log) {
                        policy_events.push(event);
                    }
                } else if self.tracked_tokens.contains(&addr)
                    && log.topics().first() == Some(&TransferPolicyUpdate::SIGNATURE_HASH)
                    && let Some(event) = PolicyEvent::decode_tip20(log)
                {
                    policy_events.push(event);
                }
            }
        }

        self.record_portal_event_metrics(&portal_events);
        (portal_events, policy_events)
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
        let mut regular = 0u64;
        let mut encrypted = 0u64;
        let mut transfer_started = 0u64;
        let mut transferred = 0u64;
        for deposit in &portal_events.deposits {
            match deposit {
                L1Deposit::Regular(_) => regular += 1,
                L1Deposit::Encrypted(_) => encrypted += 1,
            }
        }
        for event in &portal_events.sequencer_events {
            match event {
                L1SequencerEvent::TransferStarted { .. } => transfer_started += 1,
                L1SequencerEvent::Transferred { .. } => transferred += 1,
            }
        }
        if regular > 0 {
            self.subscriber_metrics
                .regular_deposit_events
                .increment(regular);
        }
        if encrypted > 0 {
            self.subscriber_metrics
                .encrypted_deposit_events
                .increment(encrypted);
        }
        if !portal_events.enabled_tokens.is_empty() {
            self.subscriber_metrics
                .token_enabled_events
                .increment(portal_events.enabled_tokens.len() as u64);
        }
        if transfer_started > 0 {
            self.subscriber_metrics
                .sequencer_transfer_started_events
                .increment(transfer_started);
        }
        if transferred > 0 {
            self.subscriber_metrics
                .sequencer_transferred_events
                .increment(transferred);
        }
    }

    /// Write decoded policy events into the shared cache.
    ///
    /// The subscriber may write events ahead of the engine's processed L1 height;
    /// the engine advances the cache baseline after it accepts the corresponding
    /// zone payload.
    pub(crate) fn apply_policy_events(&self, block_number: u64, policy_events: &[PolicyEvent]) {
        let mut cache = self.config.policy_cache.write();
        cache.apply_events(block_number, policy_events);
        if !policy_events.is_empty() {
            self.tip403_metrics
                .listener_events_applied
                .increment(policy_events.len() as u64);
        }
        self.tip403_metrics
            .cached_policies
            .set(cache.policies().len() as f64);
        self.tip403_metrics
            .cached_token_policies
            .set(cache.num_token_policies() as f64);
    }

    /// Write decoded portal state changes into the shared L1 cache at the
    /// confirmed block height.
    fn apply_portal_state_events(&self, block_number: u64, portal_events: &L1PortalEvents) {
        if portal_events.sequencer_events.is_empty() {
            return;
        }

        let mut cache = self.config.l1_state_cache.write();
        apply_sequencer_events_to_cache(
            &mut cache,
            self.config.portal_address,
            block_number,
            &portal_events.sequencer_events,
        );
    }

    /// Update the L1 state cache anchor. Detects reorgs by comparing
    /// `parent_hash` against the current anchor and clears the cache when they
    /// diverge.
    pub(crate) fn update_l1_state_anchor(&self, number: u64, hash: B256, parent_hash: B256) {
        let mut guard = self.config.l1_state_cache.write();
        let anchor = guard.anchor();
        if anchor.hash != B256::ZERO && parent_hash != anchor.hash {
            self.subscriber_metrics.reorgs_detected.increment(1);
            warn!(
                old_anchor = %anchor.hash,
                new_parent = %parent_hash,
                block_number = number,
                "Reorg detected, clearing L1 state cache"
            );
            guard.clear();
            self.config.policy_cache.write().clear();
        }
        guard.update_anchor(NumHash { number, hash });
    }
}

pub(crate) fn apply_sequencer_events_to_cache(
    cache: &mut L1StateCacheInner,
    portal_address: Address,
    block_number: u64,
    sequencer_events: &[L1SequencerEvent],
) {
    for event in sequencer_events {
        match *event {
            L1SequencerEvent::TransferStarted {
                current_sequencer,
                pending_sequencer,
            } => {
                cache.set(
                    portal_address,
                    PORTAL_SEQUENCER_SLOT,
                    block_number,
                    address_to_storage_value(current_sequencer),
                );
                cache.set(
                    portal_address,
                    PORTAL_PENDING_SEQUENCER_SLOT,
                    block_number,
                    address_to_storage_value(pending_sequencer),
                );
            }
            L1SequencerEvent::Transferred {
                previous_sequencer: _,
                new_sequencer,
            } => {
                cache.set(
                    portal_address,
                    PORTAL_SEQUENCER_SLOT,
                    block_number,
                    address_to_storage_value(new_sequencer),
                );
                cache.set(
                    portal_address,
                    PORTAL_PENDING_SEQUENCER_SLOT,
                    block_number,
                    B256::ZERO,
                );
            }
        }
    }
}

pub(crate) fn address_to_storage_value(address: Address) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(address.as_slice());
    B256::new(bytes)
}
