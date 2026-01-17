//! Main ExEx implementation for zone proving.
//!
//! The [`ZoneProverExEx`] subscribes to chain state notifications, coordinates
//! batching, proving, and submission to L1.

use crate::{
    batcher::{BatchConfig, BatchCoordinator, BatchId},
    prover::{MockProver, Prover},
    submitter::{L1Submitter, SubmitterConfig},
    types::{BatchBlock, BatchCommitment, Deposit},
};
use alloy_consensus::TxReceipt;
use alloy_primitives::{Log, B256, U128};
use eyre::Result;
use futures::TryStreamExt;
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_node_api::{FullNodeComponents, PrimitivesTy};
use reth_primitives_traits::{AlloyBlockHeader as _, BlockBody as _};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info, trace, warn};

/// Configuration for the zone prover ExEx.
#[derive(Debug, Clone)]
pub struct ZoneProverConfig {
    /// Batch configuration.
    pub batch_config: BatchConfig,
    /// Submitter configuration.
    pub submitter_config: SubmitterConfig,
    /// Whether to use mock prover.
    pub use_mock_prover: bool,
    /// Initial state root.
    pub initial_state_root: B256,
    /// Initial deposit queue hashes.
    pub initial_processed_deposit_hash: B256,
    pub initial_pending_deposit_hash: B256,
    /// Initial withdrawal queue2.
    pub initial_withdrawal_queue2: B256,
}

impl Default for ZoneProverConfig {
    fn default() -> Self {
        Self {
            batch_config: BatchConfig::default(),
            submitter_config: SubmitterConfig::default(),
            use_mock_prover: true,
            initial_state_root: B256::ZERO,
            initial_processed_deposit_hash: B256::ZERO,
            initial_pending_deposit_hash: B256::ZERO,
            initial_withdrawal_queue2: B256::ZERO,
        }
    }
}

/// Receiver for L1 deposit events.
pub type L1DepositReceiver = mpsc::UnboundedReceiver<L1Deposit>;

/// L1 deposit event from the L1Subscriber.
#[derive(Debug, Clone)]
pub struct L1Deposit {
    pub l1_block_number: u64,
    pub sender: alloy_primitives::Address,
    pub to: alloy_primitives::Address,
    pub amount: alloy_primitives::U256,
    pub data: alloy_primitives::Bytes,
}

/// Zone prover execution extension.
///
/// Subscribes to chain notifications and coordinates batch proving and submission.
pub struct ZoneProverExEx<N: FullNodeComponents> {
    ctx: ExExContext<N>,
    config: ZoneProverConfig,
    batcher: Arc<RwLock<BatchCoordinator>>,
    prover: Arc<dyn Prover>,
    submitter: L1Submitter,
    deposit_rx: Option<L1DepositReceiver>,
}

impl<N: FullNodeComponents> ZoneProverExEx<N> {
    /// Creates a new zone prover ExEx.
    pub async fn new(ctx: ExExContext<N>, config: ZoneProverConfig) -> Result<Self> {
        Self::with_deposit_receiver(ctx, config, None).await
    }

    /// Creates a new zone prover ExEx with a deposit receiver for L1 deposits.
    pub async fn with_deposit_receiver(
        ctx: ExExContext<N>,
        config: ZoneProverConfig,
        deposit_rx: Option<L1DepositReceiver>,
    ) -> Result<Self> {
        let batcher = Arc::new(RwLock::new(BatchCoordinator::new(config.batch_config.clone())));

        let prover: Arc<dyn Prover> = if config.use_mock_prover {
            Arc::new(MockProver::new())
        } else {
            // TODO: Use Sp1Prover when implemented
            Arc::new(MockProver::new())
        };

        let submitter = L1Submitter::new(config.submitter_config.clone()).await?;

        Ok(Self {
            ctx,
            config,
            batcher,
            prover,
            submitter,
            deposit_rx,
        })
    }

