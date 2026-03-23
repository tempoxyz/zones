//! L1 batch submitter for the zone sequencer.
//!
//! This module handles **Tempo L1** interactions — all transactions go to the
//! [`ZonePortal`](crate::abi::ZonePortal) contract deployed on L1. The sequencer
//! signing key is used for every L1 transaction.
//!
//! [`BatchData`] is produced by the zone block builder (not implemented here) and
//! sent to the submitter via a `tokio::sync::mpsc` channel.
//!
//! # POC limitations
//!
//! Proof validation is currently **skipped**: both `verifierConfig` and `proof`
//! are submitted as empty bytes. The L1 verifier contract must be configured to
//! accept empty proofs for this to work.
//!
//! # Anchor modes
//!
//! | Gap | Mode | Description |
//! |-----|------|-------------|
//! | < [`EIP2935_EFFECTIVE_WINDOW`] | Direct | Portal reads hash from EIP-2935. |
//! | ≥ [`EIP2935_EFFECTIVE_WINDOW`] | Stepping | Split into multiple direct-mode submissions. |
//!
//! [`AnchorGapKind`] classifies the gap in the zone monitor before
//! `submit_batch` is called. Inside `submit_batch`, [`AnchorMode`] handles
//! the rare case where the gap lands between [`EIP2935_EFFECTIVE_WINDOW`] and
//! [`EIP2935_HISTORY_WINDOW`] (e.g. due to timing) by falling back to ancestry
//! mode — a recent anchor block plus a parent-hash header chain.

use std::collections::BTreeMap;

use crate::abi::{self, BlockTransition, DepositQueueTransition, ZoneOutbox, ZonePortal};
use alloy_consensus::Transaction;
use alloy_network::ReceiptResponse;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{DynProvider, Provider};
use alloy_rlp::Encodable;
use alloy_sol_types::SolCall;
use eyre::Result;
use futures::{StreamExt, TryStreamExt};
use tempo_alloy::{TempoNetwork, rpc::TempoCallBuilderExt};
use tracing::{info, instrument, warn};

use crate::nonce_keys::SUBMIT_BATCH_NONCE_KEY;

/// EIP-2935 stores the last 8192 block hashes (~68 min at 500ms block time).
const EIP2935_HISTORY_WINDOW: u64 = 8192;

/// Safety margin (~3 min at 500ms block time) to avoid race conditions where
/// the block falls out of the window between our check and on-chain execution.
const EIP2935_SAFETY_MARGIN: u64 = 360;

/// Effective EIP-2935 window after subtracting the safety margin. Batches with
/// a gap below this threshold use direct mode; gaps at or above it require
/// stepping (splitting into multiple direct-mode submissions).
const EIP2935_EFFECTIVE_WINDOW: u64 = EIP2935_HISTORY_WINDOW - EIP2935_SAFETY_MARGIN;

/// Maximum number of pending withdrawal queue slots in the portal ring buffer.
const WITHDRAWAL_QUEUE_CAPACITY: u64 = 100;

/// Data required to submit a single batch to the ZonePortal on L1.
///
/// Produced by the zone block builder and sent to [`BatchSubmitter`] via channel.
#[derive(Debug, Clone)]
pub struct BatchData {
    /// Tempo L1 block number for EIP-2935 verification.
    pub tempo_block_number: u64,
    /// Previous zone block hash (must match portal's current `blockHash`).
    pub prev_block_hash: B256,
    /// New zone block hash after this batch.
    pub next_block_hash: B256,
    /// Deposit queue: where the zone started processing.
    pub prev_processed_deposit_hash: B256,
    /// Deposit queue: where the zone processed up to.
    pub next_processed_deposit_hash: B256,
    /// Withdrawal queue hash for this batch (`B256::ZERO` if no withdrawals).
    pub withdrawal_queue_hash: B256,
}

/// Submits zone batches to the ZonePortal contract on Tempo L1.
///
/// Holds a contract instance pointing at the portal, backed by a shared
/// [`DynProvider`] with the sequencer's signing wallet.
pub struct BatchSubmitter {
    /// ZonePortal contract address on Tempo L1 (used in tracing spans).
    portal_address: Address,
    /// Shared L1 provider (HTTP or WS) for querying the current block number
    /// (EIP-2935 window check). The same provider backs the `portal` contract
    /// instance.
    l1_provider: DynProvider<TempoNetwork>,
    /// ZonePortal contract instance for calling `submitBatch` and reading
    /// on-chain state such as `blockHash()`.
    portal: ZonePortal::ZonePortalInstance<DynProvider<TempoNetwork>, TempoNetwork>,
    /// The portal's `genesisTempoBlockNumber` — batches with a
    /// `tempo_block_number` below this value will be rejected on-chain.
    genesis_tempo_block_number: u64,
    /// Concurrency for pipelined L1 header fetching in ancestry mode.
    l1_fetch_concurrency: usize,
}

impl BatchSubmitter {
    /// Create a new batch submitter from a shared L1 provider.
    ///
    /// The provider must already include the sequencer wallet for signing.
    pub fn new(
        portal_address: Address,
        l1_provider: DynProvider<TempoNetwork>,
        genesis_tempo_block_number: u64,
    ) -> Self {
        let portal = ZonePortal::new(portal_address, l1_provider.clone());
        Self {
            portal_address,
            l1_provider,
            portal,
            genesis_tempo_block_number,
            l1_fetch_concurrency: 16,
        }
    }

