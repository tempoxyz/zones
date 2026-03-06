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
//! Three tiers based on the gap between `tempo_block_number` and the current L1 tip:
//!
//! | Gap | Mode | Description |
//! |-----|------|-------------|
//! | < 7832 | Direct | Portal reads hash from EIP-2935. |
//! | 7832–8192 | Ancestry | Collect L1 headers as proof chain. Single submitBatch. |
//! | > 8192 | Stepping | Split into multiple direct-mode submissions. |

use crate::abi::{BlockTransition, DepositQueueTransition, TempoState, ZoneInbox, ZonePortal};
use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::{DynProvider, Provider};
use alloy_rlp::Encodable;
use eyre::Result;
use futures::{StreamExt, TryStreamExt};
use tempo_alloy::TempoNetwork;
use tracing::{info, instrument, warn};

/// EIP-2935 stores the last 8192 block hashes (~68 min at 500ms block time).
/// Blocks older than this require ancestry mode.
const EIP2935_HISTORY_WINDOW: u64 = 8192;

/// Safety margin (~3 min at 500ms block time) to avoid race conditions where
/// the block falls out of the window between our check and on-chain execution.
const EIP2935_SAFETY_MARGIN: u64 = 360;

/// Effective EIP-2935 window after subtracting the safety margin.
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
/// [`DynProvider`] with the sequencer's signing wallet. Also holds a zone L2
/// provider for reading intermediate zone state during stepping mode.
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
    /// Read-only Zone L2 provider for querying intermediate zone state during
    /// stepping mode.
    zone_provider: DynProvider<TempoNetwork>,
    /// TempoState predeploy address on Zone L2.
    tempo_state_address: Address,
    /// ZoneInbox contract address on Zone L2.
    inbox_address: Address,
    /// Concurrency for pipelined L1 header fetching in ancestry mode.
    l1_fetch_concurrency: usize,
}

impl BatchSubmitter {
    /// Create a new batch submitter from a shared L1 provider and a zone L2 provider.
    ///
    /// The L1 provider must already include the sequencer wallet for signing.
    /// The zone provider is read-only, used for querying intermediate zone state
    /// during stepping mode.
    pub fn new(
        portal_address: Address,
        l1_provider: DynProvider<TempoNetwork>,
        zone_provider: DynProvider<TempoNetwork>,
        genesis_tempo_block_number: u64,
        tempo_state_address: Address,
        inbox_address: Address,
    ) -> Self {
        let portal = ZonePortal::new(portal_address, l1_provider.clone());
        Self {
            portal_address,
            l1_provider,
            portal,
            genesis_tempo_block_number,
            zone_provider,
            tempo_state_address,
            inbox_address,
            l1_fetch_concurrency: 16,
        }
    }

    /// Submit a batch to the ZonePortal on Tempo L1.
    ///
    /// Automatically selects **direct mode** or **ancestry mode** based on how
    /// old `tempoBlockNumber` is relative to the current L1 tip:
    ///
    /// - **Direct** (`recentTempoBlockNumber = 0`): used when `tempoBlockNumber`
    ///   is within the EIP-2935 history window (8192 blocks). The portal reads
    ///   its hash directly from EIP-2935.
    /// - **Ancestry** (`recentTempoBlockNumber > 0`): used when `tempoBlockNumber`
    ///   has fallen out of the EIP-2935 window (e.g. sequencer was offline for
    ///   >2 hours). A recent L1 block within the window is chosen as anchor and
    ///   the proof must include a block header chain from `recentTempoBlockNumber`
    ///   back to `tempoBlockNumber`.
    ///
    /// # POC note
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

        info!(?anchor_mode, "Submitting batch to ZonePortal on L1");

        let tx_hash = self
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
            .send()
            .await?
            .with_required_confirmations(1)
            .with_timeout(Some(std::time::Duration::from_secs(30)))
            .watch()
            .await?;

        info!(%tx_hash, "Batch submitted to L1");

