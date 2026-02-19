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

use crate::abi::{BlockTransition, DepositQueueTransition, ZonePortal};
use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::{DynProvider, Provider};
use eyre::Result;
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
    /// Verifier configuration bytes (empty for POC, populated when prover is active).
    pub verifier_config: Bytes,
    /// Proof bytes (empty for POC, populated by `zone_prover::prove_zone_batch`).
    pub proof: Bytes,
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
}

impl BatchSubmitter {
    /// Create a new batch submitter from a shared L1 provider.
    ///
    /// The provider must already include the sequencer wallet for signing.
    pub fn new(
        portal_address: Address,
        provider: DynProvider<TempoNetwork>,
        genesis_tempo_block_number: u64,
    ) -> Self {
        let portal = ZonePortal::new(portal_address, provider.clone());
        Self {
            portal_address,
            l1_provider: provider,
            portal,
            genesis_tempo_block_number,
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
    /// Submit a batch to the ZonePortal on Tempo L1.
    ///
    /// Uses the `verifier_config` and `proof` bytes from [`BatchData`].
    /// When proof generation is enabled, these are populated by
    /// `zone_prover::prove_zone_batch`. When disabled (POC mode), they default
    /// to empty bytes and the verifier contract must accept empty proofs.
    #[instrument(skip_all, fields(
        portal = %self.portal_address,
        tempo_block = batch.tempo_block_number,
        prev_block_hash = %batch.prev_block_hash,
        next_block_hash = %batch.next_block_hash,
        withdrawal_queue_hash = %batch.withdrawal_queue_hash,
        has_proof = !batch.proof.is_empty(),
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
                batch.verifier_config.clone(),
                batch.proof.clone(),
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

    /// Determine whether to use direct or ancestry mode for the given
    /// `tempo_block_number`.
    ///
    /// The ZonePortal's `submitBatch` verifies that the zone committed to a
    /// real Tempo block by reading its hash from the EIP-2935 system contract,
    /// which only stores the most recent [`EIP2935_HISTORY_WINDOW`] (8192)
    /// block hashes. If `tempo_block_number` has already fallen out of that
    /// window (e.g. the sequencer was offline for >2 hours), a direct lookup
    /// would return `bytes32(0)` and the transaction would revert.
    ///
    /// **Ancestry mode** solves this by supplying a *recent* L1 block number
    /// that IS within the EIP-2935 window as the anchor. The portal reads
    /// that anchor's hash from EIP-2935 instead, and the proof is expected to
    /// include a chain of block headers linking the anchor back to
    /// `tempo_block_number`, proving ancestry on-chain.
    ///
    /// A safety margin of [`EIP2935_SAFETY_MARGIN`] (360 blocks, ~3 min) is
    /// applied to guard against the block aging out of the window between our
    /// check here and the transaction's on-chain execution.
    async fn resolve_anchor_mode(&self, tempo_block_number: u64) -> Result<AnchorMode> {
        let current_l1_block = self.l1_provider.get_block_number().await?;

        // EIP-2935 stores the hash of block N when block N+1 is processed, so
        // getBlockHash(N) only returns non-zero when block.number > N. If our
        // tempo_block_number equals the current L1 tip the batch tx would land
        // in the same or next block where getBlockHash would still return zero.
        // Wait until L1 advances past our anchor block.
        if tempo_block_number >= current_l1_block {
            return Err(eyre::eyre!(
                "tempo_block_number ({tempo_block_number}) is not yet confirmed on L1 (tip={current_l1_block}), \
                 will retry after L1 advances"
            ));
        }

        if current_l1_block.saturating_sub(tempo_block_number) < EIP2935_EFFECTIVE_WINDOW {
            return Ok(AnchorMode::Direct);
        }

        // tempo_block_number is outside the EIP-2935 window — use ancestry mode.
        // Pick a recent block well within the window as anchor.
        let anchor_block = current_l1_block.saturating_sub(EIP2935_SAFETY_MARGIN);

        warn!(
            tempo_block_number,
            current_l1_block,
            anchor_block,
            gap = current_l1_block.saturating_sub(tempo_block_number),
            "tempo_block_number outside EIP-2935 window, using ancestry mode"
        );

        Ok(AnchorMode::Ancestry { anchor_block })
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

    /// Read the current `withdrawalBatchIndex` from the ZonePortal on L1.
    pub async fn read_portal_withdrawal_batch_index(&self) -> Result<u64> {
        let index = self.portal.withdrawalBatchIndex().call().await?;
        Ok(index)
    }
}

/// How the batch submitter should anchor `tempoBlockNumber` for EIP-2935
/// verification on the ZonePortal.
#[derive(Debug)]
enum AnchorMode {
    /// `tempoBlockNumber` is within the EIP-2935 window — the portal reads its
    /// hash directly. No extra proof data required.
    Direct,
    /// `tempoBlockNumber` has expired from EIP-2935. A recent L1 block is used
    /// as anchor, and the proof must include block headers linking the anchor
    /// back to `tempoBlockNumber`.
    // TODO: once the verifier is implemented, ancestry mode must also collect
    // the intermediate block headers (from `anchor_block` down to
    // `tempoBlockNumber`) and include them in the proof bytes.
    Ancestry {
        /// Recent L1 block number within the EIP-2935 window, used as the
        /// on-chain anchor for hash verification.
        anchor_block: u64,
    },
}

impl AnchorMode {
    /// Returns the `recentTempoBlockNumber` argument for `submitBatch`:
    /// `0` for direct mode, or the anchor block number for ancestry mode.
    const fn recent_block_number(&self) -> u64 {
        match self {
            Self::Direct => 0,
            Self::Ancestry { anchor_block } => *anchor_block,
        }
    }
}
