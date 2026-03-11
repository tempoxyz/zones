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

use crate::{
    abi::{self, BlockTransition, DepositQueueTransition, ZoneOutbox, ZonePortal},
    withdrawals::SharedWithdrawalStore,
};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{DynProvider, Provider};
use alloy_rlp::Encodable;
use eyre::Result;
use futures::{StreamExt, TryStreamExt};
use tempo_alloy::TempoNetwork;
use tracing::{info, instrument, warn};

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

        info!(?anchor_mode, "Submitting batch to ZonePortal on L1");

        let tx_hash = self
            .portal
            .submitBatch(
                batch.tempo_block_number,
                anchor_mode.recent_block_number(),
                block_transition,
                deposit_transition,
                batch.withdrawal_queue_hash,
                Bytes::new(),
                Bytes::new(),
            )
            .send()
            .await?
            .with_required_confirmations(1)
            .with_timeout(Some(std::time::Duration::from_secs(30)))
            .watch()
            .await?;

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

    /// Restore pending withdrawal data into the store by replaying L1 portal
    /// `BatchSubmitted` events and fetching `WithdrawalRequested` events from
    /// the zone L2 outbox.
    ///
    /// Returns the number of restored withdrawals.
    #[instrument(skip_all, fields(portal = %self.portal_address))]
    pub async fn restore_pending_withdrawals(
        &self,
        zone_provider: &DynProvider<TempoNetwork>,
        outbox_address: Address,
        store: &SharedWithdrawalStore,
    ) -> Result<u64> {
        let (head, tail) = tokio::try_join!(
            self.read_portal_withdrawal_queue_head(),
            self.read_portal_withdrawal_queue_tail(),
        )?;

        if head >= tail {
            return Ok(0);
        }

        info!(
            head,
            tail,
            pending = tail - head,
            "Restoring pending withdrawals from zone L2 events"
        );

        // Fetch all BatchSubmitted events from the portal.
        let all_batch_events = self
            .portal
            .BatchSubmitted_filter()
            .from_block(0)
            .query()
            .await?;

        if all_batch_events.is_empty() {
            return Ok(0);
        }

        // Resolve each event's nextBlockHash to a zone block number.
        let mut zone_end_blocks = Vec::with_capacity(all_batch_events.len());
        for (event, _) in &all_batch_events {
            let block = zone_provider
                .get_block_by_hash(event.nextBlockHash)
                .await?
                .ok_or_else(|| {
                    eyre::eyre!("zone block not found for hash {}", event.nextBlockHash)
                })?;
            zone_end_blocks.push(block.number());
        }

        // Build per-slot info for events with non-zero withdrawalQueueHash.
        // The j-th such event corresponds to portal slot j.
        struct SlotInfo {
            zone_start: u64,
            zone_end: u64,
            withdrawal_queue_hash: B256,
        }

        let mut slot_infos: BTreeMap<u64, SlotInfo> = BTreeMap::new();
        for (i, (event, _)) in all_batch_events.iter().enumerate() {
            if event.withdrawalQueueHash.is_zero() {
                continue;
            }
            let zone_start = if i == 0 {
                1
            } else {
                zone_end_blocks[i - 1] + 1
            };
            slot_infos.insert(
                event.withdrawalBatchIndex,
                SlotInfo {
                    zone_start,
                    zone_end: zone_end_blocks[i],
                    withdrawal_queue_hash: event.withdrawalQueueHash,
                },
            );
        }

        let outbox = ZoneOutbox::new(outbox_address, zone_provider.clone());
        let mut total_restored = 0u64;

        for slot in head..tail {
            let Some(info) = slot_infos.get(&slot) else {
                warn!(
                    slot,
                    "no BatchSubmitted event found for pending portal slot"
                );
                continue;
            };

            // Fetch WithdrawalRequested events from zone L2 in the batch's block range.
            let events = outbox
                .WithdrawalRequested_filter()
                .from_block(info.zone_start)
                .to_block(info.zone_end)
                .query()
                .await?;

            // Sort by (block_number, tx_index, log_index) — same pattern as ZoneMonitor::fetch_withdrawals.
            let mut events_sorted: Vec<_> = events
                .into_iter()
                .map(|(event, log)| {
                    let key = (
                        log.block_number.unwrap_or(0),
                        log.transaction_index.unwrap_or(0),
                        log.log_index.unwrap_or(0),
                    );
                    (key, event)
                })
                .collect();
            events_sorted.sort_by_key(|(key, _)| *key);

            let withdrawals: Vec<abi::Withdrawal> = events_sorted
                .into_iter()
                .map(|(_, e)| abi::Withdrawal {
                    token: e.token,
                    sender: e.sender,
                    to: e.to,
                    amount: e.amount,
                    fee: e.fee,
                    memo: e.memo,
                    gasLimit: e.gasLimit,
                    fallbackRecipient: e.fallbackRecipient,
                    callbackData: e.data,
                })
                .collect();

            if withdrawals.is_empty() {
                warn!(
                    slot,
                    zone_start = info.zone_start,
                    zone_end = info.zone_end,
                    "no withdrawal events found for slot with non-zero hash"
                );
                continue;
            }

            // Verify hash chain matches the on-chain value.
            let computed = abi::Withdrawal::queue_hash(&withdrawals);
            if computed != info.withdrawal_queue_hash {
                warn!(
                    slot,
                    computed = %computed,
                    expected = %info.withdrawal_queue_hash,
                    "withdrawal hash mismatch, skipping slot"
                );
                continue;
            }

            // For the head slot, determine how many withdrawals have already been
            // processed by comparing against the current on-chain slot hash.
            let to_store = if slot == head {
                let slot_hash: B256 = self
                    .portal
                    .withdrawalQueueSlot(U256::from(slot % WITHDRAWAL_QUEUE_CAPACITY))
                    .call()
                    .await?;

                match find_processed_offset(&withdrawals, slot_hash) {
                    Some(offset) => &withdrawals[offset..],
                    None => {
                        warn!(
                            slot,
                            %slot_hash,
                            "could not determine processed offset for head slot"
                        );
                        continue;
                    }
                }
            } else {
                &withdrawals[..]
            };

            if !to_store.is_empty() {
                let count = to_store.len();
                let mut guard = store.lock();
                for w in to_store {
                    guard.add_withdrawal(slot, w.clone());
                }
                info!(slot, count, "Restored withdrawals for portal queue slot");
                total_restored += count as u64;
            }
        }

        Ok(total_restored)
    }
}

/// Find the offset into `withdrawals` where the remaining hash chain matches
/// `current_slot_hash`. Returns `Some(0)` if no withdrawals have been processed,
/// `Some(n)` if n have been processed, or `None` if no match is found.
fn find_processed_offset(
    withdrawals: &[abi::Withdrawal],
    current_slot_hash: B256,
) -> Option<usize> {
    for offset in 0..withdrawals.len() {
        let hash = abi::Withdrawal::queue_hash(&withdrawals[offset..]);
        if hash == current_slot_hash {
            return Some(offset);
        }
    }
    None
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
