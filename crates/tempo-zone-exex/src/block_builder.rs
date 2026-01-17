//! Block Builder for Tempo Zone L2.
//!
//! Spawns a background task that produces blocks at regular intervals.

use crate::{L2Database, execution};
use reth_primitives::{RecoveredBlock, Block, TransactionSigned, Recovered};
use reth_revm::db::BundleState;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Default block production interval (1 second).
pub const DEFAULT_BLOCK_INTERVAL: Duration = Duration::from_secs(1);

/// Default gas limit per block.
pub const DEFAULT_GAS_LIMIT: u64 = 30_000_000;

/// Block builder configuration.
#[derive(Debug, Clone)]
pub struct BlockBuilderConfig {
    /// Interval between block production attempts.
    pub block_interval: Duration,
    /// Gas limit per block.
    pub gas_limit: u64,
    /// Whether to produce empty blocks.
    pub produce_empty_blocks: bool,
}

impl Default for BlockBuilderConfig {
    fn default() -> Self {
        Self {
            block_interval: DEFAULT_BLOCK_INTERVAL,
            gas_limit: DEFAULT_GAS_LIMIT,
            produce_empty_blocks: true,
        }
    }
}

/// Handle to submit transactions to the block builder.
#[derive(Debug, Clone)]
pub struct BlockBuilderHandle {
    tx_sender: mpsc::Sender<Recovered<TransactionSigned>>,
}

impl BlockBuilderHandle {
    /// Submit a transaction to be included in a future block.
    pub async fn submit_tx(&self, tx: Recovered<TransactionSigned>) -> Result<(), mpsc::error::SendError<Recovered<TransactionSigned>>> {
        self.tx_sender.send(tx).await
    }
}

/// Notification sent when a new block is produced.
#[derive(Debug, Clone)]
pub struct NewBlockNotification {
    pub block: RecoveredBlock<Block>,
    pub bundle: BundleState,
}

/// Block builder that produces blocks at regular intervals.
pub struct BlockBuilder {
    config: BlockBuilderConfig,
    chain_spec: Arc<reth_chainspec::ChainSpec>,
    db: Arc<std::sync::Mutex<L2Database>>,
    tx_receiver: mpsc::Receiver<Recovered<TransactionSigned>>,
    block_sender: mpsc::Sender<NewBlockNotification>,
    pending_txs: Vec<Recovered<TransactionSigned>>,
}

impl BlockBuilder {
    /// Create a new block builder.
    ///
    /// Returns the builder and a handle for submitting transactions.
    pub fn new(
        config: BlockBuilderConfig,
        chain_spec: Arc<reth_chainspec::ChainSpec>,
        db: Arc<std::sync::Mutex<L2Database>>,
        block_sender: mpsc::Sender<NewBlockNotification>,
    ) -> (Self, BlockBuilderHandle) {
        let (tx_sender, tx_receiver) = mpsc::channel(1024);

        let builder = Self {
            config,
            chain_spec,
            db,
            tx_receiver,
            block_sender,
            pending_txs: Vec::new(),
        };

        let handle = BlockBuilderHandle { tx_sender };

        (builder, handle)
    }

    /// Spawn the block builder as a background task.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(self.run())
    }

    /// Run the block builder loop.
    async fn run(mut self) {
        info!(
            interval_ms = self.config.block_interval.as_millis(),
            gas_limit = self.config.gas_limit,
            produce_empty_blocks = self.config.produce_empty_blocks,
            "Block builder started"
        );

        let mut interval = tokio::time::interval(self.config.block_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    if let Err(e) = self.produce_block().await {
                        warn!(?e, "Failed to produce block");
                    }
                }
                Some(tx) = self.tx_receiver.recv() => {
                    debug!(tx_hash = %tx.tx_hash(), "Received transaction");
                    self.pending_txs.push(tx);
                }
            }
        }
    }

    /// Produce a new block with pending transactions.
    async fn produce_block(&mut self) -> eyre::Result<()> {
        if self.pending_txs.is_empty() && !self.config.produce_empty_blocks {
            debug!("No pending transactions and empty blocks disabled, skipping");
            return Ok(());
        }

        let transactions = std::mem::take(&mut self.pending_txs);
        let tx_count = transactions.len();

        let (block, bundle) = {
            let mut db = self.db.lock().expect("poisoned lock");

            let current_block = db.current_block_number();
            let next_block = current_block + 1;
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();

            let parent_header = if current_block > 0 {
                db.get_block(current_block)?
                    .map(|b| b.header().clone())
            } else {
                None
            };

            let (block, bundle, _receipts, _results) = execution::execute_block(
                &mut db,
                self.chain_spec.clone(),
                parent_header.as_ref(),
                next_block,
                timestamp,
                self.config.gas_limit,
                transactions,
            )?;

            db.insert_block_with_bundle(&block, bundle.clone())?;

            (block, bundle)
        };

        let block_number = block.header().number;
        let block_hash = block.hash();

        info!(
            block_number,
            %block_hash,
            tx_count,
            "Produced new block"
        );

        if self.block_sender.send(NewBlockNotification { block, bundle }).await.is_err() {
            warn!("Block receiver dropped, stopping block builder");
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_chainspec::MAINNET;

    #[tokio::test]
    async fn test_block_builder_produces_empty_blocks() {
        let db = Arc::new(std::sync::Mutex::new(L2Database::new_in_memory().unwrap()));
        let (block_tx, mut block_rx) = mpsc::channel(16);

        let config = BlockBuilderConfig {
            block_interval: Duration::from_millis(50),
            produce_empty_blocks: true,
            ..Default::default()
        };

        let (builder, _handle) = BlockBuilder::new(
            config,
            MAINNET.clone(),
            db.clone(),
            block_tx,
        );

        let task = builder.spawn();

        let notification = tokio::time::timeout(Duration::from_secs(1), block_rx.recv())
            .await
            .expect("timeout waiting for block")
            .expect("channel closed");

        assert_eq!(notification.block.header().number, 1);

        task.abort();
    }

    #[tokio::test]
    async fn test_block_builder_skips_empty_when_disabled() {
        let db = Arc::new(std::sync::Mutex::new(L2Database::new_in_memory().unwrap()));
        let (block_tx, mut block_rx) = mpsc::channel(16);

        let config = BlockBuilderConfig {
            block_interval: Duration::from_millis(50),
            produce_empty_blocks: false,
            ..Default::default()
        };

        let (builder, _handle) = BlockBuilder::new(
            config,
            MAINNET.clone(),
            db,
            block_tx,
        );

        let task = builder.spawn();

        let result = tokio::time::timeout(Duration::from_millis(200), block_rx.recv()).await;
        assert!(result.is_err(), "Should timeout as no blocks produced");

        task.abort();
    }
}