    /// Runs the ExEx main loop.
    pub async fn run(mut self) -> Result<()> {
        info!("Starting ZoneProverExEx");

        // Initialize components
        {
            let mut batcher = self.batcher.write().await;
            batcher.initialize(
                self.config.initial_processed_deposit_hash,
                self.config.initial_pending_deposit_hash,
                self.config.initial_state_root,
                self.config.initial_withdrawal_queue2,
            );
        }

        self.submitter.initialize().await?;

        // Channel for batch proving results
        let (prove_tx, mut prove_rx) = mpsc::channel::<ProveResult>(16);

        // Spawn batch flushing task
        let batch_interval = self.config.batch_config.batch_interval;
        let (flush_tx, mut flush_rx) = mpsc::channel::<()>(1);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(batch_interval).await;
                let _ = flush_tx.send(()).await;
            }
        });

        // Spawn L1 deposit listener task if we have a receiver
        if let Some(mut deposit_rx) = self.deposit_rx.take() {
            let batcher = self.batcher.clone();
            tokio::spawn(async move {
                while let Some(l1_deposit) = deposit_rx.recv().await {
                    let deposit = Deposit {
                        l1_block_hash: B256::ZERO, // Not available from L1Subscriber
                        l1_block_number: l1_deposit.l1_block_number,
                        l1_timestamp: 0, // Not available from L1Subscriber
                        sender: l1_deposit.sender,
                        to: l1_deposit.to,
                        amount: U128::from(l1_deposit.amount.saturating_to::<u128>()),
                        memo: B256::ZERO, // Not available from L1Subscriber
                    };

                    debug!(
                        l1_block = deposit.l1_block_number,
                        sender = %deposit.sender,
                        to = %deposit.to,
                        amount = %deposit.amount,
                        "Adding L1 deposit to batcher"
                    );

                    batcher.write().await.add_deposit(deposit);
                }
                warn!("L1 deposit receiver closed");
            });
            info!("L1 deposit listener task spawned");
        }

        loop {
            tokio::select! {
                // Handle chain notifications
                notification = self.ctx.notifications.try_next() => {
                    match notification {
                        Ok(Some(notification)) => {
                            if let Err(e) = self.handle_notification(notification, &prove_tx).await {
                                error!(?e, "Failed to handle notification");
                            }
                        }
                        Ok(None) => {
                            info!("Notification stream ended");
                            break;
                        }
                        Err(e) => {
                            error!(?e, "Error receiving notification");
                            break;
                        }
                    }
                }

                // Handle flush timer
                Some(()) = flush_rx.recv() => {
                    if self.batcher.read().await.should_flush()
                        && let Err(e) = self.flush_and_prove(&prove_tx).await
                    {
                        error!(?e, "Failed to flush batch");
                    }
                }

                // Handle prove results
                Some(result) = prove_rx.recv() => {
                    if let Err(e) = self.handle_prove_result(result).await {
                        error!(?e, "Failed to handle prove result");
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_notification(
        &mut self,
        notification: ExExNotification<PrimitivesTy<N::Types>>,
        prove_tx: &mpsc::Sender<ProveResult>,
    ) -> Result<()> {
        match &notification {
            ExExNotification::ChainCommitted { new } => {
                let block_count = new.blocks().len();
                info!(blocks = block_count, "Processing committed chain");

                for (block_num, block) in new.blocks().iter() {
                    let block_hash = block.hash();
                    let header = block.header();

                    debug!(
                        number = header.number(),
                        hash = %block_hash,
                        parent_hash = %header.parent_hash(),
                        state_root = %header.state_root(),
                        transactions = block.body().transactions().len(),
                        "Processing committed block"
                    );

                    let batch_block = BatchBlock {
                        number: header.number(),
                        hash: block_hash,
                        parent_hash: header.parent_hash(),
                        state_root: header.state_root(),
                        transactions_root: header.transactions_root(),
                        receipts_root: header.receipts_root(),
                    };

                    let mut batcher = self.batcher.write().await;
                    batcher.add_block(batch_block);

                    if let Some(receipts) = new.receipts_by_block_hash(block_hash) {
                        let mut logs: Vec<Log> = Vec::new();
                        for receipt in receipts {
                            logs.extend(receipt.logs().iter().cloned());
                        }

                        if !logs.is_empty() {
                            trace!(
                                block_number = *block_num,
                                log_count = logs.len(),
                                "Extracting withdrawals from logs"
                            );
                            batcher.add_withdrawals_from_logs(&logs);
                        }
                    } else {
                        warn!(
                            block_number = *block_num,
                            block_hash = %block_hash,
                            "No receipts found for block"
                        );
                    }
                }

                // Check if we should flush based on block count or time
                let should_flush = {
                    let batcher = self.batcher.read().await;
                    if batcher.should_flush() {
                        info!(
                            pending_blocks = batcher.pending_block_count(),
                            "Batch threshold reached, initiating prove"
                        );
                        true
                    } else {
                        false
                    }
                };
                if should_flush {
                    self.flush_and_prove(prove_tx).await?;
                }
            }
            ExExNotification::ChainReverted { old } => {
                let reverted_blocks: Vec<_> = old
                    .blocks()
                    .iter()
                    .map(|(num, block)| (*num, block.hash()))
                    .collect();

                if !reverted_blocks.is_empty() {
                    let first_reverted_block =
                        reverted_blocks.first().map(|(n, _)| *n).unwrap_or(0);
                    let last_reverted_block =
                        reverted_blocks.last().map(|(n, _)| *n).unwrap_or(0);

                    warn!(
                        first_block = first_reverted_block,
                        last_block = last_reverted_block,
                        block_count = reverted_blocks.len(),
                        "Handling chain revert"
                    );

                    let mut batcher = self.batcher.write().await;

                    let removed_pending =
                        batcher.remove_pending_blocks_after(first_reverted_block);
                    if removed_pending > 0 {
                        debug!(
                            removed = removed_pending,
                            "Removed pending blocks after revert"
                        );
                    }

                    let invalidated_batches =
                        batcher.invalidate_batches_after(first_reverted_block);
                    if !invalidated_batches.is_empty() {
                        warn!(
                            batch_ids = ?invalidated_batches,
                            "Invalidated batches due to revert"
                        );
                    }

                    info!(
                        first_block = first_reverted_block,
                        last_block = last_reverted_block,
                        removed_pending,
                        invalidated_batches = invalidated_batches.len(),
                        "Chain revert handled"
                    );
                }
            }
            ExExNotification::ChainReorged { old, new } => {
                let old_block_count = old.blocks().len();
                let new_block_count = new.blocks().len();

                let old_tip = old
                    .blocks()
                    .last_key_value()
                    .map(|(n, _)| *n)
                    .unwrap_or(0);
                let new_tip = new
                    .blocks()
                    .last_key_value()
                    .map(|(n, _)| *n)
                    .unwrap_or(0);

                warn!(
                    old_blocks = old_block_count,
                    new_blocks = new_block_count,
                    old_tip,
                    new_tip,
                    "Handling chain reorg"
                );

                // Handle the revert of the old chain
                let reverted_blocks: Vec<_> = old
                    .blocks()
                    .iter()
                    .map(|(num, block)| (*num, block.hash()))
                    .collect();

                if !reverted_blocks.is_empty() {
                    let first_reverted_block =
                        reverted_blocks.first().map(|(n, _)| *n).unwrap_or(0);

                    let mut batcher = self.batcher.write().await;
                    let removed_pending =
                        batcher.remove_pending_blocks_after(first_reverted_block);
                    let invalidated_batches =
                        batcher.invalidate_batches_after(first_reverted_block);

                    if removed_pending > 0 || !invalidated_batches.is_empty() {
                        debug!(
                            removed_pending,
                            invalidated_batches = invalidated_batches.len(),
                            "Reverted old chain during reorg"
                        );
                    }
                }

                // Process the new chain as commits
                for (block_num, block) in new.blocks().iter() {
                    let block_hash = block.hash();
                    let header = block.header();

                    debug!(
                        number = header.number(),
                        hash = %block_hash,
                        "Processing reorged block"
                    );

                    let batch_block = BatchBlock {
                        number: header.number(),
                        hash: block_hash,
                        parent_hash: header.parent_hash(),
                        state_root: header.state_root(),
                        transactions_root: header.transactions_root(),
                        receipts_root: header.receipts_root(),
                    };

                    let mut batcher = self.batcher.write().await;
                    batcher.add_block(batch_block);

                    if let Some(receipts) = new.receipts_by_block_hash(block_hash) {
                        let mut logs: Vec<Log> = Vec::new();
                        for receipt in receipts {
                            logs.extend(receipt.logs().iter().cloned());
                        }

                        if !logs.is_empty() {
                            trace!(
                                block_number = *block_num,
                                log_count = logs.len(),
                                "Extracting withdrawals from reorged logs"
                            );
                            batcher.add_withdrawals_from_logs(&logs);
                        }
                    }
                }

                // Check if we should flush after reorg
                let should_flush = {
                    let batcher = self.batcher.read().await;
                    batcher.should_flush()
                };
                if should_flush {
                    info!("Batch threshold reached after reorg, initiating prove");
                    self.flush_and_prove(prove_tx).await?;
                }

                info!(
                    old_tip,
                    new_tip,
                    "Chain reorg handled successfully"
                );
            }
        }

        // Send finished height event
        if let Some(committed_chain) = notification.committed_chain() {
            self.ctx
                .events
                .send(ExExEvent::FinishedHeight(committed_chain.tip().num_hash()))?;
        }

        Ok(())
    }

    async fn flush_and_prove(&mut self, prove_tx: &mpsc::Sender<ProveResult>) -> Result<()> {
        let flush_result = self.batcher.write().await.flush_batch();
        if let Some((batch_id, batch_input)) = flush_result {
            let block_count = batch_input.blocks.len();
            info!(batch_id, blocks = block_count, "Flushing batch for proving");

            // Spawn proving task
            let prover = self.prover.clone();
            let tx = prove_tx.clone();

            tokio::spawn(async move {
                let result = match prover.prove(&batch_input).await {
                    Ok(proof_bundle) => ProveResult::Success {
                        batch_id,
                        proof_bundle: Box::new(proof_bundle),
                        batch_input: Box::new(batch_input),
                    },
                    Err(e) => ProveResult::Failure {
                        batch_id,
                        error: e.to_string(),
                        batch_input: Box::new(batch_input),
                    },
                };

                let _ = tx.send(result).await;
            });
        }

        Ok(())
    }

    async fn handle_prove_result(&mut self, result: ProveResult) -> Result<()> {
        match result {
            ProveResult::Success {
                batch_id,
                proof_bundle,
                batch_input,
            } => {
                // Check if batch is still valid (not invalidated by reorg)
                let is_valid = {
                    let batcher = self.batcher.read().await;
                    batcher.get_batch_range(batch_id).is_some()
                };

                if !is_valid {
                    warn!(batch_id, "Batch was invalidated by reorg, discarding proof");
                    return Ok(());
                }

                info!(batch_id, "Proof generated successfully, submitting to L1");

                let commitment = BatchCommitment {
                    new_processed_deposit_queue_hash: batch_input.new_processed_deposit_queue_hash,
                    new_state_root: batch_input.new_state_root,
                };

                let submission_result = self
                    .submitter
                    .submit_batch(
                        commitment,
                        batch_input.expected_withdrawal_queue2,
                        batch_input.updated_withdrawal_queue2,
                        batch_input.new_withdrawal_queue_only,
                        (*proof_bundle).clone(),
                    )
                    .await?;

                info!(
                    tx_hash = %submission_result.tx_hash,
                    batch_index = submission_result.batch_index,
                    batch_id,
                    "Batch submitted to L1"
                );

                // Update batcher state and mark batch as complete
                let mut batcher = self.batcher.write().await;
                batcher.on_batch_submitted(
                    batch_input.new_processed_deposit_queue_hash,
                    batch_input.deposits.len(),
                    batch_input.new_state_root,
                    batch_input.updated_withdrawal_queue2,
                );
                batcher.complete_batch(batch_id);
            }
            ProveResult::Failure {
                batch_id,
                error,
                batch_input,
            } => {
                error!(
                    ?error,
                    batch_id,
                    blocks = batch_input.blocks.len(),
                    "Proof generation failed"
                );

                // Mark batch as complete even on failure
                self.batcher.write().await.complete_batch(batch_id);
            }
        }

        Ok(())
    }
}

/// Result of a proving operation.
enum ProveResult {
    Success {
        batch_id: BatchId,
        proof_bundle: Box<crate::types::ProofBundle>,
        batch_input: Box<crate::types::BatchInput>,
    },
    Failure {
        batch_id: BatchId,
        error: String,
        batch_input: Box<crate::types::BatchInput>,
    },
}
