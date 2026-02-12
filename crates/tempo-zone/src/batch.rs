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
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use eyre::Result;
use tempo_alloy::TempoNetwork;
use tracing::{info, instrument};

use crate::abi::{BlockTransition, DepositQueueTransition, ZonePortal};

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

        info!("Submitting batch to ZonePortal on L1");

        let tx_hash = self
            .portal
            .submitBatch(
                batch.tempo_block_number,
                0u64, // recentTempoBlockNumber: 0 = direct mode (use EIP-2935 lookup)
                block_transition,
                deposit_transition,
                batch.withdrawal_queue_hash,
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

    /// Read the current `blockHash` from the ZonePortal on L1.
    ///
    /// Used to resync the monitor's `prev_block_hash` after repeated submission
    /// failures, ensuring subsequent batches use the portal's actual state.
    pub async fn read_portal_block_hash(&self) -> Result<B256> {
        let hash = self.portal.blockHash().call().await?;
        Ok(hash)
    }
}
