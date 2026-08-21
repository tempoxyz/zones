//! Provider-backed Zone candidate discovery for the persistent submission actor.

use alloy_consensus::BlockHeader as _;
use alloy_primitives::B256;
use futures::{StreamExt as _, future::BoxFuture};
use reth_chain_state::CanonStateNotificationStream;
use tempo_primitives::{TempoPrimitives, TempoReceipt, TempoTxEnvelope};

use crate::{
    BatchCandidate, SubmissionAnchor, ZoneSequencerProvider,
    batch_submission::BatchCandidateSource,
    monitor::ZoneMonitorConfig,
    prover::ShadowProver,
    settlement::{
        BatchData, FinalizedBatch, FinalizedBatchLog, ZoneBlockSnapshot, fetch_finalized_batch,
        fetch_next_finalized_batch_boundary, finalized_batch_boundaries_from_parts,
        finalized_batch_from_parts, read_zone_block_snapshot,
        read_zone_block_snapshot_from_receipts,
    },
};

/// Canonical stream parser with actor-controlled sessions and provider-backed lag recovery.
pub(crate) struct CandidateMonitor<P: ZoneSequencerProvider> {
    metrics: crate::metrics::ZoneMonitorMetrics,
    config: ZoneMonitorConfig,
    provider: P,
    shadow_prover: Option<ShadowProver>,
}

impl<P: ZoneSequencerProvider> CandidateMonitor<P> {
    pub(crate) fn new(
        config: ZoneMonitorConfig,
        provider: P,
        shadow_prover: Option<ShadowProver>,
    ) -> Self {
        Self {
            metrics: crate::metrics::ZoneMonitorMetrics::default(),
            config,
            provider,
            shadow_prover,
        }
    }

    fn candidate(
        &self,
        anchor: SubmissionAnchor,
        boundary: FinalizedBatchLog,
        finalized: FinalizedBatch,
        end: ZoneBlockSnapshot,
    ) -> BatchCandidate {
        BatchCandidate {
            from: anchor.zone_height + 1,
            to: boundary.block_number,
            batch: BatchData {
                zone_height: boundary.block_number,
                tempo_block_number: end.tempo_block_number,
                prev_block_hash: anchor.zone_block_hash,
                next_block_hash: end.block_hash,
                prev_processed_deposit_hash: anchor.processed_deposit_hash,
                next_processed_deposit_hash: end.processed_deposit_hash,
                prev_deposit_number: anchor.processed_deposit_number,
                next_deposit_number: end.processed_deposit_number,
                withdrawal_queue_hash: finalized.finalized_hash,
                withdrawal_batch_index: finalized.finalized_index,
            },
            withdrawals: finalized.withdrawals,
        }
    }

    fn enqueue_prover(&self, candidate: &BatchCandidate) {
        if let Some(prover) = &self.shadow_prover {
            prover.try_enqueue(candidate.from, candidate.to, candidate.batch.clone());
        }
    }

    fn record_metrics(&self, head: u64, submitted: u64) {
        self.metrics.latest_zone_block_observed.set(head as f64);
        self.metrics
            .zone_to_l1_submission_lag_blocks
            .set(head.saturating_sub(submitted) as f64);
    }

    fn record_observed(&self, latest_observed: &mut u64, head: u64, submitted: u64) {
        *latest_observed = head;
        self.record_metrics(head, submitted);
    }

