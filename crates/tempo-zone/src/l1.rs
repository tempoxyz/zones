//! L1 chain subscription and deposit extraction.
//!
//! Subscribes to L1 block headers and extracts deposit events from the
//! ZonePortal contract for each block. Supports both WebSocket (subscription)
//! and HTTP (polling) transports — the transport is auto-detected from the URL
//! scheme.

use alloy_consensus::BlockHeader as _;
use alloy_eips::NumHash;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::{BlockId, Log};
use alloy_sol_types::{SolEvent, SolEventInterface, SolValue};
use alloy_transport::Authorization;
use futures::{Stream, StreamExt, TryStreamExt as _};
use parking_lot::Mutex;
use reth_primitives_traits::SealedHeader;
use std::{pin::Pin, sync::Arc};
use tempo_alloy::TempoNetwork;
use tempo_contracts::precompiles::{ITIP20::TransferPolicyUpdate, TIP403_REGISTRY_ADDRESS};
use tempo_primitives::TempoHeader;
use tracing::{debug, error, info, instrument, warn};

use crate::{
    SharedL1StateCache, SharedPolicyCache,
    abi::{
        self, EncryptedDeposit as AbiEncryptedDeposit,
        EncryptedDepositPayload as AbiEncryptedDepositPayload, PORTAL_PENDING_SEQUENCER_SLOT,
        PORTAL_SEQUENCER_SLOT,
        ZonePortal::{
            self, BounceBack, DepositMade, EncryptedDepositMade, SequencerTransferStarted,
            SequencerTransferred, TokenEnabled, ZonePortalEvents,
        },
    },
    l1_state::{cache::L1StateCache, tip403::PolicyEvent},
};

/// Poll interval for the HTTP block filter fallback (500ms, matching L1 block time).
const HTTP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

pub type TempoL1BlockStream<'a> = Pin<
    Box<
        dyn Stream<
                Item = eyre::Result<(
                    <TempoNetwork as alloy_network::Network>::HeaderResponse,
                    Vec<<TempoNetwork as alloy_network::Network>::ReceiptResponse>,
                )>,
            > + Send
            + 'a,
    >,
>;

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
    /// The zone's current `tempoBlockNumber` read from local state at startup.
    /// Backfill starts from `local_tempo_block_number + 1` to avoid re-fetching
    /// blocks the zone has already processed.
    pub local_tempo_block_number: u64,
    /// Shared TIP-403 policy cache. The subscriber applies policy events
    /// extracted from L1 receipts directly into this cache before enqueuing
    /// blocks.
    pub policy_cache: SharedPolicyCache,
    /// Shared L1 state cache. The subscriber updates the cache anchor on each
    /// confirmed block and clears it on reorgs.
    pub l1_state_cache: SharedL1StateCache,
    /// Maximum number of concurrent L1 RPC receipt fetches. Used directly for
    /// the live stream and halved for backfill (which sends 2 requests per block).
    pub l1_fetch_concurrency: usize,
    /// Interval between WebSocket reconnection attempts.
    pub retry_connection_interval: std::time::Duration,
}

/// L1 chain subscriber that listens for new blocks and extracts deposit events.
#[derive(Clone)]
pub struct L1Subscriber {
    config: L1SubscriberConfig,
    deposit_queue: DepositQueue,
    /// Mutable set of token addresses tracked for TIP-403 policy events.
    /// Initialized from config, grows dynamically when `TokenEnabled` events are seen.
    tracked_tokens: Vec<Address>,
    /// TIP-403 metrics (cache sizes, events applied).
    tip403_metrics: crate::l1_state::tip403::Tip403Metrics,
    /// L1 subscriber metrics for connection health, backfill, and event ingestion.
    subscriber_metrics: crate::metrics::L1SubscriberMetrics,
}

