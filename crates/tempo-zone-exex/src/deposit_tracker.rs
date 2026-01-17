//! Deposit queue tracking for L1-to-L2 deposits.
//!
//! The [`DepositTracker`] maintains the deposit queue hash chain state,
//! tracking which deposits have been processed and which are pending.
//!
//! The deposit queue uses a hash chain pattern:
//! ```text
//! hash = keccak256(abi.encode(deposit, previous_hash))
//! ```

use crate::types::Deposit;
use alloy_primitives::{B256, keccak256};
use alloy_sol_types::SolValue;

/// Tracks the deposit queue state from L1.
///
/// Maintains the hash chain for deposits and tracks which deposits
/// have been processed in proven batches.
#[derive(Debug, Clone)]
pub struct DepositTracker {
    /// Current pending deposit queue hash from L1 events.
    /// This is the head of the queue - the most recent deposit hash.
    pending_deposit_queue_hash: B256,

    /// Last processed deposit queue hash (after batch proven).
    /// This marks where in the queue we've processed up to.
    processed_deposit_queue_hash: B256,

    /// List of unprocessed deposits in order (oldest first).
    unprocessed_deposits: Vec<Deposit>,
}

impl Default for DepositTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl DepositTracker {
    /// Creates a new deposit tracker with empty state.
    pub fn new() -> Self {
        Self {
            pending_deposit_queue_hash: B256::ZERO,
            processed_deposit_queue_hash: B256::ZERO,
            unprocessed_deposits: Vec::new(),
        }
    }

    /// Creates a deposit tracker initialized with known state.
    pub fn with_state(
        pending_deposit_queue_hash: B256,
        processed_deposit_queue_hash: B256,
    ) -> Self {
        Self {
            pending_deposit_queue_hash,
            processed_deposit_queue_hash,
            unprocessed_deposits: Vec::new(),
        }
    }

    /// Returns the current pending deposit queue hash.
    pub fn pending_deposit_queue_hash(&self) -> B256 {
        self.pending_deposit_queue_hash
    }

    /// Returns the last processed deposit queue hash.
    pub fn processed_deposit_queue_hash(&self) -> B256 {
        self.processed_deposit_queue_hash
    }

    /// Returns a slice of unprocessed deposits.
    pub fn unprocessed_deposits(&self) -> &[Deposit] {
        &self.unprocessed_deposits
    }

    /// Returns the number of unprocessed deposits.
    pub fn unprocessed_count(&self) -> usize {
        self.unprocessed_deposits.len()
    }

    /// Adds a new deposit from L1 and updates the pending queue hash.
    pub fn add_deposit(&mut self, deposit: Deposit) {
        let new_hash = compute_deposit_hash(&deposit, self.pending_deposit_queue_hash);
        self.pending_deposit_queue_hash = new_hash;
        self.unprocessed_deposits.push(deposit);
    }

    /// Computes the deposit queue hash chain for a given slice of deposits.
    ///
    /// Starts from `initial_hash` and chains through each deposit in order.
    pub fn compute_queue_hash(deposits: &[Deposit], initial_hash: B256) -> B256 {
        deposits
            .iter()
            .fold(initial_hash, |acc, deposit| compute_deposit_hash(deposit, acc))
    }

    /// Marks deposits as processed after a batch is proven.
    ///
    /// Updates the processed queue hash and removes the processed deposits
    /// from the unprocessed list.
    ///
    /// # Arguments
    /// * `count` - Number of deposits processed in the batch
    /// * `new_processed_hash` - The new processed deposit queue hash after the batch
    pub fn mark_processed(&mut self, count: usize, new_processed_hash: B256) {
        let count = count.min(self.unprocessed_deposits.len());
        self.unprocessed_deposits.drain(..count);
        self.processed_deposit_queue_hash = new_processed_hash;
    }

    /// Computes what the new processed hash would be after processing N deposits.
    ///
    /// Does not modify state - use `mark_processed` to actually commit the change.
    pub fn compute_new_processed_hash(&self, count: usize) -> B256 {
        let count = count.min(self.unprocessed_deposits.len());
        let deposits_to_process = &self.unprocessed_deposits[..count];
        Self::compute_queue_hash(deposits_to_process, self.processed_deposit_queue_hash)
    }

    /// Takes up to `max_count` deposits for inclusion in a batch.
    ///
    /// Returns the deposits and the new processed hash that would result
    /// from processing them.
    pub fn take_for_batch(&self, max_count: usize) -> (Vec<Deposit>, B256) {
        let count = max_count.min(self.unprocessed_deposits.len());
        let deposits = self.unprocessed_deposits[..count].to_vec();
        let new_hash = Self::compute_queue_hash(&deposits, self.processed_deposit_queue_hash);
        (deposits, new_hash)
    }