    /// Submit a batch to the ZonePortal on Tempo L1.
    ///
    /// Resolves the anchor mode based on how old `tempo_block_number` is:
    ///
    /// - **Direct** — `tempo_block_number` is within [`EIP2935_EFFECTIVE_WINDOW`],
    ///   the portal reads its hash directly from EIP-2935.
    /// - **Ancestry** — `tempo_block_number` is outside the effective window but
    ///   still within [`EIP2935_HISTORY_WINDOW`]. A recent anchor block is used
    ///   and ancestry headers are collected (for future prover integration).
    ///
    /// The caller must ensure `tempo_block_number` is within the
    /// [`EIP2935_HISTORY_WINDOW`] — use [`classify_anchor_gap`](Self::classify_anchor_gap)
    /// first and split via stepping if the gap is too large.
    ///
    /// `verifierConfig` and `proof` are set to empty bytes — the verifier
    /// contract must be configured to accept empty proofs.
    // TODO: pass real proof bytes once proof generation is implemented.
    #[instrument(skip_all, fields(
        portal = %self.portal_address,
        tempo_block = batch.tempo_block_number,
        prev_block_hash = %batch.prev_block_hash,
        next_block_hash = %batch.next_block_hash,
        withdrawal_queue_hash = %batch.withdrawal_queue_hash,
    ))]
    pub async fn submit_batch(&self, batch: &BatchData) -> Result<B256> {
        if batch.tempo_block_number < self.genesis_tempo_block_number {
            return Err(eyre::eyre!(
                "tempo_block_number ({}) is below genesis ({})",
                batch.tempo_block_number,
                self.genesis_tempo_block_number
            ));
        }

        if !batch.withdrawal_queue_hash.is_zero() {
            self.check_withdrawal_queue_capacity().await?;
        }

        let block_transition = BlockTransition {
            prevBlockHash: batch.prev_block_hash,
            nextBlockHash: batch.next_block_hash,
        };

        let deposit_transition = DepositQueueTransition {
            prevProcessedHash: batch.prev_processed_deposit_hash,
            nextProcessedHash: batch.next_processed_deposit_hash,
        };

        let anchor_mode = self.resolve_anchor_mode(batch.tempo_block_number).await?;
        let recent_tempo_block_number = anchor_mode.recent_block_number();
        let (current_l1_block, portal_block_hash) = tokio::join!(
            self.l1_provider.get_block_number(),
            self.read_portal_block_hash(),
        );
        let current_l1_block = current_l1_block?;
        let portal_block_hash = portal_block_hash?;

        info!(
            ?anchor_mode,
            recent_tempo_block_number,
            current_l1_block,
            portal_block_hash = %portal_block_hash,
            batch_prev_block_hash = %batch.prev_block_hash,
            nonce_key = ?SUBMIT_BATCH_NONCE_KEY,
            "Preparing submitBatch to ZonePortal on L1"
        );

        if portal_block_hash != batch.prev_block_hash {
            warn!(
                portal_block_hash = %portal_block_hash,
                batch_prev_block_hash = %batch.prev_block_hash,
                "Portal block hash does not match batch prev hash before submitBatch"
            );
        }

        info!(?anchor_mode, "Submitting batch to ZonePortal on L1");

        let pending = self
            .portal
            .submitBatch(
                batch.tempo_block_number,
                recent_tempo_block_number,
                block_transition,
                deposit_transition,
                batch.withdrawal_queue_hash,
                Bytes::new(),
                Bytes::new(),
            )
            .nonce_key(SUBMIT_BATCH_NONCE_KEY)
            .send()
            .await?;

        let tx_hash = *pending.tx_hash();
        info!(
            %tx_hash,
            timeout_secs = 30,
            required_confirmations = 1,
            "submitBatch tx accepted by RPC; waiting for confirmation"
        );

        let receipt_result = pending
            .with_required_confirmations(1)
            .with_timeout(Some(std::time::Duration::from_secs(30)))
            .get_receipt()
            .await;

        let receipt = match receipt_result {
            Ok(receipt) => receipt,
            Err(err) => {
                warn!(
                    %tx_hash,
                    timeout_secs = 30,
                    error = %err,
                    "submitBatch tx was broadcast but receipt not obtained"
                );
                return Err(err.into());
            }
        };

        if !receipt.status() {
            return Err(eyre::eyre!(
                "submitBatch tx {tx_hash} was included but reverted on L1"
            ));
        }

        info!(%tx_hash, "Batch submitted to L1");

        Ok(tx_hash)
    }

    /// Classify whether `tempo_block_number` can be submitted directly or
    /// requires stepping (splitting into sub-batches).
    ///
    /// Only performs a single `get_block_number` RPC call — no header fetching
    /// or contract reads.
    ///
    /// Returns an error if `tempo_block_number` is not yet confirmed on L1
    /// (i.e. it equals or exceeds the current L1 tip).
    pub(crate) async fn classify_anchor_gap(
        &self,
        tempo_block_number: u64,
    ) -> Result<AnchorGapKind> {
        let current_l1_block = self.l1_provider.get_block_number().await?;

        if tempo_block_number >= current_l1_block {
            return Err(eyre::eyre!(
                "tempo_block_number ({tempo_block_number}) is not yet confirmed on L1 (tip={current_l1_block}), \
                 will retry after L1 advances"
            ));
        }

        let gap = current_l1_block.saturating_sub(tempo_block_number);

        if gap < EIP2935_EFFECTIVE_WINDOW {
            Ok(AnchorGapKind::Direct)
        } else {
            Ok(AnchorGapKind::Ancestry {
                step_size: EIP2935_EFFECTIVE_WINDOW,
            })
        }
    }

    /// Resolve the anchor mode for the given `tempo_block_number`.
    ///
    /// - **Direct** (gap < [`EIP2935_EFFECTIVE_WINDOW`]): the portal reads the
    ///   hash directly from EIP-2935.
    /// - **Ancestry** (gap within [`EIP2935_HISTORY_WINDOW`]): a recent L1 block
    ///   within the window is used as anchor. Ancestry headers are collected
    ///   and validated for future prover integration.
    ///
    /// Returns an error if the gap exceeds [`EIP2935_HISTORY_WINDOW`] — the
    /// caller must split via stepping before calling this.
    async fn resolve_anchor_mode(&self, tempo_block_number: u64) -> Result<AnchorMode> {
        let current_l1_block = self.l1_provider.get_block_number().await?;

        if tempo_block_number >= current_l1_block {
            return Err(eyre::eyre!(
                "tempo_block_number ({tempo_block_number}) is not yet confirmed on L1 \
                 (tip={current_l1_block}), will retry after L1 advances"
            ));
        }

        let gap = current_l1_block.saturating_sub(tempo_block_number);

        if gap < EIP2935_EFFECTIVE_WINDOW {
            return Ok(AnchorMode::Direct);
        }

        if gap > EIP2935_HISTORY_WINDOW {
            return Err(eyre::eyre!(
                "tempo_block_number ({tempo_block_number}) is outside the EIP-2935 history \
                 window (gap={gap}, max={EIP2935_HISTORY_WINDOW}) — must split via stepping"
            ));
        }

        // Within ancestry range — collect L1 headers as proof chain.
        let anchor_block = current_l1_block.saturating_sub(EIP2935_SAFETY_MARGIN);
        let ancestry_headers = self
            .fetch_ancestry_headers(tempo_block_number, anchor_block)
            .await?;

        warn!(
            tempo_block_number,
            current_l1_block,
            anchor_block,
            gap,
            header_count = ancestry_headers.len(),
            total_bytes = ancestry_headers.iter().map(|h| h.len()).sum::<usize>(),
            "tempo_block_number outside EIP-2935 effective window, using ancestry mode"
        );

        Ok(AnchorMode::Ancestry {
            anchor_block,
            ancestry_headers,
        })
    }

    /// Fetch and RLP-encode L1 block headers from `from + 1` to `to` (inclusive),
    /// validating the parent-hash chain.
    ///
    /// Returns headers in ascending block-number order. The first header's
    /// `parent_hash` is validated against the hash of block `from`, ensuring the
    /// chain is rooted at the expected block.
    async fn fetch_ancestry_headers(&self, from: u64, to: u64) -> Result<Vec<Bytes>> {
        use futures::stream;

        if to <= from {
            return Ok(Vec::new());
        }

        let concurrency = self.l1_fetch_concurrency;
        let range_start = from + 1;
        let count = (to - from) as usize;

        // Fetch the base block's header to seed the parent-hash chain validation.
        let base_header = self
            .l1_provider
            .get_header_by_number(from.into())
            .await?
            .ok_or_else(|| eyre::eyre!("L1 header not found for base block {from}"))?;
        let mut base_buf = Vec::with_capacity(600);
        base_header.inner.inner.encode(&mut base_buf);
        let base_hash = alloy_primitives::keccak256(&base_buf);

        let mut fetched = stream::iter(range_start..=to)
            .map(|block_number| {
                let provider = &self.l1_provider;
                async move {
                    let header = provider
                        .get_header_by_number(block_number.into())
                        .await?
                        .ok_or_else(|| {
                            eyre::eyre!("L1 header not found for block {block_number}")
                        })?;
                    Ok::<_, eyre::Report>((block_number, header.inner.inner))
                }
            })
            .buffered(concurrency);

        let mut headers = Vec::with_capacity(count);
        let mut prev_hash: Option<B256> = Some(base_hash);

        while let Some((block_number, header)) = fetched.try_next().await? {
            if let Some(expected_parent) = prev_hash
                && header.inner.parent_hash != expected_parent
            {
                return Err(eyre::eyre!(
                    "parent-hash chain broken at block {block_number}: \
                     expected parent_hash={expected_parent}, got={}",
                    header.inner.parent_hash
                ));
            }

            let mut buf = Vec::with_capacity(600);
            header.encode(&mut buf);
            let header_hash = alloy_primitives::keccak256(&buf);
            prev_hash = Some(header_hash);

            headers.push(Bytes::from(buf));
        }

        Ok(headers)
    }

    /// Compute zone L2 block numbers that serve as split points for stepping mode.
    ///
    /// Zone blocks and L1 blocks have a 1:1 mapping (each zone block processes
    /// exactly one L1 block via `advanceTempo`), so the zone block for a target
    /// `tempoBlockNumber` can be computed arithmetically:
    ///
    /// ```text
    /// zone_block = from_zone_block + (target_tempo - from_tempo)
    /// ```
    ///
    /// Returns split points in ascending order, all within `[from_zone_block, max_zone_block]`.
    pub(crate) fn compute_step_points(
        from_zone_block: u64,
        from_tempo: u64,
        current_l1_block: u64,
        step_size: u64,
        max_zone_block: u64,
    ) -> Vec<StepPoint> {
        let mut step_points = Vec::new();
        let mut target = from_tempo + step_size;

        while target < current_l1_block.saturating_sub(EIP2935_SAFETY_MARGIN) {
            let zone_block = from_zone_block + (target - from_tempo);
            if zone_block > max_zone_block {
                break;
            }
            step_points.push(StepPoint {
                zone_block,
                target_tempo_block: target,
            });
            target += step_size;
        }

        step_points
    }

    /// Returns a reference to the L1 provider.
    pub(crate) fn l1_provider(&self) -> &DynProvider<TempoNetwork> {
        &self.l1_provider
    }

    /// Read the portal's `genesisTempoBlockNumber` from L1.
    pub async fn read_genesis_tempo_block_number(&self) -> Result<u64> {
        Ok(self.portal.genesisTempoBlockNumber().call().await?)
    }

    /// Read the current `blockHash` from the ZonePortal on L1.
    ///
    /// Used to resync the monitor's `prev_block_hash` after repeated submission
    /// failures, ensuring subsequent batches use the portal's actual state.
    pub async fn read_portal_block_hash(&self) -> Result<B256> {
        let hash = self.portal.blockHash().call().await?;
        Ok(hash)
    }

    /// Read the current withdrawal queue tail from the ZonePortal on L1.
    ///
    /// Used at startup and during resync to initialize
    /// `portal_withdrawal_queue_tail` so withdrawal data is stored under the
    /// correct portal queue slot.
    pub async fn read_portal_withdrawal_queue_tail(&self) -> Result<u64> {
        let tail = self.portal.withdrawalQueueTail().call().await?;
        let tail: u64 = tail
            .try_into()
            .map_err(|_| eyre::eyre!("withdrawal queue tail overflow"))?;
        Ok(tail)
    }

    /// Read the current withdrawal queue head from the ZonePortal on L1.
    pub async fn read_portal_withdrawal_queue_head(&self) -> Result<u64> {
        let head = self.portal.withdrawalQueueHead().call().await?;
        let head: u64 = head
            .try_into()
            .map_err(|_| eyre::eyre!("withdrawal queue head overflow"))?;
        Ok(head)
    }

    /// Check if the withdrawal queue has capacity for another batch.
    ///
    /// The portal uses a ring buffer with 100 slots. Returns an error if the
    /// queue is full (`tail - head >= 100`).
    pub async fn check_withdrawal_queue_capacity(&self) -> Result<()> {
        let (head, tail) = tokio::try_join!(
            self.read_portal_withdrawal_queue_head(),
            self.read_portal_withdrawal_queue_tail(),
        )?;
        if tail.saturating_sub(head) >= WITHDRAWAL_QUEUE_CAPACITY {
            return Err(eyre::eyre!(
                "withdrawal queue full ({} pending slots, capacity {})",
                tail.saturating_sub(head),
                WITHDRAWAL_QUEUE_CAPACITY
            ));
        }
        Ok(())
    }

    /// Re-populate the in-memory [`WithdrawalStore`](crate::withdrawals::WithdrawalStore)
    /// after a sequencer restart.
    ///
    /// The L1 portal stores only hash chains, not the actual [`Withdrawal`](abi::Withdrawal)
    /// structs. This method reconstructs them by:
    ///
    /// 1. Reading `withdrawalQueueHead` / `withdrawalQueueTail` from the **L1 portal**
    ///    to determine which slots are still pending.
    /// 2. Walking **L1** backwards from the chain tip to find the `BatchSubmitted`
    ///    event for each pending slot (plus the predecessor for zone block range
    ///    boundaries).
    /// 3. Resolving each event's `nextBlockHash` to a **zone L2** block number.
    /// 4. Fetching `WithdrawalRequested` events from the **zone L2** outbox in
    ///    the corresponding block range.
    /// 5. Reading the head slot's current on-chain hash for partial processing
    ///    detection.
    /// 6. Verifying the hash chain and trimming already-processed withdrawals.
    ///
    /// Returns a map of portal_slot → verified withdrawals ready to be stored.
    #[instrument(skip_all, fields(portal = %self.portal_address))]
    pub async fn fetch_pending_withdrawals(
        &self,
        zone_provider: &DynProvider<TempoNetwork>,
        outbox_address: Address,
    ) -> Result<BTreeMap<u64, Vec<abi::Withdrawal>>> {
        // Step 1: read pending slot range from the L1 portal.
        let (head, tail) = tokio::try_join!(
            self.read_portal_withdrawal_queue_head(),
            self.read_portal_withdrawal_queue_tail(),
        )?;

        if head >= tail {
            info!(head, tail, "No pending withdrawals to restore");
            return Ok(BTreeMap::new());
        }

        info!(
            head,
            tail,
            pending = tail - head,
            "Restoring pending withdrawals"
        );

        // Step 2: walk L1 backwards from the L1 tip to find BatchSubmitted
        // events for pending slots [head, tail) plus the predecessor (head-1).
        let l1_tip = self.l1_provider.get_block_number().await?;
        let events = self.find_batch_events_backwards(l1_tip, head, tail).await?;

        // Step 3: resolve each L1 event's nextBlockHash to a zone L2 block number.
        // Maps portal_slot → last zone L2 block in that batch.
        let mut zone_end_by_slot: BTreeMap<u64, u64> = BTreeMap::new();
        for (&portal_slot, event) in &events {
            let block = zone_provider
                .get_block_by_hash(event.nextBlockHash)
                .await?
                .ok_or_else(|| {
                    eyre::eyre!(
                        "zone block not found for hash {} (portal slot {portal_slot})",
                        event.nextBlockHash
                    )
                })?;
            zone_end_by_slot.insert(portal_slot, block.number());
        }

        // Step 4: fetch WithdrawalRequested events from zone L2 for each pending slot.
        let outbox = ZoneOutbox::new(outbox_address, zone_provider.clone());
        let mut slot_withdrawals: BTreeMap<u64, Vec<abi::Withdrawal>> = BTreeMap::new();
        for portal_slot in head..tail {
            if !events.contains_key(&portal_slot) {
                continue;
            }
            let zone_end = zone_end_by_slot[&portal_slot];
            let zone_start = if portal_slot == 0 {
                1
            } else if let Some(prev_end) = zone_end_by_slot.get(&(portal_slot - 1)) {
                prev_end + 1
            } else {
                warn!(
                    portal_slot,
                    "predecessor event missing, cannot determine zone block range start"
                );
                continue;
            };
            let withdrawals =
                fetch_slot_withdrawals(&outbox, zone_provider, zone_start, zone_end).await?;
            slot_withdrawals.insert(portal_slot, withdrawals);
        }

        // Step 5: read the head slot's current on-chain hash (for partial processing detection).
        let head_slot_hash = self
            .portal
            .withdrawalQueueSlot(U256::from(head % WITHDRAWAL_QUEUE_CAPACITY))
            .call()
            .await?;

        // Guard: verify the queue didn't change during the multi-RPC replay.
        let (head2, tail2) = tokio::try_join!(
            self.read_portal_withdrawal_queue_head(),
            self.read_portal_withdrawal_queue_tail(),
        )?;

        if head2 != head || tail2 != tail {
            eyre::bail!(
                "withdrawal queue changed during restore ({}..{} -> {}..{}), retry on next startup",
                head,
                tail,
                head2,
                tail2
            );
        }

        // Step 6: resolve all fetched data into verified withdrawal sets.
        resolve_pending_slots(head, tail, &events, &slot_withdrawals, head_slot_hash)
    }

    /// Walk **L1** backwards from `l1_tip` in 10k-block chunks to find
    /// `BatchSubmitted` events for portal slots `[head, tail)` plus the
    /// predecessor slot `head - 1` (used to determine the zone L2 block
    /// range start of the first pending slot). When `head == 0` the
    /// predecessor does not exist and is omitted; the caller falls back to
    /// zone block 1.
    ///
    /// Portal queue slots are assigned by position: the n-th non-zero-hash
    /// `BatchSubmitted` event (0-indexed, chronologically) corresponds to
    /// portal queue slot n. This is because the L1 portal increments
    /// `withdrawalBatchIndex` on every `submitBatch` call, but only advances
    /// the withdrawal queue `tail` for batches with non-zero
    /// `withdrawalQueueHash`. The two counters diverge whenever a batch has
    /// no withdrawals, so we cannot use `withdrawalBatchIndex` as the slot
    /// index.
    ///
    /// Stops as soon as enough events are found — pending slots are recent,
    /// so the first chunk typically covers them all.
    async fn find_batch_events_backwards(
        &self,
        l1_tip: u64,
        head: u64,
        tail: u64,
    ) -> Result<BTreeMap<u64, abi::ZonePortal::BatchSubmitted>> {
        let start = head.saturating_sub(1);
        let needed = (tail - start) as usize;
        if needed == 0 {
            return Ok(BTreeMap::new());
        }

        let mut all_events: Vec<abi::ZonePortal::BatchSubmitted> = Vec::new();
        let mut hi = l1_tip;

        while hi >= self.genesis_tempo_block_number {
            let lo = hi
                .saturating_sub(10_000)
                .max(self.genesis_tempo_block_number);

            let chunk: Vec<_> = self
                .portal
                .BatchSubmitted_filter()
                .from_block(lo)
                .to_block(hi)
                .query()
                .await?
                .into_iter()
                .filter(|(event, _)| !event.withdrawalQueueHash.is_zero())
                .map(|(event, _)| event)
                .collect();

            all_events.extend(chunk);

            if all_events.len() >= needed {
                break;
            }
            if lo == self.genesis_tempo_block_number {
                break;
            }
            hi = lo - 1;
        }

        // Sort chronologically — the n-th event = portal queue slot n.
        all_events.sort_by_key(|e| e.withdrawalBatchIndex);

        // Assign portal slots: if we found M events total, the last M
        // correspond to slots [tail - M, tail). Keep only [start, tail).
        // Truncate to at most `needed` events (the most recent ones) to
        // guard against extra events from race conditions.
        if all_events.len() > needed {
            all_events.drain(..all_events.len() - needed);
        }

        let mut found = BTreeMap::new();
        let first_slot = tail.saturating_sub(all_events.len() as u64);
        for (i, event) in all_events.into_iter().enumerate() {
            let portal_slot = first_slot + i as u64;
            if portal_slot >= start && portal_slot < tail {
                found.insert(portal_slot, event);
            }
        }

        Ok(found)
    }
}

