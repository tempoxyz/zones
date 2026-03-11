//! Withdrawal collection store and L1 withdrawal processor for the zone sequencer.
//!
//! This module provides two main components:
//!
//! - [`WithdrawalStore`] — an in-memory store that holds [`abi::Withdrawal`] structs grouped by
//!   batch index. The L1 portal queue only stores hashes, so the sequencer must retain the actual
//!   withdrawal data to provide it when calling `processWithdrawal`.
//!
//! - [`WithdrawalProcessor`] — a background task that polls the ZonePortal withdrawal queue on
//!   **Tempo L1** and processes withdrawals by calling `processWithdrawal(withdrawal, remainingQueue)`.
//!
//! ## Data flow
//!
//! 1. Withdrawal requests originate on the **Zone L2** (`ZoneOutbox.requestWithdrawal`).
//! 2. The sequencer observes `WithdrawalRequested` events and stores the withdrawal data in the
//!    [`WithdrawalStore`].
//! 3. At batch finalization, the sequencer calls `finalizeWithdrawalBatch` on L2, which builds a
//!    hash chain. The proof then enqueues this hash chain into the portal's withdrawal queue on L1.
//! 4. The [`WithdrawalProcessor`] polls the portal queue on L1 and processes each withdrawal by
//!    providing the original data and the remaining queue hash.
//!
//! ## Batch-to-slot mapping
//!
//! The portal's withdrawal queue slots correspond to batch indices. The store's `batch_index`
//! should match the portal slot index. The caller (batch submitter) is responsible for tracking
//! which `batch_index` maps to which portal slot.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use alloy_primitives::{Address, B256};
use alloy_provider::DynProvider;
use parking_lot::Mutex;
use tempo_alloy::TempoNetwork;
use tokio::sync::Notify;
use tracing::{debug, error, info, instrument, warn};

use crate::abi::{self, ZonePortal};

/// Shared handle to the withdrawal store.
#[derive(Clone)]
pub struct SharedWithdrawalStore(Arc<Mutex<WithdrawalStore>>);

impl SharedWithdrawalStore {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(WithdrawalStore::new())))
    }

    pub fn lock(&self) -> parking_lot::MutexGuard<'_, WithdrawalStore> {
        self.0.lock()
    }
}

impl Default for SharedWithdrawalStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Configuration for the withdrawal processor.
#[derive(Debug, Clone)]
pub struct WithdrawalProcessorConfig {
    /// ZonePortal contract address on Tempo L1.
    pub portal_address: Address,
    /// Tempo L1 RPC URL (HTTP).
    pub l1_rpc_url: String,
    /// Fallback timeout for checking the withdrawal queue if no notification arrives.
    pub fallback_poll_interval: Duration,
}

/// In-memory store for withdrawal data grouped by batch index.
///
/// The L1 portal queue only stores hash chains. The sequencer must keep the actual
/// [`abi::Withdrawal`] structs so it can provide them when calling `processWithdrawal`.
///
/// Withdrawals are grouped by batch index, where each batch is a `Vec<Withdrawal>` in FIFO order
/// (oldest first). The batch index corresponds to the portal's withdrawal queue slot index.
pub struct WithdrawalStore {
    batches: BTreeMap<u64, Vec<abi::Withdrawal>>,
}

impl WithdrawalStore {
    pub fn new() -> Self {
        Self {
            batches: BTreeMap::new(),
        }
    }

    /// Add a withdrawal to the given batch.
    ///
    /// Withdrawals within a batch are stored in FIFO order (oldest first).
    pub fn add_withdrawal(&mut self, batch_index: u64, withdrawal: abi::Withdrawal) {
        self.batches
            .entry(batch_index)
            .or_default()
            .push(withdrawal);
    }

    /// Add all withdrawals for a batch at once.
    pub fn add_batch(&mut self, batch_index: u64, withdrawals: Vec<abi::Withdrawal>) {
        self.batches
            .entry(batch_index)
            .or_default()
            .extend(withdrawals);
    }

    /// Get all withdrawals for a batch.
    pub fn get_batch(&self, batch_index: u64) -> Option<&Vec<abi::Withdrawal>> {
        self.batches.get(&batch_index)
    }

    /// Remove a batch after all its withdrawals are processed.
    pub fn remove_batch(&mut self, batch_index: u64) {
        self.batches.remove(&batch_index);
    }

    pub fn has_batch(&self, batch_index: u64) -> bool {
        self.batches.contains_key(&batch_index)
    }

    pub fn batch_count(&self) -> usize {
        self.batches.len()
    }
}

