//! L1 chain subscription and deposit extraction.
//!
//! Subscribes to L1 block headers via WebSocket and extracts deposit events
//! from the ZonePortal contract for each block.

use alloy_consensus::BlockHeader as _;
use alloy_eips::NumHash;
use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_provider::{Provider, ProviderBuilder, WsConnect};
use alloy_rpc_types_eth::{Filter, Log};
use alloy_sol_types::{SolEvent, SolValue};
use alloy_transport::Authorization;
use futures::StreamExt;
use parking_lot::Mutex;
use reth_primitives_traits::SealedHeader;
use std::{collections::BTreeMap, sync::Arc};
use tempo_alloy::TempoNetwork;
use tempo_primitives::TempoHeader;
use tracing::{error, info, warn};

use crate::{
    abi::{
        self, EncryptedDeposit as AbiEncryptedDeposit,
        EncryptedDepositPayload as AbiEncryptedDepositPayload,
    },
    bindings::ZonePortal::{self, DepositMade, EncryptedDepositMade},
};

/// Configuration for the L1 subscriber.
#[derive(Debug, Clone)]
pub struct L1SubscriberConfig {
    /// WebSocket URL of the L1 node.
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
}

/// Maximum number of blocks to request logs for in a single `eth_getLogs` call
/// during backfill.
const BACKFILL_BATCH_SIZE: u64 = 1000;

/// L1 chain subscriber that listens for new blocks and extracts deposit events.
#[derive(Clone)]
pub struct L1Subscriber {
    config: L1SubscriberConfig,
    deposit_queue: DepositQueue,
}

