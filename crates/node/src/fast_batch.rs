//! Canonical T14 fast-settlement batching reconstructed from the fsynced OpenRaft prefix.

use std::sync::Arc;

use alloy_consensus::{BlockHeader as _, Transaction as _};
use alloy_primitives::{B256, Bytes, U256};
use alloy_rlp::Decodable as _;
use alloy_sol_types::SolCall as _;

use crate::{
    fast_quorum::{
        CanonicalFastSettlementBoundary, FastBoundaryValidationError, ReplicatedBlockInput,
    },
    fast_raft_state_machine::AppliedBlock,
};
use zone_payload::{
    FAST_SETTLEMENT_BATCH_MAX_BYTES, FAST_SETTLEMENT_BATCH_MAX_DELAY, FastSettlementBoundary,
    FastSettlementTrigger, ZonePayloadAttributes,
};

/// Read-only closure over the fsynced state-machine image.
pub type AppliedPrefixReader =
    Arc<dyn Fn() -> Result<Vec<AppliedBlock>, String> + Send + Sync + 'static>;

/// Canonical scheduler status reconstructed without a process-local timer or counter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FastBatchStatus {
    pub previous_boundary: CanonicalFastSettlementBoundary,
    pub pending_retained_bytes: u64,
    pub next_deadline_unix_millis: u64,
    pub committed_head: Option<(u64, B256)>,
    pub due: Option<FastSettlementTrigger>,
}

/// Admission result for an exact candidate containing its replay/proof witness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FastBatchAdmission {
    Open,
    /// Rebuild the candidate with this marker in `ZonePayloadAttributes`.
    Boundary(FastSettlementBoundary),
}

/// Durable-prefix T14 scheduler. Clones are safe because all mutable authority is in the reader's
/// fsynced state image rather than in this value. It intentionally has no new-payment admission
/// gate: a closed-but-unretired drain authority must still close/prove the retained prefix.
#[derive(Clone)]
pub struct FastBatchScheduler {
    initial_boundary: CanonicalFastSettlementBoundary,
    applied_prefix: AppliedPrefixReader,
}

impl FastBatchScheduler {
    pub fn new(
        initial_boundary: CanonicalFastSettlementBoundary,
        applied_prefix: AppliedPrefixReader,
    ) -> Self {
        Self {
            initial_boundary,
            applied_prefix,
        }
    }

    /// Reconstruct the retained batch and its live deadline from the fsynced committed prefix.
    pub fn status(&self, now_unix_millis: u64) -> Result<FastBatchStatus, FastBatchError> {
        let prefix = (self.applied_prefix)().map_err(FastBatchError::ReadPrefix)?;
        let reconstructed = reconstruct_prefix(self.initial_boundary, &prefix)?;
        let next_deadline_unix_millis = reconstructed
            .boundary
            .timestamp_millis
            .checked_add(duration_millis())
            .ok_or(FastBatchError::TimestampOverflow)?;
        let due = select_trigger(
            (reconstructed.pending_retained_bytes > 0).then_some(next_deadline_unix_millis),
            reconstructed.size_threshold_timestamp_millis,
            now_unix_millis,
        );
        Ok(FastBatchStatus {
            previous_boundary: reconstructed.boundary,
            pending_retained_bytes: reconstructed.pending_retained_bytes,
            next_deadline_unix_millis,
            committed_head: reconstructed.committed_head,
            due,
        })
    }