impl Default for WithdrawalStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute the remaining queue hash after removing the first `processed_count` withdrawals.
///
/// This value is passed as `remainingQueue` to `processWithdrawal` on the portal contract.
///
/// - If `processed_count >= withdrawals.len()`, returns `B256::ZERO` (no remaining items).
/// - Otherwise, computes the hash chain over `withdrawals[processed_count..]` via
///   [`abi::Withdrawal::queue_hash`].
pub fn compute_remaining_queue(withdrawals: &[abi::Withdrawal], processed_count: usize) -> B256 {
    if processed_count >= withdrawals.len() {
        return B256::ZERO;
    }

    let remaining = &withdrawals[processed_count..];

    abi::Withdrawal::queue_hash(remaining)
}

// ---------------------------------------------------------------------------
//  Withdrawal processor
// ---------------------------------------------------------------------------

/// Background task that processes withdrawals from the ZonePortal queue on Tempo L1.
///
/// The processor waits for a [`Notify`] signal from the batch submitter (indicating a batch
/// has landed on L1) and then processes the head slot of the portal's withdrawal queue.
/// A fallback timeout ensures the processor still checks periodically if a notification
/// is missed.
///
/// ## POC limitations
///
/// - Transactions are submitted sequentially and the processor waits for each confirmation.
///   On failure, processing stops and remaining withdrawals are retried on the next cycle.
/// - The portal automatically pays the processing fee to the sequencer; this processor does not
///   handle fee accounting.
pub struct WithdrawalProcessor {
    config: WithdrawalProcessorConfig,
    portal: ZonePortal::ZonePortalInstance<DynProvider<TempoNetwork>, TempoNetwork>,
    store: SharedWithdrawalStore,
    notify: Arc<Notify>,
}

impl WithdrawalProcessor {
    /// Create a new withdrawal processor from a shared L1 provider.
    ///
    /// The provider must already include the sequencer wallet for signing.
    pub fn new(
        config: WithdrawalProcessorConfig,
        provider: DynProvider<TempoNetwork>,
        store: SharedWithdrawalStore,
        notify: Arc<Notify>,
    ) -> Self {
        let portal = ZonePortal::new(config.portal_address, provider);

        Self {
            config,
            portal,
            store,
            notify,
        }
    }

    /// Run the processor loop. This method never returns under normal operation.
    ///
    /// Waits for a notification from the batch submitter (or a fallback timeout) before
    /// checking the L1 withdrawal queue.
    #[instrument(skip_all, fields(portal = %self.config.portal_address))]
    pub async fn run(&self) -> eyre::Result<()> {
        info!(l1_rpc = %self.config.l1_rpc_url, "Withdrawal processor started");

        loop {
            tokio::select! {
                _ = self.notify.notified() => {
                    debug!("Woken by batch submission notification");
                }
                _ = tokio::time::sleep(self.config.fallback_poll_interval) => {
                    debug!("Fallback poll interval elapsed");
                }
            }

            if let Err(e) = self.process_queue().await {
                error!(error = %e, "Withdrawal processing cycle failed");
            }
        }
    }

