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

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_network::ReceiptResponse;
use alloy_primitives::{Address, B256};
use alloy_provider::DynProvider;
use parking_lot::Mutex;
use tempo_alloy::TempoNetwork;
use tokio::sync::Notify;
use tracing::{debug, error, info, instrument, warn};

use crate::{
    abi::{self, MAX_WITHDRAWAL_GAS_LIMIT, ZonePortal},
    metrics::WithdrawalProcessorMetrics,
    nonce_keys::PROCESS_WITHDRAWAL_NONCE_KEY,
};
use tempo_alloy::rpc::TempoCallBuilderExt;

const PROCESS_WITHDRAWAL_CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);
const PROCESS_WITHDRAWAL_CALLBACK_OVERHEAD_GAS: u64 = 2_000_000;
#[cfg(test)]
const MAX_PROCESS_WITHDRAWAL_TX_GAS: u64 =
    process_withdrawal_tx_gas_limit(MAX_WITHDRAWAL_GAS_LIMIT);

const fn eip150_cushion(gas_limit: u64) -> u64 {
    gas_limit / 63 + if gas_limit.is_multiple_of(63) { 0 } else { 1 }
}

const fn process_withdrawal_tx_gas_limit(callback_gas_limit: u64) -> u64 {
    let bounded_callback_gas = if callback_gas_limit > MAX_WITHDRAWAL_GAS_LIMIT {
        MAX_WITHDRAWAL_GAS_LIMIT
    } else {
        callback_gas_limit
    };

    bounded_callback_gas
        + PROCESS_WITHDRAWAL_CALLBACK_OVERHEAD_GAS
        + eip150_cushion(bounded_callback_gas)
}

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

    /// Set all withdrawals for a batch at once, replacing any existing data.
    pub fn add_batch(&mut self, batch_index: u64, withdrawals: Vec<abi::Withdrawal>) {
        self.batches.insert(batch_index, withdrawals);
    }

    /// Replace the entire store with an authoritative set of pending batches.
    pub(crate) fn replace_batches(&mut self, batches: BTreeMap<u64, Vec<abi::Withdrawal>>) {
        self.batches = batches;
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

    /// Return the smallest and largest portal slot indices currently present.
    fn slot_range(&self) -> Option<(u64, u64)> {
        let first = *self.batches.keys().next()?;
        let last = *self.batches.keys().next_back()?;
        Some((first, last))
    }

    /// Return a compact summary of the store as `(batch_count, first_slot, last_slot)`.
    pub(crate) fn summary(&self) -> (usize, Option<u64>, Option<u64>) {
        let (first_slot, last_slot) = self
            .slot_range()
            .map_or((None, None), |(first, last)| (Some(first), Some(last)));
        (self.batch_count(), first_slot, last_slot)
    }

    /// Return the nearest populated slots before and after `slot`, if any exist.
    fn neighboring_slots(&self, slot: u64) -> (Option<u64>, Option<u64>) {
        let prev = self.batches.range(..slot).next_back().map(|(&idx, _)| idx);
        let next = self
            .batches
            .range(slot.saturating_add(1)..)
            .next()
            .map(|(&idx, _)| idx);
        (prev, next)
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

struct StoreSnapshot {
    batch_count: usize,
    first_slot: Option<u64>,
    last_slot: Option<u64>,
    prev_slot: Option<u64>,
    next_slot: Option<u64>,
    withdrawals: Option<Vec<abi::Withdrawal>>,
}

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
    repair_notify: Arc<Notify>,
    metrics: WithdrawalProcessorMetrics,
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
        repair_notify: Arc<Notify>,
    ) -> Self {
        let portal = ZonePortal::new(config.portal_address, provider);

        Self {
            config,
            portal,
            store,
            notify,
            repair_notify,
            metrics: WithdrawalProcessorMetrics::default(),
        }
    }

    /// Read the current store contents relevant to `slot` under a single lock.
    ///
    /// This keeps the diagnostic fields used in missing-slot logs consistent
    /// with each other and with the batch lookup result.
    fn capture_store_snapshot(&self, slot: u64) -> StoreSnapshot {
        let store = self.store.lock();
        let (batch_count, first_slot, last_slot) = store.summary();
        let (prev_slot, next_slot) = store.neighboring_slots(slot);

        StoreSnapshot {
            batch_count,
            first_slot,
            last_slot,
            prev_slot,
            next_slot,
            withdrawals: store.get_batch(slot).cloned(),
        }
    }

    /// Run the processor loop. This method never returns under normal operation.
    ///
    /// Waits for a notification from the batch submitter (or a fallback timeout) before
    /// checking the L1 withdrawal queue.
    #[instrument(skip_all, fields(portal = %self.config.portal_address))]
    pub async fn run(&mut self) -> eyre::Result<()> {
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
    async fn process_queue(&mut self) -> eyre::Result<()> {
        let head_call = self.portal.withdrawalQueueHead();
        let tail_call = self.portal.withdrawalQueueTail();
        let (head, tail): (alloy_primitives::U256, alloy_primitives::U256) =
            tokio::try_join!(head_call.call(), tail_call.call())?;

        let head_val: u64 = head.try_into().map_err(|_| eyre::eyre!("head overflow"))?;
        let tail_val: u64 = tail.try_into().map_err(|_| eyre::eyre!("tail overflow"))?;
        let StoreSnapshot {
            batch_count: store_batch_count,
            first_slot: store_first_slot,
            last_slot: store_last_slot,
            prev_slot: prev_store_slot,
            next_slot: next_store_slot,
            withdrawals,
        } = self.capture_store_snapshot(head_val);
        self.record_queue_metrics(head_val, tail_val, store_batch_count);

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

        let withdrawals = match withdrawals {
            Some(w) if !w.is_empty() => w,
            _ => {
                self.repair_notify.notify_one();
                warn!(
                    slot = head_val,
                    tail = tail_val,
                    pending_slots,
                    store_batches = store_batch_count,
                    store_first_slot,
                    store_last_slot,
                    prev_store_slot,
                    next_store_slot,
                    "No withdrawal data in store for current head slot"
                );
                return Ok(());
            }
        };

        info!(
            slot = head_val,
            count = withdrawals.len(),
            "Processing withdrawal batch"
        );
        let slot_started_at = Instant::now();
        let slot_queue_hash = abi::Withdrawal::queue_hash(&withdrawals);

        for (i, withdrawal) in withdrawals.iter().enumerate() {
            self.metrics.withdrawals_processed_total.increment(1);
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
                .processWithdrawal(withdrawal.clone(), remaining_queue)
                .nonce_key(PROCESS_WITHDRAWAL_NONCE_KEY);

            // When the withdrawal has a callback (`gasLimit > 0`), we must
            // override `eth_estimateGas` because the estimate only covers the
            // revert / bounce-back path, which is much cheaper than the happy
            // path where the callback actually executes.
            //
            // The tx gas limit is composed of three parts:
            //
            //   txGas = min(gasLimit, MAX_WITHDRAWAL_GAS_LIMIT)
            //         + CALLBACK_OVERHEAD
            //         + eip150_cushion
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
            //    needs an extra `ceil(bounded_callback_gas / 63)`.
            //
            // `MAX_WITHDRAWAL_GAS_LIMIT` mirrors the contract-level cap. It
            // also bounds legacy over-cap withdrawals so RPC nodes do not
            // reject the transaction before the portal can dequeue and
            // bounce them back.
            let call = if withdrawal.gasLimit > 0 {
                let tx_gas_limit = process_withdrawal_tx_gas_limit(withdrawal.gasLimit);
                if withdrawal.gasLimit > MAX_WITHDRAWAL_GAS_LIMIT {
                    warn!(
                        slot = head_val,
                        index = i,
                        requested_gas_limit = withdrawal.gasLimit,
                        max_gas_limit = MAX_WITHDRAWAL_GAS_LIMIT,
                        tx_gas_limit,
                        "withdrawal callback gas exceeds protocol cap; submitting bounded tx"
                    );
                }
                call.gas(tx_gas_limit)
            } else {
                call
            };

            let tx_result = call.send().await;

            match tx_result {
                Ok(pending) => {
                    let tx_hash = *pending.tx_hash();
                    match pending
                        .with_timeout(Some(PROCESS_WITHDRAWAL_CONFIRM_TIMEOUT))
                        .get_receipt()
                        .await
                    {
                        Ok(receipt) => {
                            if !receipt.status() {
                                self.metrics.withdrawals_failed_total.increment(1);
                                self.metrics.withdrawals_reverted_total.increment(1);
                                self.record_slot_duration(slot_started_at.elapsed());
                                self.repair_notify.notify_one();

                                error!(
                                    slot = head_val,
                                    index = i,
                                    %tx_hash,
                                    to = %withdrawal.to,
                                    amount = %withdrawal.amount,
                                    queue_head = head_val,
                                    queue_tail = tail_val,
                                    expected_slot_queue_hash = %slot_queue_hash,
                                    expected_remaining_queue = %remaining_queue,
                                    "processWithdrawal tx was included but reverted; keeping batch in store and requesting repair"
                                );
                                return Ok(());
                            }

                            self.metrics.withdrawals_confirmed_total.increment(1);
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
                            self.metrics.withdrawals_failed_total.increment(1);
                            self.record_slot_duration(slot_started_at.elapsed());
                            error!(
                                slot = head_val,
                                index = i,
                                %tx_hash,
                                expected_slot_queue_hash = %slot_queue_hash,
                                expected_remaining_queue = %remaining_queue,
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
                    self.metrics.withdrawals_failed_total.increment(1);
                    self.record_slot_duration(slot_started_at.elapsed());
                    error!(
                        slot = head_val,
                        index = i,
                        expected_slot_queue_hash = %slot_queue_hash,
                        expected_remaining_queue = %remaining_queue,
                        to = %withdrawal.to,
                        amount = %withdrawal.amount,
                        error = %e,
                        "processWithdrawal tx failed to send, stopping batch processing"
                    );
                    return Ok(());
                }
            }
        }
        self.record_slot_duration(slot_started_at.elapsed());

        // All withdrawals in this slot confirmed — safe to remove.
        self.store.lock().remove_batch(head_val);

        info!(
            slot = head_val,
            count = withdrawals.len(),
            "Batch fully processed and removed from store"
        );
        Ok(())
    }

    fn record_queue_metrics(&mut self, head: u64, tail: u64, store_batch_count: usize) {
        self.metrics.portal_queue_head.set(head as f64);
        self.metrics.portal_queue_tail.set(tail as f64);
        self.metrics
            .portal_queue_pending_slots
            .set((tail.saturating_sub(head)) as f64);
        self.metrics.store_batch_count.set(store_batch_count as f64);
    }

    fn record_slot_duration(&self, duration: Duration) {
        self.metrics
            .slot_processing_duration_seconds
            .record(duration.as_secs_f64());
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
    repair_notify: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut processor =
            WithdrawalProcessor::new(config, provider, store, notify, repair_notify);
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
    use alloy_primitives::{Bytes, U256, address, keccak256};
    use alloy_provider::{Provider, ProviderBuilder};
    use alloy_sol_types::SolValue;
    use alloy_transport::mock::Asserter;
    use tempo_alloy::TempoNetwork;
    use tokio::time::timeout;

    fn mock_provider(asserter: Asserter) -> DynProvider<TempoNetwork> {
        ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter)
            .erased()
    }

    fn abi_encode_u64(value: u64) -> Bytes {
        Bytes::copy_from_slice(&U256::from(value).to_be_bytes::<32>())
    }

    fn test_withdrawal(to: Address, amount: u128) -> abi::Withdrawal {
        abi::Withdrawal {
            token: address!("0x0000000000000000000000000000000000001000"),
            senderTag: B256::repeat_byte(0x11),
            to,
            amount,
            fee: 0,
            memo: B256::ZERO,
            gasLimit: 0,
            fallbackRecipient: to,
            callbackData: Default::default(),
            encryptedSender: Default::default(),
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

        let expected = keccak256((w, EMPTY_SENTINEL).abi_encode_params());
        assert_eq!(hash, expected);
    }

    #[test]
    fn two_withdrawal_queue_hash() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000042"), 100);
        let w1 = test_withdrawal(address!("0x0000000000000000000000000000000000000043"), 200);

        let hash = abi::Withdrawal::queue_hash(&[w0.clone(), w1.clone()]);

        let inner = keccak256((w1, EMPTY_SENTINEL).abi_encode_params());
        let expected = keccak256((w0, inner).abi_encode_params());
        assert_eq!(hash, expected);
    }

    #[test]
    fn withdrawal_hash_requires_param_encoding() {
        let w = abi::Withdrawal {
            token: address!("0x20c0000000000000000000000000000000000000"),
            senderTag: B256::repeat_byte(0x22),
            to: address!("0x70997970c51812dc3a010c7d01b50e0d17dc79c8"),
            amount: 500_000,
            fee: 0,
            memo: B256::ZERO,
            gasLimit: 0,
            fallbackRecipient: address!("0x70997970c51812dc3a010c7d01b50e0d17dc79c8"),
            callbackData: Default::default(),
            encryptedSender: Default::default(),
        };

        let tuple_value_hash = keccak256((w.clone(), EMPTY_SENTINEL).abi_encode());
        let param_hash = keccak256((w, EMPTY_SENTINEL).abi_encode_params());

        assert_ne!(
            tuple_value_hash, param_hash,
            "tuple-value encoding must differ from Solidity abi.encode(args...) here"
        );
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
    fn callback_tx_gas_limit_is_capped_below_l1_block_limit() {
        let at_cap = process_withdrawal_tx_gas_limit(MAX_WITHDRAWAL_GAS_LIMIT);
        let over_cap = process_withdrawal_tx_gas_limit(MAX_WITHDRAWAL_GAS_LIMIT + 1);

        assert_eq!(over_cap, at_cap);
        assert_eq!(at_cap, MAX_PROCESS_WITHDRAWAL_TX_GAS);
        assert_eq!(
            at_cap,
            MAX_WITHDRAWAL_GAS_LIMIT
                + PROCESS_WITHDRAWAL_CALLBACK_OVERHEAD_GAS
                + MAX_WITHDRAWAL_GAS_LIMIT.div_ceil(63)
        );
        assert!(at_cap < 30_000_000);
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

        // Calling add_batch again replaces existing data (idempotent).
        let more: Vec<_> = (0..2).map(|i| test_withdrawal(addr, i * 200)).collect();
        store.add_batch(0, more);
        assert_eq!(store.get_batch(0).unwrap().len(), 2);

        store.add_batch(1, vec![test_withdrawal(addr, 999)]);
        assert_eq!(store.batch_count(), 2);
    }

    #[test]
    fn store_replace_batches_reconciles_authoritative_view() {
        let mut store = WithdrawalStore::new();
        let addr = address!("0x0000000000000000000000000000000000000042");

        store.add_batch(0, vec![test_withdrawal(addr, 100)]);
        store.add_batch(9, vec![test_withdrawal(addr, 900)]);

        let mut reconciled = BTreeMap::new();
        reconciled.insert(5, vec![test_withdrawal(addr, 500)]);
        reconciled.insert(6, vec![test_withdrawal(addr, 600)]);

        store.replace_batches(reconciled);

        assert!(!store.has_batch(0));
        assert!(!store.has_batch(9));
        assert!(store.has_batch(5));
        assert!(store.has_batch(6));
        assert_eq!(store.batch_count(), 2);
    }

    #[tokio::test]
    async fn process_queue_requests_monitor_resync_when_head_slot_missing() {
        let l1 = Asserter::new();
        l1.push_success(&abi_encode_u64(51));
        l1.push_success(&abi_encode_u64(71));

        let config = WithdrawalProcessorConfig {
            portal_address: address!("0x7069DeC4E64Fd07334A0933eDe836C17259c9B23"),
            l1_rpc_url: "http://unused.test".to_string(),
            fallback_poll_interval: Duration::from_secs(1),
        };
        let notify = Arc::new(Notify::new());
        let repair_notify = Arc::new(Notify::new());
        let mut processor = WithdrawalProcessor::new(
            config,
            mock_provider(l1.clone()),
            SharedWithdrawalStore::new(),
            notify,
            repair_notify.clone(),
        );

        processor.process_queue().await.unwrap();

        timeout(Duration::from_millis(50), repair_notify.notified())
            .await
            .expect("missing head slot should request a monitor resync");
        assert!(l1.read_q().is_empty());
    }
}
