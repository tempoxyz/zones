//! L1 submission logic for submitting batches to the ZonePortal contract.

use crate::types::{BatchCommitment, IZonePortal, ProofBundle, SolBatchCommitment};
use alloy_network::{Ethereum, EthereumWallet, TransactionBuilder};
use alloy_primitives::{Address, Bytes, B256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::TransactionReceipt;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall;
use eyre::{eyre, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Configuration for the L1 submitter.
#[derive(Debug, Clone)]
pub struct SubmitterConfig {
    /// ZonePortal contract address on L1 (Tempo).
    pub portal_address: Address,
    /// Sequencer private key for signing transactions.
    pub sequencer_key: B256,
    /// L1 RPC URL.
    pub l1_rpc_url: String,
    /// L1 chain ID.
    pub chain_id: u64,
    /// Maximum number of retry attempts for transient failures.
    pub max_retries: u32,
    /// Base delay between retries (exponential backoff).
    pub retry_delay: Duration,
    /// Gas limit for submitBatch transactions.
    pub gas_limit: u64,
    /// Max fee per gas (in wei). If None, will be estimated.
    pub max_fee_per_gas: Option<u128>,
    /// Max priority fee per gas (in wei). If None, will be estimated.
    pub max_priority_fee_per_gas: Option<u128>,
}

impl Default for SubmitterConfig {
    fn default() -> Self {
        Self {
            portal_address: Address::ZERO,
            sequencer_key: B256::ZERO,
            l1_rpc_url: String::new(),
            chain_id: 1,
            max_retries: 3,
            retry_delay: Duration::from_secs(1),
            gas_limit: 500_000,
            max_fee_per_gas: None,
            max_priority_fee_per_gas: None,
        }
    }
}

/// A pending batch submission.
#[derive(Debug, Clone)]
pub struct PendingSubmission {
    pub tx_hash: B256,
    pub batch_index: u64,
    pub submitted_at: std::time::Instant,
}

/// Result of a batch submission.
#[derive(Debug, Clone)]
pub struct SubmissionResult {
    pub tx_hash: B256,
    pub batch_index: u64,
    pub gas_used: u64,
}

/// Submits batches to the ZonePortal contract on L1.
pub struct L1Submitter {
    config: SubmitterConfig,
    provider: Box<dyn Provider<Ethereum>>,
    sequencer_address: Address,
    nonce: AtomicU64,
    pending_submission: Arc<Mutex<Option<PendingSubmission>>>,
}

impl std::fmt::Debug for L1Submitter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("L1Submitter")
            .field("config", &self.config)
            .field("sequencer_address", &self.sequencer_address)
            .field("nonce", &self.nonce)
            .field("pending_submission", &self.pending_submission)
            .finish()
    }
}

impl L1Submitter {
    /// Creates a new L1 submitter with the given configuration.
    pub async fn new(config: SubmitterConfig) -> Result<Self> {
        let signer = PrivateKeySigner::from_bytes(&config.sequencer_key)?;
        let sequencer_address = signer.address();
        let wallet = EthereumWallet::from(signer);

        let provider = ProviderBuilder::new()
            .wallet(wallet)
            .connect(&config.l1_rpc_url)
            .await?;

        tracing::info!(
            portal = %config.portal_address,
            sequencer = %sequencer_address,
            rpc_url = %config.l1_rpc_url,
            chain_id = config.chain_id,
            "L1Submitter created"
        );

        Ok(Self {
            config,
            provider: Box::new(provider),
            sequencer_address,
            nonce: AtomicU64::new(0),
            pending_submission: Arc::new(Mutex::new(None)),
        })
    }

    /// Initializes the submitter by fetching the current nonce.
    pub async fn initialize(&self) -> Result<()> {
        let nonce = self.provider.get_transaction_count(self.sequencer_address).await?;
        self.nonce.store(nonce, Ordering::SeqCst);

        tracing::info!(
            portal = %self.config.portal_address,
            sequencer = %self.sequencer_address,
            nonce,
            "L1Submitter initialized"
        );

        Ok(())
    }