    /// Process the current head slot of the portal's withdrawal queue on Tempo L1.
    #[instrument(skip_all)]
    async fn process_queue(&self) -> eyre::Result<()> {
        let head: alloy_primitives::U256 = self.portal.withdrawalQueueHead().call().await?;
        let tail: alloy_primitives::U256 = self.portal.withdrawalQueueTail().call().await?;

        let head_val: u64 = head.try_into().map_err(|_| eyre::eyre!("head overflow"))?;
        let tail_val: u64 = tail.try_into().map_err(|_| eyre::eyre!("tail overflow"))?;

        if head_val == tail_val {
            debug!("Withdrawal queue empty, nothing to process");
            return Ok(());
        }

        let pending_slots = tail_val - head_val;
        info!(
            head = head_val,
            tail = tail_val,
            pending_slots,
            "Withdrawal queue has pending slots"
        );

        let withdrawals = {
            let store = self.store.lock();
            store.get_batch(head_val).cloned()
        };

        let withdrawals = match withdrawals {
            Some(w) if !w.is_empty() => w,
            _ => {
                warn!(
                    slot = head_val,
                    store_batches = self.store.lock().batch_count(),
                    "No withdrawal data in store for current head slot, waiting for data"
                );
                return Ok(());
            }
        };

        info!(
            slot = head_val,
            count = withdrawals.len(),
            "Processing withdrawal batch"
        );

        for (i, withdrawal) in withdrawals.iter().enumerate() {
            let remaining_queue = compute_remaining_queue(&withdrawals, i + 1);
            let is_last = i + 1 == withdrawals.len();

            info!(
                slot = head_val,
                index = i,
                total = withdrawals.len(),
                token = %withdrawal.token,
                to = %withdrawal.to,
                amount = %withdrawal.amount,
                fee = %withdrawal.fee,
                has_callback = withdrawal.gasLimit > 0,
                is_last,
                "📤 Submitting withdrawal to L1"
            );

            let call = self
                .portal
                .processWithdrawal(withdrawal.clone(), remaining_queue);

            // When the withdrawal has a callback (`gasLimit > 0`), we must
            // override `eth_estimateGas` because the estimate only covers the
            // revert / bounce-back path, which is much cheaper than the happy
            // path where the callback actually executes.
            //
            // The tx gas limit is composed of three parts:
            //
            //   txGas = gasLimit + CALLBACK_OVERHEAD + eip150_cushion
            //
            // 1. `gasLimit`          — gas the user requested for their callback.
            // 2. `CALLBACK_OVERHEAD` — fixed cost for the portal + messenger
            //    logic that runs *around* the callback: queue dequeue & hash
            //    verification, TIP-20 transferFrom (~500k), messenger relay
            //    setup, fee payment, event emission, and the bounce-back path
            //    if the callback reverts.
            // 3. EIP-150 cushion     — the 63/64 forwarding rule means the
            //    caller must hold back 1/64 of remaining gas. To guarantee
            //    the inner CALL receives at least `gasLimit`, the outer frame
            //    needs an extra `ceil(gasLimit / 63)`.
            let call = if withdrawal.gasLimit > 0 {
                const CALLBACK_OVERHEAD: u64 = 2_000_000;
                let eip150_cushion = withdrawal.gasLimit.div_ceil(63);
                call.gas(withdrawal.gasLimit + CALLBACK_OVERHEAD + eip150_cushion)
            } else {
                call
            };

            let tx_result = call.send().await;

            match tx_result {
                Ok(pending) => {
                    let tx_hash = *pending.tx_hash();
                    match pending
                        .with_required_confirmations(1)
                        .with_timeout(Some(std::time::Duration::from_secs(30)))
                        .watch()
                        .await
                    {
                        Ok(_) => {
                            info!(
                                slot = head_val,
                                index = i,
                                %tx_hash,
                                token = %withdrawal.token,
                                to = %withdrawal.to,
                                amount = %withdrawal.amount,
                                "✅ Withdrawal confirmed on L1"
                            );
                        }
                        Err(e) => {
                            error!(
                                slot = head_val,
                                index = i,
                                %tx_hash,
                                to = %withdrawal.to,
                                amount = %withdrawal.amount,
                                error = %e,
                                "processWithdrawal tx not confirmed, stopping batch processing"
                            );
                            return Ok(());
                        }
                    }
                }
                Err(e) => {
                    error!(
                        slot = head_val,
                        index = i,
                        to = %withdrawal.to,
                        amount = %withdrawal.amount,
                        error = %e,
                        "processWithdrawal tx failed to send, stopping batch processing"
                    );
                    return Ok(());
                }
            }
        }

        // All withdrawals in this slot confirmed — safe to remove.
        self.store.lock().remove_batch(head_val);

        info!(
            slot = head_val,
            count = withdrawals.len(),
            "Batch fully processed and removed from store"
        );
        Ok(())
    }
}

