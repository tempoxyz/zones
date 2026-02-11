//! L1 chain subscription and deposit extraction.
//!
//! Subscribes to L1 block headers via WebSocket and extracts deposit events
//! from the ZonePortal contract for each block.

use alloy_consensus::Header;
use alloy_primitives::{Address, B256, keccak256};
use alloy_provider::{Provider, ProviderBuilder, WsConnect};
use alloy_rpc_types_eth::{Filter, Log};
use alloy_sol_types::{SolEvent, SolValue, sol};
use alloy_transport::Authorization;
use futures::StreamExt;
use reth_stages_types::{StageCheckpoint, StageId};
use reth_storage_api::{
    DBProvider, DatabaseProviderFactory, StageCheckpointReader, StageCheckpointWriter,
};
use std::sync::{Arc, Mutex};
use tracing::{debug, error, info, warn};

sol! {
    // TODO: Rename to DepositEnqueued once the Solidity contract is updated.
    /// Event emitted by the ZonePortal when a deposit is made.
    #[derive(Debug)]
    event DepositMade(
        bytes32 indexed newCurrentDepositQueueHash,
        address indexed sender,
        address to,
        uint128 netAmount,
        uint128 fee,
        bytes32 memo
    );
}

/// Configuration for the L1 subscriber.
#[derive(Debug, Clone)]
pub struct L1SubscriberConfig {
    /// WebSocket URL of the L1 node.
    pub l1_rpc_url: String,
    /// ZonePortal contract address on L1.
    pub portal_address: Address,
}

/// Stage ID used to persist the last synced L1 block number in the reth database.
const L1_SYNC_STAGE_ID: StageId = StageId::Other("L1Sync");

/// Maximum number of blocks to request logs for in a single `eth_getLogs` call
/// during backfill.
const BACKFILL_BATCH_SIZE: u64 = 1000;

/// L1 chain subscriber that listens for new blocks and extracts deposit events.
#[derive(Clone)]
pub struct L1Subscriber<P> {
    config: L1SubscriberConfig,
    deposit_queue: DepositQueue,
    /// Reth database provider factory for persisting the L1 sync checkpoint.
    db_provider_factory: P,
}