    /// Decide whether an exact first-pass candidate must be rebuilt as a canonical boundary.
    pub fn admit(
        &self,
        candidate: &ReplicatedBlockInput,
    ) -> Result<FastBatchAdmission, FastBatchError> {
        let prefix = (self.applied_prefix)().map_err(FastBatchError::ReadPrefix)?;
        let reconstructed = reconstruct_prefix(self.initial_boundary, &prefix)?;
        validate_candidate_parent(candidate, &reconstructed)?;
        if candidate.fast_settlement_boundary()?.is_some() {
            return Err(FastBatchError::CandidateAlreadyMarked);
        }
        if !boundary_eligible(candidate)? {
            return Ok(FastBatchAdmission::Open);
        }
        let decoded = decode_candidate(candidate)?;
        let cumulative = reconstructed
            .pending_retained_bytes
            .checked_add(candidate.retained_encoded_len()?)
            .ok_or(FastBatchError::RetainedBytesOverflow)?;
        let deadline = reconstructed
            .boundary
            .timestamp_millis
            .checked_add(duration_millis())
            .ok_or(FastBatchError::TimestampOverflow)?;
        let size_timestamp = reconstructed
            .size_threshold_timestamp_millis
            .or_else(|| (cumulative >= max_retained_bytes()).then_some(decoded.timestamp_millis));
        let Some(trigger) =
            select_trigger(Some(deadline), size_timestamp, decoded.timestamp_millis)
        else {
            return Ok(FastBatchAdmission::Open);
        };
        let finalization_calldata = match decoded.finalization_calldata {
            Some(calldata) => calldata,
            None => zone_payload::abi::IZoneOutbox::finalizeWithdrawalBatchCall {
                count: U256::ZERO,
                blockNumber: decoded.block_height,
                encryptedSenders: Vec::new(),
            }
            .abi_encode()
            .into(),
        };
        Ok(FastBatchAdmission::Boundary(FastSettlementBoundary {
            version: zone_payload::FAST_SETTLEMENT_BOUNDARY_VERSION,
            previous_boundary_height: reconstructed.boundary.block_height,
            previous_boundary_hash: reconstructed.boundary.block_hash,
            previous_boundary_timestamp_millis: reconstructed.boundary.timestamp_millis,
            block_height: decoded.block_height,
            parent_hash: candidate.parent_hash,
            timestamp_millis: decoded.timestamp_millis,
            // Filled from the final rebuilt block/witness by `record_ready`.
            cumulative_retained_bytes: 0,
            trigger,
            finalization_calldata,
        }))
    }

    /// Finalize and validate a rebuilt boundary immediately before `FastRaft::commit`.
    pub fn record_ready(
        &self,
        input: &mut ReplicatedBlockInput,
    ) -> Result<FastSettlementBoundary, FastBatchError> {
        let prefix = (self.applied_prefix)().map_err(FastBatchError::ReadPrefix)?;
        let reconstructed = reconstruct_prefix(self.initial_boundary, &prefix)?;
        validate_candidate_parent(input, &reconstructed)?;

        let mut attributes: ZonePayloadAttributes = bincode::deserialize(&input.l1_inputs)?;
        let marker = attributes
            .fast_settlement_boundary
            .as_mut()
            .ok_or(FastBatchError::MissingMarker)?;
        marker.cumulative_retained_bytes = 0;
        input.l1_inputs = bincode::serialize(&attributes)?.into();
        let cumulative = reconstructed
            .pending_retained_bytes
            .checked_add(input.retained_encoded_len()?)
            .ok_or(FastBatchError::RetainedBytesOverflow)?;
        attributes
            .fast_settlement_boundary
            .as_mut()
            .expect("marker retained across serialization")
            .cumulative_retained_bytes = cumulative;
        input.l1_inputs = bincode::serialize(&attributes)?.into();
        let exact = reconstructed
            .pending_retained_bytes
            .checked_add(input.retained_encoded_len()?)
            .ok_or(FastBatchError::RetainedBytesOverflow)?;
        if cumulative != exact {
            return Err(FastBatchError::UnstableEncoding);
        }
        self.validate_ready(input)?
            .ok_or(FastBatchError::MissingMarker)
    }

