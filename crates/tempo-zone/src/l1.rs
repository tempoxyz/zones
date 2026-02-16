//! L1 chain subscription and deposit extraction.
//!
//! Subscribes to L1 block headers via WebSocket and extracts deposit events
//! from the ZonePortal contract for each block.

use alloy_consensus::BlockHeader as _;
use alloy_primitives::{Address, B256, keccak256};
use alloy_provider::{Provider, ProviderBuilder, WsConnect};
use alloy_rpc_types_eth::{Filter, Log};
use alloy_sol_types::{SolEvent, SolValue};
use alloy_transport::Authorization;
use futures::StreamExt;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tempo_alloy::TempoNetwork;
use tempo_primitives::TempoHeader;
use tracing::{debug, error, info, warn};

use crate::{
    abi,
    bindings::ZonePortal::{self, DepositMade},
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
                    if let Err(e) = subscriber.clone().start().await {
                        error!(error = %e, "L1 subscriber failed, reconnecting in 5s");
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    }
                }
            }),
        );
    }

    /// Sync to the current L1 tip.
    ///
    /// Reads `lastSyncedTempoBlockNumber` from the ZonePortal to determine where
    /// the zone left off. If the portal hasn't synced yet, falls back to
    /// `genesisTempoBlockNumber` so we scan from the portal's creation.

    async fn sync_to_l1_tip(
        &self,
        l1_provider: &impl Provider<TempoNetwork>,
        filter: &Filter,
    ) -> eyre::Result<()> {
        // FIXME: same here this is should be cleaned up
        let tip = l1_provider.get_block_number().await?;
        let portal = ZonePortal::new(self.config.portal_address, l1_provider);
        let last_synced = portal.lastSyncedTempoBlockNumber().call().await?;

        let from = if last_synced > 0 {
            last_synced + 1
        } else {
            let genesis = if let Some(local) = self.config.genesis_tempo_block_number {
                info!(genesis = local, "Using CLI genesis block number override");
                local
            } else {
                let on_chain = portal.genesisTempoBlockNumber().call().await?;
                if on_chain == 0 {
                    warn!(
                        "Portal genesisTempoBlockNumber is 0 — skipping backfill. \
                         Set --l1.genesis-block-number to backfill from the correct block."
                    );
                    return Ok(());
                }
                info!(genesis = on_chain, "Using portal's genesisTempoBlockNumber");
                on_chain
            };
            info!(
                genesis,
                "Fresh portal, backfilling from genesis+1 (genesis block already in TempoState)"
            );
            genesis + 1
        };

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
        self.backfill(l1_provider, filter, from, tip).await
    }

    /// Backfill ALL L1 blocks from `from..=to` in batches, attaching any
    /// deposit events to the corresponding block. Every block in the range is
    /// enqueued so that `finalizeTempo` sees a strict sequential chain.
    // FIXME: same here this can be updated to be cleaner
    async fn backfill(
        &self,
        l1_provider: &impl Provider<TempoNetwork>,
        filter: &Filter,
        from: u64,
        to: u64,
    ) -> eyre::Result<()> {
        let mut cursor = from;
        while cursor <= to {
            let end = (cursor + BACKFILL_BATCH_SIZE - 1).min(to);

            // 1. Fetch deposit logs for this batch and index by block number.
            let logs = l1_provider
                .get_logs(&filter.clone().select(cursor..=end))
                .await?;

            let mut deposits_by_block: BTreeMap<u64, Vec<Deposit>> = BTreeMap::new();
            for log in logs {
                let n = log
                    .block_number
                    .ok_or_else(|| eyre::eyre!("log missing block number"))?;
                deposits_by_block
                    .entry(n)
                    .or_default()
                    .push(self.parse_deposit(log, n)?);
            }

            // 2. Fetch headers for ALL blocks in the range.
            let mut blocks = Vec::with_capacity((end - cursor + 1) as usize);
            for block_number in cursor..=end {
                let deposits = deposits_by_block.remove(&block_number).unwrap_or_default();
                if !deposits.is_empty() {
                    debug!(
                        block = block_number,
                        count = deposits.len(),
                        "Backfill deposits"
                    );
                }
                let header_resp = l1_provider
                    .get_header_by_number(block_number.into())
                    .await?
                    .ok_or_else(|| eyre::eyre!("L1 header not found for block {block_number}"))?;
                blocks.push((header_resp.inner.inner, deposits));
            }

            // 3. Enqueue every block (with or without deposits).
            for (header, deposits) in blocks {
                self.deposit_queue.enqueue(header, deposits);
            }

            cursor = end + 1;
        }

        info!(to, "Backfill complete");
        Ok(())
    }

    /// Derive an HTTP URL from the configured WSS URL for RPC calls that don't
    /// need a subscription (backfilling, one-off reads).
    fn http_url(&self) -> String {
        self.config
            .l1_rpc_url
            .replacen("wss://", "https://", 1)
            .replacen("ws://", "http://", 1)
    }

    /// Start the L1 subscriber.
    ///
    /// Syncs to the current L1 tip on startup using HTTP, then subscribes to
    /// new blocks via WebSocket.
    pub async fn start(self) -> eyre::Result<()> {
        info!(url = %self.config.l1_rpc_url, "Connecting to L1 node");

        let url: url::Url = self.config.l1_rpc_url.parse()?;

        // Connect WS first so subscription captures blocks during backfill.
        let mut ws = WsConnect::new(self.config.l1_rpc_url.clone());

        if !url.username().is_empty() {
            let auth = Authorization::basic(url.username(), url.password().unwrap_or_default());
            ws = ws.with_auth(auth);
        }

        let ws_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_ws(ws)
            .await?;

        info!("Connected to L1 node");

        // Subscribe before backfilling so we don't miss blocks between
        // sync_to_l1_tip finishing and the stream starting.
        let sub = ws_provider.subscribe_blocks().await?;
        let mut stream = sub.into_stream();

        info!(portal = %self.config.portal_address, "Subscribed to L1 blocks");

        // Use HTTP for backfilling — more reliable than hammering a WebSocket
        // with hundreds of sequential requests.
        let http_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_http(self.http_url().parse()?)
            .erased();

        let filter = Filter::new()
            .address(self.config.portal_address)
            .event_signature(DepositMade::SIGNATURE_HASH);

        self.sync_to_l1_tip(&http_provider, &filter).await?;

        // Track the last enqueued block so we can detect and backfill gaps
        // when the WebSocket subscription skips blocks.
        let mut last_enqueued: u64 = self
            .deposit_queue
            .last_enqueued_block()
            .unwrap_or(http_provider.get_block_number().await?);

        while let Some(header) = stream.next().await {
            let block_number = header.number();

            // Detect gaps: if the subscription skipped blocks, backfill them.
            // Each skipped block must be enqueued (even without deposits) to
            // maintain chain continuity for advanceTempo.
            if block_number > last_enqueued + 1 {
                let gap_from = last_enqueued + 1;
                let gap_to = block_number - 1;
                warn!(
                    gap_from,
                    gap_to,
                    skipped = gap_to - gap_from + 1,
                    "Gap detected in L1 subscription, backfilling"
                );
                self.backfill(&http_provider, &filter, gap_from, gap_to)
                    .await?;
            }

            // Fetch deposit logs for this block via HTTP
            let logs = http_provider
                .get_logs(&filter.clone().select(block_number))
                .await?;

            // Parse deposit events from the logs
            let deposits: Vec<Deposit> = logs
                .into_iter()
                .map(|log| self.parse_deposit(log, block_number))
                .collect::<eyre::Result<_>>()?;

            for d in &deposits {
                info!(
                    l1_block = block_number,
                    sender = %d.sender,
                    to = %d.to,
                    amount = %d.amount,
                    "💰 Deposit from L1"
                );
            }

            // Enqueue every block — even those without deposits — so the zone
            // sees a strict sequential chain for advanceTempo.
            self.deposit_queue
                .enqueue(header.as_ref().clone(), deposits);

            last_enqueued = block_number;
        }

        warn!("L1 block subscription stream ended");
        Ok(())
    }

    /// Parse a single log into a [`Deposit`].
    fn parse_deposit(&self, log: Log, block_number: u64) -> eyre::Result<Deposit> {
        let event = DepositMade::decode_log(&log.inner)?;
        Ok(Deposit::from_event(event.data, block_number))
    }
}