impl L1Subscriber {
    /// Create and spawn the L1 subscriber as a critical background task.
    ///
    /// The subscriber runs in a retry loop — if the connection drops or
    /// [`Self::run`] returns an error, it reconnects after the configured retry
    /// interval.
    pub fn spawn(
        config: L1SubscriberConfig,
        deposit_queue: DepositQueue,
        task_executor: reth_tasks::Runtime,
    ) {
        let tracked_tokens = config.policy_cache.read().tracked_tokens();
        let subscriber = Self {
            config,
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
        let mut conn_config = crate::rpc_connection_config(self.config.retry_connection_interval);

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
    ) -> eyre::Result<TempoL1BlockStream<'a>> {
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
    async fn resolve_start_block(
        &self,
        l1_provider: &impl Provider<TempoNetwork>,
    ) -> eyre::Result<Option<u64>> {
        // The zone's local state is the authoritative source for where to
        // resume. This avoids the bug where the portal's
        // lastSyncedTempoBlockNumber runs ahead of local zone state.
        // FIXME: is this value ever updated?
        if self.config.local_tempo_block_number > 0 {
            info!(
                local_tempo_block_number = self.config.local_tempo_block_number,
                "Resuming from local zone state"
            );
            return Ok(Some(self.config.local_tempo_block_number + 1));
        }

        if let Some(genesis) = self.config.genesis_tempo_block_number {
            info!(genesis, "Using CLI genesis block number override");
            return Ok(Some(genesis + 1));
        }

        // FIXME: should this ever happen
        if self.config.portal_address.is_zero() {
            warn!(
                "No portal address and no genesis block number override — skipping backfill. \
                 Set --l1.genesis-block-number or provide a portal address."
            );
            return Ok(None);
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
        self.tracked_tokens = self.config.policy_cache.read().tracked_tokens();

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

            self.apply_policy_events(block_number, &policy_events);

            let sealed = SealedHeader::seal_slow(header);
            self.update_l1_state_anchor(block_number, sealed.hash(), sealed.parent_hash());
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

    /// Syncs to L1 tip and subscribes to L1 blocks.
    /// Handles deposit events, TIP 403 policy changes and Zone portal contract updates
    pub async fn run(mut self) -> eyre::Result<()> {
        let provider = self.connect().await?;
        self.sync_to_l1_tip(&provider).await?;

        let stream = self.l1_block_stream(&provider).await?;
        self.handle_l1_block_stream(&provider, stream).await?;

        Ok(())
    }

    // TODO:
    async fn handle_l1_block_stream<'a>(
        mut self,
        provider: &DynProvider<TempoNetwork>,
        mut stream: TempoL1BlockStream<'a>,
    ) -> eyre::Result<()> {
        info!(portal = %self.config.portal_address, "Listening for L1 blocks");

        let mut unconfirmed_tip: Option<(
            SealedHeader<TempoHeader>,
            L1PortalEvents,
            Vec<PolicyEvent>,
        )> = None;

        while let Some((header, receipts)) = stream.try_next().await? {
            let block_number = header.number();
            // NOTE: should this be 0?
            self.record_seen_block(block_number, 0);

            let sealed = SealedHeader::seal_slow(header.inner.into_consensus());
            let (events, policy_events) = self.extract_events(block_number, &receipts);

            // If we have a buffered tip, check if the new block confirms it.
            if let Some((tip_header, tip_events, tip_policy_events)) = unconfirmed_tip.take() {
                if sealed.parent_hash() == tip_header.hash() {
                    // Confirmed — apply policy events, update L1 state anchor,
                    // and flush to the queue.
                    let tip_number = tip_header.number();
                    let tip_hash = tip_header.hash();
                    let tip_parent = tip_header.parent_hash();
                    self.apply_policy_events(tip_number, &tip_policy_events);
                    self.update_l1_state_anchor(tip_number, tip_hash, tip_parent);
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
                    // Reorg — discard the buffered tip and clear L1 state cache.
                    self.subscriber_metrics.reorgs_detected.increment(1);
                    warn!(
                        discarded_block = tip_header.number(),
                        discarded_hash = %tip_header.hash(),
                        new_block = block_number,
                        new_parent = %sealed.parent_hash(),
                        "Discarding unconfirmed L1 block (reorg)"
                    );
                    self.config.l1_state_cache.write().clear();
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
        let mut portal_events = L1PortalEvents::default();
        let mut policy_events = Vec::new();

        for receipt in receipts {
            for log in receipt.logs() {
                let addr = log.address();

                if addr == self.config.portal_address {
                    let prev_len = portal_events.enabled_tokens.len();
                    if let Err(e) = portal_events.push_log(log, block_number) {
                        warn!(block_number, %e, "Failed to decode portal event from receipt");
                    }

                    // NOTE: this is a bit odd, need to better understand how tracked tokens vs non
                    // tracked tokens behave
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

    /// Write decoded policy events into the shared cache and advance its L1 block
    /// cursor. The cursor is always updated — even for blocks with no policy events —
    /// so that cache-miss fallback queries target the correct L1 height.
    fn apply_policy_events(&self, block_number: u64, policy_events: &[PolicyEvent]) {
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
    fn update_l1_state_anchor(&self, number: u64, hash: B256, parent_hash: B256) {
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
        }
        guard.update_anchor(NumHash { number, hash });
    }
}

fn apply_sequencer_events_to_cache(
    cache: &mut L1StateCache,
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

fn address_to_storage_value(address: Address) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(address.as_slice());
    B256::new(bytes)
}

/// A deposit extracted from L1.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Deposit {
    /// TIP-20 token being deposited.
    pub token: Address,
    /// Sender on L1.
    pub sender: Address,
    /// Recipient on the zone.
    pub to: Address,
    /// Net amount deposited (fee already deducted on L1).
    pub amount: u128,
    /// Fee paid on L1.
    pub fee: u128,
    /// User-provided memo.
    pub memo: B256,
}

impl Deposit {
    /// Create a new deposit from an event.
    pub fn from_event(event: DepositMade) -> Self {
        Self {
            token: event.token,
            sender: event.sender,
            to: event.to,
            amount: event.netAmount,
            fee: event.fee,
            memo: event.memo,
        }
    }

    /// Create a bounce-back deposit from an event.
    pub fn from_bounce_back(event: BounceBack, portal_address: Address) -> Self {
        Self {
            token: event.token,
            sender: portal_address,
            to: event.fallbackRecipient,
            amount: event.amount,
            fee: 0,
            memo: B256::ZERO,
        }
    }
}

/// An encrypted deposit extracted from L1.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EncryptedDeposit {
    /// TIP-20 token being deposited.
    pub token: Address,
    /// Sender on L1.
    pub sender: Address,
    /// Net amount deposited (fee already deducted on L1).
    pub amount: u128,
    /// Fee paid on L1.
    pub fee: u128,
    /// Index of the encryption key used.
    pub key_index: U256,
    /// Ephemeral public key X coordinate.
    pub ephemeral_pubkey_x: B256,
    /// Ephemeral public key Y parity (0x02 or 0x03).
    pub ephemeral_pubkey_y_parity: u8,
    /// AES-256-GCM ciphertext.
    pub ciphertext: Vec<u8>,
    /// GCM nonce (12 bytes).
    pub nonce: [u8; 12],
    /// GCM authentication tag (16 bytes).
    pub tag: [u8; 16],
}

impl EncryptedDeposit {
    /// Create a new encrypted deposit from an event.
    pub fn from_event(event: EncryptedDepositMade) -> Self {
        Self {
            token: event.token,
            sender: event.sender,
            amount: event.netAmount,
            fee: event.fee,
            key_index: event.keyIndex,
            ephemeral_pubkey_x: event.ephemeralPubkeyX,
            ephemeral_pubkey_y_parity: event.ephemeralPubkeyYParity,
            ciphertext: event.ciphertext.to_vec(),
            nonce: event.nonce.0,
            tag: event.tag.0,
        }
    }
}

/// A deposit from L1 — either regular (plaintext) or encrypted.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum L1Deposit {
    /// A regular deposit with plaintext recipient and memo.
    Regular(Deposit),
    /// An encrypted deposit where recipient and memo are encrypted.
    Encrypted(EncryptedDeposit),
}

impl L1Deposit {
    /// Compute the next hash chain value: `keccak256(abi.encode(deposit, prevHash))`.
    pub fn hash_chain(&self, prev_hash: B256) -> B256 {
        match self {
            Self::Regular(d) => keccak256(
                (
                    abi::DepositType::Regular,
                    abi::Deposit {
                        token: d.token,
                        sender: d.sender,
                        to: d.to,
                        amount: d.amount,
                        memo: d.memo,
                    },
                    prev_hash,
                )
                    .abi_encode(),
            ),
            Self::Encrypted(d) => keccak256(
                (
                    abi::DepositType::Encrypted,
                    AbiEncryptedDeposit {
                        token: d.token,
                        sender: d.sender,
                        amount: d.amount,
                        keyIndex: d.key_index,
                        encrypted: AbiEncryptedDepositPayload {
                            ephemeralPubkeyX: d.ephemeral_pubkey_x,
                            ephemeralPubkeyYParity: d.ephemeral_pubkey_y_parity,
                            ciphertext: d.ciphertext.clone().into(),
                            nonce: d.nonce.into(),
                            tag: d.tag.into(),
                        },
                    },
                    prev_hash,
                )
                    .abi_encode(),
            ),
        }
    }
}

/// A sequencer-management event emitted by the L1 portal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum L1SequencerEvent {
    /// The current sequencer nominated a pending successor.
    TransferStarted {
        current_sequencer: Address,
        pending_sequencer: Address,
    },
    /// The pending sequencer accepted and became the active sequencer.
    Transferred {
        previous_sequencer: Address,
        new_sequencer: Address,
    },
}

/// Result of attempting to enqueue an L1 block into the deposit queue.
#[derive(Debug)]
pub(crate) enum EnqueueOutcome {
    /// Block was appended to the queue.
    Accepted,
    /// Block is a duplicate (same number and hash already present, or behind our window).
    Duplicate,
    /// Block doesn't connect — subscriber must fetch and enqueue `from..=to` first,
    /// then retry this block.
    NeedBackfill { from: u64, to: u64 },
}

/// Events extracted from the ZonePortal in a single L1 block.
///
/// Bundles all portal-emitted events for one block into a single extensible
/// type. New event types (e.g. token pausing, sequencer changes) can be
/// added as fields without restructuring the pipeline.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct L1PortalEvents {
    /// Deposit events (regular + encrypted).
    pub deposits: Vec<L1Deposit>,
    /// Tokens newly enabled for bridging in this block, with metadata.
    pub enabled_tokens: Vec<EnabledToken>,
    /// Sequencer transfer events in the order they appeared in the block.
    pub sequencer_events: Vec<L1SequencerEvent>,
}

/// A token newly enabled for bridging, with metadata for L2 creation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EnabledToken {
    /// The L1 token address (TIP-20 with 0x20C0 prefix).
    pub token: Address,
    /// Token name.
    pub name: String,
    /// Token symbol.
    pub symbol: String,
    /// Token currency (e.g. "USD", "EUR").
    pub currency: String,
}

// NOTE: why are we doing this rather than using sol macro
impl EnabledToken {
    /// Convert to the ABI type used in `advanceTempo` calldata.
    pub fn to_abi(&self) -> abi::EnabledToken {
        abi::EnabledToken {
            token: self.token,
            name: self.name.clone(),
            symbol: self.symbol.clone(),
            currency: self.currency.clone(),
        }
    }
}

impl L1PortalEvents {
    /// Event signature hashes that this container knows how to decode.
    const SIGNATURE_HASHES: [B256; 6] = [
        DepositMade::SIGNATURE_HASH,
        EncryptedDepositMade::SIGNATURE_HASH,
        BounceBack::SIGNATURE_HASH,
        TokenEnabled::SIGNATURE_HASH,
        SequencerTransferStarted::SIGNATURE_HASH,
        SequencerTransferred::SIGNATURE_HASH,
    ];

    /// Create portal events from deposits only.
    pub fn from_deposits(deposits: Vec<L1Deposit>) -> Self {
        Self {
            deposits,
            ..Default::default()
        }
    }

    /// Decode a portal log and add the event to this container.
    ///
    /// Logs whose topic0 does not match a known portal event are skipped.
    /// Known events that fail to decode return an error.
    pub fn push_log(&mut self, log: &Log, block_number: u64) -> eyre::Result<()> {
        if !Self::is_known_event(log) {
            debug!(
                l1_block = block_number,
                topic0 = ?log.topic0(),
                "Skipping unknown portal event"
            );
            return Ok(());
        }
        match ZonePortalEvents::decode_log(&log.inner)?.data {
            ZonePortalEvents::DepositMade(event) => {
                info!(
                    l1_block = block_number,
                    token = %event.token,
                    sender = %event.sender,
                    to = %event.to,
                    amount = %event.netAmount,
                    "💰 Deposit from L1"
                );
                self.deposits
                    .push(L1Deposit::Regular(Deposit::from_event(event)));
            }
            ZonePortalEvents::EncryptedDepositMade(event) => {
                info!(
                    l1_block = block_number,
                    token = %event.token,
                    sender = %event.sender,
                    amount = %event.netAmount,
                    "🔒 Encrypted deposit from L1"
                );
                self.deposits
                    .push(L1Deposit::Encrypted(EncryptedDeposit::from_event(event)));
            }
            ZonePortalEvents::BounceBack(event) => {
                info!(
                    l1_block = block_number,
                    token = %event.token,
                    to = %event.fallbackRecipient,
                    amount = %event.amount,
                    "↩️ Bounce-back deposit from L1"
                );
                self.deposits
                    .push(L1Deposit::Regular(Deposit::from_bounce_back(
                        event,
                        log.address(),
                    )));
            }
            ZonePortalEvents::TokenEnabled(event) => {
                info!(
                    l1_block = block_number,
                    token = %event.token,
                    name = %event.name,
                    symbol = %event.symbol,
                    currency = %event.currency,
                    "🪙 Token enabled on L1"
                );
                self.enabled_tokens.push(EnabledToken {
                    token: event.token,
                    name: event.name,
                    symbol: event.symbol,
                    currency: event.currency,
                });
            }
            ZonePortalEvents::SequencerTransferStarted(event) => {
                info!(
                    l1_block = block_number,
                    current_sequencer = %event.currentSequencer,
                    pending_sequencer = %event.pendingSequencer,
                    "👤 Sequencer transfer started on L1"
                );
                self.sequencer_events
                    .push(L1SequencerEvent::TransferStarted {
                        current_sequencer: event.currentSequencer,
                        pending_sequencer: event.pendingSequencer,
                    });
            }
            ZonePortalEvents::SequencerTransferred(event) => {
                info!(
                    l1_block = block_number,
                    previous_sequencer = %event.previousSequencer,
                    new_sequencer = %event.newSequencer,
                    "👤 Sequencer transferred on L1"
                );
                self.sequencer_events.push(L1SequencerEvent::Transferred {
                    previous_sequencer: event.previousSequencer,
                    new_sequencer: event.newSequencer,
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn is_known_event(log: &Log) -> bool {
        log.topic0()
            .is_some_and(|t| Self::SIGNATURE_HASHES.contains(t))
    }
}

/// An L1 block's header paired with the deposits found in that block.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct L1BlockDeposits {
    /// The sealed L1 block header (caches the block hash).
    pub header: SealedHeader<TempoHeader>,
    /// Portal events extracted from this block.
    pub events: L1PortalEvents,
    /// TIP-403 policy events extracted from this block's receipts.
    pub policy_events: Vec<PolicyEvent>,
    /// Deposit queue hash chain value before this block's deposits.
    pub queue_hash_before: B256,
    /// Deposit queue hash chain value after this block's deposits.
    pub queue_hash_after: B256,
}

impl L1BlockDeposits {
    /// Prepare all deposits for the payload builder.
    ///
    /// Decrypts encrypted deposits, checks TIP-403 policy authorization,
    /// and ABI-encodes everything into the types the `advanceTempo` call expects.
    /// The resulting [`PreparedL1Block`] is ready to be passed through payload
    /// attributes to the builder.
    pub async fn prepare(
        self,
        sequencer_key: &k256::SecretKey,
        portal_address: Address,
        policy_provider: &crate::l1_state::PolicyProvider,
    ) -> eyre::Result<PreparedL1Block> {
        use crate::precompiles::ecies;

        let start = std::time::Instant::now();
        let l1_block_number = self.header.inner.number;
        let total_deposits = self.events.deposits.len();
        let mut queued_deposits: Vec<abi::QueuedDeposit> = Vec::new();
        let mut decryptions: Vec<abi::DecryptionData> = Vec::new();

        for deposit in &self.events.deposits {
            match deposit {
                L1Deposit::Regular(d) => {
                    let deposit = abi::Deposit {
                        token: d.token,
                        sender: d.sender,
                        to: d.to,
                        amount: d.amount,
                        memo: d.memo,
                    };
                    queued_deposits.push(abi::QueuedDeposit {
                        depositType: abi::DepositType::Regular,
                        depositData: Bytes::from(deposit.abi_encode()),
                    });
                }
                L1Deposit::Encrypted(d) => {
                    let queued = abi::QueuedDeposit {
                        depositType: abi::DepositType::Encrypted,
                        depositData: Bytes::from(
                            abi::EncryptedDeposit {
                                token: d.token,
                                sender: d.sender,
                                amount: d.amount,
                                keyIndex: d.key_index,
                                encrypted: abi::EncryptedDepositPayload {
                                    ephemeralPubkeyX: d.ephemeral_pubkey_x,
                                    ephemeralPubkeyYParity: d.ephemeral_pubkey_y_parity,
                                    ciphertext: d.ciphertext.clone().into(),
                                    nonce: d.nonce.into(),
                                    tag: d.tag.into(),
                                },
                            }
                            .abi_encode(),
                        ),
                    };

                    // Attempt full ECIES decryption.
                    let dec = ecies::decrypt_deposit(
                        sequencer_key,
                        &d.ephemeral_pubkey_x,
                        d.ephemeral_pubkey_y_parity,
                        &d.ciphertext,
                        &d.nonce,
                        &d.tag,
                        portal_address,
                        d.key_index,
                    );

                    if let Some(dec) = dec {
                        debug!(
                            target: "zone::engine",
                            l1_block = l1_block_number,
                            sender = %d.sender,
                            recipient = %dec.to,
                            token = %d.token,
                            amount = %d.amount,
                            "Decrypted encrypted deposit, checking policy"
                        );

                        // Check TIP-403 policy via the provider (cache-first, RPC fallback).
                        // Errors are propagated so the engine retries rather than allowing
                        // unauthorized deposits through.
                        let authorized = policy_provider
                            .is_authorized_async(
                                d.token,
                                dec.to,
                                l1_block_number,
                                crate::l1_state::AuthRole::MintRecipient,
                            )
                            .await?;

                        let recipient = if authorized {
                            debug!(
                                target: "zone::engine",
                                recipient = %dec.to,
                                token = %d.token,
                                "Policy authorized encrypted deposit recipient"
                            );
                            dec.to
                        } else {
                            warn!(
                                target: "zone::engine",
                                sender = %d.sender,
                                recipient = %dec.to,
                                token = %d.token,
                                amount = %d.amount,
                                "Encrypted deposit recipient unauthorized, redirecting to sender"
                            );
                            d.sender
                        };

                        let decryption = abi::DecryptionData {
                            sharedSecret: dec.proof.shared_secret,
                            sharedSecretYParity: dec.proof.shared_secret_y_parity,
                            to: recipient,
                            memo: dec.memo,
                            cpProof: abi::ChaumPedersenProof {
                                s: dec.proof.cp_proof_s,
                                c: dec.proof.cp_proof_c,
                            },
                        };
                        queued_deposits.push(queued);
                        decryptions.push(decryption);
                        continue;
                    }

                    // Full decryption failed — try ECDH proof for on-chain refund.
                    let proof = ecies::compute_ecdh_proof(
                        sequencer_key,
                        &d.ephemeral_pubkey_x,
                        d.ephemeral_pubkey_y_parity,
                    );

                    if let Some(proof) = proof {
                        warn!(
                            target: "zone::payload",
                            sender = %d.sender,
                            amount = %d.amount,
                            "Encrypted deposit decryption failed, providing valid proof for on-chain refund"
                        );
                        let decryption = abi::DecryptionData {
                            sharedSecret: proof.shared_secret,
                            sharedSecretYParity: proof.shared_secret_y_parity,
                            to: d.sender,
                            memo: B256::ZERO,
                            cpProof: abi::ChaumPedersenProof {
                                s: proof.cp_proof_s,
                                c: proof.cp_proof_c,
                            },
                        };
                        queued_deposits.push(queued);
                        decryptions.push(decryption);
                        continue;
                    }

                    warn!(
                        target: "zone::payload",
                        sender = %d.sender,
                        amount = %d.amount,
                        "Encrypted deposit has invalid ephemeral pubkey, using zeroed DecryptionData"
                    );
                    let decryption = abi::DecryptionData {
                        sharedSecret: B256::ZERO,
                        sharedSecretYParity: 0x02,
                        to: d.sender,
                        memo: B256::ZERO,
                        cpProof: abi::ChaumPedersenProof {
                            s: B256::ZERO,
                            c: B256::ZERO,
                        },
                    };
                    queued_deposits.push(queued);
                    decryptions.push(decryption);
                }
            }
        }

        let enabled_tokens: Vec<_> = self
            .events
            .enabled_tokens
            .iter()
            .map(|t| t.to_abi())
            .collect();

        let elapsed = start.elapsed();
        info!(
            target: "zone::engine",
            l1_block = l1_block_number,
            total_deposits,
            encrypted = decryptions.len(),
            enabled_tokens = enabled_tokens.len(),
            ?elapsed,
            "Prepared L1 block deposits"
        );

        Ok(PreparedL1Block {
            header: self.header,
            queued_deposits,
            decryptions,
            enabled_tokens,
        })
    }
}

/// An L1 block with deposits fully prepared for the payload builder.
///
/// All ECIES decryption, TIP-403 policy checks, and ABI encoding have been
/// performed. The builder only needs to RLP-encode the header and assemble
/// the `advanceTempo` calldata.
///
/// Implements `Serialize`/`Deserialize` to satisfy the `PayloadAttributes`
/// trait bound, but the deposit fields are `#[serde(skip)]` because the sol!
/// types don't derive serde. This is fine — payload attributes only flow
/// through in-process channels and are never serialised to the wire.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PreparedL1Block {
    /// The sealed L1 block header.
    pub header: SealedHeader<TempoHeader>,
    /// ABI-encoded queued deposits (regular + encrypted).
    #[serde(skip)]
    pub queued_deposits: Vec<abi::QueuedDeposit>,
    /// Decryption data for encrypted deposits (one per encrypted deposit, in order).
    #[serde(skip)]
    pub decryptions: Vec<abi::DecryptionData>,
    /// Tokens newly enabled for bridging in this block.
    #[serde(skip)]
    pub enabled_tokens: Vec<abi::EnabledToken>,
}

/// Deposit queue hash chain state.
///
/// Tracks deposits grouped by L1 block and maintains the hash chain:
/// `newHash = keccak256(abi.encode(deposit, prevHash))`
///
/// This mirrors the L1 portal's `currentDepositQueueHash`.
#[derive(Debug)]
pub(crate) struct PendingDeposits {
    /// Deposit hash chain value after the last block consumed by the engine
    /// via `confirm` or `drain`. Serves as the rollback anchor when purging
    /// blocks during a reorg.
    processed_head_hash: B256,
    /// Deposit hash chain value after applying all pending deposits. Advances
    /// as new deposits are enqueued and rolls back to `processed_head_hash`
    /// when blocks are purged.
    enqueued_head_hash: B256,
    /// Pending L1 blocks with their deposits, not yet processed by the zone
    pending: Vec<L1BlockDeposits>,
    /// Highest L1 block ever enqueued (number + hash). Survives `confirm` /
    /// `drain` so that reconnecting subscribers know where the queue left off,
    /// even if the engine has already consumed the blocks.
    last_enqueued: Option<NumHash>,
    /// Last L1 block consumed by the engine via `confirm` or `drain`.
    /// Used as a floor during reorg backfill to prevent re-offering blocks
    /// the zone has already built into zone blocks.
    last_processed: Option<NumHash>,
}

impl Default for PendingDeposits {
    fn default() -> Self {
        Self {
            processed_head_hash: B256::ZERO,
            enqueued_head_hash: B256::ZERO,
            pending: Vec::new(),
            last_enqueued: None,
            last_processed: None,
        }
    }
}

impl PendingDeposits {
    /// Try to enqueue an L1 block, enforcing the chain continuity invariant:
    /// pending blocks must form a contiguous chain where each block's number is
    /// exactly `prev + 1` and its `parent_hash` matches the previous block's hash.
    ///
    /// Returns:
    /// - [`EnqueueOutcome::Accepted`] — block extends the chain and was appended.
    /// - [`EnqueueOutcome::Duplicate`] — block is already present or behind our
    ///   window; safe to ignore.
    /// - [`EnqueueOutcome::NeedBackfill`] — block doesn't connect due to a gap
    ///   or parent hash mismatch. The caller must fetch and enqueue the missing
    ///   range `from..=to`, then retry this block.
    pub(crate) fn try_enqueue(
        &mut self,
        header: SealedHeader<TempoHeader>,
        events: L1PortalEvents,
        policy_events: Vec<PolicyEvent>,
    ) -> EnqueueOutcome {
        let block_number = header.number();
        let block_hash = header.hash();
        let parent_hash = header.parent_hash();

        // Check if we already have this block number
        if let Some(idx) = self
            .pending
            .iter()
            .position(|e| e.header.number() == block_number)
        {
            if self.pending[idx].header.hash() == block_hash {
                return EnqueueOutcome::Duplicate;
            }
            // Different hash at same height — purge from this point (reorg)
            self.purge_from(idx);
            // Fall through to try appending
        }

        // If queue is empty, check against `last_enqueued` to detect gaps
        // from blocks that were already consumed by the engine.
        if self.pending.is_empty() {
            if let Some(last) = self.last_enqueued {
                if block_number <= last.number {
                    return EnqueueOutcome::Duplicate;
                }
                if block_number > last.number + 1 {
                    return EnqueueOutcome::NeedBackfill {
                        from: last.number + 1,
                        to: block_number - 1,
                    };
                }
                // block_number == last.number + 1 — verify parent connects.
                // If it doesn't, the consumed block was reorged — clear
                // `last_enqueued` so backfill can re-enqueue that block number
                // with its new (post-reorg) hash.
                if parent_hash != last.hash {
                    self.last_enqueued = None;
                    return EnqueueOutcome::NeedBackfill {
                        from: last.number,
                        to: block_number - 1,
                    };
                }
            } else if let Some(processed) = self.last_processed {
                // last_enqueued was cleared (reorg of consumed block) but we
                // know what the engine already consumed. Use it as a floor to
                // prevent re-offering blocks the zone already built on.
                if block_number <= processed.number {
                    return EnqueueOutcome::Duplicate;
                }
                if block_number > processed.number + 1 {
                    return EnqueueOutcome::NeedBackfill {
                        from: processed.number + 1,
                        to: block_number - 1,
                    };
                }
                // Immediate successor of a consumed block. Accept regardless
                // of parent hash — the parent was reorged but the zone already
                // committed to it. The builder will detect the hash mismatch.
            }
            self.append(header, events, policy_events);
            return EnqueueOutcome::Accepted;
        }

        let tail = self.pending.last().unwrap();
        let tail_number = tail.header.number();
        let tail_hash = tail.header.hash();

        // Block is behind our window
        if block_number <= tail_number {
            return EnqueueOutcome::Duplicate;
        }

        // Gap — need backfill
        let expected = tail_number + 1;
        if block_number > expected {
            return EnqueueOutcome::NeedBackfill {
                from: expected,
                to: block_number - 1,
            };
        }

        // block_number == expected — check parent connects
        if parent_hash != tail_hash {
            // Parent mismatch — purge tail (it was reorged) and request backfill
            let purge_number = tail_number;
            if let Some(idx) = self
                .pending
                .iter()
                .position(|e| e.header.number() == purge_number)
            {
                self.purge_from(idx);
            }
            let new_expected = self
                .pending
                .last()
                .map(|e| e.header.number() + 1)
                .unwrap_or(block_number);
            if new_expected < block_number {
                return EnqueueOutcome::NeedBackfill {
                    from: new_expected,
                    to: block_number - 1,
                };
            }
            // If new_expected == block_number, fall through to accept (it'll be the anchor)
        }

        self.append(header, events, policy_events);
        EnqueueOutcome::Accepted
    }

    /// Enqueue a block during backfill. Accepts or skips duplicates.
    ///
    /// Panics on `NeedBackfill` — backfill blocks must be fetched sequentially.
    pub(crate) fn enqueue(
        &mut self,
        header: TempoHeader,
        events: L1PortalEvents,
        policy_events: Vec<PolicyEvent>,
    ) {
        match self.try_enqueue(SealedHeader::seal_slow(header), events, policy_events) {
            EnqueueOutcome::Accepted | EnqueueOutcome::Duplicate => {}
            other => panic!("enqueue expected Accepted or Duplicate, got {other:?}"),
        }
    }

    fn append(
        &mut self,
        header: SealedHeader<TempoHeader>,
        events: L1PortalEvents,
        policy_events: Vec<PolicyEvent>,
    ) {
        let queue_hash_before = self.enqueued_head_hash;
        for deposit in &events.deposits {
            self.enqueued_head_hash = deposit.hash_chain(self.enqueued_head_hash);
        }
        let queue_hash_after = self.enqueued_head_hash;
        self.last_enqueued = Some(header.num_hash());
        self.pending.push(L1BlockDeposits {
            header,
            events,
            policy_events,
            queue_hash_before,
            queue_hash_after,
        });
    }

    fn purge_from(&mut self, idx: usize) {
        if idx == 0 {
            self.enqueued_head_hash = self.processed_head_hash;
            self.pending.clear();
        } else {
            self.enqueued_head_hash = self.pending[idx - 1].queue_hash_after;
            self.pending.truncate(idx);
        }
        self.last_enqueued = self.pending.last().map(|e| e.header.num_hash());
    }

    /// Peek at the next pending L1 block without removing it.
    ///
    /// Returns `None` if no L1 blocks are queued. Use [`confirm`](Self::confirm)
    /// after a successful build to advance the queue.
    pub(crate) fn peek(&self) -> Option<&L1BlockDeposits> {
        self.pending.first()
    }

    /// Confirm the next pending L1 block was successfully processed and remove it.
    ///
    /// The caller must pass the [`NumHash`] of the block being confirmed. If the
    /// front of the queue no longer matches (e.g. because a reorg purged it
    /// between `peek` and `confirm`), this returns `None` and the queue is left
    /// unchanged.
    ///
    /// Must be called after a successful payload build + newPayload acceptance.
    /// Advances `processed_head_hash` and `last_processed`.
    pub(crate) fn confirm(&mut self, expected: NumHash) -> Option<L1BlockDeposits> {
        let front = self.pending.first()?;
        if front.header.num_hash() != expected {
            return None;
        }
        let block = self.pending.remove(0);
        self.processed_head_hash = block.queue_hash_after;
        // Keep last_processed monotonic — never regress the floor.
        let num_hash = block.header.num_hash();
        if self
            .last_processed
            .is_none_or(|lp| num_hash.number > lp.number)
        {
            self.last_processed = Some(num_hash);
        }
        Some(block)
    }

    /// Drain all pending L1 block deposits.
    #[cfg(test)]
    fn drain(&mut self) -> Vec<L1BlockDeposits> {
        if let Some(last) = self.pending.last() {
            self.processed_head_hash = last.queue_hash_after;
            // Keep last_processed monotonic — never regress the floor.
            let num_hash = last.header.num_hash();
            if self
                .last_processed
                .is_none_or(|lp| num_hash.number > lp.number)
            {
                self.last_processed = Some(num_hash);
            }
        }
        std::mem::take(&mut self.pending)
    }

    /// Compute a [`DepositQueueTransition`] for a batch of deposits starting from `prev_hash`
    #[cfg(test)]
    fn transition(prev_hash: B256, deposits: &[L1Deposit]) -> DepositQueueTransition {
        let mut current = prev_hash;
        for d in deposits {
            current = d.hash_chain(current);
        }
        DepositQueueTransition {
            prev_processed_hash: prev_hash,
            next_processed_hash: current,
        }
    }

    /// Returns the deposit hash chain value after all enqueued deposits.
    #[cfg(test)]
    fn enqueued_head_hash(&self) -> B256 {
        self.enqueued_head_hash
    }

    /// Returns the number of pending L1 blocks.
    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Returns a reference to the pending block at the given index.
    #[cfg(test)]
    fn pending_block(&self, idx: usize) -> Option<&L1BlockDeposits> {
        self.pending.get(idx)
    }

    /// Returns a slice of all pending L1 block deposits.
    #[cfg(test)]
    fn pending_blocks(&self) -> &[L1BlockDeposits] {
        &self.pending
    }

    /// Returns the most recently enqueued L1 block (number + hash), if any.
    pub(crate) fn last_enqueued(&self) -> Option<NumHash> {
        self.last_enqueued
    }

    /// Clears the `last_enqueued` anchor. Used when a reorg invalidates
    /// the consumed block that `last_enqueued` pointed to.
    #[cfg(test)]
    fn clear_last_enqueued(&mut self) {
        self.last_enqueued = None;
    }

    /// Returns the last L1 block consumed by the engine.
    #[cfg(test)]
    fn last_processed(&self) -> Option<NumHash> {
        self.last_processed
    }
}

/// Deposit queue transition for batch proof validation.
///
/// Represents the state of the deposit hash chain for a batch
/// of deposits processed by the zone. Used to prove which deposits were
/// included in a block.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
struct DepositQueueTransition {
    /// Hash chain head before the batch is processed
    prev_processed_hash: B256,
    /// Hash chain head after the batch is processed
    next_processed_hash: B256,
}

/// Shared deposit queue with notification support.
///
/// Wraps the pending deposits with a `Notify` so the ZoneEngine can be
/// woken instantly when new L1 blocks arrive.
#[derive(Debug, Clone)]
pub struct DepositQueue {
    inner: Arc<Mutex<PendingDeposits>>,
    notify: Arc<tokio::sync::Notify>,
}

impl DepositQueue {
    /// Create a new empty deposit queue.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(PendingDeposits::default())),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Try to enqueue an L1 block. Returns the outcome — callers handle
    /// `NeedBackfill` by fetching missing blocks and retrying.
    pub(crate) fn try_enqueue(
        &self,
        header: SealedHeader<TempoHeader>,
        events: L1PortalEvents,
        policy_events: Vec<PolicyEvent>,
    ) -> EnqueueOutcome {
        let mut queue = self.inner.lock();
        let outcome = queue.try_enqueue(header, events, policy_events);
        if matches!(outcome, EnqueueOutcome::Accepted) {
            drop(queue);
            self.notify.notify_one();
        }
        outcome
    }

    /// Enqueue an L1 block with its deposits and notify waiters.
    pub fn enqueue(
        &self,
        header: TempoHeader,
        events: L1PortalEvents,
        policy_events: Vec<PolicyEvent>,
    ) {
        self.inner.lock().enqueue(header, events, policy_events);
        self.notify.notify_one();
    }

    /// Like [`enqueue`](Self::enqueue) but accepts an already-sealed header,
    /// avoiding a redundant hash computation.
    pub fn enqueue_sealed(
        &self,
        header: SealedHeader<TempoHeader>,
        events: L1PortalEvents,
        policy_events: Vec<PolicyEvent>,
    ) {
        let mut queue = self.inner.lock();
        match queue.try_enqueue(header, events, policy_events) {
            EnqueueOutcome::Accepted | EnqueueOutcome::Duplicate => {}
            other => panic!("enqueue_sealed expected Accepted or Duplicate, got {other:?}"),
        }
        drop(queue);
        self.notify.notify_one();
    }

    /// Peek at the next L1 block without removing it.
    pub fn peek(&self) -> Option<L1BlockDeposits> {
        self.inner.lock().peek().cloned()
    }

    /// Confirm the next L1 block was successfully processed and remove it.
    ///
    /// Returns `None` if the front of the queue no longer matches `expected`
    /// (e.g. a reorg purged it between `peek` and `confirm`).
    pub fn confirm(&self, expected: NumHash) -> Option<L1BlockDeposits> {
        self.inner.lock().confirm(expected)
    }

    /// Wait until an L1 block is available.
    pub async fn notified(&self) {
        self.notify.notified().await
    }

    /// Returns the most recently enqueued L1 block (number + hash), if any.
    ///
    /// This is a high-water mark that survives `confirm` / `drain`, so it
    /// reflects the last block ever enqueued — not just what's still pending.
    pub fn last_enqueued(&self) -> Option<NumHash> {
        self.inner.lock().last_enqueued()
    }

    #[cfg(test)]
    fn drain(&self) -> Vec<L1BlockDeposits> {
        self.inner.lock().drain()
    }
}

impl Default for DepositQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{DepositType, PORTAL_PENDING_SEQUENCER_SLOT, PORTAL_SEQUENCER_SLOT};
    use alloy_consensus::Header;
    use alloy_primitives::{FixedBytes, address};
    use alloy_sol_types::SolEvent;
    use std::collections::HashSet;

    fn make_test_header(number: u64) -> TempoHeader {
        TempoHeader {
            inner: Header {
                number,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Create a header that chains to the given parent.
    fn make_chained_header(number: u64, parent_hash: B256) -> TempoHeader {
        TempoHeader {
            inner: Header {
                number,
                parent_hash,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn seal(header: TempoHeader) -> SealedHeader<TempoHeader> {
        SealedHeader::seal_slow(header)
    }

    fn header_hash(header: &TempoHeader) -> B256 {
        keccak256(alloy_rlp::encode(header))
    }

    /// Confirm the front of the queue, panicking if it fails.
    fn confirm(queue: &mut PendingDeposits) -> L1BlockDeposits {
        let num_hash = queue.peek().expect("queue is empty").header.num_hash();
        queue.confirm(num_hash).expect("confirm mismatch")
    }

    /// Confirm the front of a shared `DepositQueue`, panicking if it fails.
    fn confirm_shared(queue: &DepositQueue) -> L1BlockDeposits {
        let num_hash = queue.peek().expect("queue is empty").header.num_hash();
        queue.confirm(num_hash).expect("confirm mismatch")
    }

    fn make_portal_log<E: SolEvent>(portal_address: Address, event: E) -> Log {
        Log {
            inner: alloy_primitives::Log {
                address: portal_address,
                data: event.encode_log_data(),
            },
            block_hash: None,
            block_number: None,
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        }
    }

    #[test]
    fn test_push_log_decodes_bounce_back_as_regular_deposit() {
        let portal_address = address!("0x0000000000000000000000000000000000000ABC");
        let fallback_recipient = address!("0x00000000000000000000000000000000000000F1");
        let token = address!("0x0000000000000000000000000000000000002000");
        let event = BounceBack {
            newCurrentDepositQueueHash: B256::with_last_byte(0x42),
            fallbackRecipient: fallback_recipient,
            token,
            amount: 123_456,
        };
        let log = Log {
            inner: alloy_primitives::Log {
                address: portal_address,
                data: event.encode_log_data(),
            },
            block_hash: None,
            block_number: None,
            block_timestamp: None,
            transaction_hash: None,
            transaction_index: None,
            log_index: None,
            removed: false,
        };

        let mut events = L1PortalEvents::default();
        events
            .push_log(&log, 123)
            .expect("bounce-back should decode");

        assert_eq!(events.deposits.len(), 1, "should enqueue one deposit");
        let L1Deposit::Regular(deposit) = &events.deposits[0] else {
            panic!("bounce-back should be mapped to a regular deposit");
        };
        assert_eq!(deposit.token, token);
        assert_eq!(deposit.sender, portal_address);
        assert_eq!(deposit.to, fallback_recipient);
        assert_eq!(deposit.amount, event.amount);
        assert_eq!(deposit.fee, 0, "bounce-back deposits should be fee-free");
        assert_eq!(
            deposit.memo,
            B256::ZERO,
            "bounce-back deposits should clear memo"
        );
    }

    #[test]
    fn test_push_log_decodes_sequencer_transfer_started() {
        let portal_address = address!("0x0000000000000000000000000000000000000ABC");
        let current_sequencer = address!("0x00000000000000000000000000000000000000A1");
        let pending_sequencer = address!("0x00000000000000000000000000000000000000B2");
        let event = SequencerTransferStarted {
            currentSequencer: current_sequencer,
            pendingSequencer: pending_sequencer,
        };
        let log = make_portal_log(portal_address, event);

        let mut events = L1PortalEvents::default();
        events
            .push_log(&log, 123)
            .expect("sequencer transfer start should decode");

        assert_eq!(
            events.sequencer_events,
            vec![L1SequencerEvent::TransferStarted {
                current_sequencer,
                pending_sequencer,
            }]
        );
        assert!(events.deposits.is_empty());
        assert!(events.enabled_tokens.is_empty());
    }

    #[test]
    fn test_push_log_decodes_sequencer_transferred() {
        let portal_address = address!("0x0000000000000000000000000000000000000ABC");
        let previous_sequencer = address!("0x00000000000000000000000000000000000000A1");
        let new_sequencer = address!("0x00000000000000000000000000000000000000B2");
        let event = SequencerTransferred {
            previousSequencer: previous_sequencer,
            newSequencer: new_sequencer,
        };
        let log = make_portal_log(portal_address, event);

        let mut events = L1PortalEvents::default();
        events
            .push_log(&log, 123)
            .expect("sequencer transferred should decode");

        assert_eq!(
            events.sequencer_events,
            vec![L1SequencerEvent::Transferred {
                previous_sequencer,
                new_sequencer,
            }]
        );
        assert!(events.deposits.is_empty());
        assert!(events.enabled_tokens.is_empty());
    }

    #[test]
    fn test_apply_sequencer_events_to_cache_sets_pending_sequencer() {
        let portal_address = address!("0x0000000000000000000000000000000000000ABC");
        let current_sequencer = address!("0x00000000000000000000000000000000000000A1");
        let pending_sequencer = address!("0x00000000000000000000000000000000000000B2");
        let mut cache = L1StateCache::new(HashSet::from([portal_address]));

        apply_sequencer_events_to_cache(
            &mut cache,
            portal_address,
            42,
            &[L1SequencerEvent::TransferStarted {
                current_sequencer,
                pending_sequencer,
            }],
        );

        assert_eq!(
            cache.get(portal_address, PORTAL_SEQUENCER_SLOT, 42),
            Some(address_to_storage_value(current_sequencer))
        );
        assert_eq!(
            cache.get(portal_address, PORTAL_PENDING_SEQUENCER_SLOT, 42),
            Some(address_to_storage_value(pending_sequencer))
        );
    }

    #[test]
    fn test_apply_sequencer_events_to_cache_accept_clears_pending_sequencer() {
        let portal_address = address!("0x0000000000000000000000000000000000000ABC");
        let previous_sequencer = address!("0x00000000000000000000000000000000000000A1");
        let new_sequencer = address!("0x00000000000000000000000000000000000000B2");
        let mut cache = L1StateCache::new(HashSet::from([portal_address]));

        apply_sequencer_events_to_cache(
            &mut cache,
            portal_address,
            43,
            &[L1SequencerEvent::Transferred {
                previous_sequencer,
                new_sequencer,
            }],
        );

        assert_eq!(
            cache.get(portal_address, PORTAL_SEQUENCER_SLOT, 43),
            Some(address_to_storage_value(new_sequencer))
        );
        assert_eq!(
            cache.get(portal_address, PORTAL_PENDING_SEQUENCER_SLOT, 43),
            Some(B256::ZERO)
        );
    }

    #[test]
    fn test_apply_sequencer_events_to_cache_preserves_in_block_event_order() {
        let portal_address = address!("0x0000000000000000000000000000000000000ABC");
        let sequencer_a = address!("0x00000000000000000000000000000000000000A1");
        let sequencer_b = address!("0x00000000000000000000000000000000000000B2");
        let sequencer_c = address!("0x00000000000000000000000000000000000000C3");
        let mut cache = L1StateCache::new(HashSet::from([portal_address]));

        apply_sequencer_events_to_cache(
            &mut cache,
            portal_address,
            44,
            &[
                L1SequencerEvent::TransferStarted {
                    current_sequencer: sequencer_a,
                    pending_sequencer: sequencer_b,
                },
                L1SequencerEvent::Transferred {
                    previous_sequencer: sequencer_a,
                    new_sequencer: sequencer_b,
                },
                L1SequencerEvent::TransferStarted {
                    current_sequencer: sequencer_b,
                    pending_sequencer: sequencer_c,
                },
            ],
        );

        assert_eq!(
            cache.get(portal_address, PORTAL_SEQUENCER_SLOT, 44),
            Some(address_to_storage_value(sequencer_b))
        );
        assert_eq!(
            cache.get(portal_address, PORTAL_PENDING_SEQUENCER_SLOT, 44),
            Some(address_to_storage_value(sequencer_c))
        );
    }

    #[test]
    fn test_deposit_queue_hash_chain() {
        let mut queue = PendingDeposits::default();
        assert_eq!(queue.enqueued_head_hash(), B256::ZERO);

        let d1 = L1Deposit::Regular(Deposit {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 1000,
            fee: 0,
            memo: B256::ZERO,
        });

        queue.enqueue(
            make_test_header(1),
            L1PortalEvents::from_deposits(vec![d1.clone()]),
            vec![],
        );
        let hash_after_d1 = queue.enqueued_head_hash();
        assert_ne!(hash_after_d1, B256::ZERO);

        // Verify hash is deterministic
        let mut queue2 = PendingDeposits::default();
        queue2.enqueue(
            make_test_header(1),
            L1PortalEvents::from_deposits(vec![d1]),
            vec![],
        );
        assert_eq!(hash_after_d1, queue2.enqueued_head_hash);

        let d2 = L1Deposit::Regular(Deposit {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000003"),
            to: address!("0x0000000000000000000000000000000000000004"),
            amount: 2000,
            fee: 0,
            memo: B256::ZERO,
        });

        queue.enqueue(
            make_test_header(2),
            L1PortalEvents::from_deposits(vec![d2]),
            vec![],
        );
        let hash_after_d2 = queue.enqueued_head_hash();
        assert_ne!(hash_after_d2, hash_after_d1);
    }

    #[test]
    fn test_process_deposits_transition() {
        let deposits = vec![
            L1Deposit::Regular(Deposit {
                token: address!("0x0000000000000000000000000000000000001000"),
                sender: address!("0x0000000000000000000000000000000000000001"),
                to: address!("0x0000000000000000000000000000000000000002"),
                amount: 1000,
                fee: 0,
                memo: B256::ZERO,
            }),
            L1Deposit::Regular(Deposit {
                token: address!("0x0000000000000000000000000000000000001000"),
                sender: address!("0x0000000000000000000000000000000000000003"),
                to: address!("0x0000000000000000000000000000000000000004"),
                amount: 2000,
                fee: 0,
                memo: B256::ZERO,
            }),
        ];

        let transition = PendingDeposits::transition(B256::ZERO, &deposits);

        assert_eq!(transition.prev_processed_hash, B256::ZERO);
        assert_ne!(transition.next_processed_hash, B256::ZERO);

        // Second batch with no deposits should be a no-op
        let transition2 = PendingDeposits::transition(transition.next_processed_hash, &[]);
        assert_eq!(
            transition2.prev_processed_hash,
            transition.next_processed_hash
        );
        assert_eq!(
            transition2.next_processed_hash,
            transition.next_processed_hash
        );
    }

    #[test]
    fn test_queue_and_process_deposits_hashes_match() {
        let mut queue = PendingDeposits::default();

        let deposits = vec![L1Deposit::Regular(Deposit {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 500,
            fee: 0,
            memo: FixedBytes::from([0xABu8; 32]),
        })];

        queue.enqueue(
            make_test_header(1),
            L1PortalEvents::from_deposits(deposits.clone()),
            vec![],
        );

        let transition = PendingDeposits::transition(B256::ZERO, &deposits);

        assert_eq!(queue.enqueued_head_hash(), transition.next_processed_hash);
    }

    #[test]
    fn test_drain_returns_block_grouped_deposits() {
        let mut queue = PendingDeposits::default();

        let d1 = L1Deposit::Regular(Deposit {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 100,
            fee: 0,
            memo: B256::ZERO,
        });

        let d2 = L1Deposit::Regular(Deposit {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000003"),
            to: address!("0x0000000000000000000000000000000000000004"),
            amount: 200,
            fee: 0,
            memo: B256::ZERO,
        });

        let h10 = make_test_header(10);
        let h10_hash = header_hash(&h10);
        queue.enqueue(h10, L1PortalEvents::from_deposits(vec![d1]), vec![]);
        queue.enqueue(
            make_chained_header(11, h10_hash),
            L1PortalEvents::from_deposits(vec![d2]),
            vec![],
        );

        let blocks = queue.drain();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].header.number(), 10);
        assert_eq!(blocks[0].events.deposits.len(), 1);
        assert_eq!(blocks[1].header.number(), 11);
        assert_eq!(blocks[1].events.deposits.len(), 1);

        // After drain, pending is empty
        assert!(queue.drain().is_empty());
    }

    #[test]
    fn test_encrypted_deposit_hash_chain() {
        let token = address!("0x0000000000000000000000000000000000001000");
        let sender = address!("0x0000000000000000000000000000000000001234");

        let encrypted = EncryptedDeposit {
            token,
            sender,
            amount: 1_000_000,
            fee: 0,
            key_index: U256::ZERO,
            ephemeral_pubkey_x: B256::with_last_byte(0xAA),
            ephemeral_pubkey_y_parity: 0x02,
            ciphertext: vec![0x42u8; 64],
            nonce: [0x01; 12],
            tag: [0x02; 16],
        };

        // Compute via PendingDeposits (Rust implementation)
        let transition =
            PendingDeposits::transition(B256::ZERO, &[L1Deposit::Encrypted(encrypted.clone())]);

        // Compute expected hash via direct Solidity-compatible encoding
        let abi_encrypted = abi::EncryptedDeposit {
            token: encrypted.token,
            sender: encrypted.sender,
            amount: encrypted.amount,
            keyIndex: encrypted.key_index,
            encrypted: abi::EncryptedDepositPayload {
                ephemeralPubkeyX: encrypted.ephemeral_pubkey_x,
                ephemeralPubkeyYParity: encrypted.ephemeral_pubkey_y_parity,
                ciphertext: encrypted.ciphertext.clone().into(),
                nonce: encrypted.nonce.into(),
                tag: encrypted.tag.into(),
            },
        };
        let expected = keccak256((DepositType::Encrypted, abi_encrypted, B256::ZERO).abi_encode());

        assert_eq!(
            transition.next_processed_hash, expected,
            "encrypted deposit hash chain must match Solidity keccak256(abi.encode(Encrypted, deposit, prevHash))"
        );
        assert_ne!(
            transition.next_processed_hash,
            B256::ZERO,
            "hash should be non-zero"
        );
    }

    #[test]
    fn test_mixed_deposit_hash_chain() {
        let token = address!("0x0000000000000000000000000000000000001000");
        let sender = address!("0x0000000000000000000000000000000000001111");
        let recipient = address!("0x000000000000000000000000000000000000A11C");

        let regular = Deposit {
            token,
            sender,
            to: recipient,
            amount: 500_000,
            fee: 0,
            memo: B256::ZERO,
        };

        let encrypted = EncryptedDeposit {
            token,
            sender,
            amount: 300_000,
            fee: 0,
            key_index: U256::from(1u64),
            ephemeral_pubkey_x: B256::with_last_byte(0xBB),
            ephemeral_pubkey_y_parity: 0x03,
            ciphertext: vec![0x55u8; 64],
            nonce: [0x0A; 12],
            tag: [0x0B; 16],
        };

        let deposits = vec![
            L1Deposit::Regular(regular.clone()),
            L1Deposit::Encrypted(encrypted.clone()),
        ];

        let transition = PendingDeposits::transition(B256::ZERO, &deposits);

        // Manually compute expected chain
        let hash_1 = keccak256(
            (
                DepositType::Regular,
                abi::Deposit {
                    token: regular.token,
                    sender: regular.sender,
                    to: regular.to,
                    amount: regular.amount,
                    memo: regular.memo,
                },
                B256::ZERO,
            )
                .abi_encode(),
        );

        let hash_2 = keccak256(
            (
                DepositType::Encrypted,
                abi::EncryptedDeposit {
                    token: encrypted.token,
                    sender: encrypted.sender,
                    amount: encrypted.amount,
                    keyIndex: encrypted.key_index,
                    encrypted: abi::EncryptedDepositPayload {
                        ephemeralPubkeyX: encrypted.ephemeral_pubkey_x,
                        ephemeralPubkeyYParity: encrypted.ephemeral_pubkey_y_parity,
                        ciphertext: encrypted.ciphertext.into(),
                        nonce: encrypted.nonce.into(),
                        tag: encrypted.tag.into(),
                    },
                },
                hash_1,
            )
                .abi_encode(),
        );

        assert_eq!(transition.prev_processed_hash, B256::ZERO);
        assert_eq!(transition.next_processed_hash, hash_2);
    }

    #[test]
    fn test_enqueue_and_transition_consistency() {
        let token = address!("0x0000000000000000000000000000000000001000");
        let sender = address!("0x0000000000000000000000000000000000001234");

        let encrypted = EncryptedDeposit {
            token,
            sender,
            amount: 750_000,
            fee: 0,
            key_index: U256::from(2u64),
            ephemeral_pubkey_x: B256::with_last_byte(0xCC),
            ephemeral_pubkey_y_parity: 0x02,
            ciphertext: vec![0x99u8; 64],
            nonce: [0x03; 12],
            tag: [0x04; 16],
        };

        let deposits = vec![L1Deposit::Encrypted(encrypted)];

        // Path 1: enqueue into PendingDeposits
        let mut pending = PendingDeposits::default();
        let header = make_test_header(1);
        pending.enqueue(
            header,
            L1PortalEvents::from_deposits(deposits.clone()),
            vec![],
        );

        // Path 2: compute transition directly
        let transition = PendingDeposits::transition(B256::ZERO, &deposits);

        assert_eq!(
            pending.enqueued_head_hash, transition.next_processed_hash,
            "enqueue and transition must produce the same hash"
        );
    }

    #[test]
    fn test_last_enqueued_survives_pop_and_drain() {
        let queue = DepositQueue::new();

        // Initially empty
        assert!(queue.last_enqueued().is_none());

        let h100 = make_test_header(100);
        let h100_hash = header_hash(&h100);
        queue.enqueue(h100, L1PortalEvents::from_deposits(vec![]), vec![]);
        let h101 = make_chained_header(101, h100_hash);
        let h101_hash = header_hash(&h101);
        queue.enqueue(h101, L1PortalEvents::from_deposits(vec![]), vec![]);
        let h102 = make_chained_header(102, h101_hash);
        queue.enqueue(h102, L1PortalEvents::from_deposits(vec![]), vec![]);

        let last = queue.last_enqueued().unwrap();
        assert_eq!(last.number, 102);

        // Confirm all blocks — last_enqueued must still report 102
        assert!(queue.peek().is_some());
        confirm_shared(&queue);
        assert!(queue.peek().is_some());
        confirm_shared(&queue);
        assert!(queue.peek().is_some());
        confirm_shared(&queue);
        assert!(queue.peek().is_none());

        let last = queue.last_enqueued().unwrap();
        assert_eq!(last.number, 102, "last_enqueued must survive confirm");

        // Enqueue more (continuing from 102), then drain — last_enqueued must still track
        let h102_hash = last.hash;
        let h103 = make_chained_header(103, h102_hash);
        let h103_hash = header_hash(&h103);
        queue.enqueue(h103, L1PortalEvents::from_deposits(vec![]), vec![]);
        queue.enqueue(
            make_chained_header(104, h103_hash),
            L1PortalEvents::from_deposits(vec![]),
            vec![],
        );
        assert_eq!(queue.last_enqueued().unwrap().number, 104);

        let drained = queue.drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(
            queue.last_enqueued().unwrap().number,
            104,
            "last_enqueued must survive drain"
        );
    }

    #[test]
    fn test_try_enqueue_sequential_append() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        let h2 = make_chained_header(11, h1_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h2), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        assert_eq!(queue.pending_len(), 2);
    }

    #[test]
    fn test_try_enqueue_gap_returns_need_backfill() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Skip block 11, try to enqueue 12
        let h3 = make_test_header(12);
        match queue.try_enqueue(seal(h3), L1PortalEvents::from_deposits(vec![]), vec![]) {
            EnqueueOutcome::NeedBackfill { from, to } => {
                assert_eq!(from, 11);
                assert_eq!(to, 11);
            }
            other => panic!("expected NeedBackfill, got {other:?}"),
        }
    }

    #[test]
    fn test_try_enqueue_duplicate() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        assert!(matches!(
            queue.try_enqueue(
                seal(h1.clone()),
                L1PortalEvents::from_deposits(vec![]),
                vec![]
            ),
            EnqueueOutcome::Accepted
        ));
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Duplicate
        ));
    }

    #[test]
    fn test_try_enqueue_reorg_purges_stale() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        let h2 = make_chained_header(11, h1_hash);
        let h2_hash = header_hash(&h2);
        assert!(matches!(
            queue.try_enqueue(seal(h2), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        let h3 = make_chained_header(12, h2_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h3), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        assert_eq!(queue.pending_len(), 3);

        // Reorg at height 11 — use a different header (different gas_limit makes the hash different)
        let mut h2_reorg = make_chained_header(11, h1_hash);
        h2_reorg.inner.gas_limit = 999;
        assert!(matches!(
            queue.try_enqueue(
                seal(h2_reorg),
                L1PortalEvents::from_deposits(vec![]),
                vec![]
            ),
            EnqueueOutcome::Accepted
        ));

        // Blocks 12 and the old 11 should be purged, replaced by new 11
        assert_eq!(queue.pending_len(), 2);
        assert_eq!(queue.pending_block(0).unwrap().header.number(), 10);
        assert_eq!(queue.pending_block(1).unwrap().header.number(), 11);
    }

    #[test]
    fn test_try_enqueue_parent_mismatch_at_tip() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        let h2 = make_chained_header(11, h1_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h2), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Block 12 with wrong parent hash — purges block 11, needs backfill
        let h3 = make_chained_header(12, B256::with_last_byte(0xDE));
        match queue.try_enqueue(seal(h3), L1PortalEvents::from_deposits(vec![]), vec![]) {
            EnqueueOutcome::NeedBackfill { from, to } => {
                assert_eq!(from, 11);
                assert_eq!(to, 11);
            }
            other => panic!("expected NeedBackfill, got {other:?}"),
        }
    }

    #[test]
    fn test_purge_rolls_back_deposit_hash() {
        let mut queue = PendingDeposits::default();
        let token = address!("0x0000000000000000000000000000000000001000");

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        let d1 = L1Deposit::Regular(Deposit {
            token,
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 100,
            fee: 0,
            memo: B256::ZERO,
        });
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![d1]), vec![]),
            EnqueueOutcome::Accepted
        ));
        let hash_after_h1 = queue.enqueued_head_hash();

        let h2 = make_chained_header(11, h1_hash);
        let d2 = L1Deposit::Regular(Deposit {
            token,
            sender: address!("0x0000000000000000000000000000000000000003"),
            to: address!("0x0000000000000000000000000000000000000004"),
            amount: 200,
            fee: 0,
            memo: B256::ZERO,
        });
        assert!(matches!(
            queue.try_enqueue(seal(h2), L1PortalEvents::from_deposits(vec![d2]), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Hash advanced past h1
        assert_ne!(queue.enqueued_head_hash(), hash_after_h1);

        // Now reorg at height 11 — different header
        let mut h2_reorg = make_chained_header(11, h1_hash);
        h2_reorg.inner.gas_limit = 999;
        assert!(matches!(
            queue.try_enqueue(
                seal(h2_reorg),
                L1PortalEvents::from_deposits(vec![]),
                vec![]
            ),
            EnqueueOutcome::Accepted
        ));

        // Hash should have rolled back to after h1 (since h2_reorg has no deposits)
        assert_eq!(queue.enqueued_head_hash(), hash_after_h1);
    }

    fn make_deposit(amount: u128) -> L1Deposit {
        L1Deposit::Regular(Deposit {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount,
            fee: 0,
            memo: B256::ZERO,
        })
    }

    #[test]
    fn test_pop_advances_processed_head_hash() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(1);
        let h1_hash = header_hash(&h1);
        queue.enqueue(
            h1,
            L1PortalEvents::from_deposits(vec![make_deposit(100)]),
            vec![],
        );

        let h2 = make_chained_header(2, h1_hash);
        let h2_hash = header_hash(&h2);
        queue.enqueue(
            h2,
            L1PortalEvents::from_deposits(vec![make_deposit(200)]),
            vec![],
        );

        let h3 = make_chained_header(3, h2_hash);
        queue.enqueue(
            h3,
            L1PortalEvents::from_deposits(vec![make_deposit(300)]),
            vec![],
        );

        let hash_after_all = queue.enqueued_head_hash();

        // Confirm block 1
        let peeked = queue.peek().unwrap().clone();
        assert_eq!(peeked.header.number(), 1);
        confirm(&mut queue);
        assert_eq!(queue.processed_head_hash, peeked.queue_hash_after);

        // queue.enqueued_head_hash() hasn't changed
        assert_eq!(queue.enqueued_head_hash(), hash_after_all);

        // Recompute expected hash from processed_head_hash + remaining deposits (blocks 2, 3)
        let remaining_deposits: Vec<L1Deposit> = queue
            .pending_blocks()
            .iter()
            .flat_map(|b| b.events.deposits.clone())
            .collect();
        let transition =
            PendingDeposits::transition(queue.processed_head_hash, &remaining_deposits);
        assert_eq!(transition.next_processed_hash, queue.enqueued_head_hash());
    }

    #[test]
    fn test_purge_after_pops() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(1);
        let h1_hash = header_hash(&h1);
        queue.enqueue(
            h1,
            L1PortalEvents::from_deposits(vec![make_deposit(100)]),
            vec![],
        );

        let h2 = make_chained_header(2, h1_hash);
        let h2_hash = header_hash(&h2);
        queue.enqueue(h2, L1PortalEvents::from_deposits(vec![]), vec![]);

        let h3 = make_chained_header(3, h2_hash);
        let h3_hash = header_hash(&h3);
        queue.enqueue(h3, L1PortalEvents::from_deposits(vec![]), vec![]);

        let h4 = make_chained_header(4, h3_hash);
        let h4_hash = header_hash(&h4);
        queue.enqueue(h4, L1PortalEvents::from_deposits(vec![]), vec![]);

        let h5 = make_chained_header(5, h4_hash);
        queue.enqueue(h5, L1PortalEvents::from_deposits(vec![]), vec![]);

        // Pop blocks 1 and 2
        confirm(&mut queue);
        confirm(&mut queue);
        assert_eq!(queue.pending_len(), 3); // blocks 3, 4, 5

        let hash_after_block3 = queue.pending_block(0).unwrap().queue_hash_after;

        // Trigger purge at block 4: different header at height 4
        let mut h4_reorg = make_chained_header(4, h3_hash);
        h4_reorg.inner.gas_limit = 999;
        assert!(matches!(
            queue.try_enqueue(
                seal(h4_reorg),
                L1PortalEvents::from_deposits(vec![]),
                vec![]
            ),
            EnqueueOutcome::Accepted
        ));

        // Pending should have blocks 3 and new-4
        assert_eq!(queue.pending_len(), 2);
        assert_eq!(queue.pending_block(0).unwrap().header.number(), 3);
        assert_eq!(queue.pending_block(1).unwrap().header.number(), 4);

        // New block 4 has no deposits, so hash == hash after block 3's deposits
        assert_eq!(queue.enqueued_head_hash(), hash_after_block3);
    }

    #[test]
    fn test_purge_first_pending_after_pop() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(1);
        let h1_hash = header_hash(&h1);
        queue.enqueue(
            h1,
            L1PortalEvents::from_deposits(vec![make_deposit(100)]),
            vec![],
        );

        let h2 = make_chained_header(2, h1_hash);
        let h2_hash = header_hash(&h2);
        queue.enqueue(
            h2,
            L1PortalEvents::from_deposits(vec![make_deposit(200)]),
            vec![],
        );

        let h3 = make_chained_header(3, h2_hash);
        queue.enqueue(
            h3,
            L1PortalEvents::from_deposits(vec![make_deposit(300)]),
            vec![],
        );

        // Pop block 1 — processed_head_hash advances
        let popped = confirm(&mut queue);
        let base_after_pop = popped.queue_hash_after;
        assert_eq!(queue.processed_head_hash, base_after_pop);
        assert_eq!(queue.pending_len(), 2); // blocks 2, 3

        // Purge from block 2 by enqueueing a different block 2
        let mut h2_reorg = make_chained_header(2, B256::with_last_byte(0xFF));
        h2_reorg.inner.gas_limit = 777;
        // This block has a different hash at height 2, so purge_from(0) fires.
        // Queue becomes empty, then the new block is accepted as anchor.
        let outcome = queue.try_enqueue(
            seal(h2_reorg),
            L1PortalEvents::from_deposits(vec![]),
            vec![],
        );
        assert!(matches!(outcome, EnqueueOutcome::Accepted));

        // After purge and re-anchor, pending has just the new block 2
        assert_eq!(queue.pending_len(), 1);
        assert_eq!(queue.pending_block(0).unwrap().header.number(), 2);

        // processed_head_hash should still be what it was after popping block 1
        assert_eq!(queue.processed_head_hash, base_after_pop);
    }

    #[test]
    fn test_backfill_then_duplicate_redelivery() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(1);
        let h1_hash = header_hash(&h1);
        queue.enqueue(
            h1,
            L1PortalEvents::from_deposits(vec![make_deposit(100)]),
            vec![],
        );

        // Try to enqueue block 3 (skipping 2) => NeedBackfill
        let h2 = make_chained_header(2, h1_hash);
        let h2_hash = header_hash(&h2);
        let h3 = make_chained_header(3, h2_hash);
        let h3_sealed = seal(h3);
        match queue.try_enqueue(
            seal(make_test_header(3)),
            L1PortalEvents::from_deposits(vec![]),
            vec![],
        ) {
            EnqueueOutcome::NeedBackfill { from, to } => {
                assert_eq!(from, 2);
                assert_eq!(to, 2);
            }
            other => panic!("expected NeedBackfill, got {other:?}"),
        }

        // Backfill: enqueue block 2, then block 3
        queue.enqueue(
            h2,
            L1PortalEvents::from_deposits(vec![make_deposit(200)]),
            vec![],
        );
        assert!(matches!(
            queue.try_enqueue(
                h3_sealed.clone(),
                L1PortalEvents::from_deposits(vec![make_deposit(300)]),
                vec![],
            ),
            EnqueueOutcome::Accepted
        ));

        let hash_before = queue.enqueued_head_hash();
        let len_before = queue.pending_len();

        // Re-deliver block 3 (same sealed header) => Duplicate
        assert!(matches!(
            queue.try_enqueue(
                h3_sealed,
                L1PortalEvents::from_deposits(vec![make_deposit(300)]),
                vec![],
            ),
            EnqueueOutcome::Duplicate
        ));

        assert_eq!(queue.enqueued_head_hash(), hash_before);
        assert_eq!(queue.pending_len(), len_before);
    }

