//! Batching logic for accumulating blocks into batches.
//!
//! The [`BatchCoordinator`] accumulates blocks and flushes them into batches
//! based on time interval (250ms) or block count limits.

use crate::deposit_tracker::DepositTracker;
use crate::events::extract_withdrawals;
use crate::types::{BatchBlock, BatchInput, Deposit, StateTransitionWitness, Withdrawal};
use alloy_primitives::{Address, Log, B256};
use std::time::{Duration, Instant};

/// Default batch interval (250ms).
pub const DEFAULT_BATCH_INTERVAL: Duration = Duration::from_millis(250);

/// Default maximum blocks per batch.
pub const DEFAULT_MAX_BLOCKS_PER_BATCH: usize = 100;

/// Configuration for batch coordination.
#[derive(Debug, Clone)]
pub struct BatchConfig {
    /// Time interval for flushing batches.
    pub batch_interval: Duration,
    /// Maximum number of blocks per batch.
    pub max_blocks_per_batch: usize,
    /// Address of the ZoneOutbox contract for withdrawal event parsing.
    pub outbox_address: Address,
}

impl Default for BatchConfig {
    fn default() -> Self {
        Self {
            batch_interval: DEFAULT_BATCH_INTERVAL,
            max_blocks_per_batch: DEFAULT_MAX_BLOCKS_PER_BATCH,
            outbox_address: Address::ZERO,
        }
    }
}

/// Coordinates block accumulation and batch creation.
#[derive(Debug)]
pub struct BatchCoordinator {
    config: BatchConfig,
    pending_blocks: Vec<BatchBlock>,
    pending_withdrawals: Vec<Withdrawal>,
    last_flush: Instant,

    /// Deposit queue tracker for L1 deposits
    deposit_tracker: DepositTracker,

    /// Current state root
    current_state_root: B256,

    /// Withdrawal queue state
    expected_withdrawal_queue2: B256,
}

impl BatchCoordinator {
    /// Creates a new batch coordinator with the given configuration.
    pub fn new(config: BatchConfig) -> Self {
        Self {
            config,
            pending_blocks: Vec::new(),
            pending_withdrawals: Vec::new(),
            last_flush: Instant::now(),
            deposit_tracker: DepositTracker::new(),
            current_state_root: B256::ZERO,
            expected_withdrawal_queue2: B256::ZERO,
        }
    }

    /// Initializes the coordinator with the current portal state.
    pub fn initialize(
        &mut self,
        processed_deposit_queue_hash: B256,
        pending_deposit_queue_hash: B256,
        state_root: B256,
        withdrawal_queue2: B256,
    ) {
        self.deposit_tracker.reset(pending_deposit_queue_hash, processed_deposit_queue_hash);
        self.current_state_root = state_root;
        self.expected_withdrawal_queue2 = withdrawal_queue2;
    }

    /// Returns a reference to the deposit tracker.
    pub fn deposit_tracker(&self) -> &DepositTracker {
        &self.deposit_tracker
    }

    /// Returns a mutable reference to the deposit tracker.
    pub fn deposit_tracker_mut(&mut self) -> &mut DepositTracker {
        &mut self.deposit_tracker
    }

    /// Adds a block to the pending batch.
    pub fn add_block(&mut self, block: BatchBlock) {
        self.pending_blocks.push(block);
    }

    /// Adds a deposit to the pending batch and updates the deposit queue hash.
    pub fn add_deposit(&mut self, deposit: Deposit) {
        self.deposit_tracker.add_deposit(deposit);
    }

    /// Adds a withdrawal to the pending batch.
    pub fn add_withdrawal(&mut self, withdrawal: Withdrawal) {
        self.pending_withdrawals.push(withdrawal);
    }

    /// Extracts withdrawals from block receipts/logs and adds them to the pending batch.
    pub fn add_withdrawals_from_logs(&mut self, logs: &[Log]) {
        let withdrawals = extract_withdrawals(logs, self.config.outbox_address);
        self.pending_withdrawals.extend(withdrawals);
    }