    /// Resets the tracker to a known state.
    ///
    /// Used when syncing from L1 state or recovering from errors.
    pub fn reset(
        &mut self,
        pending_deposit_queue_hash: B256,
        processed_deposit_queue_hash: B256,
    ) {
        self.pending_deposit_queue_hash = pending_deposit_queue_hash;
        self.processed_deposit_queue_hash = processed_deposit_queue_hash;
        self.unprocessed_deposits.clear();
    }
}

/// Computes the deposit queue hash for a single deposit.
///
/// Uses the hash chain pattern:
/// ```text
/// hash = keccak256(abi.encode(deposit, previous_hash))
/// ```
pub fn compute_deposit_hash(deposit: &Deposit, previous_hash: B256) -> B256 {
    let encoded = (
        deposit.l1_block_hash,
        deposit.l1_block_number,
        deposit.l1_timestamp,
        deposit.sender,
        deposit.to,
        u128::from_le_bytes(deposit.amount.to_le_bytes()),
        deposit.memo,
        previous_hash,
    )
        .abi_encode();

    keccak256(&encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{U128, address};

    fn test_deposit(index: u64) -> Deposit {
        Deposit {
            l1_block_hash: B256::from([index as u8; 32]),
            l1_block_number: 1000 + index,
            l1_timestamp: 1700000000 + index,
            sender: address!("1111111111111111111111111111111111111111"),
            to: address!("2222222222222222222222222222222222222222"),
            amount: U128::from(1_000_000_000_000_000_000u128),
            memo: B256::ZERO,
        }
    }

    #[test]
    fn test_new_tracker_is_empty() {
        let tracker = DepositTracker::new();
        assert_eq!(tracker.pending_deposit_queue_hash(), B256::ZERO);
        assert_eq!(tracker.processed_deposit_queue_hash(), B256::ZERO);
        assert!(tracker.unprocessed_deposits().is_empty());
    }

    #[test]
    fn test_add_deposit_updates_pending_hash() {
        let mut tracker = DepositTracker::new();
        let deposit = test_deposit(1);

        tracker.add_deposit(deposit.clone());

        assert_ne!(tracker.pending_deposit_queue_hash(), B256::ZERO);
        assert_eq!(tracker.unprocessed_count(), 1);

        let expected_hash = compute_deposit_hash(&deposit, B256::ZERO);
        assert_eq!(tracker.pending_deposit_queue_hash(), expected_hash);
    }

    #[test]
    fn test_hash_chain_is_deterministic() {
        let deposit1 = test_deposit(1);
        let deposit2 = test_deposit(2);

        let hash1 = compute_deposit_hash(&deposit1, B256::ZERO);
        let hash2 = compute_deposit_hash(&deposit2, hash1);

        let chain_hash = DepositTracker::compute_queue_hash(&[deposit1, deposit2], B256::ZERO);

        assert_eq!(chain_hash, hash2);
    }

    #[test]
    fn test_mark_processed_updates_state() {
        let mut tracker = DepositTracker::new();
        let deposit1 = test_deposit(1);
        let deposit2 = test_deposit(2);

        tracker.add_deposit(deposit1.clone());
        tracker.add_deposit(deposit2.clone());

        let new_hash = compute_deposit_hash(&deposit1, B256::ZERO);
        tracker.mark_processed(1, new_hash);

        assert_eq!(tracker.processed_deposit_queue_hash(), new_hash);
        assert_eq!(tracker.unprocessed_count(), 1);
    }

    #[test]
    fn test_take_for_batch() {
        let mut tracker = DepositTracker::new();
        tracker.add_deposit(test_deposit(1));
        tracker.add_deposit(test_deposit(2));
        tracker.add_deposit(test_deposit(3));

        let (deposits, new_hash) = tracker.take_for_batch(2);

        assert_eq!(deposits.len(), 2);
        assert_eq!(
            new_hash,
            DepositTracker::compute_queue_hash(&deposits, B256::ZERO)
        );
        assert_eq!(tracker.unprocessed_count(), 3);
    }

    #[test]
    fn test_compute_new_processed_hash() {
        let mut tracker = DepositTracker::new();
        let deposit1 = test_deposit(1);
        let deposit2 = test_deposit(2);

        tracker.add_deposit(deposit1.clone());
        tracker.add_deposit(deposit2.clone());

        let computed = tracker.compute_new_processed_hash(1);
        let expected = compute_deposit_hash(&deposit1, B256::ZERO);

        assert_eq!(computed, expected);
    }
}
