//! Finalized L1 batch observation for RPC-only shadow provers.

use std::{collections::HashSet, time::Duration};

use alloy_consensus::{BlockHeader as _, Sealable as _};
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256, TxKind};
use alloy_provider::{DynProvider, Provider as _};
use alloy_sol_types::SolCall as _;
use eyre::{OptionExt as _, Result, WrapErr as _, ensure};
use tempo_alloy::TempoNetwork;
use tempo_zone_contracts::ZonePortal;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{info, warn};
use zone_l1::FinalizedBatchSubmission;
use zone_sequencer::{
    BatchAnchorConfig, BatchData, ShadowProofAnchor, ShadowProver, ZoneSequencerProvider,
};

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const RECOVERY_LOG_QUERY_CHUNK_BLOCKS: u64 = 1_000;

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
            let observation_id = (submission.transaction_hash, submission.log_index);
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
        let events = portal
            .BatchSubmitted_filter()
            .from_block(from)
            .to_block(finalized)
            .chunked()
            .chunk_size(RECOVERY_LOG_QUERY_CHUNK_BLOCKS)
            .query()
            .await
            .wrap_err_with(|| {
                format!("query BatchSubmitted logs in finalized range {from}..={finalized}")
            })?;

        for (event, log) in events {
            let tx_hash = log
                .transaction_hash
                .ok_or_eyre("finalized BatchSubmitted log has no transaction hash")?;
            let log_index = log
                .log_index
                .ok_or_eyre("finalized BatchSubmitted log has no log index")?;
            let submission = FinalizedBatchSubmission {
                block_number: log
                    .block_number
                    .ok_or_eyre("finalized BatchSubmitted log has no block number")?,
                transaction_hash: tx_hash,
                log_index,
                event,
            };
            if recovery_sender.send(submission).is_err() {
                return Ok(());
            }
        }
        Ok(())
    }

    fn spawn_submission_retry(&self, submission: FinalizedBatchSubmission) {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(err) = this.process_submission(&submission).await {
                    warn!(
                        target: "zone::node::shadow_prover",
                        transaction_hash = %submission.transaction_hash,
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
            .fetch_submit_batch_calls(submission.transaction_hash)
            .await?
            .into_iter()
            .find(|call| call_matches_event(call, &submission.event))
            .ok_or_eyre(format!(
                "submitBatch transaction {} contains no call matching log {}",
                submission.transaction_hash, submission.log_index
            ))?;
        let (to, batch, anchor) = self
            .submission_target(&call, &submission.event, submission.block_number)
            .await
            .wrap_err_with(|| {
                format!(
                    "validate finalized submitBatch transaction {}",
                    submission.transaction_hash
                )
            })?;

        self.wait_and_enqueue(to, batch, anchor).await
    }

    async fn fetch_submit_batch_calls(
        &self,
        tx_hash: B256,
    ) -> Result<Vec<ZonePortal::submitBatchCall>> {
        let tx = self
            .l1_provider
            .get_transaction_by_hash(tx_hash)
            .await?
            .ok_or_eyre(format!("submitBatch transaction {tx_hash} was not found"))?;

        let calls = tx
            .inner
            .inner()
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
            "transaction {tx_hash} contains no submitBatch call to Portal {}",
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
}
