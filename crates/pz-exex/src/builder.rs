//! Zone block builder for Privacy Zone.
//!
//! Spawns a critical task that builds zone blocks on a 250ms interval.
//! Uses tempo-evm for execution and integrates with reth's infrastructure.

use crate::{
    deposit::process_deposit,
    state::ZoneState,
    types::{PendingTx, PzConfig},
};
use alloy_primitives::{B256, keccak256};
use parking_lot::Mutex;
use reth_revm::db::BundleState;
use reth_tracing::tracing::{debug, info, warn};
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};
use tokio::time::{Interval, interval};

/// Default block building interval (250ms).
pub const DEFAULT_BLOCK_INTERVAL_MS: u64 = 250;

/// Default gas limit per zone block.
pub const DEFAULT_GAS_LIMIT: u64 = 30_000_000;

/// A built zone block.
#[derive(Debug, Clone)]
pub struct ZoneBlock {
    /// Zone block number.
    pub number: u64,
    /// Block timestamp (unix millis).
    pub timestamp: u64,
    /// Parent block hash.
    pub parent_hash: B256,
    /// Block hash.
    pub hash: B256,
    /// State root after execution.
    pub state_root: B256,
    /// Transactions root.
    pub transactions_root: B256,
    /// Number of transactions executed.
    pub tx_count: usize,
    /// Number of deposits processed.
    pub deposit_count: usize,
    /// Total gas used.
    pub gas_used: u64,
    /// Bundle state changes.
    pub bundle: BundleState,
}

impl ZoneBlock {
    /// Compute block hash from header fields.
    pub fn compute_hash(
        number: u64,
        timestamp: u64,
        parent_hash: B256,
        state_root: B256,
        transactions_root: B256,
    ) -> B256 {
        let mut data = Vec::with_capacity(32 * 3 + 16);
        data.extend_from_slice(&number.to_be_bytes());
        data.extend_from_slice(&timestamp.to_be_bytes());
        data.extend_from_slice(parent_hash.as_slice());
        data.extend_from_slice(state_root.as_slice());
        data.extend_from_slice(transactions_root.as_slice());
        keccak256(&data)
    }
}

/// Shared state for the zone block builder.
///
/// This is shared between the ExEx (which queues deposits) and the builder task.
#[derive(Debug)]
pub struct SharedZoneState {
    inner: Mutex<ZoneState>,
}

impl SharedZoneState {
    /// Create new shared state.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ZoneState::new()),
        }
    }

    /// Create from existing state.
    pub fn from_state(state: ZoneState) -> Self {
        Self {
            inner: Mutex::new(state),
        }
    }

    /// Lock and access the state.
    pub fn lock(&self) -> parking_lot::MutexGuard<'_, ZoneState> {
        self.inner.lock()
    }
}

impl Default for SharedZoneState {
    fn default() -> Self {
        Self::new()
    }
}

/// Channel for receiving built blocks.
pub type BlockReceiver = tokio::sync::mpsc::UnboundedReceiver<ZoneBlock>;
/// Channel for sending built blocks.
pub type BlockSender = tokio::sync::mpsc::UnboundedSender<ZoneBlock>;

/// Zone block builder that runs on a timer.
///
/// This is a Future that runs indefinitely, building blocks every interval.
pub struct ZoneBlockBuilder {
    /// Zone configuration.
    config: PzConfig,
    /// Shared zone state.
    state: Arc<SharedZoneState>,
    /// Block building interval.
    interval: Interval,
    /// Gas limit per block.
    gas_limit: u64,
    /// Last block hash (for chaining).
    last_block_hash: B256,
    /// Current block number.
    block_number: u64,
    /// Channel to send built blocks.
    block_tx: BlockSender,
}

impl ZoneBlockBuilder {
    /// Create a new zone block builder.
    ///
    /// Returns the builder and a receiver for built blocks.
    pub fn new(config: PzConfig, state: Arc<SharedZoneState>) -> (Self, BlockReceiver) {
        let (block_tx, block_rx) = tokio::sync::mpsc::unbounded_channel();

        let builder = Self {
            last_block_hash: config.genesis_state_root,
            block_number: 0,
            interval: interval(Duration::from_millis(DEFAULT_BLOCK_INTERVAL_MS)),
            gas_limit: DEFAULT_GAS_LIMIT,
            config,
            state,
            block_tx,
        };

        (builder, block_rx)
    }

    /// Set the block building interval.
    pub fn with_interval(mut self, duration: Duration) -> Self {
        self.interval = interval(duration);
        self
    }

    /// Set the gas limit.
    pub fn with_gas_limit(mut self, gas_limit: u64) -> Self {
        self.gas_limit = gas_limit;
        self
    }

