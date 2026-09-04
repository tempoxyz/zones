//! Finalized L1 batch observation for RPC-only shadow provers.

use std::{
    collections::{BTreeSet, HashSet},
    time::Duration,
};

use alloy_consensus::{BlockHeader as _, Sealable as _, proofs::calculate_transaction_root};
use alloy_eips::{BlockId, BlockNumberOrTag, NumHash};
use alloy_primitives::{Address, TxKind};
use alloy_provider::{DynProvider, Provider as _};
use alloy_sol_types::SolCall as _;
use eyre::{OptionExt as _, Result, WrapErr as _, ensure};
use tempo_alloy::TempoNetwork;
use tempo_primitives::TempoTxEnvelope;
use tempo_zone_contracts::ZonePortal;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{info, warn};
use zone_l1::{
    FinalizedBatchSubmission, extract_finalized_batch_submissions, verify_receipts_against_header,
};
use zone_sequencer::{
    BatchAnchorConfig, BatchData, ShadowProofAnchor, ShadowProver, ZoneSequencerProvider,
};

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const RECOVERY_LOG_QUERY_BLOCKS: u64 = 1_000;

/// Feeds finalized RPC-follower submissions and their exact settlement inputs to the detached
/// shadow prover.
#[derive(Clone)]
pub(crate) struct RpcFollowerShadowProver<P> {
    portal_address: Address,
    anchor_config: BatchAnchorConfig,
    zone_provider: P,
    l1_provider: DynProvider<TempoNetwork>,
    prover: ShadowProver,
}

impl<P: ZoneSequencerProvider> RpcFollowerShadowProver<P> {
    pub(crate) fn new(
        portal_address: Address,
        anchor_config: BatchAnchorConfig,
        zone_provider: P,
        l1_provider: DynProvider<TempoNetwork>,
        prover: ShadowProver,
    ) -> Self {
        Self {
            portal_address,
            anchor_config,
            zone_provider,
            l1_provider,
            prover,
        }
    }

    /// Observe finalized `submitBatch` calls. The Portal has already verified the included quorum
    /// certificate before emitting the event; decoding the call binds each proof job to that
    /// accepted certificate.
    pub(crate) async fn run(
        self,
        mut submissions: UnboundedReceiver<FinalizedBatchSubmission>,
        recovery_sender: UnboundedSender<FinalizedBatchSubmission>,
    ) {
        self.spawn_recovery(recovery_sender);

        let mut observed = HashSet::new();
        while let Some(submission) = submissions.recv().await {
            let observation_id = (
                submission.block.hash,
                submission.transaction_index,
                submission.log_index,
            );
            if observed.insert(observation_id) {
                self.spawn_submission_retry(submission);
            }
        }
    }

