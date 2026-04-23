//! Deposit queue with hash chain tracking and notification support.

use alloy_consensus::BlockHeader as _;
use alloy_eips::NumHash;
use alloy_primitives::B256;
use parking_lot::Mutex;
use reth_primitives_traits::SealedHeader;
use std::sync::Arc;
use tempo_primitives::TempoHeader;

use super::deposits::L1BlockDeposits;
#[cfg(test)]
use super::deposits::L1Deposit;
use super::{EnqueueOutcome, L1PortalEvents};
use crate::l1_state::tip403::PolicyEvent;

/// Deposit queue hash chain state.
///
/// Tracks deposits grouped by L1 block and maintains the hash chain:
/// `newHash = keccak256(abi.encode(deposit, prevHash))`
///
/// This mirrors the L1 portal's `currentDepositQueueHash`.
#[derive(Debug)]
pub(crate) struct PendingDeposits {
    /// Deposit hash chain value after the last block consumed by the engine
    /// via `confirm` or `drain`. Serves as the rollback anchor when purging
    /// blocks during a reorg.
    pub(crate) processed_head_hash: B256,
    /// Deposit hash chain value after applying all pending deposits. Advances
    /// as new deposits are enqueued and rolls back to `processed_head_hash`
    /// when blocks are purged.
    pub(crate) enqueued_head_hash: B256,
    /// Pending L1 blocks with their deposits, not yet processed by the zone
    pending: Vec<L1BlockDeposits>,
    /// Highest L1 block ever enqueued (number + hash). Survives `confirm` /
    /// `drain` so that reconnecting subscribers know where the queue left off,
    /// even if the engine has already consumed the blocks.
    last_enqueued: Option<NumHash>,
    /// Last L1 block consumed by the engine via `confirm` or `drain`.
    /// Used as a floor during reorg backfill to prevent re-offering blocks
    /// the zone has already built into zone blocks.
    last_processed: Option<NumHash>,
}

impl Default for PendingDeposits {
    fn default() -> Self {
        Self {
            processed_head_hash: B256::ZERO,
            enqueued_head_hash: B256::ZERO,
            pending: Vec::new(),
            last_enqueued: None,
            last_processed: None,
        }
    }
}

impl PendingDeposits {
    /// Try to enqueue an L1 block, enforcing the chain continuity invariant:
    /// pending blocks must form a contiguous chain where each block's number is
    /// exactly `prev + 1` and its `parent_hash` matches the previous block's hash.
    ///
    /// Returns:
    /// - [`EnqueueOutcome::Accepted`] — block extends the chain and was appended.
    /// - [`EnqueueOutcome::Duplicate`] — block is already present or behind our
    ///   window; safe to ignore.
    /// - [`EnqueueOutcome::NeedBackfill`] — block doesn't connect due to a gap
    ///   or parent hash mismatch. The caller must fetch and enqueue the missing
    ///   range `from..=to`, then retry this block.
    pub(crate) fn try_enqueue(
        &mut self,
        header: SealedHeader<TempoHeader>,
        events: L1PortalEvents,
        policy_events: Vec<PolicyEvent>,
    ) -> EnqueueOutcome {
        let block_number = header.number();
        let block_hash = header.hash();
        let parent_hash = header.parent_hash();

        // Check if we already have this block number
        if let Some(idx) = self
            .pending
            .iter()
            .position(|e| e.header.number() == block_number)
        {
            if self.pending[idx].header.hash() == block_hash {
                return EnqueueOutcome::Duplicate;
            }
            // Different hash at same height — purge from this point (reorg)
            self.purge_from(idx);
            // Fall through to try appending
        }

        // If queue is empty, check against `last_enqueued` to detect gaps
        // from blocks that were already consumed by the engine.
        if self.pending.is_empty() {
            if let Some(last) = self.last_enqueued {
                if block_number <= last.number {
                    return EnqueueOutcome::Duplicate;
                }
                if block_number > last.number + 1 {
                    return EnqueueOutcome::NeedBackfill {
                        from: last.number + 1,
                        to: block_number - 1,
                    };
                }
                // block_number == last.number + 1 — verify parent connects.
                // If it doesn't, the consumed block was reorged — clear
                // `last_enqueued` so backfill can re-enqueue that block number
                // with its new (post-reorg) hash.
                if parent_hash != last.hash {
                    self.last_enqueued = None;
                    return EnqueueOutcome::NeedBackfill {
                        from: last.number,
                        to: block_number - 1,
                    };
                }
            } else if let Some(processed) = self.last_processed {
                // last_enqueued was cleared (reorg of consumed block) but we
                // know what the engine already consumed. Use it as a floor to
                // prevent re-offering blocks the zone already built on.
                if block_number <= processed.number {
                    return EnqueueOutcome::Duplicate;
                }
                if block_number > processed.number + 1 {
                    return EnqueueOutcome::NeedBackfill {
                        from: processed.number + 1,
                        to: block_number - 1,
                    };
                }
                // Immediate successor of a consumed block. Accept regardless
                // of parent hash — the parent was reorged but the zone already
                // committed to it. The builder will detect the hash mismatch.
            }
            self.append(header, events, policy_events);
            return EnqueueOutcome::Accepted;
        }

        let tail = self.pending.last().unwrap();
        let tail_number = tail.header.number();
        let tail_hash = tail.header.hash();

        // Block is behind our window
        if block_number <= tail_number {
            return EnqueueOutcome::Duplicate;
        }

        // Gap — need backfill
        let expected = tail_number + 1;
        if block_number > expected {
            return EnqueueOutcome::NeedBackfill {
                from: expected,
                to: block_number - 1,
            };
        }

        // block_number == expected — check parent connects
        if parent_hash != tail_hash {
            // Parent mismatch — purge tail (it was reorged) and request backfill
            let purge_number = tail_number;
            if let Some(idx) = self
                .pending
                .iter()
                .position(|e| e.header.number() == purge_number)
            {
                self.purge_from(idx);
            }
            let new_expected = self
                .pending
                .last()
                .map(|e| e.header.number() + 1)
                .unwrap_or(block_number);
            if new_expected < block_number {
                return EnqueueOutcome::NeedBackfill {
                    from: new_expected,
                    to: block_number - 1,
                };
            }
            // If new_expected == block_number, fall through to accept (it'll be the anchor)
        }

        self.append(header, events, policy_events);
        EnqueueOutcome::Accepted
    }