impl<P> L1Subscriber<P>
where
    P: DatabaseProviderFactory + StageCheckpointReader + Send + Sync + Clone + 'static,
    P::ProviderRW: StageCheckpointWriter,
{
    /// Create and spawn the L1 subscriber as a critical background task.
    pub fn spawn(
        config: L1SubscriberConfig,
        deposit_queue: DepositQueue,
        db_provider_factory: P,
        task_executor: impl reth_ethereum::tasks::TaskSpawner,
    ) {
        let subscriber = Self {
            config,
            deposit_queue,
            db_provider_factory,
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

    /// Read the last synced L1 block number from the database.
    fn last_synced_l1_block(&self) -> eyre::Result<Option<u64>> {
        Ok(self
            .db_provider_factory
            .get_stage_checkpoint(L1_SYNC_STAGE_ID)?
            .map(|cp| cp.block_number))
    }

    /// Persist the last synced L1 block number to the database.
    fn save_l1_checkpoint(&self, block_number: u64) -> eyre::Result<()> {
        let provider_rw = self.db_provider_factory.database_provider_rw()?;
        provider_rw.save_stage_checkpoint(L1_SYNC_STAGE_ID, StageCheckpoint::new(block_number))?;
        provider_rw.commit()?;
        Ok(())
    }

    /// Start the L1 subscriber.
    ///
    /// On startup, reads the last synced L1 block from the database and backfills
    /// any missed blocks before subscribing to new ones. Each processed block's
    /// number is persisted so restarts resume from the correct point.
    pub async fn start(self) -> eyre::Result<()> {
        info!(url = %self.config.l1_rpc_url, "Connecting to L1 node");

        let url: url::Url = self.config.l1_rpc_url.parse()?;
        let mut ws = WsConnect::new(self.config.l1_rpc_url.clone());

        if !url.username().is_empty() {
            let auth = Authorization::basic(url.username(), url.password().unwrap_or_default());
            ws = ws.with_auth(auth);
        }

        let provider = ProviderBuilder::new().connect_ws(ws).await?;

        info!("Connected to L1 node");

        // Init filter once — same for every block
        let filter = Filter::new()
            .address(self.config.portal_address)
            .event_signature(DepositMade::SIGNATURE_HASH);

        // ── Backfill: catch up on any L1 blocks missed while offline ──
        let tip = provider.get_block_number().await?;

        if let Some(last_synced) = self.last_synced_l1_block()? {
            let from = last_synced + 1;
            if from <= tip {
                info!(
                    from_block = from,
                    to_block = tip,
                    missed = tip - from + 1,
                    "Backfilling missed L1 blocks"
                );

                let mut cursor = from;
                while cursor <= tip {
                    let batch_end = (cursor + BACKFILL_BATCH_SIZE - 1).min(tip);
                    let logs = provider
                        .get_logs(&filter.clone().select(cursor..=batch_end))
                        .await?;

                    // Group logs by block number so we can enqueue per-block
                    let mut blocks: std::collections::BTreeMap<u64, Vec<Deposit>> =
                        std::collections::BTreeMap::new();
                    for log in logs {
                        let block_number = log
                            .block_number
                            .ok_or_else(|| eyre::eyre!("log missing block number"))?;
                        let deposit = self.parse_deposit(log, block_number)?;
                        blocks.entry(block_number).or_default().push(deposit);
                    }

                    // Enqueue each block's deposits
                    for (block_number, deposits) in &blocks {
                        info!(
                            block = block_number,
                            count = deposits.len(),
                            "Backfill: deposits from L1 block"
                        );

                        let header = Header {
                            number: *block_number,
                            ..Default::default()
                        };

                        self.deposit_queue
                            .lock()
                            .map_err(|_| eyre::eyre!("Deposit queue lock poisoned"))?
                            .enqueue(header, deposits.clone());
                    }

                    self.save_l1_checkpoint(batch_end)?;
                    debug!(from = cursor, to = batch_end, "Backfill batch complete");
                    cursor = batch_end + 1;
                }

                info!(tip, "Backfill complete, caught up to L1 tip");
            } else {
                info!(last_synced, tip, "Already synced to L1 tip");
            }
        } else {
            info!(tip, "No previous L1 sync state, starting from current tip");
            self.save_l1_checkpoint(tip)?;
        }

        // ── Live subscription: process new blocks as they arrive ──
        let sub = provider.subscribe_blocks().await?;
        let mut stream = sub.into_stream();

        info!(
            portal = %self.config.portal_address,
            "Subscribed to L1 blocks for deposit events"
        );

        while let Some(header) = stream.next().await {
            let block_number = header.number;

            // Fetch deposit logs for this block — must not skip on failure
            let logs = provider.get_logs(&filter.clone().select(block_number)).await?;

            // Parse deposit events from the logs
            let mut deposits = Vec::new();
            for log in logs {
                let deposit = self.parse_deposit(log, block_number)?;

                debug!(
                    l1_block = block_number,
                    sender = %deposit.sender,
                    to = %deposit.to,
                    amount = %deposit.amount,
                    memo = %deposit.memo,
                    "Deposit from L1"
                );

                deposits.push(deposit);
            }

            // Persist progress even for empty blocks so we never re-scan them.
            self.save_l1_checkpoint(block_number)?;

            if deposits.is_empty() {
                debug!(block = block_number, "No deposits in L1 block");
                continue;
            }

            info!(
                block = block_number,
                count = deposits.len(),
                "Received deposits from L1 block"
            );

            self.deposit_queue
                .lock()
                .map_err(|_| {
                    error!("Failed to lock deposit queue");
                    eyre::eyre!("Deposit queue lock poisoned")
                })?
                .enqueue(header.clone().into(), deposits);

            info!(block = block_number, "Enqueued L1 block deposits");
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
    pub header: Header,
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
    pub fn enqueue(&mut self, header: Header, deposits: Vec<Deposit>) {
        for deposit in &deposits {
            self.hash = keccak256(
                (
                    deposit.sender,
                    deposit.to,
                    deposit.amount,
                    deposit.memo,
                    self.hash,
                )
                    .abi_encode(),
            );
        }
        self.pending.push(L1BlockDeposits { header, deposits });
    }

    /// Drain all pending L1 block deposits.
    pub fn drain(&mut self) -> Vec<L1BlockDeposits> {
        std::mem::take(&mut self.pending)
    }

    /// Compute a [`DepositQueueTransition`] for a batch of deposits starting from `prev_hash`
    pub fn transition(prev_hash: B256, deposits: &[Deposit]) -> DepositQueueTransition {
        let mut current = prev_hash;
        for d in deposits {
            current = keccak256((d.sender, d.to, d.amount, d.memo, current).abi_encode());
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

/// Shared deposit queue for passing deposits between L1 subscriber and block builder.
pub type DepositQueue = Arc<Mutex<PendingDeposits>>;

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{FixedBytes, address};

    fn make_test_header(number: u64) -> Header {
        Header {
            number,
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
        assert_eq!(blocks[0].header.number, 10);
        assert_eq!(blocks[0].deposits.len(), 1);
        assert_eq!(blocks[1].header.number, 11);
        assert_eq!(blocks[1].deposits.len(), 1);

        // After drain, pending is empty
        assert!(queue.drain().is_empty());
    }
}