/// A deposit extracted from L1.
#[derive(Debug, Clone)]
pub struct Deposit {
    /// L1 block number where the deposit was included.
    pub l1_block_number: u64,
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
            sender: event.sender,
            to: event.to,
            amount: event.netAmount,
            fee: event.fee,
            memo: event.memo,
            queue_hash: event.newCurrentDepositQueueHash,
        }
    }
}

/// An L1 block's header paired with the deposits found in that block.
#[derive(Debug, Clone)]
pub struct L1BlockDeposits {
    /// The L1 block header.
    pub header: TempoHeader,
    /// Deposits extracted from this block.
    pub deposits: Vec<Deposit>,
}

/// Deposit queue hash chain state.
///
/// Tracks deposits grouped by L1 block and maintains the hash chain:
/// `newHash = keccak256(abi.encode(deposit, prevHash))`
///
/// This mirrors the L1 portal's `currentDepositQueueHash`.
#[derive(Debug, Default)]
pub struct PendingDeposits {
    /// Head of deposit queue hash chain
    pub hash: B256,
    /// Pending L1 blocks with their deposits, not yet processed by the zone
    pub pending: Vec<L1BlockDeposits>,
}

impl PendingDeposits {
    /// Append deposits from an L1 block to the queue and update the hash chain.
    pub fn enqueue(&mut self, header: TempoHeader, deposits: Vec<Deposit>) {
        for deposit in &deposits {
            self.hash = keccak256(
                (
                    abi::DepositType::Regular,
                    abi::Deposit {
                        sender: deposit.sender,
                        to: deposit.to,
                        amount: deposit.amount,
                        memo: deposit.memo,
                    },
                    self.hash,
                )
                    .abi_encode(),
            );
        }
        self.pending.push(L1BlockDeposits { header, deposits });
    }