/// Pure function that resolves pre-fetched data into verified withdrawal sets
/// ready to be stored.
///
/// For each pending portal slot in `[head, tail)`:
/// 1. Skips slots with no `BatchSubmitted` event or no fetched withdrawals.
/// 2. Verifies the hash chain of fetched withdrawals matches the L1 event's
///    `withdrawalQueueHash`.
/// 3. For the head slot, trims already-processed withdrawals using
///    `head_slot_hash` (the current on-chain slot hash). The L1 portal
///    processes withdrawals one-by-one, updating the slot hash after each.
///    If the sequencer crashed mid-slot, some are already consumed but `head`
///    hasn't advanced yet.
/// 4. Non-head slots are always fully unprocessed.
///
/// Returns a map of portal_slot → verified withdrawals to store.
fn resolve_pending_slots(
    head: u64,
    tail: u64,
    events: &BTreeMap<u64, abi::ZonePortal::BatchSubmitted>,
    slot_withdrawals: &BTreeMap<u64, Vec<abi::Withdrawal>>,
    head_slot_hash: B256,
) -> Result<BTreeMap<u64, Vec<abi::Withdrawal>>> {
    let mut result: BTreeMap<u64, Vec<abi::Withdrawal>> = BTreeMap::new();

    for portal_slot in head..tail {
        let Some(event) = events.get(&portal_slot) else {
            eyre::bail!("no BatchSubmitted event found for pending portal slot {portal_slot}");
        };

        let Some(withdrawals) = slot_withdrawals.get(&portal_slot) else {
            eyre::bail!("no withdrawal data fetched for pending portal slot {portal_slot}");
        };

        if withdrawals.is_empty()
            || abi::Withdrawal::queue_hash(withdrawals) != event.withdrawalQueueHash
        {
            eyre::bail!("withdrawal hash mismatch or empty for portal slot {portal_slot}");
        }

        if portal_slot == head {
            match find_processed_offset(withdrawals, head_slot_hash) {
                Some(offset) => {
                    let remaining = withdrawals[offset..].to_vec();
                    if !remaining.is_empty() {
                        result.insert(portal_slot, remaining);
                    }
                }
                None => {
                    eyre::bail!("cannot determine processed offset for head slot {portal_slot}");
                }
            }
        } else {
            result.insert(portal_slot, withdrawals.clone());
        }
    }

    Ok(result)
}

