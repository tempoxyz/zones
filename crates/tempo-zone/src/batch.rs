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

use std::sync::Arc;

use alloy_primitives::{Address, B256, Bytes};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use eyre::Result;
use tempo_alloy::TempoNetwork;
use tokio::sync::Notify;
use tracing::{error, info, instrument};

use crate::abi::{BlockTransition, DepositQueueTransition, WithdrawalQueueTransition, ZonePortal};

/// Configuration for the L1 batch submitter.
#[derive(Debug, Clone)]
pub struct BatchSubmitterConfig {
    /// ZonePortal contract address on Tempo L1.
    pub portal_address: Address,
    /// Tempo L1 RPC URL (HTTP).
    pub l1_rpc_url: String,
}

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
/// Holds a [`DynProvider`] with the sequencer's signing wallet and a contract
/// instance pointing at the portal.
pub struct BatchSubmitter {
    config: BatchSubmitterConfig,
    portal: ZonePortal::ZonePortalInstance<DynProvider<TempoNetwork>, TempoNetwork>,
}

impl BatchSubmitter {
    /// Create a new batch submitter.
    ///
    /// Builds an HTTP provider with the sequencer wallet and instantiates the
    /// ZonePortal contract for L1 calls.
    pub async fn new(config: BatchSubmitterConfig, signer: PrivateKeySigner) -> Self {
        let wallet = alloy_network::EthereumWallet::from(signer);
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .wallet(wallet)
            .connect(&config.l1_rpc_url)
            .await
            .expect("valid L1 RPC URL")
            .erased();

        let portal = ZonePortal::new(config.portal_address, provider);

        Self { config, portal }
    }

    /// Submit a batch to the ZonePortal on Tempo L1.
    ///
    /// Constructs the on-chain structs from [`BatchData`], sends the
    /// `submitBatch` transaction, and waits for the receipt.
    ///
    /// # POC note
    ///
    /// `verifierConfig` and `proof` are set to empty bytes — the verifier
    /// contract must be configured to accept empty proofs.
    // TODO: pass real proof bytes once proof generation is implemented.
    #[instrument(skip_all, fields(
        portal = %self.config.portal_address,
        tempo_block = batch.tempo_block_number,
        prev_block_hash = %batch.prev_block_hash,
        next_block_hash = %batch.next_block_hash,
        withdrawal_queue_hash = %batch.withdrawal_queue_hash,
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

        let withdrawal_transition = WithdrawalQueueTransition {
            withdrawalQueueHash: batch.withdrawal_queue_hash,
        };

        info!("Submitting batch to ZonePortal on L1");

        let tx_hash = self
            .portal
            .submitBatch(
                batch.tempo_block_number,
                block_transition,
                deposit_transition,
                withdrawal_transition,
                Bytes::new(),
                Bytes::new(),
            )
            .send()
            .await?
            .watch()
            .await?;

        info!(%tx_hash, "Batch submitted to L1");

        Ok(tx_hash)
    }
}

/// Spawn the batch submitter as a background task.
///
/// Listens on `batch_rx` for [`BatchData`] produced by the zone monitor
/// and submits each batch to the ZonePortal on Tempo L1. After a successful
/// submission, notifies the withdrawal processor via `withdrawal_notify` so
/// it can process the newly enqueued withdrawal slot.
pub fn spawn_batch_submitter(
    config: BatchSubmitterConfig,
    signer: PrivateKeySigner,
    mut batch_rx: tokio::sync::mpsc::UnboundedReceiver<BatchData>,
    withdrawal_notify: Arc<Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let submitter = BatchSubmitter::new(config, signer).await;
        while let Some(batch) = batch_rx.recv().await {
            match submitter.submit_batch(&batch).await {
                Ok(tx_hash) => {
                    info!(
                        %tx_hash,
                        next_block_hash = %batch.next_block_hash,
                        "Batch successfully submitted to L1"
                    );
                    withdrawal_notify.notify_one();
                }
                Err(e) => {
                    error!(
                        error = %e,
                        next_block_hash = %batch.next_block_hash,
                        "Failed to submit batch to L1"
                    );
                }
            }
        }

        error!("Batch submission channel closed — shutting down submitter");
    })
}
