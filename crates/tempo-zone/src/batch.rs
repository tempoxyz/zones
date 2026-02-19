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

use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::DynProvider;
use eyre::Result;
use tempo_alloy::TempoNetwork;
use tracing::{info, instrument};

use crate::abi::{BlockTransition, DepositQueueTransition, ZonePortal};

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
    portal_address: Address,
    portal: ZonePortal::ZonePortalInstance<DynProvider<TempoNetwork>, TempoNetwork>,
}

impl BatchSubmitter {
    /// Create a new batch submitter from a shared L1 provider.
    ///
    /// The provider must already include the sequencer wallet for signing.
    pub fn new(portal_address: Address, provider: DynProvider<TempoNetwork>) -> Self {
        let portal = ZonePortal::new(portal_address, provider);
        Self {
            portal_address,
            portal,
        }
    }

    /// Submit a batch to the ZonePortal on Tempo L1.
    ///
    /// Constructs the on-chain structs from [`BatchData`], sends the
    /// `submitBatch` transaction, and waits for the receipt.
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
        let block_transition = BlockTransition {
            prevBlockHash: batch.prev_block_hash,
            nextBlockHash: batch.next_block_hash,
        };

        let deposit_transition = DepositQueueTransition {
            prevProcessedHash: batch.prev_processed_deposit_hash,
            nextProcessedHash: batch.next_processed_deposit_hash,
        };

        info!("Submitting batch to ZonePortal on L1");

        let tx_hash = self
            .portal
            .submitBatch(
                batch.tempo_block_number,
                0u64, // recentTempoBlockNumber: 0 = direct mode (use EIP-2935 lookup)
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

    /// Read the current `blockHash` from the ZonePortal on L1.
    ///
    /// Used to resync the monitor's `prev_block_hash` after repeated submission
    /// failures, ensuring subsequent batches use the portal's actual state.
    pub async fn read_portal_block_hash(&self) -> Result<B256> {
        let hash = self.portal.blockHash().call().await?;
        Ok(hash)
    }

    /// Read the current `withdrawalBatchIndex` from the ZonePortal on L1.
    pub async fn read_portal_withdrawal_batch_index(&self) -> Result<u64> {
        let index = self.portal.withdrawalBatchIndex().call().await?;
        Ok(index)
    }
}
