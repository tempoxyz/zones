//! L1 chain subscription and deposit extraction.
//!
//! Subscribes to L1 chain notifications via RPC and extracts deposit events
//! from the ZonePortal contract.

use alloy_primitives::{Address, B256, keccak256};
use alloy_provider::{Provider, ProviderBuilder, WsConnect};
use alloy_rpc_types_eth::{Filter, Log};
use alloy_sol_types::{SolEvent, SolValue, sol};
use alloy_transport::Authorization;
use futures::StreamExt;
use reth_tracing::tracing::{debug, error, info, warn};
use std::sync::{Arc, Mutex};

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

/// L1 chain subscriber that listens for deposit events.
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

    /// Start the L1 subscriber.
    ///
    /// This will connect to the L1 node and subscribe to chain notifications,
    /// extracting deposit events and sending them to the deposit queue.
    pub async fn start(self) -> eyre::Result<()> {
        info!(url = %self.config.l1_rpc_url, "Connecting to L1 node");

        let url: url::Url = self.config.l1_rpc_url.parse()?;
        let mut ws = WsConnect::new(self.config.l1_rpc_url.clone());

        if !url.username().is_empty() {
            let auth = Authorization::basic(url.username(), url.password().unwrap_or_default());
            ws = ws.with_auth(auth);
        }

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

                self.deposit_queue
                    .lock()
                    .map_err(|_| {
                        error!("Failed to lock deposit queue");
                        eyre::eyre!("Deposit queue lock poisoned")
                    })?
                    .enqueue(deposit);

                info!(l1_block = block_number, "Enqueued deposit from L1");
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
pub struct PendingDeposits {
    /// Head of deposit queue hash chain
    pub hash: B256,
    /// Pending deposits not yet processed by the zone
    pub pending_deposits: Vec<Deposit>,
}

impl PendingDeposits {
    /// Append a deposit to the queue and update the hash chain
    pub fn enqueue(&mut self, deposit: Deposit) {
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
        self.pending_deposits.push(deposit);
    }

    pub fn drain(&mut self) -> Vec<Deposit> {
        std::mem::take(&mut self.pending_deposits)
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

        queue.enqueue(d1.clone());
        let hash_after_d1 = queue.hash;
        assert_ne!(hash_after_d1, B256::ZERO);

        // Verify hash is deterministic
        let mut queue2 = PendingDeposits::default();
        queue2.enqueue(d1);
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

        queue.enqueue(d2);
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

        for d in &deposits {
            queue.enqueue(d.clone());
        }

        let transition = PendingDeposits::transition(B256::ZERO, &deposits);

        assert_eq!(queue.hash, transition.next_processed_hash);
    }
}