impl L1Subscriber {
    /// Create and spawn the L1 subscriber as a critical background task.
    ///
    /// The subscriber runs in a retry loop — if the WebSocket connection drops
    /// or [`Self::run`] returns an error, it reconnects after a 5-second delay.
    pub fn spawn(
        config: L1SubscriberConfig,
        deposit_queue: DepositQueue,
        task_executor: impl reth_ethereum::tasks::TaskSpawner,
    ) {
        let subscriber = Self {
            config,
            deposit_queue,
        };

        task_executor.spawn_critical(
            "l1-deposit-subscriber",
            Box::pin(async move {
                loop {
                    if let Err(e) = subscriber.clone().run().await {
                        error!(error = %e, "L1 subscriber failed, reconnecting in 5s");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            }),
        );
    }

    /// Connect to the L1 node via WebSocket.
    async fn connect(&self) -> eyre::Result<impl Provider<TempoNetwork>> {
        info!(url = %self.config.l1_rpc_url, "Connecting to L1 node");
        let url: url::Url = self.config.l1_rpc_url.parse()?;
        let mut ws = WsConnect::new(self.config.l1_rpc_url.clone());
        if !url.username().is_empty() {
            let auth = Authorization::basic(url.username(), url.password().unwrap_or_default());
            ws = ws.with_auth(auth);
        }
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_ws(ws)
            .await?;
        info!("Connected to L1 node");
        Ok(provider)
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
    async fn sync_to_l1_tip(&self, l1_provider: &impl Provider<TempoNetwork>) -> eyre::Result<()> {
        let Some(mut from) = self.resolve_start_block(l1_provider).await? else {
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
        if from > tip {
            info!(from, tip, "Already synced to L1 tip");
            return Ok(());
        }

        info!(
            from,
            tip,
            blocks = tip - from + 1,
            "Backfilling deposit events"
        );
        self.backfill(l1_provider, from, tip).await
    }

    /// Backfill L1 blocks from `from..=to` in batches.
    ///
    /// Fetches deposit logs in bulk per batch, then fetches each block header
    /// individually. Every block is enqueued (even without deposits) to maintain
    /// chain continuity.
    async fn backfill(
        &self,
        l1_provider: &impl Provider<TempoNetwork>,
        from: u64,
        to: u64,
    ) -> eyre::Result<()> {
        let filter = self.deposit_log_filter();
        let mut cursor = from;
        while cursor <= to {
            let end = (cursor + BACKFILL_BATCH_SIZE - 1).min(to);

            // Fetch deposit logs for this batch and index by block number.
            let logs = l1_provider
                .get_logs(&filter.clone().select(cursor..=end))
                .await?;

            let mut deposits_by_block: BTreeMap<u64, Vec<L1Deposit>> = BTreeMap::new();
            for log in logs {
                let n = log
                    .block_number
                    .ok_or_else(|| eyre::eyre!("log missing block number"))?;
                deposits_by_block
                    .entry(n)
                    .or_default()
                    .push(self.parse_deposit(log, n)?);
            }

            for block_number in cursor..=end {
                let deposits = deposits_by_block.remove(&block_number).unwrap_or_default();
                if !deposits.is_empty() {
                    info!(
                        block = block_number,
                        count = deposits.len(),
                        "💰 Backfill deposits"
                    );
                }
                let header_resp = l1_provider
                    .get_header_by_number(block_number.into())
                    .await?
                    .ok_or_else(|| eyre::eyre!("L1 header not found for block {block_number}"))?;
                let header: TempoHeader = header_resp.inner.inner;
                self.deposit_queue.enqueue(header, deposits);
            }

            cursor = end + 1;
        }

        info!(from, to, blocks = to - from + 1, "Backfill complete");
        Ok(())
    }

    /// Run the L1 subscriber until the stream ends or an error occurs.
    ///
    /// Connects via WebSocket, backfills deposit events to the current L1 tip,
    /// then subscribes to new block headers. Each block — with or without
    /// deposits — is enqueued so the zone engine sees a strict sequential
    /// chain.
    ///
    /// Callers should retry on error (see [`Self::spawn`]).
    pub async fn run(self) -> eyre::Result<()> {
        let provider = self.connect().await?;

        // Backfill to the current tip before subscribing.
        self.sync_to_l1_tip(&provider).await?;

        let sub = provider.subscribe_blocks().await?;
        let mut stream = sub.into_stream();
        info!(portal = %self.config.portal_address, "Subscribed to L1 blocks");

        while let Some(header) = stream.next().await {
            let block_number = header.number();
            let sealed = SealedHeader::seal_slow(header.inner.into_consensus());

            let deposits = self.fetch_deposits(&provider, block_number).await?;

            match self.deposit_queue.try_enqueue(sealed, deposits) {
                EnqueueOutcome::Accepted | EnqueueOutcome::Duplicate => {}
                EnqueueOutcome::NeedBackfill { from, to } => {
                    // Gap detected — backfill fills the missing range and also
                    // enqueues the current block (included in from..=block_number).
                    warn!(from, to, block_number, "Gap in L1 blocks, backfilling");
                    self.backfill(&provider, from, block_number).await?;
                }
            }
        }

        warn!("L1 block subscription stream ended");
        Ok(())
    }

    /// Fetch and parse deposit logs for a single L1 block.
    async fn fetch_deposits(
        &self,
        provider: &impl Provider<TempoNetwork>,
        block_number: u64,
    ) -> eyre::Result<Vec<L1Deposit>> {
        let logs = provider
            .get_logs(&self.deposit_log_filter().select(block_number))
            .await?;

        let deposits: Vec<L1Deposit> = logs
            .into_iter()
            .map(|log| self.parse_deposit(log, block_number))
            .collect::<eyre::Result<_>>()?;

        for d in &deposits {
            match d {
                L1Deposit::Regular(d) => {
                    info!(
                        l1_block = block_number,
                        token = %d.token,
                        sender = %d.sender,
                        to = %d.to,
                        amount = %d.amount,
                        "💰 Deposit from L1"
                    );
                }
                L1Deposit::Encrypted(d) => {
                    info!(
                        l1_block = block_number,
                        token = %d.token,
                        sender = %d.sender,
                        amount = %d.amount,
                        "🔒 Encrypted deposit from L1"
                    );
                }
            }
        }

        Ok(deposits)
    }

    /// Returns a log filter for deposit events emitted by the ZonePortal.
    fn deposit_log_filter(&self) -> Filter {
        Filter::new()
            .address(self.config.portal_address)
            .event_signature(vec![
                DepositMade::SIGNATURE_HASH,
                EncryptedDepositMade::SIGNATURE_HASH,
            ])
    }

    /// Parse a single deposit log into an [`L1Deposit`].
    ///
    /// Supports both [`DepositMade`] and [`EncryptedDepositMade`] events.
    fn parse_deposit(&self, log: Log, block_number: u64) -> eyre::Result<L1Deposit> {
        if log.topic0() == Some(&DepositMade::SIGNATURE_HASH) {
            let event = DepositMade::decode_log(&log.inner)?;
            Ok(L1Deposit::Regular(Deposit::from_event(
                event.data,
                block_number,
            )))
        } else if log.topic0() == Some(&EncryptedDepositMade::SIGNATURE_HASH) {
            let event = EncryptedDepositMade::decode_log(&log.inner)?;
            Ok(L1Deposit::Encrypted(EncryptedDeposit::from_event(
                event.data,
                block_number,
            )))
        } else {
            Err(eyre::eyre!("unknown deposit event topic"))
        }
    }
}

/// A deposit extracted from L1.
#[derive(Debug, Clone)]
pub struct Deposit {
    /// L1 block number where the deposit was included.
    pub l1_block_number: u64,
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
    /// New deposit queue hash after this deposit.
    pub queue_hash: B256,
}

impl Deposit {
    /// Create a new deposit from an event and block number.
    pub fn from_event(event: DepositMade, l1_block_number: u64) -> Self {
        Self {
            l1_block_number,
            token: event.token,
            sender: event.sender,
            to: event.to,
            amount: event.netAmount,
            fee: event.fee,
            memo: event.memo,
            queue_hash: event.newCurrentDepositQueueHash,
        }
    }
}

/// An encrypted deposit extracted from L1.
#[derive(Debug, Clone)]
pub struct EncryptedDeposit {
    /// L1 block number where the deposit was included.
    pub l1_block_number: u64,
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
    /// New deposit queue hash after this deposit.
    pub queue_hash: B256,
}

impl EncryptedDeposit {
    /// Create a new encrypted deposit from an event and block number.
    pub fn from_event(event: EncryptedDepositMade, l1_block_number: u64) -> Self {
        Self {
            l1_block_number,
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
            queue_hash: event.newCurrentDepositQueueHash,
        }
    }
}

/// A deposit from L1 — either regular (plaintext) or encrypted.
#[derive(Debug, Clone)]
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

/// Result of attempting to enqueue an L1 block into the deposit queue.
#[derive(Debug)]
pub enum EnqueueOutcome {
    /// Block was appended to the queue.
    Accepted,
    /// Block is a duplicate (same number and hash already present, or behind our window).
    Duplicate,
    /// Block doesn't connect — subscriber must fetch and enqueue `from..=to` first,
    /// then retry this block.
    NeedBackfill { from: u64, to: u64 },
}

/// An L1 block's header paired with the deposits found in that block.
#[derive(Debug, Clone)]
pub struct L1BlockDeposits {
    /// The sealed L1 block header (caches the block hash).
    pub header: SealedHeader<TempoHeader>,
    /// Deposits extracted from this block.
    pub deposits: Vec<L1Deposit>,
    /// Deposit queue hash chain value before this block's deposits.
    pub queue_hash_before: B256,
    /// Deposit queue hash chain value after this block's deposits.
    pub queue_hash_after: B256,
}

/// Deposit queue hash chain state.
///
/// Tracks deposits grouped by L1 block and maintains the hash chain:
/// `newHash = keccak256(abi.encode(deposit, prevHash))`
///
/// This mirrors the L1 portal's `currentDepositQueueHash`.
#[derive(Debug)]
pub struct PendingDeposits {
    /// Deposit hash chain value after the last block consumed by the builder
    /// via `pop_next` or `drain`. Serves as the rollback anchor when purging
    /// blocks during a reorg.
    processed_head_hash: B256,
    /// Deposit hash chain value after applying all pending deposits. Advances
    /// as new deposits are enqueued and rolls back to `processed_head_hash`
    /// when blocks are purged.
    pub enqueued_head_hash: B256,
    /// Pending L1 blocks with their deposits, not yet processed by the zone
    pub pending: Vec<L1BlockDeposits>,
    /// Highest L1 block ever enqueued (number + hash). Survives `pop_next` /
    /// `drain` so that reconnecting subscribers know where the queue left off,
    /// even if the engine has already consumed the blocks.
    pub last_enqueued: Option<NumHash>,
}

impl Default for PendingDeposits {
    fn default() -> Self {
        Self {
            processed_head_hash: B256::ZERO,
            enqueued_head_hash: B256::ZERO,
            pending: Vec::new(),
            last_enqueued: None,
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
    pub fn try_enqueue(
        &mut self,
        header: SealedHeader<TempoHeader>,
        deposits: Vec<L1Deposit>,
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
            }
            self.append(header, deposits);
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

        self.append(header, deposits);
        EnqueueOutcome::Accepted
    }

    /// Enqueue a block during backfill. Accepts or skips duplicates.
    ///
    /// Panics on `NeedBackfill` — backfill blocks must be fetched sequentially.
    pub fn enqueue(&mut self, header: TempoHeader, deposits: Vec<L1Deposit>) {
        match self.try_enqueue(SealedHeader::seal_slow(header), deposits) {
            EnqueueOutcome::Accepted | EnqueueOutcome::Duplicate => {}
            other => panic!("enqueue expected Accepted or Duplicate, got {other:?}"),
        }
    }

    fn append(&mut self, header: SealedHeader<TempoHeader>, deposits: Vec<L1Deposit>) {
        let queue_hash_before = self.enqueued_head_hash;
        for deposit in &deposits {
            self.enqueued_head_hash = deposit.hash_chain(self.enqueued_head_hash);
        }
        let queue_hash_after = self.enqueued_head_hash;
        self.last_enqueued = Some(header.num_hash());
        self.pending.push(L1BlockDeposits {
            header,
            deposits,
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

    /// Take the next pending L1 block (oldest first).
    ///
    /// Returns `None` if no L1 blocks are queued. The zone builder calls this once per zone
    /// block to advance Tempo state by exactly one L1 block at a time.
    pub fn pop_next(&mut self) -> Option<L1BlockDeposits> {
        if self.pending.is_empty() {
            None
        } else {
            let block = self.pending.remove(0);
            self.processed_head_hash = block.queue_hash_after;
            Some(block)
        }
    }

    /// Drain all pending L1 block deposits.
    pub fn drain(&mut self) -> Vec<L1BlockDeposits> {
        if let Some(last) = self.pending.last() {
            self.processed_head_hash = last.queue_hash_after;
        }
        std::mem::take(&mut self.pending)
    }

    /// Compute a [`DepositQueueTransition`] for a batch of deposits starting from `prev_hash`
    pub fn transition(prev_hash: B256, deposits: &[L1Deposit]) -> DepositQueueTransition {
        let mut current = prev_hash;
        for d in deposits {
            current = d.hash_chain(current);
        }
        DepositQueueTransition {
            prev_processed_hash: prev_hash,
            next_processed_hash: current,
        }
    }
}

/// Deposit queue transition for batch proof validation.
///
/// Represents the state of the deposit hash chain for a batch
/// of deposits processed by the zone. Used to prove which deposits were
/// included in a block.
#[derive(Debug, Clone, Default)]
pub struct DepositQueueTransition {
    /// Hash chain head before the batch is processed
    pub prev_processed_hash: B256,
    /// Hash chain head after the batch is processed
    pub next_processed_hash: B256,
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
    pub fn try_enqueue(
        &self,
        header: SealedHeader<TempoHeader>,
        deposits: Vec<L1Deposit>,
    ) -> EnqueueOutcome {
        let mut queue = self.inner.lock();
        let outcome = queue.try_enqueue(header, deposits);
        if matches!(outcome, EnqueueOutcome::Accepted) {
            drop(queue);
            self.notify.notify_one();
        }
        outcome
    }

    /// Enqueue an L1 block with its deposits and notify waiters.
    pub fn enqueue(&self, header: TempoHeader, deposits: Vec<L1Deposit>) {
        self.inner.lock().enqueue(header, deposits);
        self.notify.notify_one();
    }

    /// Pop the next L1 block from the queue.
    pub fn pop_next(&self) -> Option<L1BlockDeposits> {
        self.inner.lock().pop_next()
    }

    /// Returns the number of pending L1 blocks.
    pub fn pending_count(&self) -> usize {
        self.inner.lock().pending.len()
    }

    /// Wait until an L1 block is available.
    pub async fn notified(&self) {
        self.notify.notified().await
    }

    /// Get a reference to the notify for external use.
    pub fn notify_ref(&self) -> &Arc<tokio::sync::Notify> {
        &self.notify
    }

    /// Returns the most recently enqueued L1 block (number + hash), if any.
    ///
    /// This is a high-water mark that survives `pop_next` / `drain`, so it
    /// reflects the last block ever enqueued — not just what's still pending.
    pub fn last_enqueued(&self) -> Option<NumHash> {
        self.inner.lock().last_enqueued
    }

    /// Lock the inner queue directly.
    pub fn lock(&self) -> parking_lot::MutexGuard<'_, PendingDeposits> {
        self.inner.lock()
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
    use crate::abi::DepositType;
    use alloy_consensus::Header;
    use alloy_primitives::{FixedBytes, address};

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

    #[test]
    fn test_deposit_queue_hash_chain() {
        let mut queue = PendingDeposits::default();
        assert_eq!(queue.enqueued_head_hash, B256::ZERO);

        let d1 = L1Deposit::Regular(Deposit {
            l1_block_number: 1,
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 1000,
            fee: 0,
            memo: B256::ZERO,
            queue_hash: B256::ZERO,
        });

        queue.enqueue(make_test_header(1), vec![d1.clone()]);
        let hash_after_d1 = queue.enqueued_head_hash;
        assert_ne!(hash_after_d1, B256::ZERO);

        // Verify hash is deterministic
        let mut queue2 = PendingDeposits::default();
        queue2.enqueue(make_test_header(1), vec![d1]);
        assert_eq!(hash_after_d1, queue2.enqueued_head_hash);

        let d2 = L1Deposit::Regular(Deposit {
            l1_block_number: 2,
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000003"),
            to: address!("0x0000000000000000000000000000000000000004"),
            amount: 2000,
            fee: 0,
            memo: B256::ZERO,
            queue_hash: B256::ZERO,
        });

        queue.enqueue(make_test_header(2), vec![d2]);
        let hash_after_d2 = queue.enqueued_head_hash;
        assert_ne!(hash_after_d2, hash_after_d1);
    }

    #[test]
    fn test_process_deposits_transition() {
        let deposits = vec![
            L1Deposit::Regular(Deposit {
                l1_block_number: 1,
                token: address!("0x0000000000000000000000000000000000001000"),
                sender: address!("0x0000000000000000000000000000000000000001"),
                to: address!("0x0000000000000000000000000000000000000002"),
                amount: 1000,
                fee: 0,
                memo: B256::ZERO,
                queue_hash: B256::ZERO,
            }),
            L1Deposit::Regular(Deposit {
                l1_block_number: 2,
                token: address!("0x0000000000000000000000000000000000001000"),
                sender: address!("0x0000000000000000000000000000000000000003"),
                to: address!("0x0000000000000000000000000000000000000004"),
                amount: 2000,
                fee: 0,
                memo: B256::ZERO,
                queue_hash: B256::ZERO,
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
            l1_block_number: 1,
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 500,
            fee: 0,
            memo: FixedBytes::from([0xABu8; 32]),
            queue_hash: B256::ZERO,
        })];

        queue.enqueue(make_test_header(1), deposits.clone());

        let transition = PendingDeposits::transition(B256::ZERO, &deposits);

        assert_eq!(queue.enqueued_head_hash, transition.next_processed_hash);
    }

    #[test]
    fn test_drain_returns_block_grouped_deposits() {
        let mut queue = PendingDeposits::default();

        let d1 = L1Deposit::Regular(Deposit {
            l1_block_number: 10,
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 100,
            fee: 0,
            memo: B256::ZERO,
            queue_hash: B256::ZERO,
        });

        let d2 = L1Deposit::Regular(Deposit {
            l1_block_number: 11,
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000003"),
            to: address!("0x0000000000000000000000000000000000000004"),
            amount: 200,
            fee: 0,
            memo: B256::ZERO,
            queue_hash: B256::ZERO,
        });

        let h10 = make_test_header(10);
        let h10_hash = header_hash(&h10);
        queue.enqueue(h10, vec![d1]);
        queue.enqueue(make_chained_header(11, h10_hash), vec![d2]);

        let blocks = queue.drain();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].header.number(), 10);
        assert_eq!(blocks[0].deposits.len(), 1);
        assert_eq!(blocks[1].header.number(), 11);
        assert_eq!(blocks[1].deposits.len(), 1);

        // After drain, pending is empty
        assert!(queue.drain().is_empty());
    }

    #[test]
    fn test_encrypted_deposit_hash_chain() {
        let token = address!("0x0000000000000000000000000000000000001000");
        let sender = address!("0x0000000000000000000000000000000000001234");

        let encrypted = EncryptedDeposit {
            l1_block_number: 1,
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
            queue_hash: B256::ZERO,
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
            l1_block_number: 1,
            token,
            sender,
            to: recipient,
            amount: 500_000,
            fee: 0,
            memo: B256::ZERO,
            queue_hash: B256::ZERO,
        };

        let encrypted = EncryptedDeposit {
            l1_block_number: 1,
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
            queue_hash: B256::ZERO,
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
            l1_block_number: 1,
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
            queue_hash: B256::ZERO,
        };

        let deposits = vec![L1Deposit::Encrypted(encrypted)];

        // Path 1: enqueue into PendingDeposits
        let mut pending = PendingDeposits::default();
        let header = make_test_header(1);
        pending.enqueue(header, deposits.clone());

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
        queue.enqueue(h100, vec![]);
        let h101 = make_chained_header(101, h100_hash);
        let h101_hash = header_hash(&h101);
        queue.enqueue(h101, vec![]);
        let h102 = make_chained_header(102, h101_hash);
        queue.enqueue(h102, vec![]);

        let last = queue.last_enqueued().unwrap();
        assert_eq!(last.number, 102);

        // Pop all blocks — last_enqueued must still report 102
        assert!(queue.pop_next().is_some());
        assert!(queue.pop_next().is_some());
        assert!(queue.pop_next().is_some());
        assert!(queue.pop_next().is_none());

        let last = queue.last_enqueued().unwrap();
        assert_eq!(last.number, 102, "last_enqueued must survive pop_next");

        // Enqueue more (continuing from 102), then drain — last_enqueued must still track
        let h102_hash = last.hash;
        let h103 = make_chained_header(103, h102_hash);
        let h103_hash = header_hash(&h103);
        queue.enqueue(h103, vec![]);
        queue.enqueue(make_chained_header(104, h103_hash), vec![]);
        assert_eq!(queue.last_enqueued().unwrap().number, 104);

        let drained = queue.lock().drain();
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
            queue.try_enqueue(seal(h1), vec![]),
            EnqueueOutcome::Accepted
        ));

        let h2 = make_chained_header(11, h1_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h2), vec![]),
            EnqueueOutcome::Accepted
        ));

        assert_eq!(queue.pending.len(), 2);
    }

    #[test]
    fn test_try_enqueue_gap_returns_need_backfill() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        assert!(matches!(
            queue.try_enqueue(seal(h1), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Skip block 11, try to enqueue 12
        let h3 = make_test_header(12);
        match queue.try_enqueue(seal(h3), vec![]) {
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
            queue.try_enqueue(seal(h1.clone()), vec![]),
            EnqueueOutcome::Accepted
        ));
        assert!(matches!(
            queue.try_enqueue(seal(h1), vec![]),
            EnqueueOutcome::Duplicate
        ));
    }

    #[test]
    fn test_try_enqueue_reorg_purges_stale() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        assert!(matches!(
            queue.try_enqueue(seal(h1), vec![]),
            EnqueueOutcome::Accepted
        ));

        let h2 = make_chained_header(11, h1_hash);
        let h2_hash = header_hash(&h2);
        assert!(matches!(
            queue.try_enqueue(seal(h2), vec![]),
            EnqueueOutcome::Accepted
        ));

        let h3 = make_chained_header(12, h2_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h3), vec![]),
            EnqueueOutcome::Accepted
        ));

        assert_eq!(queue.pending.len(), 3);

        // Reorg at height 11 — use a different header (different gas_limit makes the hash different)
        let mut h2_reorg = make_chained_header(11, h1_hash);
        h2_reorg.inner.gas_limit = 999;
        assert!(matches!(
            queue.try_enqueue(seal(h2_reorg), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Blocks 12 and the old 11 should be purged, replaced by new 11
        assert_eq!(queue.pending.len(), 2);
        assert_eq!(queue.pending[0].header.number(), 10);
        assert_eq!(queue.pending[1].header.number(), 11);
    }

    #[test]
    fn test_try_enqueue_parent_mismatch_at_tip() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        assert!(matches!(
            queue.try_enqueue(seal(h1), vec![]),
            EnqueueOutcome::Accepted
        ));

        let h2 = make_chained_header(11, h1_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h2), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Block 12 with wrong parent hash — purges block 11, needs backfill
        let h3 = make_chained_header(12, B256::with_last_byte(0xDE));
        match queue.try_enqueue(seal(h3), vec![]) {
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
            l1_block_number: 10,
            token,
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 100,
            fee: 0,
            memo: B256::ZERO,
            queue_hash: B256::ZERO,
        });
        assert!(matches!(
            queue.try_enqueue(seal(h1), vec![d1]),
            EnqueueOutcome::Accepted
        ));
        let hash_after_h1 = queue.enqueued_head_hash;

        let h2 = make_chained_header(11, h1_hash);
        let d2 = L1Deposit::Regular(Deposit {
            l1_block_number: 11,
            token,
            sender: address!("0x0000000000000000000000000000000000000003"),
            to: address!("0x0000000000000000000000000000000000000004"),
            amount: 200,
            fee: 0,
            memo: B256::ZERO,
            queue_hash: B256::ZERO,
        });
        assert!(matches!(
            queue.try_enqueue(seal(h2), vec![d2]),
            EnqueueOutcome::Accepted
        ));

        // Hash advanced past h1
        assert_ne!(queue.enqueued_head_hash, hash_after_h1);

        // Now reorg at height 11 — different header
        let mut h2_reorg = make_chained_header(11, h1_hash);
        h2_reorg.inner.gas_limit = 999;
        assert!(matches!(
            queue.try_enqueue(seal(h2_reorg), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Hash should have rolled back to after h1 (since h2_reorg has no deposits)
        assert_eq!(queue.enqueued_head_hash, hash_after_h1);
    }

    fn make_deposit(block: u64, amount: u128) -> L1Deposit {
        L1Deposit::Regular(Deposit {
            l1_block_number: block,
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount,
            fee: 0,
            memo: B256::ZERO,
            queue_hash: B256::ZERO,
        })
    }

    #[test]
    fn test_pop_advances_processed_head_hash() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(1);
        let h1_hash = header_hash(&h1);
        queue.enqueue(h1, vec![make_deposit(1, 100)]);

        let h2 = make_chained_header(2, h1_hash);
        let h2_hash = header_hash(&h2);
        queue.enqueue(h2, vec![make_deposit(2, 200)]);

        let h3 = make_chained_header(3, h2_hash);
        queue.enqueue(h3, vec![make_deposit(3, 300)]);

        let hash_after_all = queue.enqueued_head_hash;

        // Pop block 1
        let popped = queue.pop_next().unwrap();
        assert_eq!(popped.header.number(), 1);
        assert_eq!(queue.processed_head_hash, popped.queue_hash_after);

        // queue.enqueued_head_hash hasn't changed
        assert_eq!(queue.enqueued_head_hash, hash_after_all);

        // Recompute expected hash from processed_head_hash + remaining deposits (blocks 2, 3)
        let remaining_deposits: Vec<L1Deposit> = queue
            .pending
            .iter()
            .flat_map(|b| b.deposits.clone())
            .collect();
        let transition =
            PendingDeposits::transition(queue.processed_head_hash, &remaining_deposits);
        assert_eq!(transition.next_processed_hash, queue.enqueued_head_hash);
    }

    #[test]
    fn test_purge_after_pops() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(1);
        let h1_hash = header_hash(&h1);
        queue.enqueue(h1, vec![make_deposit(1, 100)]);

        let h2 = make_chained_header(2, h1_hash);
        let h2_hash = header_hash(&h2);
        queue.enqueue(h2, vec![]);

        let h3 = make_chained_header(3, h2_hash);
        let h3_hash = header_hash(&h3);
        queue.enqueue(h3, vec![]);

        let h4 = make_chained_header(4, h3_hash);
        let h4_hash = header_hash(&h4);
        queue.enqueue(h4, vec![]);

        let h5 = make_chained_header(5, h4_hash);
        queue.enqueue(h5, vec![]);

        // Pop blocks 1 and 2
        queue.pop_next().unwrap();
        queue.pop_next().unwrap();
        assert_eq!(queue.pending.len(), 3); // blocks 3, 4, 5

        let hash_after_block3 = queue.pending[0].queue_hash_after;

        // Trigger purge at block 4: different header at height 4
        let mut h4_reorg = make_chained_header(4, h3_hash);
        h4_reorg.inner.gas_limit = 999;
        assert!(matches!(
            queue.try_enqueue(seal(h4_reorg), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Pending should have blocks 3 and new-4
        assert_eq!(queue.pending.len(), 2);
        assert_eq!(queue.pending[0].header.number(), 3);
        assert_eq!(queue.pending[1].header.number(), 4);

        // New block 4 has no deposits, so hash == hash after block 3's deposits
        assert_eq!(queue.enqueued_head_hash, hash_after_block3);
    }

    #[test]
    fn test_purge_first_pending_after_pop() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(1);
        let h1_hash = header_hash(&h1);
        queue.enqueue(h1, vec![make_deposit(1, 100)]);

        let h2 = make_chained_header(2, h1_hash);
        let h2_hash = header_hash(&h2);
        queue.enqueue(h2, vec![make_deposit(2, 200)]);

        let h3 = make_chained_header(3, h2_hash);
        queue.enqueue(h3, vec![make_deposit(3, 300)]);

        // Pop block 1 — processed_head_hash advances
        let popped = queue.pop_next().unwrap();
        let base_after_pop = popped.queue_hash_after;
        assert_eq!(queue.processed_head_hash, base_after_pop);
        assert_eq!(queue.pending.len(), 2); // blocks 2, 3

        // Purge from block 2 by enqueueing a different block 2
        let mut h2_reorg = make_chained_header(2, B256::with_last_byte(0xFF));
        h2_reorg.inner.gas_limit = 777;
        // This block has a different hash at height 2, so purge_from(0) fires.
        // Queue becomes empty, then the new block is accepted as anchor.
        let outcome = queue.try_enqueue(seal(h2_reorg), vec![]);
        assert!(matches!(outcome, EnqueueOutcome::Accepted));

        // After purge and re-anchor, pending has just the new block 2
        assert_eq!(queue.pending.len(), 1);
        assert_eq!(queue.pending[0].header.number(), 2);

        // processed_head_hash should still be what it was after popping block 1
        assert_eq!(queue.processed_head_hash, base_after_pop);
    }

    #[test]
    fn test_backfill_then_duplicate_redelivery() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(1);
        let h1_hash = header_hash(&h1);
        queue.enqueue(h1, vec![make_deposit(1, 100)]);

        // Try to enqueue block 3 (skipping 2) => NeedBackfill
        let h2 = make_chained_header(2, h1_hash);
        let h2_hash = header_hash(&h2);
        let h3 = make_chained_header(3, h2_hash);
        let h3_sealed = seal(h3);
        match queue.try_enqueue(seal(make_test_header(3)), vec![]) {
            EnqueueOutcome::NeedBackfill { from, to } => {
                assert_eq!(from, 2);
                assert_eq!(to, 2);
            }
            other => panic!("expected NeedBackfill, got {other:?}"),
        }

        // Backfill: enqueue block 2, then block 3
        queue.enqueue(h2, vec![make_deposit(2, 200)]);
        assert!(matches!(
            queue.try_enqueue(h3_sealed.clone(), vec![make_deposit(3, 300)]),
            EnqueueOutcome::Accepted
        ));

        let hash_before = queue.enqueued_head_hash;
        let len_before = queue.pending.len();

        // Re-deliver block 3 (same sealed header) => Duplicate
        assert!(matches!(
            queue.try_enqueue(h3_sealed, vec![make_deposit(3, 300)]),
            EnqueueOutcome::Duplicate
        ));

        assert_eq!(queue.enqueued_head_hash, hash_before);
        assert_eq!(queue.pending.len(), len_before);
    }

    #[test]
    fn test_zero_deposit_block_hash_invariant() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(1);
        let h1_hash = header_hash(&h1);
        queue.enqueue(h1, vec![make_deposit(1, 100)]);

        let h2 = make_chained_header(2, h1_hash);
        let h2_hash = header_hash(&h2);
        queue.enqueue(h2, vec![]); // no deposits

        let h3 = make_chained_header(3, h2_hash);
        let d3 = make_deposit(3, 300);
        queue.enqueue(h3, vec![d3.clone()]);

        // Block 2 has no deposits => queue_hash_before == queue_hash_after
        assert_eq!(
            queue.pending[1].queue_hash_before, queue.pending[1].queue_hash_after,
            "zero-deposit block must not change queue hash"
        );

        let hash_after_all_original = queue.enqueued_head_hash;

        // Purge at block 2 (different header) — purges blocks 2, 3
        let mut h2_reorg = make_chained_header(2, h1_hash);
        h2_reorg.inner.gas_limit = 888;
        let h2_reorg_hash = header_hash(&h2_reorg);
        assert!(matches!(
            queue.try_enqueue(seal(h2_reorg), vec![]),
            EnqueueOutcome::Accepted
        ));

        // After purge, only block 1 and new block 2 remain
        assert_eq!(queue.pending.len(), 2);
        let hash_after_block1 = queue.pending[0].queue_hash_after;
        // New block 2 has no deposits so hash == hash after block 1
        assert_eq!(queue.enqueued_head_hash, hash_after_block1);

        // Re-enqueue new block 3 with same deposits as original
        let h3_new = make_chained_header(3, h2_reorg_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h3_new), vec![d3]),
            EnqueueOutcome::Accepted
        ));

        // The hash should match original because the deposit content and
        // chain of hashes are identical (both block 2 variants had no deposits)
        assert_eq!(
            queue.enqueued_head_hash, hash_after_all_original,
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
            queue.try_enqueue(seal(h1), vec![]),
            EnqueueOutcome::Accepted
        ));

        let h2 = make_chained_header(11, h1_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h2), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Drain everything
        queue.pop_next().unwrap();
        queue.pop_next().unwrap();
        assert!(queue.pending.is_empty());
        assert_eq!(queue.last_enqueued.unwrap().number, 11);

        // Block 12 arrives with wrong parent — consumed block 11 was reorged
        let h3_bad = make_chained_header(12, B256::with_last_byte(0xDE));
        match queue.try_enqueue(seal(h3_bad), vec![]) {
            EnqueueOutcome::NeedBackfill { from, to } => {
                // Must re-fetch from block 11 (the consumed block that was reorged)
                assert_eq!(from, 11);
                assert_eq!(to, 11);
            }
            other => panic!("expected NeedBackfill, got {other:?}"),
        }

        // last_enqueued must be cleared so backfill can re-enqueue block 11
        assert!(
            queue.last_enqueued.is_none(),
            "last_enqueued must be cleared after parent mismatch on drained queue"
        );

        // Backfill can now enqueue the reorged block 11 as a fresh anchor
        let h2_reorg = make_test_header(11);
        let h2_reorg_hash = header_hash(&h2_reorg);
        queue.enqueue(h2_reorg, vec![]);
        assert_eq!(queue.last_enqueued.unwrap().number, 11);

        // And block 12 can follow
        let h3 = make_chained_header(12, h2_reorg_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h3), vec![]),
            EnqueueOutcome::Accepted
        ));
    }

    #[test]
    fn test_disconnected_after_partial_drain() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        let h1_hash = header_hash(&h1);
        assert!(matches!(
            queue.try_enqueue(seal(h1), vec![make_deposit(10, 100)]),
            EnqueueOutcome::Accepted
        ));

        let h2 = make_chained_header(11, h1_hash);
        let h2_hash = header_hash(&h2);
        assert!(matches!(
            queue.try_enqueue(seal(h2), vec![make_deposit(11, 200)]),
            EnqueueOutcome::Accepted
        ));

        let h3 = make_chained_header(12, h2_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h3), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Pop only block 10
        queue.pop_next().unwrap();
        assert_eq!(queue.pending.len(), 2); // blocks 11, 12

        // Block 13 with wrong parent — this is a normal parent mismatch on the
        // non-empty queue path, should purge block 12 and request backfill
        let h4_bad = make_chained_header(13, B256::with_last_byte(0xAB));
        match queue.try_enqueue(seal(h4_bad), vec![]) {
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
            queue.try_enqueue(seal(h1), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Drain
        queue.pop_next().unwrap();
        assert!(queue.pending.is_empty());

        // Wrong parent → NeedBackfill
        let h2_bad = make_chained_header(11, B256::with_last_byte(0xFF));
        assert!(matches!(
            queue.try_enqueue(seal(h2_bad), vec![]),
            EnqueueOutcome::NeedBackfill { .. }
        ));

        // Correct parent → Accepted
        let h2_good = make_chained_header(11, h1_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h2_good), vec![make_deposit(11, 500)]),
            EnqueueOutcome::Accepted
        ));
        assert_eq!(queue.pending.len(), 1);
        assert_eq!(queue.pending[0].header.number(), 11);
    }

    #[test]
    fn test_disconnected_with_multi_block_gap() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        assert!(matches!(
            queue.try_enqueue(seal(h1), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Drain
        queue.pop_next().unwrap();

        // Block 14 arrives — gap of 11..13 plus wrong parent is moot because
        // the gap check triggers first
        let h5 = make_test_header(14);
        match queue.try_enqueue(seal(h5), vec![]) {
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
            queue.try_enqueue(seal(h1), vec![]),
            EnqueueOutcome::Accepted
        ));

        let h2 = make_chained_header(11, h1_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h2), vec![]),
            EnqueueOutcome::Accepted
        ));

        // Drain everything
        queue.pop_next().unwrap();
        queue.pop_next().unwrap();
        assert!(queue.pending.is_empty());

        // Re-deliver block 10 or 11 — should be Duplicate
        assert!(matches!(
            queue.try_enqueue(seal(make_test_header(10)), vec![]),
            EnqueueOutcome::Duplicate
        ));
        assert!(matches!(
            queue.try_enqueue(seal(make_chained_header(11, h1_hash)), vec![]),
            EnqueueOutcome::Duplicate
        ));
    }

    #[test]
    fn test_disconnected_preserves_processed_head_hash_and_deposits() {
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        assert!(matches!(
            queue.try_enqueue(seal(h1), vec![make_deposit(10, 100)]),
            EnqueueOutcome::Accepted
        ));

        // Pop and record processed_head_hash
        let popped = queue.pop_next().unwrap();
        let base = queue.processed_head_hash;
        assert_eq!(base, popped.queue_hash_after);

        // Disconnected block should not alter processed_head_hash
        let h2_bad = make_chained_header(11, B256::with_last_byte(0xBB));
        assert!(matches!(
            queue.try_enqueue(seal(h2_bad), vec![]),
            EnqueueOutcome::NeedBackfill { .. }
        ));
        assert_eq!(
            queue.processed_head_hash, base,
            "processed_head_hash must not change on NeedBackfill"
        );
        assert_eq!(
            queue.enqueued_head_hash, base,
            "enqueued_head_hash must not change on NeedBackfill"
        );
        assert!(queue.pending.is_empty());
    }

    #[test]
    fn test_reconnect_duplicate_does_not_clear_last_enqueued() {
        // A reconnect may re-deliver the same block we already consumed.
        // This must return Duplicate without clearing last_enqueued.
        let mut queue = PendingDeposits::default();

        let h1 = make_test_header(10);
        queue.enqueue(h1.clone(), vec![]);

        // Drain
        queue.pop_next().unwrap();
        assert!(queue.pending.is_empty());
        assert_eq!(queue.last_enqueued.unwrap().number, 10);

        // Re-deliver same block 10 — must be Duplicate, last_enqueued preserved
        assert!(matches!(
            queue.try_enqueue(seal(h1), vec![]),
            EnqueueOutcome::Duplicate
        ));
        assert_eq!(
            queue.last_enqueued.unwrap().number,
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
        queue.enqueue(h1.clone(), vec![make_deposit(10, 100)]);

        let h2 = make_chained_header(11, h1_hash);
        queue.enqueue(h2.clone(), vec![make_deposit(11, 200)]);

        let hash_before = queue.enqueued_head_hash;
        let len_before = queue.pending.len();

        // Re-enqueue both — should be Duplicate, no state change
        assert!(matches!(
            queue.try_enqueue(seal(h1), vec![make_deposit(10, 100)]),
            EnqueueOutcome::Duplicate
        ));
        assert!(matches!(
            queue.try_enqueue(seal(h2), vec![make_deposit(11, 200)]),
            EnqueueOutcome::Duplicate
        ));
        assert_eq!(queue.enqueued_head_hash, hash_before);
        assert_eq!(queue.pending.len(), len_before);
    }

    #[test]
    fn test_reorg_within_pending_recomputes_hash() {
        // Reorg at a middle block in pending should purge from that point,
        // accept the new block, and recompute the hash chain consistently.
        let mut queue = PendingDeposits::default();

        let h10 = make_test_header(10);
        let h10_hash = header_hash(&h10);
        queue.enqueue(h10, vec![make_deposit(10, 100)]);
        let hash_after_10 = queue.enqueued_head_hash;

        let h11 = make_chained_header(11, h10_hash);
        let h11_hash = header_hash(&h11);
        queue.enqueue(h11, vec![make_deposit(11, 200)]);

        let h12 = make_chained_header(12, h11_hash);
        let h12_hash = header_hash(&h12);
        queue.enqueue(h12, vec![make_deposit(12, 300)]);

        let h13 = make_chained_header(13, h12_hash);
        queue.enqueue(h13, vec![make_deposit(13, 400)]);

        assert_eq!(queue.pending.len(), 4);

        // Reorg at block 11 — new header with same parent but different content
        let mut h11_reorg = make_chained_header(11, h10_hash);
        h11_reorg.inner.gas_limit = 42;
        let h11_reorg_hash = header_hash(&h11_reorg);

        assert!(matches!(
            queue.try_enqueue(seal(h11_reorg), vec![make_deposit(11, 999)]),
            EnqueueOutcome::Accepted
        ));

        // Blocks 12, 13 purged; now have 10 + new 11
        assert_eq!(queue.pending.len(), 2);
        assert_eq!(queue.pending[0].header.number(), 10);
        assert_eq!(queue.pending[1].header.number(), 11);
        assert_eq!(queue.last_enqueued.unwrap().number, 11);

        // Hash should differ from original because deposit content changed
        assert_ne!(queue.enqueued_head_hash, hash_after_10);

        // Can continue building on the new fork
        let h12_new = make_chained_header(12, h11_reorg_hash);
        assert!(matches!(
            queue.try_enqueue(seal(h12_new), vec![]),
            EnqueueOutcome::Accepted
        ));
        assert_eq!(queue.pending.len(), 3);
    }

    #[test]
    fn test_drained_reorg_same_height_returns_duplicate() {
        // If the queue is drained and we receive the same block number with
        // the same hash (not a reorg), it must be Duplicate.
        let mut queue = PendingDeposits::default();

        let h10 = make_test_header(10);
        let h10_hash = header_hash(&h10);
        queue.enqueue(h10.clone(), vec![]);

        let h11 = make_chained_header(11, h10_hash);
        queue.enqueue(h11.clone(), vec![]);

        // Drain
        queue.pop_next().unwrap();
        queue.pop_next().unwrap();

        // Re-deliver block 11 with same hash — Duplicate, last_enqueued intact
        assert!(matches!(
            queue.try_enqueue(seal(h11), vec![]),
            EnqueueOutcome::Duplicate
        ));
        assert_eq!(queue.last_enqueued.unwrap().number, 11);
    }
}
