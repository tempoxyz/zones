use super::*;
use std::collections::VecDeque;

/// Finalized L1 blocks waiting to be processed by the Zone engine.
///
/// Tempo finality is deterministic, so this queue is append-only. Conflicting,
/// skipped, or disconnected finalized blocks are errors rather than forks to
/// reconcile locally.
#[derive(Debug, Default)]
pub(crate) struct PendingDeposits {
    /// Pending L1 blocks with their portal events, not yet processed by the Zone.
    pending: VecDeque<L1BlockDeposits>,
    /// Highest L1 block ever enqueued (number + hash). Survives `confirm` /
    /// `drain` so that reconnecting subscribers know where the queue left off,
    /// even if the engine has already consumed the blocks.
    last_enqueued: Option<NumHash>,
}

impl PendingDeposits {
    /// Enqueue a finalized L1 block.
    ///
    /// Returns `true` when the block was appended and `false` for an exact
    /// redelivery of the current tip. Any other non-contiguous observation is a
    /// finality or provider-integrity failure and leaves the queue unchanged.
    pub(crate) fn try_enqueue(
        &mut self,
        header: SealedHeader<TempoHeader>,
        events: L1PortalEvents,
    ) -> eyre::Result<bool> {
        let block_number = header.number();
        let block_hash = header.hash();

        if let Some(last) = self.last_enqueued {
            if block_number < last.number {
                eyre::bail!(
                    "out-of-order finalized L1 block {block_number}; latest enqueued block is {}",
                    last.number
                );
            }
            if block_number == last.number {
                eyre::ensure!(
                    block_hash == last.hash,
                    "conflicting finalized L1 block at height {block_number}: \
                     existing={}, received={block_hash}",
                    last.hash
                );
                return Ok(false);
            }

            let expected = last
                .number
                .checked_add(1)
                .ok_or_else(|| eyre::eyre!("finalized L1 block number overflow"))?;
            eyre::ensure!(
                block_number == expected,
                "non-contiguous finalized L1 block: expected {expected}, received {block_number}"
            );
            eyre::ensure!(
                header.parent_hash() == last.hash,
                "finalized L1 parent mismatch at height {block_number}: \
                 expected={}, received={}",
                last.hash,
                header.parent_hash()
            );
        }

        self.last_enqueued = Some(header.num_hash());
        self.pending.push_back(L1BlockDeposits { header, events });
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn enqueue(&mut self, header: TempoHeader, events: L1PortalEvents) {
        self.try_enqueue(SealedHeader::seal_slow(header), events)
            .expect("test finalized blocks must be contiguous");
    }

    /// Peek at the next pending L1 block without removing it.
    ///
    /// Returns `None` if no L1 blocks are queued. Use [`confirm`](Self::confirm)
    /// after a successful build to advance the queue.
    pub(crate) fn peek(&self) -> Option<&L1BlockDeposits> {
        self.pending.front()
    }

    /// Confirm the next pending L1 block was successfully processed and remove it.
    ///
    /// The caller must pass the [`NumHash`] returned by [`Self::peek`]. A
    /// mismatch is an internal consumer-ordering error and leaves the queue
    /// unchanged.
    pub(crate) fn confirm(&mut self, expected: NumHash) -> eyre::Result<L1BlockDeposits> {
        let front = self
            .pending
            .front()
            .ok_or_else(|| eyre::eyre!("cannot confirm an empty finalized L1 queue"))?;
        eyre::ensure!(
            front.header.num_hash() == expected,
            "finalized L1 queue confirmation mismatch: expected {expected:?}, front is {:?}",
            front.header.num_hash()
        );
        Ok(self
            .pending
            .pop_front()
            .expect("front was checked immediately before pop"))
    }

    /// Confirm every pending L1 block up to and including `expected`.
    ///
    /// Follower import calls this only after the corresponding zone block is
    /// canonical. It is therefore idempotent and tolerates stale entries before
    /// `expected`, but rejects a different hash at the expected height.
    pub(crate) fn confirm_through(&mut self, expected: NumHash) -> eyre::Result<()> {
        while let Some(front) = self.pending.front().map(|entry| entry.header.num_hash()) {
            if front.number > expected.number {
                break;
            }
            eyre::ensure!(
                front.number < expected.number || front.hash == expected.hash,
                "deposit queue holds L1 block {} with hash {}, but the consumed block is {}",
                front.number,
                front.hash,
                expected.hash,
            );
            self.confirm(front)
                .expect("front was just read and matches by construction");
        }
        Ok(())
    }

    /// Drain all pending L1 block deposits.
    #[cfg(test)]
    pub(crate) fn drain(&mut self) -> Vec<L1BlockDeposits> {
        self.pending.drain(..).collect()
    }

    /// Returns the number of pending L1 blocks.
    #[cfg(test)]
    pub(crate) fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Returns the most recently enqueued L1 block (number + hash), if any.
    pub(crate) fn last_enqueued(&self) -> Option<NumHash> {
        self.last_enqueued
    }
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

    /// Enqueue a finalized L1 block and notify waiters when it was appended.
    pub fn enqueue(&self, header: TempoHeader, events: L1PortalEvents) {
        self.enqueue_sealed(SealedHeader::seal_slow(header), events);
    }

    /// Enqueue an already-sealed header and report invariant violations to the
    /// caller instead of panicking.
    ///
    /// Subscriber and peer-import inputs are external, so they use this path.
    /// Returns whether the block was newly appended.
    pub fn try_enqueue_sealed(
        &self,
        header: SealedHeader<TempoHeader>,
        events: L1PortalEvents,
    ) -> eyre::Result<bool> {
        let mut queue = self.inner.lock();
        if let Some(queued) = queue
            .pending
            .iter()
            .find(|queued| queued.header.number() == header.number())
        {
            eyre::ensure!(
                queued.header.hash() == header.hash(),
                "conflicting finalized L1 block at height {}: existing={}, received={}",
                header.number(),
                queued.header.hash(),
                header.hash()
            );
            return Ok(false);
        }
        let appended = queue.try_enqueue(header, events)?;
        drop(queue);
        if appended {
            self.notify.notify_one();
        }
        Ok(appended)
    }

    /// Like [`enqueue`](Self::enqueue) but accepts an already-sealed header,
    /// avoiding a redundant hash computation.
    pub fn enqueue_sealed(
        &self,
        header: SealedHeader<TempoHeader>,
        events: L1PortalEvents,
    ) -> bool {
        let appended = self
            .inner
            .lock()
            .try_enqueue(header, events)
            .unwrap_or_else(|err| panic!("finalized L1 queue invariant violated: {err}"));
        if appended {
            self.notify.notify_one();
        }
        appended
    }

    /// Peek at the next L1 block without removing it.
    pub fn peek(&self) -> Option<L1BlockDeposits> {
        self.inner.lock().peek().cloned()
    }

    /// Confirm the next L1 block was successfully processed and remove it.
    ///
    pub fn confirm(&self, expected: NumHash) -> eyre::Result<L1BlockDeposits> {
        self.inner.lock().confirm(expected)
    }

    /// Advance the queue past a canonical follower anchor.
    pub fn confirm_through(&self, expected: NumHash) -> eyre::Result<()> {
        self.inner.lock().confirm_through(expected)
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