    /// Returns true if the batch should be flushed based on time or block count.
    pub fn should_flush(&self) -> bool {
        if self.pending_blocks.is_empty() {
            return false;
        }

        let time_exceeded = self.last_flush.elapsed() >= self.config.batch_interval;
        let blocks_exceeded = self.pending_blocks.len() >= self.config.max_blocks_per_batch;

        time_exceeded || blocks_exceeded
    }

    /// Returns the pending batch if there are blocks, without consuming.
    pub fn get_pending_batch(&self) -> Option<PendingBatch> {
        if self.pending_blocks.is_empty() {
            return None;
        }

        Some(PendingBatch {
            blocks: self.pending_blocks.clone(),
            deposits: self.deposit_tracker.unprocessed_deposits().to_vec(),
            withdrawals: self.pending_withdrawals.clone(),
            processed_deposit_queue_hash: self.deposit_tracker.processed_deposit_queue_hash(),
            pending_deposit_queue_hash: self.deposit_tracker.pending_deposit_queue_hash(),
            prev_state_root: self.current_state_root,
            expected_withdrawal_queue2: self.expected_withdrawal_queue2,
        })
    }

    /// Flushes the pending batch and returns it for proving.
    ///
    /// Returns `None` if there are no pending blocks.
    pub fn flush_batch(&mut self) -> Option<BatchInput> {
        if self.pending_blocks.is_empty() {
            return None;
        }

        let blocks = std::mem::take(&mut self.pending_blocks);
        let withdrawals = std::mem::take(&mut self.pending_withdrawals);

        // Take all unprocessed deposits and compute new processed hash
        let deposit_count = self.deposit_tracker.unprocessed_count();
        let (deposits, new_processed_deposit_queue_hash) =
            self.deposit_tracker.take_for_batch(deposit_count);

        let new_state_root = blocks
            .last()
            .map(|b| b.state_root)
            .unwrap_or(self.current_state_root);

        let batch_input = BatchInput {
            processed_deposit_queue_hash: self.deposit_tracker.processed_deposit_queue_hash(),
            pending_deposit_queue_hash: self.deposit_tracker.pending_deposit_queue_hash(),
            new_processed_deposit_queue_hash,
            prev_state_root: self.current_state_root,
            new_state_root,
            expected_withdrawal_queue2: self.expected_withdrawal_queue2,
            updated_withdrawal_queue2: B256::ZERO, // Computed during proving
            new_withdrawal_queue_only: B256::ZERO, // Computed during proving
            blocks,
            deposits,
            withdrawals,
            witness: StateTransitionWitness::Mock,
        };

        self.last_flush = Instant::now();
        self.current_state_root = new_state_root;

        Some(batch_input)
    }

    /// Updates state after a successful batch submission.
    pub fn on_batch_submitted(
        &mut self,
        new_processed_deposit_queue_hash: B256,
        deposits_processed: usize,
        new_state_root: B256,
        updated_withdrawal_queue2: B256,
    ) {
        self.deposit_tracker
            .mark_processed(deposits_processed, new_processed_deposit_queue_hash);
        self.current_state_root = new_state_root;
        self.expected_withdrawal_queue2 = updated_withdrawal_queue2;
    }

    /// Returns the number of pending blocks.
    pub fn pending_block_count(&self) -> usize {
        self.pending_blocks.len()
    }

    /// Returns true if there are no pending blocks.
    pub fn is_empty(&self) -> bool {
        self.pending_blocks.is_empty()
    }
}

/// A pending batch before proving.
#[derive(Debug, Clone)]
pub struct PendingBatch {
    pub blocks: Vec<BatchBlock>,
    pub deposits: Vec<Deposit>,
    pub withdrawals: Vec<Withdrawal>,
    pub processed_deposit_queue_hash: B256,
    pub pending_deposit_queue_hash: B256,
    pub prev_state_root: B256,
    pub expected_withdrawal_queue2: B256,
}