/// Find the offset into `withdrawals` where the remaining hash chain matches
/// `current_slot_hash`. Returns `Some(0)` if no withdrawals have been processed,
/// `Some(n)` if n have been processed (n remaining), or `None` if no match is
/// found.
///
/// Also checks `offset == len` (all consumed, hash chain = `B256::ZERO`).
fn find_processed_offset(
    withdrawals: &[abi::Withdrawal],
    current_slot_hash: B256,
) -> Option<usize> {
    for offset in 0..=withdrawals.len() {
        let hash = abi::Withdrawal::queue_hash(&withdrawals[offset..]);
        if hash == current_slot_hash {
            return Some(offset);
        }
    }
    None
}

/// Fetch `WithdrawalRequested` events from zone L2 in the given block range,
/// sorted by log order, and convert to [`abi::Withdrawal`] structs.
pub(crate) async fn fetch_slot_withdrawals(
    outbox: &ZoneOutbox::ZoneOutboxInstance<DynProvider<TempoNetwork>, TempoNetwork>,
    zone_provider: &DynProvider<TempoNetwork>,
    from: u64,
    to: u64,
) -> Result<Vec<abi::Withdrawal>> {
    struct RequestedWithdrawalLog {
        block_number: u64,
        tx_index: u64,
        log_index: u64,
        tx_hash: B256,
        event: abi::ZoneOutbox::WithdrawalRequested,
    }

    struct FinalizedBatchLog {
        block_number: u64,
        tx_index: u64,
        log_index: u64,
        tx_hash: B256,
    }

    let mut requests: Vec<_> = outbox
        .WithdrawalRequested_filter()
        .from_block(from)
        .to_block(to)
        .query()
        .await?
        .into_iter()
        .map(|(event, log)| -> Result<_> {
            Ok(RequestedWithdrawalLog {
                block_number: log.block_number.unwrap_or(0),
                tx_index: log.transaction_index.unwrap_or(0),
                log_index: log.log_index.unwrap_or(0),
                tx_hash: log.transaction_hash.ok_or_else(|| {
                    eyre::eyre!("WithdrawalRequested log missing transaction hash")
                })?,
                event,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    requests.sort_by_key(|request| (request.block_number, request.tx_index, request.log_index));

    let mut finalized_batches: Vec<_> = outbox
        .BatchFinalized_filter()
        .from_block(from)
        .to_block(to)
        .query()
        .await?
        .into_iter()
        .filter(|(event, _)| !event.withdrawalQueueHash.is_zero())
        .map(|(_, log)| -> Result<_> {
            Ok(FinalizedBatchLog {
                block_number: log.block_number.unwrap_or(0),
                tx_index: log.transaction_index.unwrap_or(0),
                log_index: log.log_index.unwrap_or(0),
                tx_hash: log
                    .transaction_hash
                    .ok_or_else(|| eyre::eyre!("BatchFinalized log missing transaction hash"))?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    finalized_batches.sort_by_key(|batch| (batch.block_number, batch.tx_index, batch.log_index));

    let mut requests_by_block: BTreeMap<u64, Vec<RequestedWithdrawalLog>> = BTreeMap::new();
    for request in requests {
        requests_by_block
            .entry(request.block_number)
            .or_default()
            .push(request);
    }

    let mut withdrawals = Vec::new();
    for finalized_batch in finalized_batches {
        let requests = requests_by_block
            .remove(&finalized_batch.block_number)
            .unwrap_or_default();
        if requests.is_empty() {
            return Err(eyre::eyre!(
                "BatchFinalized at zone block {} has no matching WithdrawalRequested events",
                finalized_batch.block_number
            ));
        }

        let finalize_tx = zone_provider
            .get_transaction_by_hash(finalized_batch.tx_hash)
            .await?
            .ok_or_else(|| {
                eyre::eyre!(
                    "missing finalizeWithdrawalBatch tx {} for zone block {}",
                    finalized_batch.tx_hash,
                    finalized_batch.block_number
                )
            })?;
        let finalize_call =
            abi::ZoneOutbox::finalizeWithdrawalBatchCall::abi_decode(finalize_tx.input().as_ref())
                .map_err(|err| {
                    eyre::eyre!(
                        "failed to decode finalizeWithdrawalBatch calldata for {}: {err}",
                        finalized_batch.tx_hash
                    )
                })?;

        if finalize_call.encryptedSenders.len() != requests.len() {
            return Err(eyre::eyre!(
                "encrypted sender count mismatch at zone block {}: {} encrypted senders for {} requests",
                finalized_batch.block_number,
                finalize_call.encryptedSenders.len(),
                requests.len()
            ));
        }

        withdrawals.extend(
            requests
                .into_iter()
                .zip(finalize_call.encryptedSenders)
                .map(|(request, encrypted_sender)| {
                    abi::Withdrawal::from_requested_event(
                        &request.event,
                        request.tx_hash,
                        encrypted_sender,
                    )
                }),
        );
    }

    if !requests_by_block.is_empty() {
        return Err(eyre::eyre!(
            "found WithdrawalRequested events without matching non-zero BatchFinalized events in range {from}..={to}"
        ));
    }

    Ok(withdrawals)
}

/// Classification of the EIP-2935 gap, returned by
/// [`BatchSubmitter::classify_anchor_gap`].
#[derive(Debug)]
pub(crate) enum AnchorGapKind {
    /// Gap < [`EIP2935_EFFECTIVE_WINDOW`] — the portal can read the block hash
    /// directly from EIP-2935. No extra proof data needed.
    Direct,
    /// Gap ≥ [`EIP2935_EFFECTIVE_WINDOW`] — `tempo_block_number` is too old
    /// for a direct EIP-2935 lookup. The batch must be split into multiple
    /// direct-mode sub-range submissions (stepping).
    Ancestry {
        /// Each sub-batch covers at most this many L1 blocks.
        step_size: u64,
    },
}

/// How the batch submitter anchors `tempoBlockNumber` for EIP-2935 verification.
///
/// Resolved by [`BatchSubmitter::resolve_anchor_mode`] inside `submit_batch`.
/// Stepping is handled at a higher level by [`AnchorGapKind`] — by the time
/// `submit_batch` is called, the gap must already be within
/// [`EIP2935_HISTORY_WINDOW`].
#[derive(Debug)]
#[allow(dead_code)] // Ancestry::ancestry_headers is collected but not yet consumed — available for prover integration
enum AnchorMode {
    /// `tempoBlockNumber` is within the effective EIP-2935 window — the portal
    /// reads its hash directly. No extra proof data required.
    Direct,
    /// `tempoBlockNumber` is outside the effective window but within the full
    /// history window. A recent L1 block is used as anchor, and the collected
    /// headers prove the parent-hash chain.
    Ancestry {
        /// Recent L1 block number within the EIP-2935 window, used as the
        /// on-chain anchor for hash verification.
        anchor_block: u64,
        /// RLP-encoded L1 block headers from `tempo_block_number + 1` to
        /// `anchor_block`, in ascending order. Available for the prover to
        /// consume when integrated.
        ancestry_headers: Vec<Bytes>,
    },
}

impl AnchorMode {
    /// Returns the `recentTempoBlockNumber` argument for `submitBatch`:
    /// `0` for direct mode, or the anchor block number for ancestry mode.
    const fn recent_block_number(&self) -> u64 {
        match self {
            Self::Direct => 0,
            Self::Ancestry { anchor_block, .. } => *anchor_block,
        }
    }
}

/// A step split point for stepping mode: identifies a zone L2 block at which
/// to cut an intermediate batch submission.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct StepPoint {
    /// Zone L2 block number at which to cut the intermediate batch.
    pub zone_block: u64,
    /// Target `tempoBlockNumber` value at this split point.
    pub target_tempo_block: u64,
}

/// Zone L2 state read at a specific block, used to populate [`BatchData`].
pub(crate) struct ZoneBlockSnapshot {
    /// Latest Tempo L1 block number as seen by the zone.
    pub tempo_block_number: u64,
    /// Cumulative hash of all deposits processed by the zone up to this block.
    pub processed_deposit_hash: B256,
    /// Zone L2 block hash.
    pub block_hash: B256,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi;
    use alloy_primitives::{B256, address};

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
    fn find_offset_no_withdrawals_processed() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let w1 = test_withdrawal(address!("0x0000000000000000000000000000000000000002"), 200);
        let withdrawals = vec![w0, w1];
        let full_hash = abi::Withdrawal::queue_hash(&withdrawals);
        assert_eq!(find_processed_offset(&withdrawals, full_hash), Some(0));
    }

    #[test]
    fn find_offset_one_processed() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let w1 = test_withdrawal(address!("0x0000000000000000000000000000000000000002"), 200);
        let withdrawals = vec![w0, w1];
        let hash = abi::Withdrawal::queue_hash(&withdrawals[1..]);
        assert_eq!(find_processed_offset(&withdrawals, hash), Some(1));
    }

    #[test]
    fn find_offset_all_processed() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let withdrawals = vec![w0];
        // B256::ZERO = queue_hash(&[]), meaning all withdrawals have been consumed.
        assert_eq!(find_processed_offset(&withdrawals, B256::ZERO), Some(1));
    }

    #[test]
    fn find_offset_no_match() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let withdrawals = vec![w0];
        let random_hash = B256::from([0xdeu8; 32]);
        assert_eq!(find_processed_offset(&withdrawals, random_hash), None);
    }

    #[test]
    fn find_offset_single_withdrawal_unprocessed() {
        let w = test_withdrawal(address!("0x0000000000000000000000000000000000000042"), 999);
        let withdrawals = vec![w];
        let hash = abi::Withdrawal::queue_hash(&withdrawals);
        assert_eq!(find_processed_offset(&withdrawals, hash), Some(0));
    }

    #[test]
    fn find_offset_partial_three_withdrawals() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let w1 = test_withdrawal(address!("0x0000000000000000000000000000000000000002"), 200);
        let w2 = test_withdrawal(address!("0x0000000000000000000000000000000000000003"), 300);
        let withdrawals = vec![w0, w1, w2];
        let hash = abi::Withdrawal::queue_hash(&withdrawals[2..]);
        assert_eq!(find_processed_offset(&withdrawals, hash), Some(2));
    }

    fn test_batch_event(withdrawal_queue_hash: B256) -> abi::ZonePortal::BatchSubmitted {
        abi::ZonePortal::BatchSubmitted {
            withdrawalBatchIndex: 0,
            nextProcessedDepositQueueHash: B256::ZERO,
            nextBlockHash: B256::ZERO,
            withdrawalQueueHash: withdrawal_queue_hash,
        }
    }

    #[test]
    fn resolve_empty_range() {
        let result =
            resolve_pending_slots(5, 5, &BTreeMap::new(), &BTreeMap::new(), B256::ZERO).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn resolve_single_slot_unprocessed() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let w1 = test_withdrawal(address!("0x0000000000000000000000000000000000000002"), 200);
        let withdrawals = vec![w0, w1];
        let full_hash = abi::Withdrawal::queue_hash(&withdrawals);

        let mut events = BTreeMap::new();
        events.insert(5, test_batch_event(full_hash));
        let mut slot_withdrawals = BTreeMap::new();
        slot_withdrawals.insert(5, withdrawals);

        let result = resolve_pending_slots(5, 6, &events, &slot_withdrawals, full_hash).unwrap();
        let returned = result.get(&5).unwrap();
        assert_eq!(returned.len(), 2);
        assert_eq!(abi::Withdrawal::queue_hash(returned), full_hash);
    }

    #[test]
    fn resolve_single_slot_partially_processed() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let w1 = test_withdrawal(address!("0x0000000000000000000000000000000000000002"), 200);
        let w2 = test_withdrawal(address!("0x0000000000000000000000000000000000000003"), 300);
        let withdrawals = vec![w0, w1, w2];
        let full_hash = abi::Withdrawal::queue_hash(&withdrawals);
        // head_slot_hash reflects that w0 has been processed (hash of remaining [w1, w2])
        let head_slot_hash = abi::Withdrawal::queue_hash(&withdrawals[1..]);

        let mut events = BTreeMap::new();
        events.insert(5, test_batch_event(full_hash));
        let mut slot_withdrawals = BTreeMap::new();
        slot_withdrawals.insert(5, withdrawals);

        let result =
            resolve_pending_slots(5, 6, &events, &slot_withdrawals, head_slot_hash).unwrap();
        let returned = result.get(&5).unwrap();
        assert_eq!(returned.len(), 2);
        assert_eq!(abi::Withdrawal::queue_hash(returned), head_slot_hash);
    }

    #[test]
    fn resolve_single_slot_fully_processed() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let withdrawals = vec![w0];
        let full_hash = abi::Withdrawal::queue_hash(&withdrawals);

        let mut events = BTreeMap::new();
        events.insert(5, test_batch_event(full_hash));
        let mut slot_withdrawals = BTreeMap::new();
        slot_withdrawals.insert(5, withdrawals);

        // B256::ZERO = queue_hash(&[]), all consumed. find_processed_offset returns
        // Some(1) (offset == len), so remaining is empty and slot is not stored.
        let result = resolve_pending_slots(5, 6, &events, &slot_withdrawals, B256::ZERO).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn resolve_multiple_slots() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let w1 = test_withdrawal(address!("0x0000000000000000000000000000000000000002"), 200);
        let w2 = test_withdrawal(address!("0x0000000000000000000000000000000000000003"), 300);

        let head_withdrawals = vec![w0];
        let tail_withdrawals = vec![w1, w2];

        let head_hash = abi::Withdrawal::queue_hash(&head_withdrawals);
        let tail_hash = abi::Withdrawal::queue_hash(&tail_withdrawals);

        let mut events = BTreeMap::new();
        events.insert(5, test_batch_event(head_hash));
        events.insert(6, test_batch_event(tail_hash));

        let mut slot_withdrawals = BTreeMap::new();
        slot_withdrawals.insert(5, head_withdrawals);
        slot_withdrawals.insert(6, tail_withdrawals);

        // head slot fully unprocessed (head_slot_hash == full hash of slot 5)
        let result = resolve_pending_slots(5, 7, &events, &slot_withdrawals, head_hash).unwrap();
        let slot5 = result.get(&5).unwrap();
        let slot6 = result.get(&6).unwrap();
        assert_eq!(slot5.len(), 1);
        assert_eq!(slot6.len(), 2);
        assert_eq!(abi::Withdrawal::queue_hash(slot5), head_hash);
        assert_eq!(abi::Withdrawal::queue_hash(slot6), tail_hash);
    }

    #[test]
    fn resolve_hash_mismatch_skipped() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let withdrawals = vec![w0];
        let wrong_hash = B256::from([0xabu8; 32]);

        let mut events = BTreeMap::new();
        events.insert(5, test_batch_event(wrong_hash));
        let mut slot_withdrawals = BTreeMap::new();
        slot_withdrawals.insert(5, withdrawals);

        let result = resolve_pending_slots(5, 6, &events, &slot_withdrawals, B256::ZERO);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_missing_event_skipped() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let withdrawals = vec![w0];

        let mut slot_withdrawals = BTreeMap::new();
        slot_withdrawals.insert(5, withdrawals);

        // No event for slot 5
        let result = resolve_pending_slots(5, 6, &BTreeMap::new(), &slot_withdrawals, B256::ZERO);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_head_partial_with_non_head_slot() {
        let w0 = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let w1 = test_withdrawal(address!("0x0000000000000000000000000000000000000002"), 200);
        let w2 = test_withdrawal(address!("0x0000000000000000000000000000000000000003"), 300);

        let head_withdrawals = vec![w0, w1];
        let non_head_withdrawals = vec![w2];

        let head_hash = abi::Withdrawal::queue_hash(&head_withdrawals);
        let non_head_hash = abi::Withdrawal::queue_hash(&non_head_withdrawals);
        // w0 already processed, head_slot_hash = hash of [w1] only
        let head_slot_hash = abi::Withdrawal::queue_hash(&head_withdrawals[1..]);

        let mut events = BTreeMap::new();
        events.insert(5, test_batch_event(head_hash));
        events.insert(6, test_batch_event(non_head_hash));

        let mut slot_withdrawals = BTreeMap::new();
        slot_withdrawals.insert(5, head_withdrawals);
        slot_withdrawals.insert(6, non_head_withdrawals);

        let result =
            resolve_pending_slots(5, 7, &events, &slot_withdrawals, head_slot_hash).unwrap();
        // Head slot trimmed to 1 remaining withdrawal
        assert_eq!(result.get(&5).unwrap().len(), 1);
        assert_eq!(
            abi::Withdrawal::queue_hash(result.get(&5).unwrap()),
            head_slot_hash
        );
        // Non-head slot fully present
        assert_eq!(result.get(&6).unwrap().len(), 1);
        assert_eq!(
            abi::Withdrawal::queue_hash(result.get(&6).unwrap()),
            non_head_hash
        );
    }

    #[test]
    fn resolve_empty_withdrawals_vec_skipped() {
        let mut events = BTreeMap::new();
        events.insert(5, test_batch_event(B256::from([0x11u8; 32])));

        let mut slot_withdrawals = BTreeMap::new();
        slot_withdrawals.insert(5, vec![]);

        let result = resolve_pending_slots(5, 6, &events, &slot_withdrawals, B256::ZERO);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_missing_withdrawals_data_skipped() {
        let w = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let hash = abi::Withdrawal::queue_hash(std::slice::from_ref(&w));

        let mut events = BTreeMap::new();
        events.insert(5, test_batch_event(hash));
        // slot_withdrawals has no entry for slot 5
        let slot_withdrawals = BTreeMap::new();

        let result = resolve_pending_slots(5, 6, &events, &slot_withdrawals, hash);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_head_slot_corrupted_hash_skipped() {
        let w = test_withdrawal(address!("0x0000000000000000000000000000000000000001"), 100);
        let withdrawals = vec![w];
        let full_hash = abi::Withdrawal::queue_hash(&withdrawals);
        // head_slot_hash doesn't match any tail of the withdrawal list
        let corrupted_hash = B256::from([0xdeu8; 32]);

        let mut events = BTreeMap::new();
        events.insert(5, test_batch_event(full_hash));
        let mut slot_withdrawals = BTreeMap::new();
        slot_withdrawals.insert(5, withdrawals);

        let result = resolve_pending_slots(5, 6, &events, &slot_withdrawals, corrupted_hash);
        assert!(result.is_err());
    }
}