    /// Take the next pending L1 block (oldest first).
    ///
    /// Returns `None` if no L1 blocks are queued. The zone builder calls this once per zone
    /// block to advance Tempo state by exactly one L1 block at a time.
    pub fn pop_next(&mut self) -> Option<L1BlockDeposits> {
        if self.pending.is_empty() {
            None
        } else {
            Some(self.pending.remove(0))
        }
    }

    /// Drain all pending L1 block deposits.
    pub fn drain(&mut self) -> Vec<L1BlockDeposits> {
        std::mem::take(&mut self.pending)
    }

    /// Compute a [`DepositQueueTransition`] for a batch of deposits starting from `prev_hash`
    pub fn transition(prev_hash: B256, deposits: &[Deposit]) -> DepositQueueTransition {
        let mut current = prev_hash;
        for d in deposits {
            current = keccak256(
                (
                    abi::DepositType::Regular,
                    abi::Deposit {
                        sender: d.sender,
                        to: d.to,
                        amount: d.amount,
                        memo: d.memo,
                    },
                    current,
                )
                    .abi_encode(),
            );
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
    /// Hash chain head before the is processed
    pub prev_processed_hash: B256,
    /// Hash chain head after the is processed
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

    /// Enqueue an L1 block with its deposits and notify waiters.
    pub fn enqueue(&self, header: TempoHeader, deposits: Vec<Deposit>) {
        let mut queue = self.inner.lock().expect("deposit queue poisoned");
        queue.enqueue(header, deposits);
        drop(queue); // release lock before notifying
        self.notify.notify_one();
    }

    /// Pop the next L1 block from the queue.
    pub fn pop_next(&self) -> Option<L1BlockDeposits> {
        self.inner
            .lock()
            .expect("deposit queue poisoned")
            .pop_next()
    }

    /// Returns the number of pending L1 blocks.
    pub fn pending_count(&self) -> usize {
        self.inner
            .lock()
            .expect("deposit queue poisoned")
            .pending
            .len()
    }

    /// Wait until an L1 block is available.
    pub async fn notified(&self) {
        self.notify.notified().await
    }

    /// Get a reference to the notify for external use.
    pub fn notify_ref(&self) -> &Arc<tokio::sync::Notify> {
        &self.notify
    }

    /// Returns the block number of the most recently enqueued L1 block, if any.
    pub fn last_enqueued_block(&self) -> Option<u64> {
        self.inner
            .lock()
            .expect("deposit queue poisoned")
            .pending
            .last()
            .map(|b| b.header.inner.number)
    }

    /// Lock the inner queue directly (for backward compat where needed).
    pub fn lock(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, PendingDeposits>> {
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

    #[test]
    fn test_deposit_queue_hash_chain() {
        let mut queue = PendingDeposits::default();
        assert_eq!(queue.hash, B256::ZERO);

        let d1 = Deposit {
            l1_block_number: 1,
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 1000,
            fee: 0,
            memo: B256::ZERO,
            queue_hash: B256::ZERO,
        };

        queue.enqueue(make_test_header(1), vec![d1.clone()]);
        let hash_after_d1 = queue.hash;
        assert_ne!(hash_after_d1, B256::ZERO);

        // Verify hash is deterministic
        let mut queue2 = PendingDeposits::default();
        queue2.enqueue(make_test_header(1), vec![d1]);
        assert_eq!(hash_after_d1, queue2.hash);

        let d2 = Deposit {
            l1_block_number: 2,
            sender: address!("0x0000000000000000000000000000000000000003"),
            to: address!("0x0000000000000000000000000000000000000004"),
            amount: 2000,
            fee: 0,
            memo: B256::ZERO,
            queue_hash: B256::ZERO,
        };

        queue.enqueue(make_test_header(2), vec![d2]);
        let hash_after_d2 = queue.hash;
        assert_ne!(hash_after_d2, hash_after_d1);
    }

    #[test]
    fn test_process_deposits_transition() {
        let deposits = vec![
            Deposit {
                l1_block_number: 1,
                sender: address!("0x0000000000000000000000000000000000000001"),
                to: address!("0x0000000000000000000000000000000000000002"),
                amount: 1000,
                fee: 0,
                memo: B256::ZERO,
                queue_hash: B256::ZERO,
            },
            Deposit {
                l1_block_number: 2,
                sender: address!("0x0000000000000000000000000000000000000003"),
                to: address!("0x0000000000000000000000000000000000000004"),
                amount: 2000,
                fee: 0,
                memo: B256::ZERO,
                queue_hash: B256::ZERO,
            },
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

        let deposits = vec![Deposit {
            l1_block_number: 1,
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 500,
            fee: 0,
            memo: FixedBytes::from([0xABu8; 32]),
            queue_hash: B256::ZERO,
        }];

        queue.enqueue(make_test_header(1), deposits.clone());

        let transition = PendingDeposits::transition(B256::ZERO, &deposits);

        assert_eq!(queue.hash, transition.next_processed_hash);
    }

    #[test]
    fn test_drain_returns_block_grouped_deposits() {
        let mut queue = PendingDeposits::default();

        let d1 = Deposit {
            l1_block_number: 10,
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 100,
            fee: 0,
            memo: B256::ZERO,
            queue_hash: B256::ZERO,
        };

        let d2 = Deposit {
            l1_block_number: 11,
            sender: address!("0x0000000000000000000000000000000000000003"),
            to: address!("0x0000000000000000000000000000000000000004"),
            amount: 200,
            fee: 0,
            memo: B256::ZERO,
            queue_hash: B256::ZERO,
        };

        queue.enqueue(make_test_header(10), vec![d1]);
        queue.enqueue(make_test_header(11), vec![d2]);

        let blocks = queue.drain();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].header.inner.number, 10);
        assert_eq!(blocks[0].deposits.len(), 1);
        assert_eq!(blocks[1].header.inner.number, 11);
        assert_eq!(blocks[1].deposits.len(), 1);

        // After drain, pending is empty
        assert!(queue.drain().is_empty());
    }
}
