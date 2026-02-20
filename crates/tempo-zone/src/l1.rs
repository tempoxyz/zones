//! L1 chain subscription and deposit extraction.
//!
//! Subscribes to L1 block headers via WebSocket and extracts deposit events
//! from the ZonePortal contract for each block.

use alloy_consensus::BlockHeader as _;
use alloy_eips::NumHash;
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
        let tip = l1_provider.get_block_number().await?;

        // When genesis_tempo_block_number is set via CLI, always use it as the
        // starting point. The portal's `lastSyncedTempoBlockNumber` tracks batch
        // submissions and may be ahead of where the zone chain actually is
        // (e.g. after a restart where batches were submitted but the zone has
        // no local blocks yet). The zone engine needs ALL L1 blocks from genesis
        // to maintain chain continuity.
        let from = if let Some(genesis) = self.config.genesis_tempo_block_number {
            info!(genesis, "Using CLI genesis block number override");
            genesis + 1
        } else {
            if self.config.portal_address.is_zero() {
                warn!(
                    "No portal address and no genesis block number override — skipping backfill. \
                     Set --l1.genesis-block-number or provide a portal address."
                );
                return Ok(());
            }
            let portal = ZonePortal::new(self.config.portal_address, l1_provider);
            let last_synced = portal.lastSyncedTempoBlockNumber().call().await?;
            if last_synced > 0 {
                last_synced + 1
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
                on_chain + 1
            }
        };

        // If we already have blocks in the deposit queue (e.g. from a previous
        // `start()` that failed mid-stream), skip past them to avoid re-enqueueing
        // duplicates that would break chain continuity in the payload builder.
        let from = if let Some(last) = self.deposit_queue.last_enqueued() {
            let adjusted = last.number + 1;
            if adjusted > from {
                info!(
                    portal_from = from,
                    queue_last = last.number,
                    adjusted_from = adjusted,
                    "Skipping blocks already in deposit queue"
                );
            }
            from.max(adjusted)
        } else {
            from
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
                blocks.push((header_resp.inner.inner, deposits));
            }

            // 3. Enqueue every block (with or without deposits).
            for (header, deposits) in blocks {
                self.deposit_queue.enqueue(header, deposits);
            }

            cursor = end + 1;
        }

        info!(from, to, blocks = to - from + 1, "Backfill complete");
        Ok(())
    }

    /// Start the L1 subscriber.
    ///
    /// Connects via WebSocket, backfills to the current tip, then subscribes
    /// to new blocks.
    pub async fn start(self) -> eyre::Result<()> {
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

        let filter = Filter::new()
            .address(self.config.portal_address)
            .event_signature(DepositMade::SIGNATURE_HASH);

        // Backfill to the current tip before subscribing. Subscribing first
        // would buffer block notifications during the (potentially long)
        // backfill, risking backpressure deadlocks on the WS transport.
        self.sync_to_l1_tip(&provider, &filter).await?;

        // Subscribe after backfill, then catch up any blocks produced during
        // the backfill window.
        let sub = provider.subscribe_blocks().await?;
        let mut stream = sub.into_stream();
        let tip_after_sub = provider.get_block_number().await?;

        let last_backfilled = self
            .deposit_queue
            .last_enqueued()
            .map(|nh| nh.number)
            .unwrap_or(tip_after_sub);

        if tip_after_sub > last_backfilled {
            info!(
                from = last_backfilled + 1,
                to = tip_after_sub,
                "Catching up blocks produced during backfill"
            );
            self.backfill(&provider, &filter, last_backfilled + 1, tip_after_sub)
                .await?;
        }

        info!(portal = %self.config.portal_address, "Subscribed to L1 blocks");

        let mut last_enqueued: u64 = self
            .deposit_queue
            .last_enqueued()
            .map(|nh| nh.number)
            .unwrap_or(tip_after_sub);

        while let Some(header) = stream.next().await {
            let block_number = header.number();

            // Skip blocks already enqueued during backfill. The WS subscription
            // starts before backfill so it may buffer blocks that were already
            // processed. Enqueuing them again would break chain continuity.
            if block_number <= last_enqueued {
                debug!(
                    block_number,
                    last_enqueued, "Skipping already-enqueued L1 block"
                );
                continue;
            }

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
                self.backfill(&provider, &filter, gap_from, gap_to).await?;
            }

            // Fetch deposit logs for this block.
            let logs = provider
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
                    token = %d.token,
                    sender = %d.sender,
                    to = %d.to,
                    amount = %d.amount,
                    fee = %d.fee,
                    "💰 Deposit detected on L1"
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
    /// Highest L1 block ever enqueued (number + hash). Survives `pop_next` /
    /// `drain` so that reconnecting subscribers know where the queue left off,
    /// even if the engine has already consumed the blocks.
    pub last_enqueued: Option<NumHash>,
}