    /// Try to build a block from pending transactions.
    ///
    /// Returns None if no transactions to process.
    fn try_build_block(&mut self) -> Option<ZoneBlock> {
        let mut state = self.state.lock();

        // Take pending txs
        let pending_txs = state.take_pending_txs();
        if pending_txs.is_empty() {
            return None;
        }

        let block_number = self.block_number + 1;
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let parent_hash = self.last_block_hash;

        let mut tx_count = 0usize;
        let mut deposit_count = 0usize;
        let mut gas_used = 0u64;
        let exit_index = state.pending_exits().len() as u64;

        info!(
            block_number,
            pending_txs = pending_txs.len(),
            "Building zone block"
        );

        for pending_tx in pending_txs {
            // Check gas limit
            if gas_used >= self.gas_limit {
                debug!(
                    gas_used,
                    gas_limit = self.gas_limit,
                    "Block gas limit reached"
                );
                // TODO: re-queue remaining txs
                break;
            }

            match pending_tx {
                PendingTx::Deposit { deposit, .. } => {
                    match process_deposit(
                        &mut state,
                        self.config.gas_token,
                        &deposit,
                        block_number,
                        exit_index,
                    ) {
                        Ok(result) => {
                            gas_used += result.gas_used;
                            deposit_count += 1;
                            tx_count += 1;

                            if result.success {
                                debug!(
                                    to = %deposit.to,
                                    amount = %deposit.amount,
                                    gas_used = result.gas_used,
                                    "Deposit processed"
                                );
                            } else if let Some(refund_exit) = result.refund_exit {
                                warn!(
                                    to = %deposit.to,
                                    refund_to = %refund_exit.recipient,
                                    "Deposit calldata failed, queued refund exit"
                                );
                                let exit_hash = refund_exit.hash(state.zone_state().exits_hash);
                                state.zone_state_mut().exits_hash = exit_hash;
                                state.queue_exit(refund_exit, exit_hash);
                            }
                        }
                        Err(err) => {
                            warn!(%err, "Failed to process deposit");
                            // TODO: handle deposit failure
                        }
                    }
                }
                PendingTx::UserTx { tx_hash, .. } => {
                    // TODO: implement user tx execution
                    debug!(%tx_hash, "User tx execution not yet implemented");
                }
            }
        }

        if tx_count == 0 {
            return None;
        }

        // Compute roots (simplified for now)
        // TODO: compute proper state root from bundle
        let state_root = B256::repeat_byte(0x42); // Placeholder
        let transactions_root = B256::repeat_byte(0x43); // Placeholder

        let hash = ZoneBlock::compute_hash(
            block_number,
            timestamp,
            parent_hash,
            state_root,
            transactions_root,
        );

        // Update builder state
        self.block_number = block_number;
        self.last_block_hash = hash;

        // Update zone state
        state.zone_state_mut().zone_block = block_number;
        state.zone_state_mut().state_root = state_root;

        let block = ZoneBlock {
            number: block_number,
            timestamp,
            parent_hash,
            hash,
            state_root,
            transactions_root,
            tx_count,
            deposit_count,
            gas_used,
            bundle: BundleState::default(), // TODO: capture actual bundle
        };

        info!(
            block_number = block.number,
            block_hash = %block.hash,
            tx_count = block.tx_count,
            deposit_count = block.deposit_count,
            gas_used = block.gas_used,
            "Built zone block"
        );

        Some(block)
    }
}

impl Future for ZoneBlockBuilder {
    type Output = eyre::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            // Wait for next interval tick
            if self.interval.poll_tick(cx).is_pending() {
                return Poll::Pending;
            }

            // Try to build a block
            if let Some(block) = self.try_build_block() {
                // Send block to receiver
                if self.block_tx.send(block).is_err() {
                    // Receiver dropped, shut down
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Deposit, L1Cursor};
    use alloy_primitives::{Address, U256};

    fn test_config() -> PzConfig {
        PzConfig {
            zone_id: 1,
            portal_address: Address::ZERO,
            gas_token: Address::ZERO,
            sequencer: Address::ZERO,
            genesis_state_root: B256::ZERO,
            data_dir: None,
        }
    }

    #[test]
    fn test_shared_state() {
        let shared = Arc::new(SharedZoneState::new());

        // Queue a deposit
        {
            let mut state = shared.lock();
            let deposit = Deposit {
                l1_block_hash: B256::ZERO,
                l1_block_number: 100,
                l1_timestamp: 1000,
                sender: Address::repeat_byte(0x01),
                to: Address::repeat_byte(0x02),
                amount: U256::from(1000),
                gas_limit: 0,
                data: Default::default(),
            };
            state.queue_deposit(L1Cursor::new(100, 0), deposit, B256::repeat_byte(0x01));
        }

        // Check pending
        {
            let state = shared.lock();
            assert_eq!(state.pending_txs().len(), 1);
        }
    }

    #[test]
    fn test_block_hash_deterministic() {
        let hash1 = ZoneBlock::compute_hash(1, 1000, B256::ZERO, B256::ZERO, B256::ZERO);
        let hash2 = ZoneBlock::compute_hash(1, 1000, B256::ZERO, B256::ZERO, B256::ZERO);
        assert_eq!(hash1, hash2);

        let hash3 = ZoneBlock::compute_hash(2, 1000, B256::ZERO, B256::ZERO, B256::ZERO);
        assert_ne!(hash1, hash3);
    }
}