    /// Submits a batch to the ZonePortal contract.
    pub async fn submit_batch(
        &self,
        commitment: BatchCommitment,
        expected_withdrawal_queue2: B256,
        updated_withdrawal_queue2: B256,
        new_withdrawal_queue_only: B256,
        proof_bundle: ProofBundle,
    ) -> Result<SubmissionResult> {
        let mut last_error = None;

        for attempt in 0..=self.config.max_retries {
            if attempt > 0 {
                let delay = self.config.retry_delay * 2u32.pow(attempt - 1);
                tracing::warn!(
                    attempt,
                    delay_ms = delay.as_millis(),
                    "Retrying batch submission"
                );
                tokio::time::sleep(delay).await;
            }

            match self
                .try_submit(
                    &commitment,
                    expected_withdrawal_queue2,
                    updated_withdrawal_queue2,
                    new_withdrawal_queue_only,
                    &proof_bundle,
                )
                .await
            {
                Ok(result) => return Ok(result),
                Err(e) => {
                    let error_str = e.to_string().to_lowercase();

                    if error_str.contains("nonce too low") {
                        tracing::warn!("Nonce too low, refreshing nonce");
                        if let Err(refresh_err) = self.refresh_nonce().await {
                            tracing::error!(?refresh_err, "Failed to refresh nonce");
                        }
                        last_error = Some(e);
                        continue;
                    }

                    if Self::is_transient_error(&e) {
                        last_error = Some(e);
                        continue;
                    }
                    return Err(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| eyre!("No submission attempts made")))
    }

    /// Refreshes the nonce from L1.
    async fn refresh_nonce(&self) -> Result<()> {
        let nonce = self.provider.get_transaction_count(self.sequencer_address).await?;
        self.nonce.store(nonce, Ordering::SeqCst);
        tracing::info!(nonce, "Nonce refreshed");
        Ok(())
    }

    async fn try_submit(
        &self,
        commitment: &BatchCommitment,
        expected_withdrawal_queue2: B256,
        updated_withdrawal_queue2: B256,
        new_withdrawal_queue_only: B256,
        proof_bundle: &ProofBundle,
    ) -> Result<SubmissionResult> {
        let nonce = self.nonce.fetch_add(1, Ordering::SeqCst);

        tracing::info!(
            portal = %self.config.portal_address,
            nonce,
            new_state_root = %commitment.new_state_root,
            "Submitting batch to ZonePortal"
        );

        let calldata = self.encode_submit_batch_call(
            commitment,
            expected_withdrawal_queue2,
            updated_withdrawal_queue2,
            new_withdrawal_queue_only,
            proof_bundle,
        );

        let (max_fee_per_gas, max_priority_fee_per_gas) = self.get_gas_prices().await?;

        let tx = alloy_rpc_types_eth::TransactionRequest::default()
            .with_to(self.config.portal_address)
            .with_input(calldata)
            .with_nonce(nonce)
            .with_chain_id(self.config.chain_id)
            .with_gas_limit(self.config.gas_limit)
            .with_max_fee_per_gas(max_fee_per_gas)
            .with_max_priority_fee_per_gas(max_priority_fee_per_gas);

        let pending_tx = self.provider.send_transaction(tx).await?;
        let tx_hash = *pending_tx.tx_hash();

        tracing::info!(
            tx_hash = %tx_hash,
            nonce,
            "Transaction sent, waiting for confirmation"
        );

        {
            let mut pending = self.pending_submission.lock().await;
            *pending = Some(PendingSubmission {
                tx_hash,
                batch_index: nonce,
                submitted_at: std::time::Instant::now(),
            });
        }

        let receipt = self.wait_for_receipt(tx_hash).await?;

        let gas_used = receipt.gas_used;

        if !receipt.status() {
            return Err(eyre!("Transaction reverted: {}", tx_hash));
        }

        tracing::info!(
            tx_hash = %tx_hash,
            gas_used,
            block_number = ?receipt.block_number,
            "Batch submitted successfully"
        );

        Ok(SubmissionResult {
            tx_hash,
            batch_index: nonce,
            gas_used,
        })
    }

    /// Wait for a transaction receipt with retries.
    pub async fn wait_for_receipt(&self, tx_hash: B256) -> Result<TransactionReceipt> {
        let mut attempts = 0;
        let max_attempts = 60;
        let poll_interval = Duration::from_secs(2);

        loop {
            match self.provider.get_transaction_receipt(tx_hash).await {
                Ok(Some(receipt)) => return Ok(receipt),
                Ok(None) => {
                    attempts += 1;
                    if attempts >= max_attempts {
                        return Err(eyre!(
                            "Transaction {} not confirmed after {} attempts",
                            tx_hash,
                            max_attempts
                        ));
                    }
                    tracing::debug!(
                        tx_hash = %tx_hash,
                        attempts,
                        "Waiting for transaction confirmation"
                    );
                    tokio::time::sleep(poll_interval).await;
                }
                Err(e) => {
                    if Self::is_transient_error(&eyre::eyre!("{}", e)) {
                        attempts += 1;
                        if attempts >= max_attempts {
                            return Err(eyre!("Failed to get receipt after {} attempts: {}", max_attempts, e));
                        }
                        tokio::time::sleep(poll_interval).await;
                        continue;
                    }
                    return Err(eyre!("Failed to get transaction receipt: {}", e));
                }
            }
        }
    }

    async fn get_gas_prices(&self) -> Result<(u128, u128)> {
        if let (Some(max_fee), Some(priority_fee)) = (
            self.config.max_fee_per_gas,
            self.config.max_priority_fee_per_gas,
        ) {
            return Ok((max_fee, priority_fee));
        }

        let gas_price = self.provider.get_gas_price().await?;

        let max_priority_fee = self
            .config
            .max_priority_fee_per_gas
            .unwrap_or(1_500_000_000);

        let max_fee = self.config.max_fee_per_gas.unwrap_or_else(|| {
            gas_price.saturating_mul(2).max(max_priority_fee + gas_price)
        });

        Ok((max_fee, max_priority_fee))
    }

    fn encode_submit_batch_call(
        &self,
        commitment: &BatchCommitment,
        expected_withdrawal_queue2: B256,
        updated_withdrawal_queue2: B256,
        new_withdrawal_queue_only: B256,
        proof_bundle: &ProofBundle,
    ) -> Bytes {
        let sol_commitment: SolBatchCommitment = commitment.clone().into();

        let call = IZonePortal::submitBatchCall {
            commitment: sol_commitment,
            expectedWithdrawalQueue2: expected_withdrawal_queue2,
            updatedWithdrawalQueue2: updated_withdrawal_queue2,
            newWithdrawalQueueOnly: new_withdrawal_queue_only,
            verifierData: proof_bundle.verifier_data.clone(),
            proof: proof_bundle.proof.clone(),
        };

        call.abi_encode().into()
    }

    fn is_transient_error(error: &eyre::Error) -> bool {
        let error_str = error.to_string().to_lowercase();
        error_str.contains("timeout")
            || error_str.contains("connection")
            || error_str.contains("rate limit")
            || error_str.contains("too many requests")
            || error_str.contains("temporarily unavailable")
            || error_str.contains("503")
            || error_str.contains("502")
    }

    /// Returns the current pending submission, if any.
    pub async fn get_pending_submission(&self) -> Option<PendingSubmission> {
        self.pending_submission.lock().await.clone()
    }

    /// Clears the pending submission after confirmation.
    pub async fn clear_pending_submission(&self) {
        let mut pending = self.pending_submission.lock().await;
        *pending = None;
    }

    /// Returns the sequencer address.
    pub fn sequencer_address(&self) -> Address {
        self.sequencer_address
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_submitter_config_default() {
        let config = SubmitterConfig::default();
        assert_eq!(config.portal_address, Address::ZERO);
        assert_eq!(config.chain_id, 1);
        assert_eq!(config.max_retries, 3);
        assert_eq!(config.gas_limit, 500_000);
    }

    #[test]
    fn test_is_transient_error() {
        assert!(L1Submitter::is_transient_error(&eyre!("connection timeout")));
        assert!(L1Submitter::is_transient_error(&eyre!("rate limit exceeded")));
        assert!(L1Submitter::is_transient_error(&eyre!("503 service unavailable")));
        assert!(!L1Submitter::is_transient_error(&eyre!("invalid signature")));
        assert!(!L1Submitter::is_transient_error(&eyre!("out of gas")));
    }
}
