//! L1 chain subscription and deposit extraction.
//!
//! Subscribes to L1 chain notifications via RPC and extracts deposit events
//! from the ZonePortal contract.

use alloy_primitives::{Address, B256, keccak256};
use alloy_provider::{Provider, ProviderBuilder, WsConnect};
use alloy_rpc_types_eth::{Filter, Log};
use alloy_sol_types::{SolEvent, SolValue, sol};
use futures::StreamExt;
use reth_tracing::tracing::{debug, error, info, warn};
use std::sync::{Arc, Mutex};

sol! {
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

/// Deposit queue hash chain state.
///
/// Tracks deposits and maintains the hash chain:
/// `newHash = keccak256(abi.encode(deposit, prevHash))`
///
/// This mirrors the L1 portal's `currentDepositQueueHash`.
#[derive(Debug, Default)]
pub struct DepositQueueState {
    /// Current deposit queue hash (head of the hash chain).
    pub current_hash: B256,
    /// Pending deposits not yet processed by the zone.
    pub pending_deposits: Vec<Deposit>,
}

impl DepositQueueState {
    /// Append a deposit to the queue and update the hash chain.
    ///
    /// Computes: `currentHash = keccak256(abi.encode(deposit, currentHash))`
    pub fn enqueue(&mut self, deposit: Deposit) {
        self.current_hash = deposit_queue_hash(&deposit, self.current_hash);
        self.pending_deposits.push(deposit);
    }
}

/// Compute the deposit queue hash for a single deposit.
///
/// `keccak256(abi.encode(deposit, prevHash))`
///
/// The deposit is ABI-encoded as the Solidity struct `Deposit{sender, to, amount, memo}`,
/// followed by the previous hash.
pub fn deposit_queue_hash(deposit: &Deposit, prev_hash: B256) -> B256 {
    let encoded = (
        deposit.sender,
        deposit.to,
        deposit.amount,
        deposit.memo,
        prev_hash,
    )
        .abi_encode();
    keccak256(&encoded)
}

/// Deposit queue transition for batch proof validation.
///
/// Tracks where the zone started and stopped processing deposits.
#[derive(Debug, Clone, Default)]
pub struct DepositQueueTransition {
    /// Where the zone started processing (verified against zone state).
    pub prev_processed_hash: B256,
    /// Where the zone processed up to (proof output).
    pub next_processed_hash: B256,
}

/// Process a batch of deposits starting from `processed_hash`, returning the transition.
///
/// Computes the hash chain for the given deposits and returns a `DepositQueueTransition`
/// with the before/after hashes for batch proof validation.
pub fn process_deposits(processed_hash: B256, deposits: &[Deposit]) -> DepositQueueTransition {
    let mut current = processed_hash;
    for deposit in deposits {
        current = deposit_queue_hash(deposit, current);
    }
    DepositQueueTransition {
        prev_processed_hash: processed_hash,
        next_processed_hash: current,
    }
}

/// Shared deposit queue for passing deposits between L1 subscriber and block builder.
#[derive(Debug, Clone, Default)]
pub struct DepositQueue(Arc<Mutex<DepositQueueState>>);

impl DepositQueue {
    /// Lock the queue and return a guard.
    pub fn lock(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, DepositQueueState>> {
        self.0.lock()
    }
}

/// Configuration for the L1 subscriber.
#[derive(Debug, Clone)]
pub struct L1SubscriberConfig {
    /// WebSocket URL of the L1 node.
    pub l1_rpc_url: String,
    /// ZonePortal contract address on L1.
    pub portal_address: Address,
}

/// L1 chain subscriber that listens for deposit events.
#[derive(Clone)]
pub struct L1Subscriber {
    config: L1SubscriberConfig,
    deposit_queue: DepositQueue,
}

impl L1Subscriber {
    /// Create a new L1 subscriber.
    pub fn new(config: L1SubscriberConfig, deposit_queue: DepositQueue) -> Self {
        Self {
            config,
            deposit_queue,
        }
    }

    /// Start the L1 subscriber.
    ///
    /// This will connect to the L1 node and subscribe to chain notifications,
    /// extracting deposit events and sending them to the deposit queue.
    pub async fn start(self) -> eyre::Result<()> {
        info!(url = %self.config.l1_rpc_url, "Connecting to L1 node");

        let ws = WsConnect::new(&self.config.l1_rpc_url);
        let provider = ProviderBuilder::new().connect_ws(ws).await?;

        info!("Connected to L1 node, subscribing to logs");

        // Subscribe to DepositEnqueued events from the ZonePortal
        let filter = Filter::new()
            .address(self.config.portal_address)
            .event_signature(DepositMade::SIGNATURE_HASH);

        let sub = provider.subscribe_logs(&filter).await?;
        let mut stream = sub.into_stream();

        info!(
            portal = %self.config.portal_address,
            "Subscribed to L1 deposit events"
        );

        while let Some(log) = stream.next().await {
            if let Err(e) = self.process_log(log) {
                warn!(error = %e, "Failed to process L1 log");
            }
        }

        warn!("L1 subscription stream ended");
        Ok(())
    }

    /// Process a single log from L1.
    fn process_log(&self, log: Log) -> eyre::Result<()> {
        let block_number = log.block_number.unwrap_or(0);

        match DepositMade::decode_log(&log.inner) {
            Ok(event) => {
                let deposit = Deposit::from_event(event.data, block_number);

                debug!(
                    l1_block = block_number,
                    sender = %deposit.sender,
                    to = %deposit.to,
                    amount = %deposit.amount,
                    memo = %deposit.memo,
                    "Received deposit from L1"
                );

                if let Ok(mut queue) = self.deposit_queue.lock() {
                    queue.enqueue(deposit);
                    info!(
                        l1_block = block_number,
                        queue_len = queue.pending_deposits.len(),
                        current_hash = %queue.current_hash,
                        "Enqueued deposit from L1"
                    );
                } else {
                    error!("Failed to lock deposit queue");
                    return Err(eyre::eyre!("Deposit queue lock poisoned"));
                }
            }
            Err(e) => {
                debug!(
                    error = %e,
                    "Log from ZonePortal is not a DepositMade event"
                );
            }
        }

        Ok(())
    }
}

/// Spawn the L1 subscriber as a background task.
///
/// Returns the shared deposit queue that the block builder can drain.
pub fn spawn_l1_subscriber(
    config: L1SubscriberConfig,
    deposit_queue: DepositQueue,
    task_executor: impl reth_ethereum::tasks::TaskSpawner,
) {
    let subscriber = L1Subscriber::new(config, deposit_queue);

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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{FixedBytes, address};

    #[test]
    fn test_deposit_queue_hash_chain() {
        let mut queue = DepositQueueState::default();
        assert_eq!(queue.current_hash, B256::ZERO);

        let d1 = Deposit {
            l1_block_number: 1,
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 1000,
            fee: 0,
            memo: B256::ZERO,
            queue_hash: B256::ZERO,
        };

        queue.enqueue(d1.clone());
        let hash_after_d1 = queue.current_hash;
        assert_ne!(hash_after_d1, B256::ZERO);

        // Verify hash is deterministic
        let expected = deposit_queue_hash(&d1, B256::ZERO);
        assert_eq!(hash_after_d1, expected);

        let d2 = Deposit {
            l1_block_number: 2,
            sender: address!("0x0000000000000000000000000000000000000003"),
            to: address!("0x0000000000000000000000000000000000000004"),
            amount: 2000,
            fee: 0,
            memo: B256::ZERO,
            queue_hash: B256::ZERO,
        };

        queue.enqueue(d2.clone());
        let hash_after_d2 = queue.current_hash;
        assert_ne!(hash_after_d2, hash_after_d1);

        // Verify chaining: hash(d2, hash(d1, 0))
        let expected = deposit_queue_hash(&d2, hash_after_d1);
        assert_eq!(hash_after_d2, expected);
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

        let transition = process_deposits(B256::ZERO, &deposits);

        assert_eq!(transition.prev_processed_hash, B256::ZERO);
        assert_ne!(transition.next_processed_hash, B256::ZERO);

        // Second batch with no deposits should be a no-op
        let transition2 = process_deposits(transition.next_processed_hash, &[]);
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
        let mut queue = DepositQueueState::default();

        let deposits = vec![Deposit {
            l1_block_number: 1,
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 500,
            fee: 0,
            memo: FixedBytes::from([0xABu8; 32]),
            queue_hash: B256::ZERO,
        }];

        for d in &deposits {
            queue.enqueue(d.clone());
        }

        let transition = process_deposits(B256::ZERO, &deposits);

        assert_eq!(queue.current_hash, transition.next_processed_hash);
    }
}
