//! L1 chain subscription and deposit extraction.
//!
//! Subscribes to L1 chain notifications via RPC and extracts deposit events
//! from the ZonePortal contract.

use alloy_primitives::{Address, U256, address};
use alloy_provider::{Provider, ProviderBuilder, WsConnect};
use alloy_rpc_types_eth::{Filter, Log};
use alloy_sol_types::{SolEvent, sol};
use futures::StreamExt;
use reth_tracing::tracing::{debug, error, info, warn};
use std::sync::{Arc, Mutex};

/// Default ZonePortal contract address on L1.
///
/// TODO: Make this configurable per chain spec.
pub const ZONE_PORTAL_ADDRESS: Address = address!("0x0000000000000000000000000000000000004242");

sol! {
    /// Event emitted by the ZonePortal when a deposit is enqueued.
    #[derive(Debug)]
    event DepositEnqueued(
        address indexed sender,
        address indexed to,
        uint256 amount,
        bytes data
    );
}

/// A deposit extracted from L1.
#[derive(Debug, Clone)]
pub struct Deposit {
    /// L1 block number where the deposit was included.
    pub l1_block_number: u64,
    /// Sender on L1.
    pub sender: Address,
    /// Recipient on L2.
    pub to: Address,
    /// Amount deposited (in wei).
    pub amount: U256,
    /// Optional calldata for contract interactions.
    pub data: alloy_primitives::Bytes,
}

impl Deposit {
    /// Create a new deposit from an event and block number.
    pub fn from_event(event: DepositEnqueued, l1_block_number: u64) -> Self {
        Self {
            l1_block_number,
            sender: event.sender,
            to: event.to,
            amount: event.amount,
            data: event.data,
        }
    }
}

/// Shared deposit queue for passing deposits between L1 subscriber and block builder.
pub type DepositQueue = Arc<Mutex<Vec<Deposit>>>;

/// Create a new empty deposit queue.
pub fn deposit_queue() -> DepositQueue {
    Arc::new(Mutex::new(Vec::new()))
}

/// Configuration for the L1 subscriber.
#[derive(Debug, Clone)]
pub struct L1SubscriberConfig {
    /// WebSocket URL of the L1 node.
    pub l1_rpc_url: String,
    /// ZonePortal contract address on L1.
    pub portal_address: Address,
}

impl Default for L1SubscriberConfig {
    fn default() -> Self {
        Self {
            l1_rpc_url: "ws://localhost:8546".to_string(),
            portal_address: ZONE_PORTAL_ADDRESS,
        }
    }
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
            .event_signature(DepositEnqueued::SIGNATURE_HASH);

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

        match DepositEnqueued::decode_log(&log.inner) {
            Ok(event) => {
                let deposit = Deposit::from_event(event.data, block_number);

                debug!(
                    l1_block = block_number,
                    sender = %deposit.sender,
                    to = %deposit.to,
                    amount = %deposit.amount,
                    "Received deposit from L1"
                );

                // Add deposit to the queue
                if let Ok(mut queue) = self.deposit_queue.lock() {
                    queue.push(deposit);
                    info!(
                        l1_block = block_number,
                        queue_len = queue.len(),
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
                    "Log from ZonePortal is not a DepositEnqueued event"
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