        Ok(tx_hash)
    }

    /// Determine the anchor mode for the given `tempo_block_number`.
    ///
    /// Three tiers based on the gap between `tempo_block_number` and L1 tip:
    ///
    /// - **Direct** (gap < [`EIP2935_EFFECTIVE_WINDOW`]): the portal reads the
    ///   hash directly from EIP-2935.
    /// - **Ancestry** (gap within [`EIP2935_HISTORY_WINDOW`]): a recent L1 block
    ///   within the window is used as anchor. The proof must include block headers
    ///   linking the anchor back to `tempo_block_number`.
    /// - **Stepping** (gap > [`EIP2935_HISTORY_WINDOW`]): the gap is too large
    ///   for a single ancestry proof. The batch must be split into multiple
    ///   direct-mode submissions.
    pub(crate) async fn resolve_anchor_mode(&self, tempo_block_number: u64) -> Result<AnchorMode> {
        let current_l1_block = self.l1_provider.get_block_number().await?;

        // EIP-2935 stores the hash of block N when block N+1 is processed, so
        // getBlockHash(N) only returns non-zero when block.number > N. If our
        // tempo_block_number equals the current L1 tip the batch tx would land
        // in the same or next block where getBlockHash would still return zero.
        if tempo_block_number >= current_l1_block {
            return Err(eyre::eyre!(
                "tempo_block_number ({tempo_block_number}) is not yet confirmed on L1 (tip={current_l1_block}), \
                 will retry after L1 advances"
            ));
        }

        let gap = current_l1_block.saturating_sub(tempo_block_number);

        if gap < EIP2935_EFFECTIVE_WINDOW {
            return Ok(AnchorMode::Direct);
        }

        if gap <= EIP2935_HISTORY_WINDOW {
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

            return Ok(AnchorMode::Ancestry {
                anchor_block,
                ancestry_headers,
            });
        }

        // Gap exceeds EIP-2935 history window — need stepping.
        warn!(
            tempo_block_number,
            current_l1_block,
            gap,
            step_size = EIP2935_EFFECTIVE_WINDOW,
            "tempo_block_number far outside EIP-2935 window, using stepping mode"
        );

        Ok(AnchorMode::Stepping {
            step_size: EIP2935_EFFECTIVE_WINDOW,
        })
    }

    /// Fetch and RLP-encode L1 block headers from `from + 1` to `to` (inclusive),
    /// validating the parent-hash chain.
    ///
    /// Returns headers in ascending block-number order. Uses the same concurrent
    /// fetching pattern as the L1 subscriber backfill.
    async fn fetch_ancestry_headers(&self, from: u64, to: u64) -> Result<Vec<Bytes>> {
        use futures::stream;

        if to <= from {
            return Ok(Vec::new());
        }

        let concurrency = self.l1_fetch_concurrency;
        let range_start = from + 1;
        let count = (to - from) as usize;

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
        let mut prev_hash: Option<B256> = None;

        while let Some((block_number, header)) = fetched.try_next().await? {
            // Validate parent-hash chain: each header's parent_hash must match
            // the hash of the previous header.
            if let Some(expected_parent) = prev_hash
                && header.inner.parent_hash != expected_parent
            {
                return Err(eyre::eyre!(
                    "parent-hash chain broken at block {block_number}: \
                     expected parent_hash={expected_parent}, got={}",
                    header.inner.parent_hash
                ));
            }

            // Compute this header's hash for the next iteration's check.
            let mut buf = Vec::with_capacity(600);
            header.encode(&mut buf);
            let header_hash = alloy_primitives::keccak256(&buf);
            prev_hash = Some(header_hash);

            headers.push(Bytes::from(buf));
        }

        Ok(headers)
    }

    /// Find zone L2 block numbers whose `tempoBlockNumber` value can serve as
    /// split points for stepping mode.
    ///
    /// Starting from the zone's latest block, walks backwards to find zone blocks
    /// where `tempoBlockNumber` falls on step boundaries (multiples of `step_size`
    /// blocks from the target `tempo_block_number`). Returns split points in
    /// ascending order.
    pub(crate) async fn find_step_points(
        &self,
        tempo_block_number: u64,
        current_l1_block: u64,
        step_size: u64,
    ) -> Result<Vec<StepPoint>> {
        let tempo_state = TempoState::new(self.tempo_state_address, self.zone_provider.clone());

        // Compute target tempo block numbers for each step boundary.
        let mut targets = Vec::new();
        let mut target = tempo_block_number + step_size;
        while target < current_l1_block.saturating_sub(EIP2935_SAFETY_MARGIN) {
            targets.push(target);
            target += step_size;
        }

        if targets.is_empty() {
            return Ok(Vec::new());
        }

        // Walk backwards from the latest zone block to find zone blocks that
        // match each target tempo_block_number (or the closest one after it).
        let latest_zone_block = self.zone_provider.get_block_number().await?;

        let mut step_points = Vec::with_capacity(targets.len());
        let mut target_idx = targets.len() - 1; // start from largest target

        // Binary search for each target: find the first zone block whose
        // tempoBlockNumber >= target.
        for &target_tempo in targets.iter().rev() {
            // Search for the zone block where tempoBlockNumber transitions past this target.
            let zone_block = self
                .find_zone_block_for_tempo(&tempo_state, target_tempo, 1, latest_zone_block)
                .await?;

            if let Some(zone_block) = zone_block {
                step_points.push(StepPoint {
                    zone_block,
                    target_tempo_block: target_tempo,
                });
            }

            if target_idx == 0 {
                break;
            }
            target_idx -= 1;
        }

        step_points.reverse(); // ascending order
        Ok(step_points)
    }

    /// Binary search for the smallest zone block whose `tempoBlockNumber >= target`.
    async fn find_zone_block_for_tempo(
        &self,
        tempo_state: &TempoState::TempoStateInstance<DynProvider<TempoNetwork>, TempoNetwork>,
        target_tempo: u64,
        lo: u64,
        hi: u64,
    ) -> Result<Option<u64>> {
        if lo > hi {
            return Ok(None);
        }

        let mut low = lo;
        let mut high = hi;
        let mut result = None;

        while low <= high {
            let mid = low + (high - low) / 2;
            let tempo_at_mid: u64 = tempo_state
                .tempoBlockNumber()
                .block(mid.into())
                .call()
                .await
                .unwrap_or(0);

            if tempo_at_mid >= target_tempo {
                result = Some(mid);
                if mid == 0 {
                    break;
                }
                high = mid - 1;
            } else {
                low = mid + 1;
            }
        }

        Ok(result)
    }

    /// Read intermediate zone state at the given zone block number for stepping.
    #[allow(dead_code)]
    pub(crate) async fn fetch_zone_snapshot(&self, zone_block: u64) -> Result<ZoneBlockSnapshot> {
        let tempo_state = TempoState::new(self.tempo_state_address, self.zone_provider.clone());
        let inbox = ZoneInbox::new(self.inbox_address, self.zone_provider.clone());

        let tempo_call = tempo_state.tempoBlockNumber().block(zone_block.into());
        let deposit_call = inbox.processedDepositQueueHash().block(zone_block.into());
        let block_fut = async {
            self.zone_provider
                .get_block_by_number(zone_block.into())
                .await
                .map_err(Into::into)
        };
        let (tempo_block_number, processed_deposit_hash, block) =
            tokio::try_join!(tempo_call.call(), deposit_call.call(), block_fut)?;

        let block_hash = block
            .ok_or_else(|| eyre::eyre!("zone block {zone_block} not found"))?
            .header
            .hash;

        Ok(ZoneBlockSnapshot {
            tempo_block_number,
            processed_deposit_hash,
            block_hash,
        })
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
}