impl PendingDeposits {
    /// Append deposits from an L1 block to the queue and update the hash chain.
    pub fn enqueue(&mut self, header: TempoHeader, deposits: Vec<Deposit>) {
        let block_number = header.inner.number;
        for deposit in &deposits {
            self.hash = keccak256(
                (
                    abi::DepositType::Regular,
                    abi::Deposit {
                        token: deposit.token,
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
        let block_hash = keccak256(alloy_rlp::encode(&header));
        self.pending.push(L1BlockDeposits { header, deposits });
        self.last_enqueued = Some(NumHash {
            number: block_number,
            hash: block_hash,
        });
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
                        token: d.token,
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

    /// Returns the most recently enqueued L1 block (number + hash), if any.
    ///
    /// This is a high-water mark that survives `pop_next` / `drain`, so it
    /// reflects the last block ever enqueued — not just what's still pending.
    pub fn last_enqueued(&self) -> Option<NumHash> {
        self.inner
            .lock()
            .expect("deposit queue poisoned")
            .last_enqueued
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
            token: address!("0x0000000000000000000000000000000000001000"),
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
            token: address!("0x0000000000000000000000000000000000001000"),
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
                token: address!("0x0000000000000000000000000000000000001000"),
                sender: address!("0x0000000000000000000000000000000000000001"),
                to: address!("0x0000000000000000000000000000000000000002"),
                amount: 1000,
                fee: 0,
                memo: B256::ZERO,
                queue_hash: B256::ZERO,
            },
            Deposit {
                l1_block_number: 2,
                token: address!("0x0000000000000000000000000000000000001000"),
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
            token: address!("0x0000000000000000000000000000000000001000"),
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
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 100,
            fee: 0,
            memo: B256::ZERO,
            queue_hash: B256::ZERO,
        };

        let d2 = Deposit {
            l1_block_number: 11,
            token: address!("0x0000000000000000000000000000000000001000"),
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

    #[test]
    fn test_last_enqueued_survives_pop_and_drain() {
        let queue = DepositQueue::new();

        // Initially empty
        assert!(queue.last_enqueued().is_none());

        queue.enqueue(make_test_header(100), vec![]);
        queue.enqueue(make_test_header(101), vec![]);
        queue.enqueue(make_test_header(102), vec![]);

        let last = queue.last_enqueued().unwrap();
        assert_eq!(last.number, 102);

        // Pop all blocks — last_enqueued must still report 102
        assert!(queue.pop_next().is_some());
        assert!(queue.pop_next().is_some());
        assert!(queue.pop_next().is_some());
        assert!(queue.pop_next().is_none());

        let last = queue.last_enqueued().unwrap();
        assert_eq!(last.number, 102, "last_enqueued must survive pop_next");

        // Enqueue more, then drain — last_enqueued must still track
        queue.enqueue(make_test_header(200), vec![]);
        queue.enqueue(make_test_header(201), vec![]);
        assert_eq!(queue.last_enqueued().unwrap().number, 201);

        let drained = queue.lock().unwrap().drain();
        assert_eq!(drained.len(), 2);
        assert_eq!(
            queue.last_enqueued().unwrap().number,
            201,
            "last_enqueued must survive drain"
        );
    }
}