    #[test]
    fn test_zero_deposit_block_hash_invariant() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(1);
        let h1_hash = header_hash(&h1);
        queue.enqueue(
            h1,
            L1PortalEvents::from_deposits(vec![make_deposit(100)]),
            vec![],
        );

        let h2 = make_chained_header(2, h1_hash);
        let h2_hash = header_hash(&h2);
        queue.enqueue(h2, L1PortalEvents::from_deposits(vec![]), vec![]); // no deposits

        let h3 = make_chained_header(3, h2_hash);
        let d3 = make_deposit(300);
        queue.enqueue(h3, L1PortalEvents::from_deposits(vec![d3.clone()]), vec![]);

        // Block 2 has no deposits => queue_hash_before == queue_hash_after
        assert_eq!(
            queue.pending_block(1).unwrap().queue_hash_before,
            queue.pending_block(1).unwrap().queue_hash_after,
            "zero-deposit block must not change queue hash"
        );

        let hash_after_all_original = queue.enqueued_head_hash();

        // Purge at block 2 (different header) — purges blocks 2, 3
        let mut h2_reorg = make_chained_header(2, h1_hash);
        h2_reorg.inner.gas_limit = 888;
        let h2_reorg_hash = header_hash(&h2_reorg);
        assert!(matches!(
            queue.try_enqueue(
                seal(h2_reorg),
                L1PortalEvents::from_deposits(vec![]),
                vec![]
            ),
            EnqueueOutcome::Accepted
        ));