/// How the batch submitter should anchor `tempoBlockNumber` for EIP-2935
/// verification on the ZonePortal.
#[derive(Debug)]
#[allow(dead_code)] // ancestry_headers available for future prover integration
pub(crate) enum AnchorMode {
    /// `tempoBlockNumber` is within the EIP-2935 window — the portal reads its
    /// hash directly. No extra proof data required.
    Direct,
    /// `tempoBlockNumber` has expired from EIP-2935 but is within the full
    /// history window. A recent L1 block is used as anchor, and the collected
    /// headers prove the parent-hash chain from anchor back to `tempoBlockNumber`.
    Ancestry {
        /// Recent L1 block number within the EIP-2935 window, used as the
        /// on-chain anchor for hash verification.
        anchor_block: u64,
        /// RLP-encoded L1 block headers from `tempo_block_number + 1` to
        /// `anchor_block`, in ascending order. Available for the prover to
        /// consume when integrated.
        ancestry_headers: Vec<Bytes>,
    },
    /// The gap exceeds the full EIP-2935 history window. The batch must be split
    /// into multiple direct-mode submissions, each within the effective window.
    Stepping {
        /// Each sub-batch covers at most this many L1 blocks.
        step_size: u64,
    },
}

impl AnchorMode {
    /// Returns the `recentTempoBlockNumber` argument for `submitBatch`:
    /// `0` for direct mode, or the anchor block number for ancestry mode.
    /// Panics if called on `Stepping` — the caller must split before submitting.
    const fn recent_block_number(&self) -> u64 {
        match self {
            Self::Direct => 0,
            Self::Ancestry { anchor_block, .. } => *anchor_block,
            Self::Stepping { .. } => panic!("Stepping mode must be split before submitting"),
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