    /// Validate a leader proposal against the local fsynced prefix before deterministic execution.
    /// Every replica must call this; leader-side `record_ready` is not a substitute.
    pub fn validate_ready(
        &self,
        input: &ReplicatedBlockInput,
    ) -> Result<Option<FastSettlementBoundary>, FastBatchError> {
        let prefix = (self.applied_prefix)().map_err(FastBatchError::ReadPrefix)?;
        let reconstructed = reconstruct_prefix(self.initial_boundary, &prefix)?;
        validate_candidate_parent(input, &reconstructed)?;
        let decoded = decode_candidate(input)?;
        let eligible = boundary_eligible(input)?;
        let cumulative = reconstructed
            .pending_retained_bytes
            .checked_add(input.retained_encoded_len()?)
            .ok_or(FastBatchError::RetainedBytesOverflow)?;
        let deadline = reconstructed
            .boundary
            .timestamp_millis
            .checked_add(duration_millis())
            .ok_or(FastBatchError::TimestampOverflow)?;
        let size_timestamp = reconstructed
            .size_threshold_timestamp_millis
            .or_else(|| (cumulative >= max_retained_bytes()).then_some(decoded.timestamp_millis));
        let expected_trigger =
            select_trigger(Some(deadline), size_timestamp, decoded.timestamp_millis);
        let Some(_) = input.fast_settlement_boundary()? else {
            if eligible && expected_trigger.is_some() {
                return Err(FastBatchError::MissingDueBoundary);
            }
            return Ok(None);
        };
        if !eligible {
            return Err(FastBatchError::CheckpointBoundary);
        }
        let marker = input.validate_fast_settlement_boundary(reconstructed.boundary, cumulative)?;
        let expected_trigger = expected_trigger.ok_or(FastBatchError::BoundaryNotDue)?;
        if marker.trigger != expected_trigger {
            return Err(FastBatchError::TriggerMismatch {
                expected: expected_trigger,
                actual: marker.trigger,
            });
        }
        Ok(Some(marker))
    }
}

#[derive(Clone, Copy)]
struct ReconstructedPrefix {
    boundary: CanonicalFastSettlementBoundary,
    pending_retained_bytes: u64,
    size_threshold_timestamp_millis: Option<u64>,
    committed_head: Option<(u64, B256)>,
}

fn reconstruct_prefix(
    initial_boundary: CanonicalFastSettlementBoundary,
    prefix: &[AppliedBlock],
) -> Result<ReconstructedPrefix, FastBatchError> {
    let mut boundary = initial_boundary;
    let mut pending_retained_bytes = 0u64;
    let mut size_threshold_timestamp_millis = None;
    let mut committed_head = Some((initial_boundary.block_height, initial_boundary.block_hash));
    let mut last_log_index = None;
    let mut last_block = committed_head;

    for applied in prefix {
        if last_log_index.is_some_and(|index| applied.log_id.index <= index) {
            return Err(FastBatchError::NonMonotonicPrefix);
        }
        if applied.output.input_digest != applied.input.digest() {
            return Err(FastBatchError::InputDigestMismatch);
        }
        if let Some((height, hash)) = last_block
            && (applied.output.block_height != height + 1 || applied.input.parent_hash != hash)
        {
            return Err(FastBatchError::NonContiguousPrefix);
        }
        let decoded = decode_candidate(&applied.input)?;
        if decoded.block_height != applied.output.block_height {
            return Err(FastBatchError::BlockHeightMismatch);
        }
        let retained = applied.input.retained_encoded_len()?;
        let cumulative = pending_retained_bytes
            .checked_add(retained)
            .ok_or(FastBatchError::RetainedBytesOverflow)?;
        let candidate_size_timestamp = size_threshold_timestamp_millis
            .or_else(|| (cumulative >= max_retained_bytes()).then_some(decoded.timestamp_millis));
        let marker = applied.input.fast_settlement_boundary()?;
        if marker.is_some() && !boundary_eligible(&applied.input)? {
            return Err(FastBatchError::CheckpointBoundary);
        }
        match (marker, decoded.finalization_calldata) {
            (Some(_), Some(_)) => {
                let marker = applied
                    .input
                    .validate_fast_settlement_boundary(boundary, cumulative)?;
                let deadline = boundary
                    .timestamp_millis
                    .checked_add(duration_millis())
                    .ok_or(FastBatchError::TimestampOverflow)?;
                let expected = select_trigger(
                    Some(deadline),
                    candidate_size_timestamp,
                    marker.timestamp_millis,
                )
                .ok_or(FastBatchError::BoundaryNotDue)?;
                if marker.trigger != expected {
                    return Err(FastBatchError::TriggerMismatch {
                        expected,
                        actual: marker.trigger,
                    });
                }
                boundary = CanonicalFastSettlementBoundary {
                    block_height: applied.output.block_height,
                    block_hash: applied.output.block_hash,
                    timestamp_millis: decoded.timestamp_millis,
                };
                pending_retained_bytes = 0;
                size_threshold_timestamp_millis = None;
            }
            (Some(_), None) => return Err(FastBatchError::MissingFinalization),
            (None, Some(_)) => {
                // Pre-T14 cadence and required deposit/withdrawal finalizations remain canonical
                // reset points even though they predate the versioned fast trigger record.
                boundary = CanonicalFastSettlementBoundary {
                    block_height: applied.output.block_height,
                    block_hash: applied.output.block_hash,
                    timestamp_millis: decoded.timestamp_millis,
                };
                pending_retained_bytes = 0;
                size_threshold_timestamp_millis = None;
            }
            (None, None) => {
                pending_retained_bytes = cumulative;
                size_threshold_timestamp_millis = candidate_size_timestamp;
            }
        }
        last_log_index = Some(applied.log_id.index);
        last_block = Some((applied.output.block_height, applied.output.block_hash));
        committed_head = last_block;
    }

    Ok(ReconstructedPrefix {
        boundary,
        pending_retained_bytes,
        size_threshold_timestamp_millis,
        committed_head,
    })
}