    /// Enqueue a block during backfill. Accepts or skips duplicates.
    ///
    /// Panics on `NeedBackfill` — backfill blocks must be fetched sequentially.
    pub(crate) fn enqueue(
        &mut self,
        header: TempoHeader,
        events: L1PortalEvents,
        policy_events: Vec<PolicyEvent>,
    ) {
        match self.try_enqueue(SealedHeader::seal_slow(header), events, policy_events) {
            EnqueueOutcome::Accepted | EnqueueOutcome::Duplicate => {}
            other => panic!("enqueue expected Accepted or Duplicate, got {other:?}"),
        }
    }

    fn append(
        &mut self,
        header: SealedHeader<TempoHeader>,
        events: L1PortalEvents,
        policy_events: Vec<PolicyEvent>,
    ) {
        let queue_hash_before = self.enqueued_head_hash;
        for deposit in &events.deposits {
            self.enqueued_head_hash = deposit.hash_chain(self.enqueued_head_hash);
        }
        let queue_hash_after = self.enqueued_head_hash;
        self.last_enqueued = Some(header.num_hash());
        self.pending.push(L1BlockDeposits {
            header,
            events,
            policy_events,
            queue_hash_before,
            queue_hash_after,
        });
    }

    fn purge_from(&mut self, idx: usize) {
        if idx == 0 {
            self.enqueued_head_hash = self.processed_head_hash;
            self.pending.clear();
        } else {
            self.enqueued_head_hash = self.pending[idx - 1].queue_hash_after;
            self.pending.truncate(idx);
        }
        self.last_enqueued = self.pending.last().map(|e| e.header.num_hash());
    }

    /// Peek at the next pending L1 block without removing it.
    ///
    /// Returns `None` if no L1 blocks are queued. Use [`confirm`](Self::confirm)
    /// after a successful build to advance the queue.
    pub(crate) fn peek(&self) -> Option<&L1BlockDeposits> {
        self.pending.first()
    }

    /// Confirm the next pending L1 block was successfully processed and remove it.
    ///
    /// The caller must pass the [`NumHash`] of the block being confirmed. If the
    /// front of the queue no longer matches (e.g. because a reorg purged it
    /// between `peek` and `confirm`), this returns `None` and the queue is left
    /// unchanged.
    ///
    /// Must be called after a successful payload build + newPayload acceptance.
    /// Advances `processed_head_hash` and `last_processed`.
    pub(crate) fn confirm(&mut self, expected: NumHash) -> Option<L1BlockDeposits> {
        let front = self.pending.first()?;
        if front.header.num_hash() != expected {
            return None;
        }
        let block = self.pending.remove(0);
        self.processed_head_hash = block.queue_hash_after;
        // Keep last_processed monotonic — never regress the floor.
        let num_hash = block.header.num_hash();
        if self
            .last_processed
            .is_none_or(|lp| num_hash.number > lp.number)
        {
            self.last_processed = Some(num_hash);
        }
        Some(block)
    }

    /// Drain all pending L1 block deposits.
    #[cfg(test)]
    pub(crate) fn drain(&mut self) -> Vec<L1BlockDeposits> {
        if let Some(last) = self.pending.last() {
            self.processed_head_hash = last.queue_hash_after;
            // Keep last_processed monotonic — never regress the floor.
            let num_hash = last.header.num_hash();
            if self
                .last_processed
                .is_none_or(|lp| num_hash.number > lp.number)
            {
                self.last_processed = Some(num_hash);
            }
        }
        std::mem::take(&mut self.pending)
    }

    /// Compute a [`DepositQueueTransition`] for a batch of deposits starting from `prev_hash`
    #[cfg(test)]
    pub(crate) fn transition(prev_hash: B256, deposits: &[L1Deposit]) -> DepositQueueTransition {
        let mut current = prev_hash;
        for d in deposits {
            current = d.hash_chain(current);
        }
        DepositQueueTransition {
            prev_processed_hash: prev_hash,
            next_processed_hash: current,
        }
    }