    async fn catch_up(
        &self,
        anchor: SubmissionAnchor,
        latest_observed: &mut u64,
    ) -> eyre::Result<Option<BatchCandidate>> {
        let head = self.provider.best_block_number()?;
        let from = latest_observed.saturating_add(1);
        if head < from {
            return Ok(None);
        }
        let boundary = fetch_next_finalized_batch_boundary(
            &self.provider,
            self.config.outbox_address,
            from,
            head,
        )?;
        let Some(boundary) = boundary else {
            self.record_observed(latest_observed, head, anchor.zone_height);
            return Ok(None);
        };
        let finalized =
            fetch_finalized_batch(&self.provider, self.config.outbox_address, &boundary).await?;
        let end = read_zone_block_snapshot(
            &self.provider,
            self.config.inbox_address,
            boundary.block_number,
        )?;
        *latest_observed = boundary.block_number;
        self.record_metrics(head, anchor.zone_height);
        let candidate = self.candidate(anchor, boundary, finalized, end);
        self.enqueue_prover(&candidate);
        Ok(Some(candidate))
    }

    fn notified_candidate(
        &self,
        anchor: SubmissionAnchor,
        latest_observed: &mut u64,
        number: u64,
        block_hash: B256,
        transactions: &[TempoTxEnvelope],
        receipts: &[TempoReceipt],
    ) -> eyre::Result<Option<BatchCandidate>> {
        let boundaries = finalized_batch_boundaries_from_parts(
            number,
            transactions,
            receipts,
            self.config.outbox_address,
        )?;
        eyre::ensure!(
            boundaries.len() <= 1,
            "zone block {number} contains more than one BatchFinalized event"
        );
        let Some(boundary) = boundaries.into_iter().next() else {
            self.record_observed(latest_observed, number, anchor.zone_height);
            return Ok(None);
        };
        let finalized = finalized_batch_from_parts(
            number,
            transactions,
            receipts,
            self.config.outbox_address,
            &boundary,
        )?;
        let end = read_zone_block_snapshot_from_receipts(
            self.config.inbox_address,
            number,
            block_hash,
            receipts,
        )?;
        *latest_observed = number;
        self.record_metrics(number, anchor.zone_height);
        let candidate = self.candidate(anchor, boundary, finalized, end);
        self.enqueue_prover(&candidate);
        Ok(Some(candidate))
    }

    async fn next(&self, anchor: SubmissionAnchor) -> eyre::Result<BatchCandidate> {
        // Subscribe before reading the provider head so canonical changes racing with the initial
        // backfill are either scanned from storage or retained in the notification stream.
        let mut canonical: CanonStateNotificationStream<TempoPrimitives> =
            self.provider.canonical_state_stream();
        let mut fallback = tokio::time::interval_at(
            tokio::time::Instant::now() + self.config.poll_interval,
            self.config.poll_interval,
        );
        let mut latest_observed = anchor.zone_height;
        let mut needs_catchup = true;
        loop {
            if needs_catchup {
                needs_catchup = false;
                if let Some(candidate) = self.catch_up(anchor, &mut latest_observed).await? {
                    return Ok(candidate);
                }
            }
            tokio::select! {
                biased;
                notification = canonical.next() => {
                    let Some(notification) = notification else {
                        eyre::bail!("canonical zone state notification stream closed");
                    };
                    if notification.reverted().is_some() {
                        eyre::bail!("canonical zone chain reorged while the sequencer was active");
                    }
                    let committed = notification.committed();
                    for (block, receipts) in committed.blocks_and_receipts() {
                        let block = block.sealed_block();
                        let number = block.number();
                        if number <= latest_observed {
                            continue;
                        }
                        if number != latest_observed.saturating_add(1) {
                            needs_catchup = true;
                            break;
                        }
                        if let Some(candidate) = self.notified_candidate(
                            anchor,
                            &mut latest_observed,
                            number,
                            block.hash(),
                            &block.body().transactions,
                            receipts,
                        )? {
                            return Ok(candidate);
                        }
                    }
                }
                _ = fallback.tick() => needs_catchup = true,
            }
        }
    }
}

impl<P: ZoneSequencerProvider> BatchCandidateSource for CandidateMonitor<P> {
    fn next_candidate(
        &self,
        anchor: SubmissionAnchor,
    ) -> BoxFuture<'_, eyre::Result<BatchCandidate>> {
        Box::pin(self.next(anchor))
    }
}