fn validate_candidate_parent(
    candidate: &ReplicatedBlockInput,
    prefix: &ReconstructedPrefix,
) -> Result<(), FastBatchError> {
    if let Some((height, hash)) = prefix.committed_head {
        let decoded = decode_candidate(candidate)?;
        if candidate.parent_hash != hash || decoded.block_height != height + 1 {
            return Err(FastBatchError::CandidateParentMismatch);
        }
    }
    Ok(())
}

fn boundary_eligible(input: &ReplicatedBlockInput) -> Result<bool, FastBatchError> {
    let attributes: ZonePayloadAttributes = bincode::deserialize(&input.l1_inputs)?;
    Ok(!matches!(
        attributes.tempo_import,
        zone_payload::TempoImport::CheckpointOnly(_)
    ))
}

struct DecodedCandidate {
    block_height: u64,
    timestamp_millis: u64,
    finalization_calldata: Option<Bytes>,
}

fn decode_candidate(input: &ReplicatedBlockInput) -> Result<DecodedCandidate, FastBatchError> {
    let mut encoded = input.block_input.as_ref();
    let block = tempo_primitives::Block::decode(&mut encoded)
        .map_err(|error| FastBatchError::Block(error.to_string()))?;
    if !encoded.is_empty() {
        return Err(FastBatchError::TrailingBlockBytes);
    }
    if block.header.parent_hash() != input.parent_hash {
        return Err(FastBatchError::CandidateParentMismatch);
    }
    let selector = zone_payload::abi::IZoneOutbox::finalizeWithdrawalBatchCall::SELECTOR;
    let finalizations = block
        .body
        .transactions
        .iter()
        .enumerate()
        .filter(|(_, transaction)| is_finalization_system_transaction(transaction, selector))
        .collect::<Vec<_>>();
    let finalization_calldata = match finalizations.as_slice() {
        [] => None,
        [(index, transaction)] if *index + 1 == block.body.transactions.len() => {
            Some(transaction.input().clone())
        }
        _ => return Err(FastBatchError::MalformedFinalization),
    };
    Ok(DecodedCandidate {
        block_height: block.header.number(),
        timestamp_millis: block.header.timestamp_millis(),
        finalization_calldata,
    })
}

fn is_finalization_system_transaction(
    transaction: &tempo_primitives::TempoTxEnvelope,
    selector: [u8; 4],
) -> bool {
    if transaction.to() != Some(zone_payload::abi::ZONE_OUTBOX_ADDRESS)
        || !transaction.input().starts_with(&selector)
    {
        return false;
    }
    let tempo_primitives::TempoTxEnvelope::Legacy(signed) = transaction else {
        return false;
    };
    let transaction = signed.tx();
    signed.signature() == &tempo_primitives::transaction::envelope::TEMPO_SYSTEM_TX_SIGNATURE
        && transaction.chain_id.is_some()
        && transaction.nonce == 0
        && transaction.gas_price == 0
        && transaction.gas_limit == 0
        && transaction.value == U256::ZERO
}

