//! L1 batch submitter for the zone sequencer.
//!
//! This module handles **Tempo L1** interactions — all transactions go to the
//! [`ZonePortal`](crate::abi::ZonePortal) contract deployed on L1. The sequencer
//! signing key is used for every L1 transaction.
//!
//! [`BatchData`] is produced by the zone monitor and passed to the submitter.
//!
//! # POC limitations
//!
//! Proof validation is currently **skipped** by the stub verifier. Both direct
//! and ancestry submissions use empty proof bytes until real proof generation is
//! implemented.
//!
//! # Anchor modes
//!
//! | Gap | Mode | Description |
//! |-----|------|-------------|
//! | < configured effective window | Direct | Portal reads hash from EIP-2935. |
//! | ≥ configured effective window | Ancestry | Use a recent anchor and collect ancestry headers for the batch. |
//!
//! [`AnchorMode`] handles submissions whose `tempoBlockNumber` is outside the
//! configured direct window by falling back to ancestry mode — a recent anchor
//! block plus a locally validated parent-hash header chain.

use std::collections::BTreeMap;

use crate::abi::{self, BlockTransition, DepositQueueTransition, ZoneOutbox, ZonePortal};
use alloy_consensus::Transaction;
use alloy_network::ReceiptResponse;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_provider::{DynProvider, Provider};
use alloy_rlp::Encodable;
use alloy_sol_types::{SolCall, SolEvent};
use eyre::Result;
use futures::{StreamExt, TryStreamExt};
use tempo_alloy::{TempoNetwork, rpc::TempoCallBuilderExt};
use tracing::{info, instrument, warn};

use crate::nonce_keys::SUBMIT_BATCH_NONCE_KEY;

/// EIP-2935 stores the last 8192 block hashes, so the usable window is 8191 blocks.
const DEFAULT_EIP2935_HISTORY_WINDOW: u64 = 8192 - 1;

/// Safety margin (~3 min at 500ms block time) to avoid race conditions where
/// the block falls out of the window between our check and on-chain execution.
const DEFAULT_EIP2935_SAFETY_MARGIN: u64 = 360;

/// EIP-2935 anchor limits used by the batch submitter.
///
/// Production uses the real 8191-block EIP-2935 history window with a safety
/// margin. This type exists primarily so tests can shrink that otherwise large
/// window and exercise ancestry behavior without mining thousands of L1 blocks.
/// Production code should normally use [`Default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchAnchorConfig {
    /// Total L1 block-hash history window to treat as available for EIP-2935
    /// anchoring.
    history_window: u64,
    /// Number of most-recent L1 blocks to avoid when choosing an anchor, reducing
    /// the chance that an anchor ages out before the on-chain transaction lands.
    safety_margin: u64,
}

impl BatchAnchorConfig {
    /// Build an anchor config with explicit limits.
    pub fn new(history_window: u64, safety_margin: u64) -> Result<Self> {
        if history_window == 0 {
            return Err(eyre::eyre!("EIP-2935 history window must be non-zero"));
        }

        if safety_margin >= history_window {
            return Err(eyre::eyre!(
                "EIP-2935 safety margin ({safety_margin}) must be smaller than history window ({history_window})"
            ));
        }

        Ok(Self {
            history_window,
            safety_margin,
        })
    }

    /// Configured history window in L1 blocks.
    pub const fn history_window(self) -> u64 {
        self.history_window
    }

    /// Configured safety margin in L1 blocks.
    pub const fn safety_margin(self) -> u64 {
        self.safety_margin
    }

    /// Effective direct-submission window after subtracting the safety margin.
    pub const fn effective_window(self) -> u64 {
        self.history_window - self.safety_margin
    }
}

impl Default for BatchAnchorConfig {
    fn default() -> Self {
        Self {
            history_window: DEFAULT_EIP2935_HISTORY_WINDOW,
            safety_margin: DEFAULT_EIP2935_SAFETY_MARGIN,
        }
    }
}