        // After purge, only block 1 and new block 2 remain
        assert_eq!(queue.pending_len(), 2);
        let hash_after_block1 = queue.pending_block(0).unwrap().queue_hash_after;
        // New block 2 has no deposits so hash == hash after block 1
        assert_eq!(queue.enqueued_head_hash(), hash_after_block1);

        // Re-enqueue new block 3 with same deposits as original
        let h3_new = make_chained_header(3, h2_reorg_hash);
        assert!(matches!(
            queue.try_enqueue(
                seal(h3_new),
                L1PortalEvents::from_deposits(vec![d3]),
                vec![]
            ),
            EnqueueOutcome::Accepted
        ));

        // The hash should match original because the deposit content and
        // chain of hashes are identical (both block 2 variants had no deposits)
        assert_eq!(
            queue.enqueued_head_hash(),
            hash_after_all_original,
            "hash should be identical when deposit content is the same"
        );
    }

    // --- Disconnected scenario tests (parent mismatch on drained queue) ---

    #[test]
    fn test_disconnected_after_full_drain() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        let h2 = make_chained_header(11, h1_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h2), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Drain everything
        confirm(&mut queue);
        confirm(&mut queue);
        assert!(queue.pending_len() == 0);
        assert_eq!(queue.last_enqueued().unwrap().number, 11);

        // Block 12 arrives with wrong parent — consumed block 11 was reorged
        let h3_bad = make_chained_header(12, B256::with_last_byte(0xDE));
        match queue.try_enqueue(seal(h3_bad), L1PortalEvents::from_deposits(vec![]), vec![]) {
            EnqueueOutcome::NeedBackfill { from, to } => {
                // Must re-fetch from block 11 (the consumed block that was reorged)
                assert_eq!(from, 11);
                assert_eq!(to, 11);
            }
            other => panic!("expected NeedBackfill, got {other:?}"),
        }

        // last_enqueued must be cleared so backfill can re-enqueue block 11
        assert!(
            queue.last_enqueued().is_none(),
            "last_enqueued must be cleared after parent mismatch on drained queue"
        );

        // Backfill tries to re-enqueue the reorged block 11 — must be Duplicate
        // because last_processed knows block 11 was already consumed by the engine.
        let h2_reorg = make_test_header(11);
        let h2_reorg_hash = header_hash(&h2_reorg);
        assert!(matches!(
            queue.try_enqueue(
                seal(h2_reorg),
                L1PortalEvents::from_deposits(vec![]),
                vec![]
            ),
            EnqueueOutcome::Duplicate
        ));

        // Block 12 on the new chain is accepted as the immediate successor
        // of the consumed window (parent hash mismatch is expected here —
        // the builder will detect it).
        let h3 = make_chained_header(12, h2_reorg_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h3), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));
        assert_eq!(queue.pending_len(), 1);
        assert_eq!(queue.pending_block(0).unwrap().header.number(), 12);
    }

    #[test]
    fn test_disconnected_after_partial_drain() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        assert!(matches!(
            queue.try_enqueue(
                seal(h1),
                L1PortalEvents::from_deposits(vec![make_deposit(100)]),
                vec![],
            ),
            EnqueueOutcome::Accepted
        ));

        let h2 = make_chained_header(11, h1_hash);
        let h2_hash = header_hash(&h2);
        assert!(matches!(
            queue.try_enqueue(
                seal(h2),
                L1PortalEvents::from_deposits(vec![make_deposit(200)]),
                vec![],
            ),
            EnqueueOutcome::Accepted
        ));

        let h3 = make_chained_header(12, h2_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h3), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Pop only block 10
        confirm(&mut queue);
        assert_eq!(queue.pending_len(), 2); // blocks 11, 12

        // Block 13 with wrong parent — this is a normal parent mismatch on the
        // non-empty queue path, should purge block 12 and request backfill
        let h4_bad = make_chained_header(13, B256::with_last_byte(0xAB));
        match queue.try_enqueue(seal(h4_bad), L1PortalEvents::from_deposits(vec![]), vec![]) {
            EnqueueOutcome::NeedBackfill { from, to } => {
                assert_eq!(from, 12);
                assert_eq!(to, 12);
            }
            other => panic!("expected NeedBackfill, got {other:?}"),
        }
    }

    #[test]
    fn test_disconnected_recovery_accepts_correct_block() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Drain
        confirm(&mut queue);
        assert!(queue.pending_len() == 0);

        // Wrong parent → NeedBackfill
        let h2_bad = make_chained_header(11, B256::with_last_byte(0xFF));
        assert!(matches!(
            queue.try_enqueue(seal(h2_bad), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::NeedBackfill { .. }
        ));

        // Correct parent → Accepted
        let h2_good = make_chained_header(11, h1_hash);
        assert!(matches!(
            queue.try_enqueue(
                seal(h2_good),
                L1PortalEvents::from_deposits(vec![make_deposit(500)]),
                vec![],
            ),
            EnqueueOutcome::Accepted
        ));
        assert_eq!(queue.pending_len(), 1);
        assert_eq!(queue.pending_block(0).unwrap().header.number(), 11);
    }

    #[test]
    fn test_disconnected_with_multi_block_gap() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Drain
        confirm(&mut queue);

        // Block 14 arrives — gap of 11..13 plus wrong parent is moot because
        // the gap check triggers first
        let h5 = make_test_header(14);
        match queue.try_enqueue(seal(h5), L1PortalEvents::from_deposits(vec![]), vec![]) {
            EnqueueOutcome::NeedBackfill { from, to } => {
                assert_eq!(from, 11);
                assert_eq!(to, 13);
            }
            other => panic!("expected NeedBackfill, got {other:?}"),
        }
    }

    #[test]
    fn test_duplicate_on_drained_queue() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        let h2 = make_chained_header(11, h1_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h2), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Drain everything
        confirm(&mut queue);
        confirm(&mut queue);
        assert!(queue.pending_len() == 0);

        // Re-deliver block 10 or 11 — should be Duplicate
        assert!(matches!(
            queue.try_enqueue(
                seal(make_test_header(10)),
                L1PortalEvents::from_deposits(vec![]),
                vec![],
            ),
            EnqueueOutcome::Duplicate
        ));
        assert!(matches!(
            queue.try_enqueue(
                seal(make_chained_header(11, h1_hash)),
                L1PortalEvents::from_deposits(vec![]),
                vec![],
            ),
            EnqueueOutcome::Duplicate
        ));
    }

    #[test]
    fn test_disconnected_preserves_processed_head_hash_and_deposits() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        assert!(matches!(
            queue.try_enqueue(
                seal(h1),
                L1PortalEvents::from_deposits(vec![make_deposit(100)]),
                vec![],
            ),
            EnqueueOutcome::Accepted
        ));

        // Pop and record processed_head_hash
        let popped = confirm(&mut queue);
        let base = queue.processed_head_hash;
        assert_eq!(base, popped.queue_hash_after);

        // Disconnected block should not alter processed_head_hash
        let h2_bad = make_chained_header(11, B256::with_last_byte(0xBB));
        assert!(matches!(
            queue.try_enqueue(seal(h2_bad), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::NeedBackfill { .. }
        ));
        assert_eq!(
            queue.processed_head_hash, base,
            "processed_head_hash must not change on NeedBackfill"
        );
        assert_eq!(
            queue.enqueued_head_hash(),
            base,
            "enqueued_head_hash must not change on NeedBackfill"
        );
        assert!(queue.pending_len() == 0);
    }

    #[test]
    fn test_reconnect_duplicate_does_not_clear_last_enqueued() {
        // A reconnect may re-deliver the same block we already consumed.
        // This must return Duplicate without clearing last_enqueued.
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        queue.enqueue(h1.clone(), L1PortalEvents::from_deposits(vec![]), vec![]);

        // Drain
        confirm(&mut queue);
        assert!(queue.pending_len() == 0);
        assert_eq!(queue.last_enqueued().unwrap().number, 10);

        // Re-deliver same block 10 — must be Duplicate, last_enqueued preserved
        assert!(matches!(
            queue.try_enqueue(seal(h1), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Duplicate
        ));
        assert_eq!(
            queue.last_enqueued().unwrap().number,
            10,
            "last_enqueued must not be cleared on Duplicate"
        );
    }

    #[test]
    fn test_backfill_overlap_idempotency() {
        // If backfill re-delivers blocks already in pending, duplicates are
        // tolerated and the queue state is unchanged.
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        queue.enqueue(
            h1.clone(),
            L1PortalEvents::from_deposits(vec![make_deposit(100)]),
            vec![],
        );

        let h2 = make_chained_header(11, h1_hash);
        queue.enqueue(
            h2.clone(),
            L1PortalEvents::from_deposits(vec![make_deposit(200)]),
            vec![],
        );

        let hash_before = queue.enqueued_head_hash();
        let len_before = queue.pending_len();

        // Re-enqueue both — should be Duplicate, no state change
        assert!(matches!(
            queue.try_enqueue(
                seal(h1),
                L1PortalEvents::from_deposits(vec![make_deposit(100)]),
                vec![],
            ),
            EnqueueOutcome::Duplicate
        ));
        assert!(matches!(
            queue.try_enqueue(
                seal(h2),
                L1PortalEvents::from_deposits(vec![make_deposit(200)]),
                vec![],
            ),
            EnqueueOutcome::Duplicate
        ));
        assert_eq!(queue.enqueued_head_hash(), hash_before);
        assert_eq!(queue.pending_len(), len_before);
    }

    #[test]
    fn test_reorg_within_pending_recomputes_hash() {
        // Reorg at a middle block in pending should purge from that point,
        // accept the new block, and recompute the hash chain consistently.
        let mut queue = PendingDeposits::default();

        let h10 = make_test_header(10);
        let h10_hash = header_hash(&h10);
        queue.enqueue(
            h10,
            L1PortalEvents::from_deposits(vec![make_deposit(100)]),
            vec![],
        );
        let hash_after_10 = queue.enqueued_head_hash();

        let h11 = make_chained_header(11, h10_hash);
        let h11_hash = header_hash(&h11);
        queue.enqueue(
            h11,
            L1PortalEvents::from_deposits(vec![make_deposit(200)]),
            vec![],
        );

        let h12 = make_chained_header(12, h11_hash);
        let h12_hash = header_hash(&h12);
        queue.enqueue(
            h12,
            L1PortalEvents::from_deposits(vec![make_deposit(300)]),
            vec![],
        );

        let h13 = make_chained_header(13, h12_hash);
        queue.enqueue(
            h13,
            L1PortalEvents::from_deposits(vec![make_deposit(400)]),
            vec![],
        );

        assert_eq!(queue.pending_len(), 4);

        // Reorg at block 11 — new header with same parent but different content
        let mut h11_reorg = make_chained_header(11, h10_hash);
        h11_reorg.inner.gas_limit = 42;
        let h11_reorg_hash = header_hash(&h11_reorg);

        assert!(matches!(
            queue.try_enqueue(
                seal(h11_reorg),
                L1PortalEvents::from_deposits(vec![make_deposit(999)]),
                vec![],
            ),
            EnqueueOutcome::Accepted
        ));

        // Blocks 12, 13 purged; now have 10 + new 11
        assert_eq!(queue.pending_len(), 2);
        assert_eq!(queue.pending_block(0).unwrap().header.number(), 10);
        assert_eq!(queue.pending_block(1).unwrap().header.number(), 11);
        assert_eq!(queue.last_enqueued().unwrap().number, 11);

        // Hash should differ from original because deposit content changed
        assert_ne!(queue.enqueued_head_hash(), hash_after_10);

        // Can continue building on the new fork
        let h12_new = make_chained_header(12, h11_reorg_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h12_new), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));
        assert_eq!(queue.pending_len(), 3);
    }

    #[test]
    fn test_drained_reorg_same_height_returns_duplicate() {
        // If the queue is drained and we receive the same block number with
        // the same hash (not a reorg), it must be Duplicate.
        let mut queue = PendingDeposits::default();

        let h10 = make_test_header(10);
        let h10_hash = header_hash(&h10);
        queue.enqueue(h10, L1PortalEvents::from_deposits(vec![]), vec![]);

        let h11 = make_chained_header(11, h10_hash);
        queue.enqueue(h11.clone(), L1PortalEvents::from_deposits(vec![]), vec![]);

        // Drain
        confirm(&mut queue);
        confirm(&mut queue);

        // Re-deliver block 11 with same hash — Duplicate, last_enqueued intact
        assert!(matches!(
            queue.try_enqueue(seal(h11), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Duplicate
        ));
        assert_eq!(queue.last_enqueued().unwrap().number, 11);
    }

    // --- last_processed floor tests ---

    #[test]
    fn test_pop_sets_last_processed() {
        let mut queue = PendingDeposits::default();

        assert!(queue.last_processed().is_none());

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        queue.enqueue(h1, L1PortalEvents::from_deposits(vec![]), vec![]);

        let h2 = make_chained_header(11, h1_hash);
        queue.enqueue(h2, L1PortalEvents::from_deposits(vec![]), vec![]);

        let popped = confirm(&mut queue);
        assert_eq!(queue.last_processed().unwrap().number, 10);
        assert_eq!(queue.last_processed().unwrap().hash, popped.header.hash());

        confirm(&mut queue);
        assert_eq!(queue.last_processed().unwrap().number, 11);
    }

    #[test]
    fn test_drain_sets_last_processed() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        queue.enqueue(h1, L1PortalEvents::from_deposits(vec![]), vec![]);

        let h2 = make_chained_header(11, h1_hash);
        queue.enqueue(h2, L1PortalEvents::from_deposits(vec![]), vec![]);

        let drained = queue.drain();
        assert_eq!(queue.last_processed().unwrap().number, 11);
        assert_eq!(
            queue.last_processed().unwrap().hash,
            drained.last().unwrap().header.hash()
        );
    }

    #[test]
    fn test_reorg_of_consumed_block_skips_stale_during_backfill() {
        // Simulates the exact production bug:
        // 1. Blocks 10, 11 enqueued and consumed (popped)
        // 2. L1 reorgs at block 11 — new block 12 arrives with wrong parent
        // 3. try_enqueue clears last_enqueued, returns NeedBackfill{11, 11}
        // 4. Backfill re-enqueues NEW block 11 — must be Duplicate (already consumed)
        // 5. Backfill enqueues NEW block 12 — must be Accepted
        let mut queue = PendingDeposits::default();

        let h10 = make_test_header(10);
        let h10_hash = header_hash(&h10);
        queue.enqueue(h10, L1PortalEvents::from_deposits(vec![]), vec![]);

        let h11 = make_chained_header(11, h10_hash);
        let _h11_hash = header_hash(&h11);
        queue.enqueue(h11, L1PortalEvents::from_deposits(vec![]), vec![]);

        // Engine consumes both blocks
        confirm(&mut queue); // block 10
        confirm(&mut queue); // block 11
        assert!(queue.pending_len() == 0);
        assert_eq!(queue.last_processed().unwrap().number, 11);
        assert_eq!(queue.last_enqueued().unwrap().number, 11);

        // New block 12 arrives with parent pointing to NEW (reorged) block 11
        let h12_new = make_chained_header(12, B256::with_last_byte(0xDE));
        match queue.try_enqueue(seal(h12_new), L1PortalEvents::from_deposits(vec![]), vec![]) {
            EnqueueOutcome::NeedBackfill { from, to } => {
                assert_eq!(from, 11);
                assert_eq!(to, 11);
            }
            other => panic!("expected NeedBackfill, got {other:?}"),
        }
        // last_enqueued cleared by the parent mismatch path
        assert!(queue.last_enqueued().is_none());

        // Backfill: try to re-enqueue NEW block 11 — must be Duplicate
        // because last_processed knows block 11 was already consumed
        let mut h11_reorg = make_test_header(11);
        h11_reorg.inner.gas_limit = 999;
        let h11_reorg_hash = header_hash(&h11_reorg);
        assert!(matches!(
            queue.try_enqueue(
                seal(h11_reorg),
                L1PortalEvents::from_deposits(vec![]),
                vec![]
            ),
            EnqueueOutcome::Duplicate
        ));

        // Backfill: enqueue NEW block 12 — must be Accepted
        // (immediate successor of consumed block, parent hash mismatch is
        // expected and will be caught by the builder)
        let h12 = make_chained_header(12, h11_reorg_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h12), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Queue now has exactly one block: NEW block 12
        assert_eq!(queue.pending_len(), 1);
        assert_eq!(queue.pending_block(0).unwrap().header.number(), 12);
    }

    #[test]
    fn test_consumed_reorg_gap_uses_last_processed_floor() {
        // After consuming blocks and clearing last_enqueued, a block arriving
        // with a gap should use last_processed as the floor.
        let mut queue = PendingDeposits::default();

        let h10 = make_test_header(10);
        let _h10_hash = header_hash(&h10);
        queue.enqueue(h10, L1PortalEvents::from_deposits(vec![]), vec![]);
        confirm(&mut queue);

        // Simulate last_enqueued being cleared (reorg path)
        queue.clear_last_enqueued();

        // Block 13 arrives — gap from 11..12
        let h13 = make_test_header(13);
        match queue.try_enqueue(seal(h13), L1PortalEvents::from_deposits(vec![]), vec![]) {
            EnqueueOutcome::NeedBackfill { from, to } => {
                assert_eq!(from, 11); // last_processed.number + 1
                assert_eq!(to, 12);
            }
            other => panic!("expected NeedBackfill, got {other:?}"),
        }
    }

    #[test]
    fn test_builder_skips_stale_blocks_in_queue() {
        // Simulates the builder's perspective: queue contains a stale block
        // (number < expected) followed by the correct block. The builder
        // should skip the stale one and use the correct one.
        let queue = DepositQueue::new();

        // Enqueue block 10 (stale — zone already processed it)
        let h10 = make_test_header(10);
        let h10_hash = header_hash(&h10);
        queue.enqueue(h10, L1PortalEvents::from_deposits(vec![]), vec![]);

        // Enqueue block 11 (the one the builder actually needs)
        let h11 = make_chained_header(11, h10_hash);
        queue.enqueue(h11, L1PortalEvents::from_deposits(vec![]), vec![]);

        // Builder expects block 11 (tempoBlockNumber=10, expected=11).
        // Peek/confirm loop should skip block 10 and use the correct one.
        let expected = 11u64;
        let l1_block = loop {
            let block = queue.peek().expect("queue should not be empty");
            if block.header.number() < expected {
                confirm_shared(&queue);
                continue;
            }
            break block;
        };
        assert_eq!(l1_block.header.number(), 11);
    }

    #[test]
    fn test_reorg_consumed_then_continue_on_new_chain() {
        // Full end-to-end scenario: reorg of consumed block, backfill skips it,
        // zone gets the correct next block. Subsequent blocks also work.
        let mut queue = PendingDeposits::default();

        // Build a 3-block chain: 100, 101, 102
        let h100 = make_test_header(100);
        let h100_hash = header_hash(&h100);
        queue.enqueue(h100, L1PortalEvents::from_deposits(vec![]), vec![]);

        let h101 = make_chained_header(101, h100_hash);
        let h101_hash = header_hash(&h101);
        queue.enqueue(h101, L1PortalEvents::from_deposits(vec![]), vec![]);

        let h102 = make_chained_header(102, h101_hash);
        queue.enqueue(h102, L1PortalEvents::from_deposits(vec![]), vec![]);

        // Engine consumes all 3
        confirm(&mut queue);
        confirm(&mut queue);
        confirm(&mut queue);
        assert_eq!(queue.last_processed().unwrap().number, 102);

        // L1 reorgs at 102: new block 103 has different parent
        let new_parent = B256::with_last_byte(0xAA);
        let h103 = make_chained_header(103, new_parent);
        match queue.try_enqueue(seal(h103), L1PortalEvents::from_deposits(vec![]), vec![]) {
            EnqueueOutcome::NeedBackfill { from, to } => {
                assert_eq!(from, 102);
                assert_eq!(to, 102);
            }
            other => panic!("expected NeedBackfill, got {other:?}"),
        }

        // Backfill: re-enqueue NEW block 102 → Duplicate (consumed)
        let mut h102_new = make_test_header(102);
        h102_new.inner.gas_limit = 42;
        let h102_new_hash = header_hash(&h102_new);
        assert!(matches!(
            queue.try_enqueue(
                seal(h102_new),
                L1PortalEvents::from_deposits(vec![]),
                vec![]
            ),
            EnqueueOutcome::Duplicate
        ));

        // Backfill: enqueue NEW block 103 → Accepted
        let h103 = make_chained_header(103, h102_new_hash);
        let h103_hash = header_hash(&h103);
        assert!(matches!(
            queue.try_enqueue(seal(h103), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Block 104 continues on the new chain
        let h104 = make_chained_header(104, h103_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h104), L1PortalEvents::from_deposits(vec![]), vec![]),
            EnqueueOutcome::Accepted
        ));

        assert_eq!(queue.pending_len(), 2);
        assert_eq!(queue.pending_block(0).unwrap().header.number(), 103);
        assert_eq!(queue.pending_block(1).unwrap().header.number(), 104);
    }
}