fn select_trigger(
    elapsed_deadline_millis: Option<u64>,
    size_threshold_millis: Option<u64>,
    observed_timestamp_millis: u64,
) -> Option<FastSettlementTrigger> {
    let elapsed = elapsed_deadline_millis.filter(|at| *at <= observed_timestamp_millis);
    let size = size_threshold_millis.filter(|at| *at <= observed_timestamp_millis);
    match (elapsed, size) {
        (None, None) => None,
        (Some(_), None) => Some(FastSettlementTrigger::Elapsed),
        (None, Some(_)) => Some(FastSettlementTrigger::RetainedBytes),
        (Some(elapsed), Some(size)) if elapsed < size => Some(FastSettlementTrigger::Elapsed),
        (Some(elapsed), Some(size)) if size < elapsed => Some(FastSettlementTrigger::RetainedBytes),
        (Some(_), Some(_)) => Some(FastSettlementTrigger::Both),
    }
}

const fn duration_millis() -> u64 {
    FAST_SETTLEMENT_BATCH_MAX_DELAY.as_millis() as u64
}

const fn max_retained_bytes() -> u64 {
    FAST_SETTLEMENT_BATCH_MAX_BYTES as u64
}

#[derive(Debug, thiserror::Error)]
pub enum FastBatchError {
    #[error("failed to read fsynced applied prefix: {0}")]
    ReadPrefix(String),
    #[error(transparent)]
    Boundary(#[from] FastBoundaryValidationError),
    #[error("failed to encode/decode payload attributes: {0}")]
    Attributes(#[from] bincode::Error),
    #[error("failed to decode candidate block: {0}")]
    Block(String),
    #[error("candidate block has trailing bytes")]
    TrailingBlockBytes,
    #[error("committed applied prefix is not monotonic")]
    NonMonotonicPrefix,
    #[error("committed applied prefix is not block-contiguous")]
    NonContiguousPrefix,
    #[error("committed input digest does not match its applied output")]
    InputDigestMismatch,
    #[error("decoded block height does not match its applied output")]
    BlockHeightMismatch,
    #[error("candidate does not extend the fsynced committed head")]
    CandidateParentMismatch,
    #[error("candidate already carries a fast-settlement marker")]
    CandidateAlreadyMarked,
    #[error("checkpoint-only catch-up payload cannot carry a settlement boundary")]
    CheckpointBoundary,
    #[error("ready boundary is missing its marker")]
    MissingMarker,
    #[error("proposal omitted a canonical fast-settlement boundary that is already due")]
    MissingDueBoundary,
    #[error("boundary marker has no actual finalization transaction")]
    MissingFinalization,
    #[error("candidate contains duplicate or non-final finalization transactions")]
    MalformedFinalization,
    #[error("fast-settlement timestamp arithmetic overflow")]
    TimestampOverflow,
    #[error("fast-settlement retained byte arithmetic overflow")]
    RetainedBytesOverflow,
    #[error("boundary metadata changed the supposedly fixed-width input encoding")]
    UnstableEncoding,
    #[error("fast-settlement boundary was not due")]
    BoundaryNotDue,
    #[error("fast-settlement trigger mismatch: expected {expected:?}, got {actual:?}")]
    TriggerMismatch {
        expected: FastSettlementTrigger,
        actual: FastSettlementTrigger,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Signed, TxLegacy};
    use alloy_eips::eip2718::Encodable2718 as _;
    use alloy_rlp::Decodable as _;
    use alloy_rpc_types_engine::PayloadAttributes as EthPayloadAttributes;
    use openraft::{CommittedLeaderId, LogId};
    use tempo_primitives::{Block, TempoHeader, TempoTxEnvelope};
    use zone_evm::same_anchor::SameAnchorOpening;
    use zone_payload::TempoImport;

    fn initial_boundary() -> CanonicalFastSettlementBoundary {
        CanonicalFastSettlementBoundary {
            block_height: 10,
            block_hash: B256::repeat_byte(0x10),
            timestamp_millis: 10_000,
        }
    }

    fn input(
        timestamp_millis: u64,
        witness_len: usize,
        marker: Option<FastSettlementBoundary>,
    ) -> ReplicatedBlockInput {
        let boundary = initial_boundary();
        let mut transactions = Vec::new();
        if let Some(marker) = &marker {
            let tx = TxLegacy {
                chain_id: Some(1337),
                nonce: 0,
                gas_price: 0,
                gas_limit: 0,
                to: zone_payload::abi::ZONE_OUTBOX_ADDRESS.into(),
                value: U256::ZERO,
                input: marker.finalization_calldata.clone(),
            };
            transactions.push(TempoTxEnvelope::Legacy(Signed::new_unhashed(
                tx,
                tempo_primitives::transaction::envelope::TEMPO_SYSTEM_TX_SIGNATURE,
            )));
        }
        let block = Block {
            header: TempoHeader {
                timestamp_millis_part: timestamp_millis % 1_000,
                inner: alloy_consensus::Header {
                    number: boundary.block_height + 1,
                    parent_hash: boundary.block_hash,
                    timestamp: timestamp_millis / 1_000,
                    ..Default::default()
                },
                ..Default::default()
            },
            body: alloy_consensus::BlockBody {
                transactions: transactions.clone(),
                ommers: Vec::new(),
                withdrawals: None,
            },
        };
        let attributes = ZonePayloadAttributes {
            inner: EthPayloadAttributes {
                timestamp: timestamp_millis / 1_000,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: Default::default(),
                withdrawals: None,
                parent_beacon_block_root: None,
                slot_number: None,
                target_gas_limit: None,
            },
            timestamp_millis_part: timestamp_millis % 1_000,
            tempo_import: TempoImport::SameAnchor(SameAnchorOpening::v1(
                7,
                B256::repeat_byte(7),
                10,
                0,
                1,
            )),
            fast_settlement_boundary: marker,
        };
        ReplicatedBlockInput {
            epoch: 1,
            parent_hash: boundary.block_hash,
            block_input: alloy_rlp::encode(block).into(),
            transactions: transactions
                .iter()
                .map(|transaction| transaction.encoded_2718().into())
                .collect(),
            l1_inputs: bincode::serialize(&attributes).unwrap().into(),
            replay_witness: vec![0x55; witness_len].into(),
        }
    }

    fn applied(input: ReplicatedBlockInput) -> AppliedBlock {
        let digest = input.digest();
        AppliedBlock {
            log_id: LogId::new(CommittedLeaderId::new(1, 1), 1),
            input,
            output: crate::fast_quorum::CommittedBlock {
                input_digest: digest,
                block_height: 11,
                block_hash: B256::repeat_byte(0x11),
                state_root: B256::repeat_byte(0x12),
                receipts_root: B256::repeat_byte(0x13),
            },
        }
    }

    fn scheduler(prefix: Vec<AppliedBlock>) -> FastBatchScheduler {
        FastBatchScheduler::new(initial_boundary(), Arc::new(move || Ok(prefix.clone())))
    }

    #[test]
    fn elapsed_boundary_is_exactly_five_hundred_milliseconds() {
        assert_eq!(select_trigger(Some(500), None, 499), None);
        assert_eq!(
            select_trigger(Some(500), None, 500),
            Some(FastSettlementTrigger::Elapsed)
        );
    }

    #[test]
    fn retained_size_boundary_is_exactly_one_mibibyte() {
        assert!(max_retained_bytes() > 0);
        assert_eq!(
            select_trigger(None, Some(100), 100),
            Some(FastSettlementTrigger::RetainedBytes)
        );
    }

    #[test]
    fn combined_boundary_records_both_limits() {
        assert_eq!(
            select_trigger(Some(500), Some(500), 500),
            Some(FastSettlementTrigger::Both)
        );
        assert_eq!(
            select_trigger(Some(500), Some(499), 500),
            Some(FastSettlementTrigger::RetainedBytes)
        );
        assert_eq!(
            select_trigger(Some(500), Some(501), 501),
            Some(FastSettlementTrigger::Elapsed)
        );
    }

    #[test]
    fn deadline_math_preserves_zero_four_ninety_nine_and_five_hundred() {
        let boundary = 10_000u64;
        let deadline = boundary + duration_millis();
        assert!(boundary < deadline);
        assert!(boundary + 499 < deadline);
        assert_eq!(boundary + 500, deadline);
        assert_eq!(scheduler(Vec::new()).status(u64::MAX).unwrap().due, None);
    }

    #[test]
    fn size_math_preserves_below_and_at_one_mibibyte() {
        assert!(max_retained_bytes() - 1 < max_retained_bytes());
        assert_eq!(1024 * 1024, max_retained_bytes());
    }

    #[test]
    fn restart_recovers_pending_elapsed_trigger_from_applied_prefix() {
        let prefix = vec![applied(input(10_100, 0, None))];
        let before_restart = scheduler(prefix.clone()).status(10_499).unwrap();
        assert_eq!(before_restart.due, None);
        let after_restart = scheduler(prefix).status(10_500).unwrap();
        assert_eq!(after_restart.due, Some(FastSettlementTrigger::Elapsed));
        assert_eq!(
            before_restart.pending_retained_bytes,
            after_restart.pending_retained_bytes
        );
    }

    #[test]
    fn exact_retained_witness_bytes_trigger_at_one_mibibyte() {
        let empty = input(10_100, 0, None);
        let base = empty.retained_encoded_len().unwrap();
        let padding = usize::try_from(max_retained_bytes() - base).unwrap();
        let below = input(10_100, padding - 1, None);
        assert_eq!(
            scheduler(Vec::new()).admit(&below).unwrap(),
            FastBatchAdmission::Open
        );
        let at = input(10_100, padding, None);
        let FastBatchAdmission::Boundary(marker) = scheduler(Vec::new()).admit(&at).unwrap() else {
            panic!("one MiB candidate must close the retained batch")
        };
        assert_eq!(marker.trigger, FastSettlementTrigger::RetainedBytes);
    }

    #[test]
    fn checkpoint_catchup_defers_wall_clock_boundary() {
        let mut checkpoint = input(10_500, 0, None);
        let mut attributes: ZonePayloadAttributes =
            bincode::deserialize(&checkpoint.l1_inputs).unwrap();
        attributes.tempo_import = TempoImport::CheckpointOnly(Vec::new());
        checkpoint.l1_inputs = bincode::serialize(&attributes).unwrap().into();
        assert_eq!(
            scheduler(Vec::new()).admit(&checkpoint).unwrap(),
            FastBatchAdmission::Open
        );
        assert_eq!(
            scheduler(Vec::new()).validate_ready(&checkpoint).unwrap(),
            None
        );
    }

    #[test]
    fn closed_drain_ready_record_binds_immutable_calldata_and_root() {
        let candidate = input(10_500, 0, None);
        assert!(matches!(
            scheduler(Vec::new()).validate_ready(&candidate),
            Err(FastBatchError::MissingDueBoundary)
        ));
        let FastBatchAdmission::Boundary(marker) = scheduler(Vec::new()).admit(&candidate).unwrap()
        else {
            panic!("deadline candidate must close the retained batch")
        };
        let mut ready = input(10_500, 0, Some(marker));
        let recorded = scheduler(Vec::new()).record_ready(&mut ready).unwrap();
        assert_eq!(
            recorded.finalization_calldata,
            ready
                .fast_settlement_boundary()
                .unwrap()
                .unwrap()
                .finalization_calldata
        );

        let mut changed_root = ready.clone();
        let mut encoded = changed_root.block_input.as_ref();
        let mut block = Block::decode(&mut encoded).unwrap();
        assert!(encoded.is_empty());
        block.header.inner.state_root = B256::repeat_byte(0x99);
        changed_root.block_input = alloy_rlp::encode(block).into();
        assert_ne!(ready.digest(), changed_root.digest());

        let mut attributes: ZonePayloadAttributes = bincode::deserialize(&ready.l1_inputs).unwrap();
        let boundary = attributes.fast_settlement_boundary.as_mut().unwrap();
        let mut changed_calldata = boundary.finalization_calldata.to_vec();
        changed_calldata[0] ^= 1;
        boundary.finalization_calldata = changed_calldata.into();
        ready.l1_inputs = bincode::serialize(&attributes).unwrap().into();
        assert!(scheduler(Vec::new()).record_ready(&mut ready).is_err());
    }
}