/// Maximum number of pending withdrawal queue slots in the portal ring buffer.
pub(crate) const WITHDRAWAL_QUEUE_CAPACITY: u64 = 100;

/// Maximum zone-block span for a single `eth_getLogs` request during catch-up.
///
/// Large backlog scans can exceed the zone node's RPC response size limit if we
/// query the entire unsent range in one request.
pub(crate) const LOG_QUERY_BLOCK_CHUNK: u64 = 5_000;

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
    /// Deposit counter at the start of processing.
    pub prev_deposit_number: u64,
    /// Deposit counter after processing.
    pub next_deposit_number: u64,
    /// Withdrawal queue hash for this batch (`B256::ZERO` if no withdrawals).
    pub withdrawal_queue_hash: B256,
}

/// One L2 withdrawal batch finalized by `ZoneOutbox`.
#[derive(Debug, Clone)]
pub(crate) struct FinalizedBatch {
    /// Authoritative hash emitted by `BatchFinalized` and stored in `lastBatch()`.
    pub finalized_hash: B256,
    /// Authoritative L2 withdrawal batch index emitted by `BatchFinalized`.
    pub finalized_index: u64,
    /// Reconstructed withdrawal payloads for the off-chain processor store.
    pub withdrawals: Vec<abi::Withdrawal>,
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
    /// EIP-2935 history and safety-margin limits used for anchor decisions.
    anchor_config: BatchAnchorConfig,
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
        Self::with_anchor_config(
            portal_address,
            l1_provider,
            genesis_tempo_block_number,
            BatchAnchorConfig::default(),
        )
    }

    /// Create a new batch submitter with custom EIP-2935 anchor limits.
    pub fn with_anchor_config(
        portal_address: Address,
        l1_provider: DynProvider<TempoNetwork>,
        genesis_tempo_block_number: u64,
        anchor_config: BatchAnchorConfig,
    ) -> Self {
        let portal = ZonePortal::new(portal_address, l1_provider.clone());
        Self {
            portal_address,
            l1_provider,
            portal,
            genesis_tempo_block_number,
            l1_fetch_concurrency: 16,
            anchor_config,
        }
    }

    /// Submit a batch to the ZonePortal on Tempo L1.
    ///
    /// Resolves the anchor mode based on how old `tempo_block_number` is:
    ///
    /// - **Direct** — `tempo_block_number` is within the configured effective window,
    ///   the portal reads its hash directly from EIP-2935.
    /// - **Ancestry** — `tempo_block_number` is outside the effective window. A
    ///   recent anchor block is used and ancestry headers are collected (for
    ///   future prover integration).
    ///
    /// `verifierConfig` and `proof` are empty until real proof generation is
    /// implemented.
    ///
    /// Returns the `BatchSubmitted` event decoded from the confirmed receipt.
    // TODO: pass real proof bytes once proof generation is implemented.
    #[instrument(skip_all, fields(
        portal = %self.portal_address,
        tempo_block = batch.tempo_block_number,
        prev_block_hash = %batch.prev_block_hash,
        next_block_hash = %batch.next_block_hash,
        withdrawal_queue_hash = %batch.withdrawal_queue_hash,
    ))]
    pub async fn submit_batch(&self, batch: &BatchData) -> Result<ZonePortal::BatchSubmitted> {
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
            prevDepositNumber: batch.prev_deposit_number,
            nextDepositNumber: batch.next_deposit_number,
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
                // verifierConfig and proof stay empty until real proof generation is wired in.
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

        let event = self.decode_batch_submitted(receipt.logs())?;

        info!(
            %tx_hash,
            withdrawal_batch_index = event.withdrawalBatchIndex,
            withdrawal_queue_index = %event.withdrawalQueueIndex,
            "Batch submitted to L1"
        );

        Ok(event)
    }

    /// Decode the `BatchSubmitted` event from a confirmed `submitBatch` receipt's logs.
    fn decode_batch_submitted(
        &self,
        logs: &[alloy_rpc_types_eth::Log],
    ) -> Result<ZonePortal::BatchSubmitted> {
        logs.iter()
            .filter(|log| log.address() == self.portal_address)
            .find_map(|log| ZonePortal::BatchSubmitted::decode_log(&log.inner).ok())
            .map(|log| log.data)
            .ok_or_else(|| {
                eyre::eyre!("confirmed submitBatch receipt is missing the BatchSubmitted event")
            })
    }

    /// Resolve the anchor mode for the given `tempo_block_number`.
    ///
    /// - **Direct** (gap < configured effective window): the portal reads the
    ///   hash directly from EIP-2935.
    /// - **Ancestry** (gap ≥ configured effective window): a recent L1 block
    ///   behind the configured safety margin is used as anchor. Ancestry headers
    ///   are collected and validated for future prover integration.
    async fn resolve_anchor_mode(&self, tempo_block_number: u64) -> Result<AnchorMode> {
        let current_l1_block = self.l1_provider.get_block_number().await?;

        if tempo_block_number >= current_l1_block {
            return Err(eyre::eyre!(
                "tempo_block_number ({tempo_block_number}) is not yet confirmed on L1 \
                 (tip={current_l1_block}), will retry after L1 advances"
            ));
        }

        let gap = current_l1_block.saturating_sub(tempo_block_number);

        if gap < self.anchor_config.effective_window() {
            return Ok(AnchorMode::Direct);
        }

        let anchor_block = current_l1_block.saturating_sub(self.anchor_config.safety_margin());
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

    /// Read the current logical withdrawal queue tail from the ZonePortal on L1.
    pub async fn read_portal_withdrawal_queue_tail(&self) -> Result<u64> {
        let tail = self.portal.withdrawalQueueTail().call().await?;
        let tail: u64 = tail
            .try_into()
            .map_err(|_| eyre::eyre!("withdrawal queue tail overflow"))?;
        Ok(tail)
    }

    /// Read the current withdrawal batch index from the ZonePortal on L1.
    pub async fn read_portal_withdrawal_batch_index(&self) -> Result<u64> {
        Ok(self.portal.withdrawalBatchIndex().call().await?)
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
    /// 2. Querying the `BatchSubmitted` event for each pending slot (plus the
    ///    predecessor for zone block range boundaries) via the indexed
    ///    `withdrawalQueueIndex` topic.
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

        // Step 2: query BatchSubmitted events for pending slots [head, tail)
        // plus the predecessor (head-1) by their indexed withdrawalQueueIndex.
        let events = self
            .find_batch_events_by_index(head.saturating_sub(1), tail)
            .await?;

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

    /// Fetch `BatchSubmitted` events for logical queue indices `[first_index, tail)`
    /// by walking L1 backwards in chunks while filtering by the indexed
    /// `withdrawalQueueIndex` topic. Logical queue indices never repeat
    /// (head/tail are non-wrapping counters), so the topic filter identifies
    /// each batch exactly without positional counting.
    ///
    /// The caller passes `first_index = head - 1` so the predecessor batch is
    /// included (its `nextBlockHash` bounds the zone block range of the first
    /// pending slot). When `head == 0` the predecessor does not exist; the
    /// caller falls back to zone block 1.
    async fn find_batch_events_by_index(
        &self,
        first_index: u64,
        tail: u64,
    ) -> Result<BTreeMap<u64, abi::ZonePortal::BatchSubmitted>> {
        if first_index >= tail {
            return Ok(BTreeMap::new());
        }

        let index_topics: Vec<B256> = (first_index..tail)
            .map(|index| B256::from(U256::from(index)))
            .collect();
        let needed = index_topics.len();

        let mut found = BTreeMap::new();
        let mut hi = self.l1_provider.get_block_number().await?;

        while hi >= self.genesis_tempo_block_number && found.len() < needed {
            let lo = backward_log_query_start(hi, self.genesis_tempo_block_number);

            let events = self
                .portal
                .BatchSubmitted_filter()
                .topic2(index_topics.clone())
                .from_block(lo)
                .to_block(hi)
                .query()
                .await?;

            for (event, _) in events {
                let index: u64 = event.withdrawalQueueIndex.try_into().map_err(|_| {
                    eyre::eyre!("withdrawal queue index overflow in BatchSubmitted")
                })?;
                if found.insert(index, event).is_some() {
                    eyre::bail!("duplicate BatchSubmitted event for portal queue index {index}");
                }
            }

            if lo == self.genesis_tempo_block_number {
                break;
            }
            hi = lo - 1;
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
pub(crate) fn find_processed_offset(
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

#[derive(Debug)]
struct RequestedWithdrawalLog {
    block_number: u64,
    tx_index: u64,
    log_index: u64,
    tx_hash: B256,
    event: abi::ZoneOutbox::WithdrawalRequested,
}

#[derive(Debug, Clone)]
struct FinalizedBatchLog {
    block_number: u64,
    tx_index: u64,
    log_index: u64,
    tx_hash: B256,
    withdrawal_queue_hash: B256,
    withdrawal_batch_index: u64,
}

/// Fetch all zone block numbers in `[from, to]` that finalized a withdrawal batch.
///
/// This includes zero-withdrawal batches because they still advance the L2
/// withdrawal batch index and therefore require a matching L1 `submitBatch`.
pub(crate) async fn fetch_finalized_batch_boundaries(
    outbox: &ZoneOutbox::ZoneOutboxInstance<DynProvider<TempoNetwork>, TempoNetwork>,
    from: u64,
    to: u64,
) -> Result<Vec<u64>> {
    if from > to {
        return Ok(Vec::new());
    }

    let mut boundaries: Vec<_> = outbox
        .BatchFinalized_filter()
        .from_block(from)
        .to_block(to)
        .chunked()
        .chunk_size(LOG_QUERY_BLOCK_CHUNK)
        .concurrent(2)
        .query()
        .await?
        .into_iter()
        .map(|(_, log)| log.block_number.unwrap_or(0))
        .collect();

    boundaries.sort_unstable();
    boundaries.dedup();
    Ok(boundaries)
}

/// Fetch one finalized L2 withdrawal batch for a range ending at `to`.
///
/// The submitted hash and index come from the batch's `BatchFinalized` event.
/// Withdrawal structs are reconstructed from `WithdrawalRequested` logs since
/// the immediately preceding batch boundary so the off-chain processor can
/// service the portal queue.
pub(crate) async fn fetch_finalized_batch(
    outbox: &ZoneOutbox::ZoneOutboxInstance<DynProvider<TempoNetwork>, TempoNetwork>,
    zone_provider: &DynProvider<TempoNetwork>,
    from: u64,
    to: u64,
) -> Result<FinalizedBatch> {
    let mut finalized_batches = fetch_finalized_batch_logs(outbox, from, to).await?;

    if finalized_batches.is_empty() {
        return Err(eyre::eyre!(
            "range {from}..={to} does not contain a BatchFinalized boundary"
        ));
    }

    finalized_batches.sort_by_key(|batch| (batch.block_number, batch.tx_index, batch.log_index));
    let target_position = finalized_batches
        .iter()
        .rposition(|batch| batch.block_number == to)
        .ok_or_else(|| {
            eyre::eyre!("range {from}..={to} does not end on a BatchFinalized boundary")
        })?;

    if finalized_batches
        .iter()
        .filter(|batch| batch.block_number == to)
        .count()
        != 1
    {
        return Err(eyre::eyre!(
            "zone block {to} contains more than one BatchFinalized event"
        ));
    }

    let target = finalized_batches[target_position].clone();
    let previous_boundary = finalized_batches[..target_position]
        .last()
        .map(|batch| batch.block_number)
        .unwrap_or(from.saturating_sub(1));
    let request_from = previous_boundary.saturating_add(1);

    let requests = if request_from <= to {
        fetch_requested_withdrawal_logs(outbox, request_from, to).await?
    } else {
        Vec::new()
    };

    let finalize_tx = zone_provider
        .get_transaction_by_hash(target.tx_hash)
        .await?
        .ok_or_else(|| {
            eyre::eyre!(
                "missing finalizeWithdrawalBatch tx {} for zone block {}",
                target.tx_hash,
                target.block_number
            )
        })?;
    let encrypted_senders =
        abi::ZoneOutbox::finalizeWithdrawalBatchCall::abi_decode(finalize_tx.input().as_ref())
            .map_err(|err| {
                eyre::eyre!(
                    "failed to decode finalizeWithdrawalBatch calldata for {}: {err}",
                    target.tx_hash
                )
            })?
            .encryptedSenders;

    if encrypted_senders.len() != requests.len() {
        return Err(eyre::eyre!(
            "encrypted sender count mismatch for batch ending at zone block {}: {} encrypted senders for {} requests",
            target.block_number,
            encrypted_senders.len(),
            requests.len()
        ));
    }

    let withdrawals = requests
        .into_iter()
        .zip(encrypted_senders)
        .map(|(request, encrypted_sender)| {
            abi::Withdrawal::from_requested_event(&request.event, request.tx_hash, encrypted_sender)
        })
        .collect::<Vec<_>>();

    let recomputed_hash = abi::Withdrawal::queue_hash(&withdrawals);
    if recomputed_hash != target.withdrawal_queue_hash {
        return Err(eyre::eyre!(
            "withdrawal hash mismatch for batch ending at zone block {}: event hash {}, reconstructed hash {}",
            target.block_number,
            target.withdrawal_queue_hash,
            recomputed_hash
        ));
    }

    Ok(FinalizedBatch {
        finalized_hash: target.withdrawal_queue_hash,
        finalized_index: target.withdrawal_batch_index,
        withdrawals,
    })
}

/// Fetch `WithdrawalRequested` events for one portal queue slot.
pub(crate) async fn fetch_slot_withdrawals(
    outbox: &ZoneOutbox::ZoneOutboxInstance<DynProvider<TempoNetwork>, TempoNetwork>,
    zone_provider: &DynProvider<TempoNetwork>,
    from: u64,
    to: u64,
) -> Result<Vec<abi::Withdrawal>> {
    Ok(fetch_finalized_batch(outbox, zone_provider, from, to)
        .await?
        .withdrawals)
}

async fn fetch_requested_withdrawal_logs(
    outbox: &ZoneOutbox::ZoneOutboxInstance<DynProvider<TempoNetwork>, TempoNetwork>,
    from: u64,
    to: u64,
) -> Result<Vec<RequestedWithdrawalLog>> {
    let mut requests: Vec<_> = outbox
        .WithdrawalRequested_filter()
        .from_block(from)
        .to_block(to)
        .chunked()
        .chunk_size(LOG_QUERY_BLOCK_CHUNK)
        .concurrent(2)
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

    Ok(requests)
}

async fn fetch_finalized_batch_logs(
    outbox: &ZoneOutbox::ZoneOutboxInstance<DynProvider<TempoNetwork>, TempoNetwork>,
    from: u64,
    to: u64,
) -> Result<Vec<FinalizedBatchLog>> {
    let mut finalized_batches: Vec<_> = outbox
        .BatchFinalized_filter()
        .from_block(from)
        .to_block(to)
        .chunked()
        .chunk_size(LOG_QUERY_BLOCK_CHUNK)
        .concurrent(2)
        .query()
        .await?
        .into_iter()
        .map(|(event, log)| -> Result<_> {
            Ok(FinalizedBatchLog {
                block_number: log.block_number.unwrap_or(0),
                tx_index: log.transaction_index.unwrap_or(0),
                log_index: log.log_index.unwrap_or(0),
                tx_hash: log
                    .transaction_hash
                    .ok_or_else(|| eyre::eyre!("BatchFinalized log missing transaction hash"))?,
                withdrawal_queue_hash: event.withdrawalQueueHash,
                withdrawal_batch_index: event.withdrawalBatchIndex,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    finalized_batches.sort_by_key(|batch| (batch.block_number, batch.tx_index, batch.log_index));
    Ok(finalized_batches)
}

/// Lazily split an inclusive block range into bounded query windows.
pub(crate) fn log_query_ranges(from: u64, to: u64) -> impl Iterator<Item = (u64, u64)> {
    std::iter::successors(Some(from), move |&start| {
        let end = start.saturating_add(LOG_QUERY_BLOCK_CHUNK - 1).min(to);
        if end >= to { None } else { end.checked_add(1) }
    })
    .map(move |start| {
        (
            start,
            start.saturating_add(LOG_QUERY_BLOCK_CHUNK - 1).min(to),
        )
    })
}

fn backward_log_query_start(hi: u64, floor: u64) -> u64 {
    hi.saturating_sub(LOG_QUERY_BLOCK_CHUNK - 1).max(floor)
}

/// How the batch submitter anchors `tempoBlockNumber` for EIP-2935 verification.
///
/// Resolved by [`BatchSubmitter::resolve_anchor_mode`] inside `submit_batch`.
/// `submit_batch` can use ancestry mode when the batch-final block's
/// `tempoBlockNumber` has fallen outside the configured direct-submission
/// window.
#[derive(Debug)]
#[allow(dead_code)] // Ancestry::ancestry_headers is collected but not yet consumed — available for prover integration
enum AnchorMode {
    /// `tempoBlockNumber` is within the effective EIP-2935 window — the portal
    /// reads its hash directly. No extra proof data required.
    Direct,
    /// `tempoBlockNumber` is outside the effective window. A recent L1 block is
    /// used as anchor, and the collected headers prove the parent-hash chain.
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

/// Zone L2 state read at a specific block, used to populate [`BatchData`].
pub(crate) struct ZoneBlockSnapshot {
    /// Latest Tempo L1 block number as seen by the zone.
    pub tempo_block_number: u64,
    /// Cumulative hash of all deposits processed by the zone up to this block.
    pub processed_deposit_hash: B256,
    /// Total number of deposits processed by the zone up to this block.
    pub processed_deposit_number: u64,
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
    fn batch_anchor_config_validates_effective_window() {
        let config = BatchAnchorConfig::new(10, 4).unwrap();
        assert_eq!(config.history_window(), 10);
        assert_eq!(config.safety_margin(), 4);
        assert_eq!(config.effective_window(), 6);

        assert!(BatchAnchorConfig::new(0, 0).is_err());
        assert!(BatchAnchorConfig::new(10, 10).is_err());
        assert!(BatchAnchorConfig::new(10, 11).is_err());
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

    #[test]
    fn log_query_ranges_chunk_large_ranges() {
        let end = 100 + (LOG_QUERY_BLOCK_CHUNK * 2) + 234;
        let ranges: Vec<_> = log_query_ranges(100, end).collect();

        assert_eq!(ranges.len(), 3);
        assert_eq!(ranges[0], (100, 100 + LOG_QUERY_BLOCK_CHUNK - 1));
        assert_eq!(
            ranges[1],
            (
                100 + LOG_QUERY_BLOCK_CHUNK,
                100 + (LOG_QUERY_BLOCK_CHUNK * 2) - 1
            )
        );
        assert_eq!(ranges[2], (100 + (LOG_QUERY_BLOCK_CHUNK * 2), end));
    }

    #[test]
    fn backward_log_query_window_is_bounded() {
        let hi = 10_000;
        let lo = backward_log_query_start(hi, 0);
        assert_eq!(lo, hi - LOG_QUERY_BLOCK_CHUNK + 1);
        assert_eq!(hi - lo + 1, LOG_QUERY_BLOCK_CHUNK);
        assert_eq!(backward_log_query_start(100, 50), 50);
    }

    fn test_batch_event(withdrawal_queue_hash: B256) -> abi::ZonePortal::BatchSubmitted {
        abi::ZonePortal::BatchSubmitted {
            withdrawalBatchIndex: 0,
            withdrawalQueueIndex: U256::ZERO,
            nextProcessedDepositQueueHash: B256::ZERO,
            nextBlockHash: B256::ZERO,
            withdrawalQueueHash: withdrawal_queue_hash,
            lastProcessedDepositNumber: 0,
        }
    }

    #[test]
    fn decode_batch_submitted_from_receipt_logs() {
        use alloy_provider::ProviderBuilder;
        use alloy_transport::mock::Asserter;

        let portal_address = address!("0x7069DeC4E64Fd07334A0933eDe836C17259c9B23");
        let provider = ProviderBuilder::new_with_network::<tempo_alloy::TempoNetwork>()
            .connect_mocked_client(Asserter::new())
            .erased();
        let submitter = BatchSubmitter::new(portal_address, provider, 0);

        let event = abi::ZonePortal::BatchSubmitted {
            withdrawalBatchIndex: 7,
            withdrawalQueueIndex: U256::from(3),
            nextProcessedDepositQueueHash: B256::repeat_byte(0x11),
            nextBlockHash: B256::repeat_byte(0x22),
            withdrawalQueueHash: B256::repeat_byte(0x33),
            lastProcessedDepositNumber: 9,
        };
        let log = alloy_rpc_types_eth::Log {
            inner: alloy_primitives::Log {
                address: portal_address,
                data: event.encode_log_data(),
            },
            ..Default::default()
        };
        let unrelated = alloy_rpc_types_eth::Log {
            inner: alloy_primitives::Log {
                address: Address::repeat_byte(0x99),
                data: event.encode_log_data(),
            },
            ..Default::default()
        };

        let decoded = submitter
            .decode_batch_submitted(&[unrelated.clone(), log])
            .unwrap();
        assert_eq!(decoded.withdrawalBatchIndex, 7);
        assert_eq!(decoded.withdrawalQueueIndex, U256::from(3));
        assert_eq!(decoded.nextBlockHash, B256::repeat_byte(0x22));

        assert!(submitter.decode_batch_submitted(&[unrelated]).is_err());
    }

    #[tokio::test]
    async fn finds_batch_events_by_logical_index_across_ring_wrap() {
        use alloy_provider::ProviderBuilder;
        use alloy_transport::mock::Asserter;

        let portal_address = address!("0x7069DeC4E64Fd07334A0933eDe836C17259c9B23");
        let asserter = Asserter::new();
        let provider = ProviderBuilder::new_with_network::<tempo_alloy::TempoNetwork>()
            .connect_mocked_client(asserter.clone())
            .erased();
        let submitter = BatchSubmitter::new(portal_address, provider, 0);

        asserter.push_success(&10_000_u64);
        let logs: Vec<_> = [99_u64, 100, 101]
            .into_iter()
            .map(|index| {
                let event = abi::ZonePortal::BatchSubmitted {
                    withdrawalBatchIndex: index + 20,
                    withdrawalQueueIndex: U256::from(index),
                    nextProcessedDepositQueueHash: B256::ZERO,
                    nextBlockHash: B256::from(U256::from(index + 1)),
                    withdrawalQueueHash: B256::from(U256::from(index + 2)),
                    lastProcessedDepositNumber: 0,
                };
                alloy_rpc_types_eth::Log {
                    inner: alloy_primitives::Log {
                        address: portal_address,
                        data: event.encode_log_data(),
                    },
                    block_number: Some(9_900 + index),
                    ..Default::default()
                }
            })
            .collect();
        asserter.push_success(&logs);

        let events = submitter.find_batch_events_by_index(99, 102).await.unwrap();

        assert_eq!(
            events.keys().copied().collect::<Vec<_>>(),
            vec![99, 100, 101]
        );
        assert_eq!(events[&99].withdrawalBatchIndex, 119);
        assert_eq!(events[&100].withdrawalQueueIndex, U256::from(100));
        assert_eq!(events[&101].withdrawalQueueIndex, U256::from(101));
        assert!(asserter.read_q().is_empty());
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