    /// Returns the deposit hash chain value after all enqueued deposits.
    #[cfg(test)]
    pub(crate) fn enqueued_head_hash(&self) -> B256 {
        self.enqueued_head_hash
    }

    /// Returns the number of pending L1 blocks.
    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Returns a reference to the pending block at the given index.
    #[cfg(test)]
    pub(crate) fn pending_block(&self, idx: usize) -> Option<&L1BlockDeposits> {
        self.pending.get(idx)
    }

    /// Returns a slice of all pending L1 block deposits.
    #[cfg(test)]
    pub(crate) fn pending_blocks(&self) -> &[L1BlockDeposits] {
        &self.pending
    }

    /// Returns the most recently enqueued L1 block (number + hash), if any.
    pub(crate) fn last_enqueued(&self) -> Option<NumHash> {
        self.last_enqueued
    }

    /// Clears the `last_enqueued` anchor. Used when a reorg invalidates
    /// the consumed block that `last_enqueued` pointed to.
    #[cfg(test)]
    pub(crate) fn clear_last_enqueued(&mut self) {
        self.last_enqueued = None;
    }

    /// Returns the last L1 block consumed by the engine.
    #[cfg(test)]
    pub(crate) fn last_processed(&self) -> Option<NumHash> {
        self.last_processed
    }
}

/// Deposit queue transition for batch proof validation.
///
/// Represents the state of the deposit hash chain for a batch
/// of deposits processed by the zone. Used to prove which deposits were
/// included in a block.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub(crate) struct DepositQueueTransition {
    /// Hash chain head before the batch is processed
    pub(crate) prev_processed_hash: B256,
    /// Hash chain head after the batch is processed
    pub(crate) next_processed_hash: B256,
}

/// Shared deposit queue with notification support.
///
/// Wraps the pending deposits with a `Notify` so the ZoneEngine can be
/// woken instantly when new L1 blocks arrive.
#[derive(Debug, Clone)]
pub struct DepositQueue {
    inner: Arc<Mutex<PendingDeposits>>,
    notify: Arc<tokio::sync::Notify>,
}

impl DepositQueue {
    /// Create a new empty deposit queue.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(PendingDeposits::default())),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Try to enqueue an L1 block. Returns the outcome — callers handle
    /// `NeedBackfill` by fetching missing blocks and retrying.
    pub(crate) fn try_enqueue(
        &self,
        header: SealedHeader<TempoHeader>,
        events: L1PortalEvents,
        policy_events: Vec<PolicyEvent>,
    ) -> EnqueueOutcome {
        let mut queue = self.inner.lock();
        let outcome = queue.try_enqueue(header, events, policy_events);
        if matches!(outcome, EnqueueOutcome::Accepted) {
            drop(queue);
            self.notify.notify_one();
        }
        outcome
    }

    /// Enqueue an L1 block with its deposits and notify waiters.
    pub fn enqueue(
        &self,
        header: TempoHeader,
        events: L1PortalEvents,
        policy_events: Vec<PolicyEvent>,
    ) {
        self.inner.lock().enqueue(header, events, policy_events);
        self.notify.notify_one();
    }

    /// Like [`enqueue`](Self::enqueue) but accepts an already-sealed header,
    /// avoiding a redundant hash computation.
    pub fn enqueue_sealed(
        &self,
        header: SealedHeader<TempoHeader>,
        events: L1PortalEvents,
        policy_events: Vec<PolicyEvent>,
    ) {
        let mut queue = self.inner.lock();
        match queue.try_enqueue(header, events, policy_events) {
            EnqueueOutcome::Accepted | EnqueueOutcome::Duplicate => {}
            other => panic!("enqueue_sealed expected Accepted or Duplicate, got {other:?}"),
        }
        drop(queue);
        self.notify.notify_one();
    }

    /// Peek at the next L1 block without removing it.
    pub fn peek(&self) -> Option<L1BlockDeposits> {
        self.inner.lock().peek().cloned()
    }

    /// Confirm the next L1 block was successfully processed and remove it.
    ///
    /// Returns `None` if the front of the queue no longer matches `expected`
    /// (e.g. a reorg purged it between `peek` and `confirm`).
    pub fn confirm(&self, expected: NumHash) -> Option<L1BlockDeposits> {
        self.inner.lock().confirm(expected)
    }

    /// Wait until an L1 block is available.
    pub async fn notified(&self) {
        self.notify.notified().await
    }

    /// Returns the most recently enqueued L1 block (number + hash), if any.
    ///
    /// This is a high-water mark that survives `confirm` / `drain`, so it
    /// reflects the last block ever enqueued — not just what's still pending.
    pub fn last_enqueued(&self) -> Option<NumHash> {
        self.inner.lock().last_enqueued()
    }

    #[cfg(test)]
    pub(crate) fn drain(&self) -> Vec<L1BlockDeposits> {
        self.inner.lock().drain()
    }
}

impl Default for DepositQueue {
    fn default() -> Self {
        Self::new()
    }
}