    fn spawn_recovery(&self, recovery_sender: UnboundedSender<FinalizedBatchSubmission>) {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                if recovery_sender.is_closed() {
                    return;
                }
                match this.recover_recent_submissions(&recovery_sender).await {
                    Ok(()) => return,
                    Err(err) => {
                        warn!(
                            target: "zone::node::shadow_prover",
                            error = ?err,
                            "Failed to recover recent finalized batch submissions; retrying"
                        );
                        tokio::time::sleep(POLL_INTERVAL).await;
                    }
                }
            }
        });
    }

    async fn recover_recent_submissions(
        &self,
        recovery_sender: &UnboundedSender<FinalizedBatchSubmission>,
    ) -> Result<()> {
        let finalized = self
            .l1_provider
            .get_header_by_number(BlockNumberOrTag::Finalized)
            .await?
            .ok_or_eyre("L1 finalized block is not available")?
            .number();
        let from = finalized.saturating_sub(self.anchor_config.history_window().saturating_sub(1));

        let portal = ZonePortal::new(self.portal_address, &self.l1_provider);
        let mut page_from = from;
        while page_from <= finalized {
            let page_to = page_from
                .saturating_add(RECOVERY_LOG_QUERY_BLOCKS - 1)
                .min(finalized);
            let events = portal
                .BatchSubmitted_filter()
                .from_block(page_from)
                .to_block(page_to)
                .query()
                .await
                .wrap_err_with(|| {
                    format!("query BatchSubmitted logs in finalized range {page_from}..={page_to}")
                })?;

            let candidate_blocks = events
                .into_iter()
                .map(|(_, log)| {
                    log.block_number
                        .ok_or_eyre("finalized BatchSubmitted log has no block number")
                })
                .collect::<Result<BTreeSet<_>>>()?;
            for block_number in candidate_blocks {
                for submission in self
                    .fetch_verified_submissions(block_number)
                    .await
                    .wrap_err_with(|| {
                        format!("authenticate recovered submissions in block {block_number}")
                    })?
                {
                    if recovery_sender.send(submission).is_err() {
                        return Ok(());
                    }
                }
            }
            if page_to == finalized {
                break;
            }
            page_from = page_to + 1;
        }
        Ok(())
    }

    async fn fetch_verified_submissions(
        &self,
        block_number: u64,
    ) -> Result<Vec<FinalizedBatchSubmission>> {
        let header = self
            .l1_provider
            .get_header_by_number(block_number.into())
            .await?
            .ok_or_eyre(format!("L1 block {block_number} is not available"))?;
        ensure!(
            header.number() == block_number,
            "requested L1 block {block_number}, received {}",
            header.number()
        );
        let block = NumHash::new(block_number, header.as_ref().hash_slow());
        let receipts = self
            .l1_provider
            .get_block_receipts(BlockId::hash(block.hash))
            .await?
            .ok_or_eyre(format!("L1 block {} has no receipts", block.hash))?;
        verify_receipts_against_header(
            block,
            header.receipts_root(),
            header.logs_bloom(),
            &receipts,
        )?;
        Ok(extract_finalized_batch_submissions(
            block,
            self.portal_address,
            &receipts,
        ))
    }

    fn spawn_submission_retry(&self, submission: FinalizedBatchSubmission) {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(err) = this.process_submission(&submission).await {
                    warn!(
                        target: "zone::node::shadow_prover",
                        l1_block_hash = %submission.block.hash,
                        transaction_index = submission.transaction_index,
                        log_index = submission.log_index,
                        error = ?err,
                        "Failed to process finalized batch submission; retrying"
                    );
                    tokio::time::sleep(POLL_INTERVAL).await;
                    continue;
                }
                return;
            }
        });
    }

    async fn process_submission(&self, submission: &FinalizedBatchSubmission) -> Result<()> {
        let call = self
            .fetch_submit_batch_calls(submission)
            .await?
            .into_iter()
            .find(|call| call_matches_event(call, &submission.event))
            .ok_or_eyre(format!(
                "L1 block {} transaction {} contains no submitBatch call matching log {}",
                submission.block.hash, submission.transaction_index, submission.log_index
            ))?;
        let (to, batch, anchor) = self
            .submission_target(&call, &submission.event, submission.block.number)
            .await
            .wrap_err_with(|| {
                format!(
                    "validate finalized submitBatch in L1 block {} transaction {}",
                    submission.block.hash, submission.transaction_index
                )
            })?;

        self.wait_and_enqueue(to, batch, anchor).await
    }

    async fn fetch_submit_batch_calls(
        &self,
        submission: &FinalizedBatchSubmission,
    ) -> Result<Vec<ZonePortal::submitBatchCall>> {
        let block = self
            .l1_provider
            .get_block_by_hash(submission.block.hash)
            .full()
            .await?
            .ok_or_eyre(format!("L1 block {} was not found", submission.block.hash))?;
        ensure!(
            block.header.number() == submission.block.number,
            "L1 block {} has number {}, expected {}",
            submission.block.hash,
            block.header.number(),
            submission.block.number
        );
        ensure!(
            block.header.as_ref().hash_slow() == submission.block.hash,
            "L1 block response does not match authenticated hash {}",
            submission.block.hash
        );
        let expected_transactions_root = block.header.transactions_root();
        let transactions = block
            .try_into_transactions()
            .map_err(|_| {
                eyre::eyre!(
                    "L1 block {} did not return full transactions",
                    submission.block.hash
                )
            })?
            .into_iter()
            .map(|transaction| transaction.inner.into_inner())
            .collect::<Vec<_>>();
        verify_transactions_root(
            &transactions,
            expected_transactions_root,
            submission.block.hash,
        )?;
        let transaction_index = usize::try_from(submission.transaction_index)
            .wrap_err("L1 transaction index overflows usize")?;
        let tx = transactions.get(transaction_index).ok_or_eyre(format!(
            "L1 block {} has no transaction at authenticated receipt index {}",
            submission.block.hash, submission.transaction_index
        ))?;

        let calls = tx
            .calls()
            .filter_map(|(kind, input)| {
                if kind != TxKind::Call(self.portal_address) {
                    return None;
                }
                ZonePortal::submitBatchCall::abi_decode(input).ok()
            })
            .collect::<Vec<_>>();

        ensure!(
            !calls.is_empty(),
            "L1 block {} transaction {} contains no submitBatch call to Portal {}",
            submission.block.hash,
            submission.transaction_index,
            self.portal_address
        );
        Ok(calls)
    }

    async fn submission_target(
        &self,
        call: &ZonePortal::submitBatchCall,
        event: &ZonePortal::BatchSubmitted,
        submission_block_number: u64,
    ) -> Result<(u64, BatchData, ShadowProofAnchor)> {
        ensure!(
            !call.signatures.is_empty(),
            "accepted submitBatch call has an empty quorum certificate"
        );
        let to =
            u64::try_from(call.nextZoneHeight).wrap_err("submitted Zone height overflows u64")?;
        let anchor_number =
            submitted_anchor_number(call.tempoBlockNumber, call.recentTempoBlockNumber)?;
        ensure!(
            anchor_number <= submission_block_number,
            "submitted anchor {anchor_number} is above submission block {submission_block_number}"
        );
        let anchor_hash = self
            .l1_provider
            .get_header_by_number(anchor_number.into())
            .await?
            .ok_or_eyre(format!(
                "submitted Tempo anchor {anchor_number} is unavailable"
            ))?
            .hash_slow();

        Ok((
            to,
            BatchData {
                zone_height: to,
                tempo_block_number: call.tempoBlockNumber,
                prev_block_hash: call.blockTransition.prevBlockHash,
                next_block_hash: call.blockTransition.nextBlockHash,
                prev_processed_deposit_hash: call.depositQueueTransition.prevProcessedHash,
                next_processed_deposit_hash: call.depositQueueTransition.nextProcessedHash,
                prev_deposit_number: call.depositQueueTransition.prevDepositNumber,
                next_deposit_number: call.depositQueueTransition.nextDepositNumber,
                withdrawal_queue_hash: call.withdrawalQueueHash,
                withdrawal_batch_index: event.withdrawalBatchIndex,
            },
            ShadowProofAnchor {
                number: anchor_number,
                hash: anchor_hash,
            },
        ))
    }

    async fn wait_and_enqueue(
        &self,
        to: u64,
        batch: BatchData,
        anchor: ShadowProofAnchor,
    ) -> Result<()> {
        let from = loop {
            if batch.prev_block_hash.is_zero() {
                break 1;
            }
            if let Some(previous) = self.zone_provider.block_number(batch.prev_block_hash)? {
                break previous.saturating_add(1);
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        };
        ensure!(from <= to, "submitted Zone range {from}..={to} is invalid");

        loop {
            let local_head = self.zone_provider.last_block_number()?;
            if local_head >= to {
                break;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }

        ensure!(
            self.zone_provider.block_hash(to)? == Some(batch.next_block_hash),
            "local canonical Zone block {to} does not match finalized submission {}",
            batch.next_block_hash
        );
        if from > 1 {
            ensure!(
                self.zone_provider.block_hash(from - 1)? == Some(batch.prev_block_hash),
                "local parent of Zone range {from}..={to} does not match finalized submission {}",
                batch.prev_block_hash
            );
        }

        info!(
            target: "zone::node::shadow_prover",
            zone_from = from,
            zone_to = to,
            anchor_number = anchor.number,
            anchor_hash = %anchor.hash,
            "Queueing finalized quorum-certified batch for shadow proving"
        );
        self.prover
            .enqueue_with_anchor(from, to, batch, anchor)
            .await
    }
}

fn verify_transactions_root(
    transactions: &[TempoTxEnvelope],
    expected: alloy_primitives::B256,
    block_hash: alloy_primitives::B256,
) -> Result<()> {
    let computed = calculate_transaction_root(transactions);
    ensure!(
        computed == expected,
        "transaction root mismatch for L1 block {block_hash}: expected {expected}, got {computed}"
    );
    Ok(())
}

fn call_matches_event(
    call: &ZonePortal::submitBatchCall,
    event: &ZonePortal::BatchSubmitted,
) -> bool {
    event.nextBlockHash == call.blockTransition.nextBlockHash
        && event.nextProcessedDepositQueueHash == call.depositQueueTransition.nextProcessedHash
        && event.lastProcessedDepositNumber == call.depositQueueTransition.nextDepositNumber
        && event.withdrawalQueueHash == call.withdrawalQueueHash
}

fn submitted_anchor_number(tempo_block_number: u64, recent_tempo_block_number: u64) -> Result<u64> {
    if recent_tempo_block_number == 0 {
        return Ok(tempo_block_number);
    }
    ensure!(
        recent_tempo_block_number > tempo_block_number,
        "submitted ancestry anchor does not follow its Tempo checkpoint"
    );
    Ok(recent_tempo_block_number)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_submission_anchors_to_checkpoint() {
        assert_eq!(submitted_anchor_number(42, 0).unwrap(), 42);
    }

    #[test]
    fn ancestry_submission_uses_committed_recent_block() {
        assert_eq!(submitted_anchor_number(42, 100).unwrap(), 100);
    }

    #[test]
    fn ancestry_submission_must_follow_checkpoint() {
        assert!(submitted_anchor_number(42, 42).is_err());
        assert!(submitted_anchor_number(42, 41).is_err());
    }

    #[test]
    fn transaction_root_authentication_rejects_mismatch() {
        let transactions = Vec::<TempoTxEnvelope>::new();
        let expected = calculate_transaction_root(&transactions);

        verify_transactions_root(&transactions, expected, alloy_primitives::B256::ZERO).unwrap();
        assert!(
            verify_transactions_root(
                &transactions,
                alloy_primitives::B256::repeat_byte(0x42),
                alloy_primitives::B256::ZERO,
            )
            .is_err()
        );
    }
}