/// Spawn the withdrawal processor as a background task.
///
/// The processor waits for notifications from the batch submitter (via `notify`) and then
/// processes withdrawals from the ZonePortal queue on Tempo L1.
///
/// The `provider` must already include the sequencer wallet for signing L1 transactions.
pub fn spawn_withdrawal_processor(
    config: WithdrawalProcessorConfig,
    provider: DynProvider<TempoNetwork>,
    store: SharedWithdrawalStore,
    notify: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let processor = WithdrawalProcessor::new(config, provider, store, notify);
        loop {
            if let Err(e) = processor.run().await {
                error!(error = %e, "Withdrawal processor failed, restarting in 5s");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::EMPTY_SENTINEL;
    use alloy_primitives::{address, keccak256};
    use alloy_sol_types::SolValue;

    fn test_withdrawal(to: Address, amount: u128) -> abi::Withdrawal {
        abi::Withdrawal {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to,
            amount,
            fee: 0,
            memo: B256::ZERO,
            gasLimit: 0,
            fallbackRecipient: to,
            callbackData: Default::default(),
        }
    }

    #[test]
    fn empty_queue_hash_is_zero() {
        assert_eq!(abi::Withdrawal::queue_hash(&[]), B256::ZERO);
    }

    #[test]
    fn single_withdrawal_queue_hash() {
        let w = test_withdrawal(address!("0x0000000000000000000000000000000000000042"), 1000);
        let hash = abi::Withdrawal::queue_hash(std::slice::from_ref(&w));

        let expected = keccak256((w, EMPTY_SENTINEL).abi_encode());
        assert_eq!(hash, expected);
    }

    #[test]
    fn two_withdrawal_queue_hash() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000042"), 100);
        let w1 = test_withdrawal(address!("0x0000000000000000000000000000000000000043"), 200);

        let hash = abi::Withdrawal::queue_hash(&[w0.clone(), w1.clone()]);

        let inner = keccak256((w1, EMPTY_SENTINEL).abi_encode());
        let expected = keccak256((w0, inner).abi_encode());
        assert_eq!(hash, expected);
    }

    #[test]
    fn remaining_queue_single_item_is_hash() {
        let w = test_withdrawal(address!("0x0000000000000000000000000000000000000042"), 1000);
        let expected = abi::Withdrawal::queue_hash(std::slice::from_ref(&w));
        assert_eq!(compute_remaining_queue(&[w], 0), expected);
    }

    #[test]
    fn remaining_queue_all_consumed() {
        let w = test_withdrawal(address!("0x0000000000000000000000000000000000000042"), 1000);
        assert_eq!(
            compute_remaining_queue(std::slice::from_ref(&w), 1),
            B256::ZERO
        );
        assert_eq!(compute_remaining_queue(&[w], 5), B256::ZERO);
    }

    #[test]
    fn remaining_queue_partial() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000042"), 100);
        let w1 = test_withdrawal(address!("0x0000000000000000000000000000000000000043"), 200);
        let w2 = test_withdrawal(address!("0x0000000000000000000000000000000000000044"), 300);

        let remaining = compute_remaining_queue(&[w0, w1.clone(), w2.clone()], 1);
        let expected = abi::Withdrawal::queue_hash(&[w1, w2]);
        assert_eq!(remaining, expected);
    }

    #[test]
    fn store_operations() {
        let mut store = WithdrawalStore::new();
        let w = test_withdrawal(address!("0x0000000000000000000000000000000000000042"), 100);

        assert_eq!(store.batch_count(), 0);
        assert!(!store.has_batch(0));

        store.add_withdrawal(0, w.clone());
        assert!(store.has_batch(0));
        assert_eq!(store.batch_count(), 1);
        assert_eq!(store.get_batch(0).unwrap().len(), 1);

        store.add_withdrawal(0, w);
        assert_eq!(store.get_batch(0).unwrap().len(), 2);

        store.remove_batch(0);
        assert!(!store.has_batch(0));
        assert_eq!(store.batch_count(), 0);
    }

    #[test]
    fn store_slot_index_must_match_portal_tail() {
        // Demonstrates that withdrawals must be stored under the portal's actual
        // queue tail index. If the monitor starts with tail=0 but the portal is
        // at tail=5, withdrawals end up in slot 0 while the withdrawal processor
        // looks for them in slot 5.
        let mut store = WithdrawalStore::new();
        let w = test_withdrawal(address!("0x0000000000000000000000000000000000000042"), 100);

        // Simulate storing under the wrong slot (tail=0 when portal is at 5).
        store.add_withdrawal(0, w.clone());
        assert!(store.has_batch(0));
        assert!(
            !store.has_batch(5),
            "withdrawal processor would look at slot 5 and find nothing"
        );

        // Correct: store under the portal's actual tail.
        let portal_tail = 5u64;
        store.add_withdrawal(portal_tail, w);
        assert!(store.has_batch(portal_tail));
    }

    #[test]
    fn store_add_batch() {
        let mut store = WithdrawalStore::new();
        let addr = address!("0x0000000000000000000000000000000000000042");
        let batch: Vec<_> = (0..3).map(|i| test_withdrawal(addr, i * 100)).collect();

        store.add_batch(0, batch);
        assert!(store.has_batch(0));
        assert_eq!(store.get_batch(0).unwrap().len(), 3);

        let more: Vec<_> = (0..2).map(|i| test_withdrawal(addr, i * 200)).collect();
        store.add_batch(0, more);
        assert_eq!(store.get_batch(0).unwrap().len(), 5);

        store.add_batch(1, vec![test_withdrawal(addr, 999)]);
        assert_eq!(store.batch_count(), 2);
    }
}
