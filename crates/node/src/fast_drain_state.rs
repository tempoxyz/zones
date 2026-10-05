//! Production C5 projection over the exact fsynced OpenRaft state image.

use std::collections::{BTreeMap, BTreeSet};

use alloy_consensus::{Transaction as _, transaction::TxHashRef as _};
use alloy_eips::eip2718::Decodable2718 as _;
use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_sol_types::SolCall as _;
use openraft::LogId;
use reth_storage_api::{BlockNumReader, BlockReader, HeaderProvider, ReceiptProvider};
use serde::{Deserialize, Serialize};
use tempo_zone_contracts::{
    FAST_TRANSFER_ADDRESS, IFastTransfer, ImportedBarrierCall, imported_barrier_call,
};
use tokio::sync::{mpsc, oneshot};
use zone_fast_transfer::{
    EpochRoster,
    drain::{
        BarrierInventory, BarrierResolutionInventory, CheckpointImage, CommittedDisposition,
        CommittedSourceLock, CommittedTerminal, DrainCertificate, barriers_hash,
    },
};
use zone_payload::{TempoImport, ZonePayloadAttributes};
use zone_primitives::fast_transfer::{
    ExposureRetirementEvidence, FastCheckpointStatement, OutcomeCertificate, TransferIntent,
    TransferOutcome,
};

use crate::{
    fast_drain::{
        CommittedDrainPoint, DrainFuture, DrainObjectKey, DrainPeer, DrainSigningPurpose,
        FastDrainCommittedState, FastDrainError, FinalAcceptedPrefix, SourceDrainSnapshot,
        SourceLockEvidence, final_settlement_digest,
    },
    fast_execution::CanonicalFastExecution,
    fast_raft_state_machine::{
        AppliedBlock, CertifiedExecutionRecord, CheckpointResourceImage, CommittedProtocolKind,
        CommittedProtocolRecord, CommittedStateHandle, ExactStateImage,
    },
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NoNewLocksPayload {
    pub source_epoch: u64,
    pub destination_portal: Address,
    pub destination_epoch: u64,
    pub closure_hash: B256,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LocalClosurePayload {
    pub epoch: u64,
    pub closure_hash: B256,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DrainCheckpointResources {
    /// Registry order, with canonical durable barrier/resolution encodings.
    pub peers: Vec<(Address, Vec<u8>, Vec<u8>)>,
}

/// Request consumed by the runtime's actual producer. `NoNewLocks` waits for the canonical L1
/// import entry whose opening transaction imported the named closure; it must not invent a second
/// native closure ABI. Other protocol mutations use the ordinary fast transaction path. Every
/// response is returned only after `CommittedStateHandle` observes the successful receipt.
pub enum FastDrainCommitRequest {
    NoNewLocks {
        canonical_payload: Vec<u8>,
        response: oneshot::Sender<Result<CommittedProtocolRecord, String>>,
    },
    ImportedBarrier {
        call: ImportedBarrierCall,
        certificate_digest: B256,
        /// Exact `BarrierInventory::durable_bytes`, retained and fsynced separately from the
        /// compact native calldata before this request is acknowledged.
        canonical_inventory: Vec<u8>,
        response: oneshot::Sender<Result<CommittedProtocolRecord, String>>,
    },
    InstallCheckpoint {
        image: CheckpointImage,
        response: oneshot::Sender<Result<CommittedDrainPoint, String>>,
    },
}

#[derive(Clone)]
pub struct FastDrainCommitHandle {
    sender: mpsc::Sender<FastDrainCommitRequest>,
}

impl FastDrainCommitHandle {
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<FastDrainCommitRequest>) {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        (Self { sender }, receiver)
    }

    async fn protocol(
        &self,
        kind: CommittedProtocolKind,
        payload: Vec<u8>,
    ) -> Result<CommittedProtocolRecord, FastDrainError> {
        let (response, receive) = oneshot::channel();
        let request = match kind {
            CommittedProtocolKind::NoNewLocks => FastDrainCommitRequest::NoNewLocks {
                canonical_payload: payload.clone(),
                response,
            },
            CommittedProtocolKind::ImportedBarrier => {
                return Err(FastDrainError::Consensus(
                    "imported barriers require the exact prepared native call".to_owned(),
                ));
            }
            CommittedProtocolKind::InstalledCheckpoint => {
                return Err(FastDrainError::Consensus(
                    "checkpoint installation uses the exact image request".to_owned(),
                ));
            }
            CommittedProtocolKind::ObservedLocalClosure => {
                return Err(FastDrainError::Consensus(
                    "local closure is recorded by canonical L1 import".to_owned(),
                ));
            }
        };
        self.sender
            .send(request)
            .await
            .map_err(|_| FastDrainError::Consensus("fast drain producer stopped".to_owned()))?;
        let record = receive
            .await
            .map_err(|_| FastDrainError::Consensus("fast drain producer dropped reply".to_owned()))?
            .map_err(FastDrainError::Consensus)?;
        if record.kind != kind || record.canonical_payload != payload {
            return Err(FastDrainError::Consensus(
                "producer returned a different committed protocol body".to_owned(),
            ));
        }
        Ok(record)
    }

    async fn imported_barrier(
        &self,
        call: ImportedBarrierCall,
        certificate_digest: B256,
        canonical_inventory: Vec<u8>,
    ) -> Result<CommittedProtocolRecord, FastDrainError> {
        let (response, receive) = oneshot::channel();
        self.sender
            .send(FastDrainCommitRequest::ImportedBarrier {
                call,
                certificate_digest,
                canonical_inventory: canonical_inventory.clone(),
                response,
            })
            .await
            .map_err(|_| FastDrainError::Consensus("fast drain producer stopped".to_owned()))?;
        let record = receive
            .await
            .map_err(|_| FastDrainError::Consensus("fast drain producer dropped reply".to_owned()))?
            .map_err(FastDrainError::Consensus)?;
        if record.kind != CommittedProtocolKind::ImportedBarrier
            || record.canonical_payload != canonical_inventory
        {
            return Err(FastDrainError::Consensus(
                "producer returned a different imported-barrier inventory".to_owned(),
            ));
        }
        Ok(record)
    }

    async fn install(&self, image: CheckpointImage) -> Result<CommittedDrainPoint, FastDrainError> {
        let (response, receive) = oneshot::channel();
        self.sender
            .send(FastDrainCommitRequest::InstallCheckpoint { image, response })
            .await
            .map_err(|_| FastDrainError::Consensus("fast drain producer stopped".to_owned()))?;
        receive
            .await
            .map_err(|_| FastDrainError::Consensus("fast drain producer dropped reply".to_owned()))?
            .map_err(FastDrainError::Consensus)
    }
}

/// Concrete committed-state adapter. All projection inputs come from one exact fsynced image.
pub struct ProductionFastDrainState<P> {
    committed: CommittedStateHandle<CanonicalFastExecution<P>>,
    producer: FastDrainCommitHandle,
    local_roster: EpochRoster,
    next_roster: Option<EpochRoster>,
    peers: [DrainPeer; zone_fast_transfer::drain::DRAIN_PEER_COUNT],
}

impl<P> ProductionFastDrainState<P> {
    pub const fn new(
        committed: CommittedStateHandle<CanonicalFastExecution<P>>,
        producer: FastDrainCommitHandle,
        local_roster: EpochRoster,
        next_roster: Option<EpochRoster>,
        peers: [DrainPeer; zone_fast_transfer::drain::DRAIN_PEER_COUNT],
    ) -> Self {
        Self {
            committed,
            producer,
            local_roster,
            next_roster,
            peers,
        }
    }

    pub fn committed_handle(&self) -> &CommittedStateHandle<CanonicalFastExecution<P>> {
        &self.committed
    }
}

impl<P> ProductionFastDrainState<P>
where
    P: BlockNumReader
        + BlockReader<Block = tempo_primitives::Block>
        + HeaderProvider<Header = tempo_primitives::TempoHeader>
        + ReceiptProvider<Receipt = tempo_primitives::TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    fn image(&self) -> Result<ExactStateImage, FastDrainError> {
        self.committed.exact_state_image().map_err(state_error)
    }

    fn point_at(
        image: &ExactStateImage,
        log_id: LogId<u64>,
    ) -> Result<CommittedDrainPoint, FastDrainError> {
        let applied = image
            .blocks
            .iter()
            .find(|block| block.log_id == log_id)
            .ok_or(FastDrainError::InvalidCommittedPoint)?;
        let (imported_anchor_number, imported_anchor_hash) =
            anchor_through(&image.blocks, log_id.index)?;
        Ok(CommittedDrainPoint {
            log_term: log_id.leader_id.term,
            log_index: log_id.index,
            block_height: applied.output.block_height,
            block_hash: applied.output.block_hash,
            state_root: applied.output.state_root,
            imported_anchor_number,
            imported_anchor_hash,
        })
    }

    fn snapshot_at(
        &self,
        image: &ExactStateImage,
        log_index: u64,
        destination: &EpochRoster,
    ) -> Result<SourceDrainSnapshot, FastDrainError> {
        let mut result = SourceDrainSnapshot::default();
        for raw in image
            .certified_history
            .iter()
            .filter(|entry| entry.log_id.index <= log_index)
        {
            let (intent, certificate) = decode_history(raw)?;
            if intent.source != self.local_roster.domain || intent.destination != destination.domain
            {
                continue;
            }
            let transfer_id = intent.transfer_id();
            match certificate.body.outcome {
                TransferOutcome::Locked { .. } if certificate.body.zone == intent.source => {
                    if result
                        .locks
                        .iter()
                        .any(|lock| lock.transfer_id() == transfer_id)
                    {
                        return Err(FastDrainError::IncompleteCommittedHistory);
                    }
                    result.lock_evidence.insert(
                        transfer_id,
                        SourceLockEvidence {
                            native_calldata: raw.native_calldata.clone(),
                            replay_witness: raw.replay_witness.clone(),
                            canonical_receipt: raw.canonical_receipt.clone(),
                        },
                    );
                    result.locks.push(CommittedSourceLock {
                        intent,
                        lock: certificate,
                    });
                }
                TransferOutcome::Paid { .. } | TransferOutcome::Rejected { .. }
                    if certificate.body.zone == intent.destination =>
                {
                    insert_exact(
                        &mut result.terminals,
                        transfer_id,
                        CommittedTerminal { certificate },
                    )?;
                }
                TransferOutcome::Released { .. } | TransferOutcome::Refunded { .. }
                    if certificate.body.zone == intent.source =>
                {
                    insert_exact(
                        &mut result.dispositions,
                        transfer_id,
                        CommittedDisposition { certificate },
                    )?;
                }
                _ => return Err(FastDrainError::IncompleteCommittedHistory),
            }
        }
        result
            .locks
            .sort_by_key(|lock| (lock.lock.body.log_index, lock.transfer_id()));
        Ok(result)
    }

    fn no_new_locks_record<'a>(
        image: &'a ExactStateImage,
        log_index: u64,
        destination: &EpochRoster,
        closure_hash: Option<B256>,
    ) -> Result<&'a CommittedProtocolRecord, FastDrainError> {
        let mut matches = image.protocol_records.iter().filter(|record| {
            record.kind == CommittedProtocolKind::NoNewLocks
                && record.log_id.index == log_index
                && bincode::deserialize::<NoNewLocksPayload>(&record.canonical_payload).is_ok_and(
                    |payload| {
                        payload.destination_portal == destination.domain.portal
                            && payload.destination_epoch == destination.domain.authority_epoch
                            && closure_hash.is_none_or(|expected| payload.closure_hash == expected)
                    },
                )
        });
        let record = matches
            .next()
            .ok_or(FastDrainError::IncompleteCommittedHistory)?;
        if matches.next().is_some() {
            return Err(FastDrainError::IncompleteCommittedHistory);
        }
        Ok(record)
    }

    fn required_next_roster(&self) -> Result<&EpochRoster, FastDrainError> {
        self.next_roster
            .as_ref()
            .ok_or(FastDrainError::MissingNextRoster)
    }

    fn assert_successful_protocol_record(
        &self,
        record: &CommittedProtocolRecord,
    ) -> Result<(), FastDrainError> {
        (self
            .committed
            .committed_transaction_result(record.transaction_hash)
            .map_err(state_error)?
            == Some(true))
        .then_some(())
        .ok_or(FastDrainError::IncompleteCommittedHistory)
    }

    fn resources(image: &ExactStateImage) -> Result<&CheckpointResourceImage, FastDrainError> {
        image
            .checkpoint_resources
            .as_ref()
            .ok_or(FastDrainError::IncompleteCommittedHistory)
    }

    fn build_checkpoint(
        &self,
        image: &ExactStateImage,
        settlement_hash: B256,
        next: &EpochRoster,
    ) -> Result<CheckpointImage, FastDrainError> {
        let resources = Self::resources(image)?;
        if resources.final_settlement_hash != settlement_hash || resources.at != image.last_applied
        {
            return Err(FastDrainError::IncompleteCommittedHistory);
        }
        let point = Self::point_at(image, image.last_applied)?;
        let transfer_history = bincode::serialize(&image.certified_history).map_err(storage)?;
        let replay_witnesses = bincode::serialize(
            &image
                .blocks
                .iter()
                .map(|block| block.input.replay_witness.clone())
                .collect::<Vec<_>>(),
        )
        .map_err(storage)?;
        let raft_prefix = image
            .blocks
            .iter()
            .map(|block| block.output.block_hash)
            .collect::<Vec<_>>();
        let checkpoint = CheckpointImage {
            l1_chain_id: self.local_roster.domain.l1_chain_id,
            statement: FastCheckpointStatement {
                portal: self.local_roster.domain.portal,
                old_epoch: self.local_roster.domain.authority_epoch,
                next_epoch: next.domain.authority_epoch,
                next_roster_hash: next.domain.roster_hash,
                final_zone_height: U256::from(point.block_height),
                final_block_hash: point.block_hash,
                final_withdrawal_batch_index: resources.withdrawal_batch_index,
                final_settlement_hash: settlement_hash,
                checkpoint_log_term: point.log_term,
                checkpoint_log_index: point.log_index,
                checkpoint_height: U256::from(point.block_height),
                checkpoint_block_hash: point.block_hash,
                checkpoint_state_root: point.state_root,
            },
            canonical_head_hash: point.block_hash,
            canonical_state_root: point.state_root,
            witness_root: keccak256(&replay_witnesses),
            outcomes_root: keccak256(&transfer_history),
            replay_barriers_root: keccak256(&resources.drain_barriers),
            raft_prefix,
            transfer_history,
            replay_witnesses,
            replay_barriers: resources.drain_barriers.clone(),
            fast_service_journal: resources.service_protocol_journal.clone(),
            batch_boundaries: resources.canonical_batch_boundary.clone(),
            replenishment_state: resources.replenishment_inventory.clone(),
            consensus_snapshot: image.bytes.clone(),
        };
        checkpoint.validate()?;
        Ok(checkpoint)
    }
}

impl<P> FastDrainCommittedState for ProductionFastDrainState<P>
where
    P: BlockNumReader
        + BlockReader<Block = tempo_primitives::Block>
        + HeaderProvider<Header = tempo_primitives::TempoHeader>
        + ReceiptProvider<Receipt = tempo_primitives::TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    fn commit_no_new_locks<'a>(
        &'a self,
        destination: &'a EpochRoster,
        closure_hash: B256,
    ) -> DrainFuture<'a, Result<CommittedDrainPoint, FastDrainError>> {
        Box::pin(async move {
            if closure_hash.is_zero() {
                return Err(FastDrainError::InvalidClosure);
            }
            let mut destinations = self.peers.iter().filter(|peer| peer.roster == *destination);
            let configured = destinations.next().ok_or(FastDrainError::WrongBarrier)?;
            if destinations.next().is_some() {
                return Err(FastDrainError::WrongBarrier);
            }
            let payload = bincode::serialize(&NoNewLocksPayload {
                source_epoch: self.local_roster.domain.authority_epoch,
                destination_portal: configured.roster.domain.portal,
                destination_epoch: configured.roster.domain.authority_epoch,
                closure_hash,
            })
            .map_err(storage)?;
            let record = self
                .producer
                .protocol(CommittedProtocolKind::NoNewLocks, payload)
                .await?;
            self.assert_successful_protocol_record(&record)?;
            let image = self.image()?;
            assert_l1_import_entry(&image, record.log_id)?;
            self.committed
                .persist_protocol_record(record.clone())
                .map_err(state_error)?;
            Self::point_at(&image, record.log_id)
        })
    }

    fn local_destination_closure_point(
        &self,
        epoch: u64,
        closure_hash: B256,
    ) -> Result<CommittedDrainPoint, FastDrainError> {
        let image = self.image()?;
        let record = image
            .protocol_records
            .iter()
            .find(|record| {
                record.kind == CommittedProtocolKind::ObservedLocalClosure
                    && bincode::deserialize::<LocalClosurePayload>(&record.canonical_payload)
                        .is_ok_and(|payload| {
                            payload.epoch == epoch && payload.closure_hash == closure_hash
                        })
            })
            .ok_or(FastDrainError::IncompleteCommittedHistory)?;
        assert_l1_import_entry(&image, record.log_id)?;
        self.assert_successful_protocol_record(record)?;
        Self::point_at(&image, record.log_id)
    }

    fn source_snapshot(
        &self,
        point: CommittedDrainPoint,
        destination: &EpochRoster,
    ) -> Result<SourceDrainSnapshot, FastDrainError> {
        let image = self.image()?;
        assert_point(&image, point)?;
        self.snapshot_at(&image, point.log_index, destination)
    }

    fn assert_imported_destination_closure(
        &self,
        point: CommittedDrainPoint,
        destination: &EpochRoster,
        closure_hash: B256,
    ) -> Result<(), FastDrainError> {
        let image = self.image()?;
        assert_point(&image, point)?;
        let record =
            Self::no_new_locks_record(&image, point.log_index, destination, Some(closure_hash))?;
        let payload: NoNewLocksPayload =
            bincode::deserialize(&record.canonical_payload).map_err(storage)?;
        self.assert_successful_protocol_record(record)?;
        ((payload.source_epoch == self.local_roster.domain.authority_epoch)
            && payload.destination_portal == destination.domain.portal
            && payload.destination_epoch == destination.domain.authority_epoch
            && payload.closure_hash == closure_hash)
            .then_some(())
            .ok_or(FastDrainError::WrongBarrier)
    }

    fn current_source_snapshot(
        &self,
        point: CommittedDrainPoint,
        destination: &EpochRoster,
    ) -> Result<(SourceDrainSnapshot, CommittedDrainPoint), FastDrainError> {
        let image = self.image()?;
        assert_point(&image, point)?;
        let current = Self::point_at(&image, image.last_applied)?;
        Ok((
            self.snapshot_at(&image, current.log_index, destination)?,
            current,
        ))
    }

    fn commit_imported_barrier<'a>(
        &'a self,
        inventory: &'a BarrierInventory,
        certificate: DrainCertificate,
    ) -> DrainFuture<'a, Result<CommittedDrainPoint, FastDrainError>> {
        Box::pin(async move {
            inventory.verify_complete()?;
            let canonical_inventory = inventory.durable_bytes()?;
            let call = imported_barrier_call(
                &inventory.statement,
                certificate.digest,
                certificate.signatures,
            );
            let record = self
                .producer
                .imported_barrier(call, certificate.digest, canonical_inventory)
                .await?;
            self.assert_successful_protocol_record(&record)?;
            Self::point_at(&self.image()?, record.log_id)
        })
    }

    fn assert_signing_body(
        &self,
        purpose: DrainSigningPurpose,
        key: &DrainObjectKey,
        digest: B256,
        point: CommittedDrainPoint,
    ) -> Result<(), FastDrainError> {
        let image = self.image()?;
        assert_point(&image, point)?;
        let expected = match (purpose, key) {
            (
                DrainSigningPurpose::Barrier,
                DrainObjectKey::OutboundBarrier { epoch, destination },
            ) => {
                let peer = self
                    .peers
                    .iter()
                    .find(|peer| peer.roster.domain.portal == *destination)
                    .ok_or(FastDrainError::WrongBarrier)?;
                let closure =
                    Self::no_new_locks_record(&image, point.log_index, &peer.roster, None)?;
                self.assert_successful_protocol_record(closure)?;
                let closure: NoNewLocksPayload =
                    bincode::deserialize(&closure.canonical_payload).map_err(storage)?;
                if closure.source_epoch != self.local_roster.domain.authority_epoch {
                    return Err(FastDrainError::ConflictingCertificate);
                }
                let snapshot = self.snapshot_at(&image, point.log_index, &peer.roster)?;
                BarrierInventory::build(
                    self.local_roster.domain.l1_chain_id,
                    *destination,
                    peer.roster.domain.authority_epoch,
                    closure.closure_hash,
                    self.local_roster.domain.portal,
                    *epoch,
                    point.imported_anchor_number,
                    point.imported_anchor_hash,
                    point.log_term,
                    point.log_index,
                    point.block_height,
                    point.block_hash,
                    point.state_root,
                    snapshot.locks,
                    &snapshot.terminals,
                    &snapshot.dispositions,
                    &snapshot.policy_blocked,
                )?
                .statement
                .registry_digest(self.local_roster.domain.l1_chain_id)
            }
            (
                DrainSigningPurpose::Resolution,
                DrainObjectKey::Resolution {
                    destination_epoch,
                    destination,
                    source,
                },
            ) => {
                if *source != self.local_roster.domain.portal {
                    return Err(FastDrainError::ConflictingCertificate);
                }
                let peer = self
                    .peers
                    .iter()
                    .find(|peer| {
                        peer.roster.domain.portal == *destination
                            && peer.roster.domain.authority_epoch == *destination_epoch
                    })
                    .ok_or(FastDrainError::WrongBarrier)?;
                let resources = Self::resources(&image)?;
                let drain: DrainCheckpointResources =
                    bincode::deserialize(&resources.drain_barriers).map_err(storage)?;
                let barrier_bytes = drain
                    .peers
                    .iter()
                    .find(|entry| entry.0 == *destination)
                    .ok_or(FastDrainError::MissingBarrier)?
                    .1
                    .clone();
                let barrier = BarrierInventory::decode_durable(&barrier_bytes)?;
                let snapshot = self.snapshot_at(&image, point.log_index, &peer.roster)?;
                BarrierResolutionInventory::build(
                    &barrier,
                    &snapshot.terminals,
                    &snapshot.dispositions,
                    &snapshot.policy_blocked,
                )?
                .resolution
                .registry_digest(
                    self.local_roster.domain.l1_chain_id,
                    *destination,
                    *destination_epoch,
                    *source,
                )
            }
            (DrainSigningPurpose::FinalSettlement, DrainObjectKey::FinalSettlement { epoch }) => {
                let resources = Self::resources(&image)?;
                let prefix = prefix_from(&image, resources)?;
                let entries = drain_entries(resources)?;
                final_settlement_digest(
                    self.local_roster.domain.l1_chain_id,
                    self.local_roster.domain.portal,
                    *epoch,
                    self.local_roster.domain.roster_hash,
                    resources.local_closure_hash,
                    prefix,
                    barriers_hash(&entries)?,
                )
            }
            (
                DrainSigningPurpose::Checkpoint,
                DrainObjectKey::Checkpoint {
                    old_epoch,
                    next_epoch,
                },
            ) => {
                if *old_epoch != self.local_roster.domain.authority_epoch {
                    return Err(FastDrainError::ConflictingCertificate);
                }
                let next = self.required_next_roster()?;
                if next.domain.authority_epoch != *next_epoch {
                    return Err(FastDrainError::ConflictingCertificate);
                }
                self.build_checkpoint(&image, Self::resources(&image)?.final_settlement_hash, next)?
                    .statement
                    .registry_digest(self.local_roster.domain.l1_chain_id)
            }
            _ => return Err(FastDrainError::ConflictingCertificate),
        };
        (expected == digest)
            .then_some(())
            .ok_or(FastDrainError::ConflictingCertificate)
    }

    fn final_accepted_prefix(
        &self,
    ) -> Result<(FinalAcceptedPrefix, CommittedDrainPoint), FastDrainError> {
        let image = self.image()?;
        let resources = Self::resources(&image)?;
        let point = Self::point_at(&image, image.last_applied)?;
        Ok((prefix_from(&image, resources)?, point))
    }

    fn record_final_settlement_hash(&self, settlement_hash: B256) -> Result<(), FastDrainError> {
        self.committed
            .finalize_checkpoint_resources(settlement_hash)
            .map(|_| ())
            .map_err(state_error)
    }

    fn assert_exposure_retirement_complete(&self, epoch: u64) -> Result<(), FastDrainError> {
        if self.local_roster.domain.authority_epoch != epoch {
            return Err(FastDrainError::IncompleteCommittedHistory);
        }
        let image = self.image()?;
        let mut paid = BTreeMap::new();
        for record in &image.certified_history {
            let (intent, certificate) = decode_history(record)?;
            if intent.destination != self.local_roster.domain
                || certificate.body.zone != intent.destination
                || !matches!(certificate.body.outcome, TransferOutcome::Paid { .. })
            {
                continue;
            }
            validate_paid(&intent, &certificate)?;
            insert_history_exact(
                &mut paid,
                record.transfer_id,
                (intent, certificate, record.log_id.index),
            )?;
        }
        for native in self.committed.committed_transfers().map_err(state_error)? {
            if native.intent.destination == self.local_roster.domain
                && matches!(native.body.outcome, TransferOutcome::Paid { .. })
                && !paid.contains_key(&native.intent.transfer_id())
            {
                return Err(FastDrainError::IncompleteCommittedHistory);
            }
        }

        let mut retired = BTreeSet::new();
        for block in &image.blocks {
            for encoded in &block.input.transactions {
                let mut bytes = encoded.as_ref();
                let transaction =
                    tempo_primitives::TempoTxEnvelope::decode_2718(&mut bytes).map_err(storage)?;
                if !bytes.is_empty()
                    || transaction.to() != Some(FAST_TRANSFER_ADDRESS)
                    || !transaction
                        .input()
                        .starts_with(&IFastTransfer::retireExposureCall::SELECTOR)
                {
                    continue;
                }
                if self
                    .committed
                    .committed_transaction_result(*transaction.tx_hash())
                    .map_err(state_error)?
                    != Some(true)
                {
                    continue;
                }
                let call = IFastTransfer::retireExposureCall::abi_decode(transaction.input())
                    .map_err(storage)?;
                let evidence =
                    ExposureRetirementEvidence::decode(&call.canonicalEvidence).map_err(storage)?;
                let Some((intent, _, paid_index)) = paid.get(&evidence.transfer_id) else {
                    continue;
                };
                if block.log_id.index < *paid_index
                    || evidence.intent_hash != intent.intent_hash()
                    || evidence.destination_token != intent.asset.destination_token
                    || evidence.beneficiary != intent.reimbursement_account
                    || evidence.principal != intent.principal
                    || evidence.accepted_source_block_hash.is_zero()
                    || evidence.release_receipt_hash.is_zero()
                    || evidence.receipt_proof.is_empty()
                    || evidence.header_chain.is_empty()
                {
                    return Err(FastDrainError::IncompleteCommittedHistory);
                }
                retired.insert(evidence.transfer_id);
            }
        }
        paid.keys()
            .all(|transfer_id| retired.contains(transfer_id))
            .then_some(())
            .ok_or(FastDrainError::IncompleteCommittedHistory)
    }

    fn assert_source_dispositions_complete(&self, epoch: u64) -> Result<(), FastDrainError> {
        if self.local_roster.domain.authority_epoch != epoch {
            return Err(FastDrainError::IncompleteCommittedHistory);
        }
        let image = self.image()?;
        let mut locks = BTreeMap::new();
        let mut terminals = BTreeMap::new();
        let mut dispositions = BTreeMap::new();
        for record in &image.certified_history {
            let (intent, certificate) = decode_history(record)?;
            if intent.source != self.local_roster.domain {
                continue;
            }
            match &certificate.body.outcome {
                TransferOutcome::Locked { .. } if certificate.body.zone == intent.source => {
                    validate_lock(&intent, &certificate)?;
                    insert_history_exact(&mut locks, record.transfer_id, (intent, certificate))?;
                }
                TransferOutcome::Paid { .. } | TransferOutcome::Rejected { .. }
                    if certificate.body.zone == intent.destination =>
                {
                    insert_history_exact(&mut terminals, record.transfer_id, certificate)?;
                }
                TransferOutcome::Released { .. } | TransferOutcome::Refunded { .. }
                    if certificate.body.zone == intent.source =>
                {
                    insert_history_exact(&mut dispositions, record.transfer_id, certificate)?;
                }
                _ => return Err(FastDrainError::IncompleteCommittedHistory),
            }
        }
        if terminals
            .keys()
            .any(|transfer_id| !locks.contains_key(transfer_id))
            || dispositions
                .keys()
                .any(|transfer_id| !locks.contains_key(transfer_id))
        {
            return Err(FastDrainError::IncompleteCommittedHistory);
        }
        for native in self.committed.committed_transfers().map_err(state_error)? {
            if native.intent.source == self.local_roster.domain
                && !locks.contains_key(&native.intent.transfer_id())
            {
                return Err(FastDrainError::IncompleteCommittedHistory);
            }
        }
        for (transfer_id, (intent, _lock)) in locks {
            let terminal = terminals
                .get(&transfer_id)
                .ok_or(FastDrainError::IncompleteCommittedHistory)?;
            let disposition = dispositions
                .get(&transfer_id)
                .ok_or(FastDrainError::IncompleteCommittedHistory)?;
            validate_terminal_disposition(&intent, terminal, disposition)?;
        }
        Ok(())
    }

    fn checkpoint_image(
        &self,
        settlement_hash: B256,
        next: &EpochRoster,
    ) -> Result<CheckpointImage, FastDrainError> {
        if self.required_next_roster()? != next {
            return Err(FastDrainError::MissingNextRoster);
        }
        self.build_checkpoint(&self.image()?, settlement_hash, next)
    }

    fn install_checkpoint<'a>(
        &'a self,
        image: &'a CheckpointImage,
    ) -> DrainFuture<'a, Result<CommittedDrainPoint, FastDrainError>> {
        Box::pin(async move {
            image.validate()?;
            let next = self.required_next_roster()?;
            if image.statement.next_epoch != next.domain.authority_epoch
                || image.statement.next_roster_hash != next.domain.roster_hash
            {
                return Err(FastDrainError::CheckpointInstallationMismatch);
            }
            let point = self.producer.install(image.clone()).await?;
            let local = self.image()?;
            if image.consensus_snapshot != local.bytes
                || image.canonical_head_hash
                    != local
                        .blocks
                        .last()
                        .ok_or(FastDrainError::InvalidCommittedPoint)?
                        .output
                        .block_hash
            {
                return Err(FastDrainError::CheckpointInstallationMismatch);
            }
            assert_point(&local, point)?;
            self.committed
                .persist_installed_checkpoint_hash(image.image_hash()?)
                .map_err(state_error)?;
            Ok(point)
        })
    }

    fn installed_checkpoint_hash(&self) -> Result<B256, FastDrainError> {
        self.image()?
            .installed_checkpoint_hash
            .ok_or(FastDrainError::CheckpointInstallationMismatch)
    }
}

fn decode_history(
    record: &CertifiedExecutionRecord,
) -> Result<(TransferIntent, OutcomeCertificate), FastDrainError> {
    let intent = TransferIntent::decode(&record.canonical_intent)
        .map_err(|_| FastDrainError::IncompleteCommittedHistory)?;
    let certificate = OutcomeCertificate::decode(&record.canonical_certificate)
        .map_err(|_| FastDrainError::IncompleteCommittedHistory)?;
    if record.transfer_id != intent.transfer_id()
        || certificate.body.transfer_id != record.transfer_id
        || certificate.body.intent_hash != intent.intent_hash()
    {
        return Err(FastDrainError::IncompleteCommittedHistory);
    }
    Ok((intent, certificate))
}

fn insert_exact<T: Eq>(
    map: &mut BTreeMap<B256, T>,
    key: B256,
    value: T,
) -> Result<(), FastDrainError> {
    if map.insert(key, value).is_some() {
        return Err(FastDrainError::IncompleteCommittedHistory);
    }
    Ok(())
}

fn insert_history_exact<T: Eq>(
    map: &mut BTreeMap<B256, T>,
    key: B256,
    value: T,
) -> Result<(), FastDrainError> {
    if let Some(existing) = map.get(&key) {
        return (existing == &value)
            .then_some(())
            .ok_or(FastDrainError::IncompleteCommittedHistory);
    }
    map.insert(key, value);
    Ok(())
}

fn validate_paid(
    intent: &TransferIntent,
    certificate: &OutcomeCertificate,
) -> Result<(), FastDrainError> {
    match certificate.body.outcome {
        TransferOutcome::Paid {
            pool,
            recipient,
            principal,
        } if certificate.body.zone == intent.destination
            && pool == intent.destination_pool
            && recipient == intent.recipient
            && principal == intent.principal =>
        {
            Ok(())
        }
        _ => Err(FastDrainError::IncompleteCommittedHistory),
    }
}

fn validate_lock(
    intent: &TransferIntent,
    certificate: &OutcomeCertificate,
) -> Result<(), FastDrainError> {
    let total = intent
        .principal
        .checked_add(intent.fee)
        .ok_or(FastDrainError::IncompleteCommittedHistory)?;
    match certificate.body.outcome {
        TransferOutcome::Locked { escrow, amount }
            if certificate.body.zone == intent.source
                && escrow == FAST_TRANSFER_ADDRESS
                && amount == total =>
        {
            Ok(())
        }
        _ => Err(FastDrainError::IncompleteCommittedHistory),
    }
}

fn validate_terminal_disposition(
    intent: &TransferIntent,
    terminal: &OutcomeCertificate,
    disposition: &OutcomeCertificate,
) -> Result<(), FastDrainError> {
    let total = intent
        .principal
        .checked_add(intent.fee)
        .ok_or(FastDrainError::IncompleteCommittedHistory)?;
    if disposition.body.zone != intent.source {
        return Err(FastDrainError::IncompleteCommittedHistory);
    }
    match (&terminal.body.outcome, &disposition.body.outcome) {
        (
            TransferOutcome::Paid {
                pool,
                recipient,
                principal,
            },
            TransferOutcome::Released {
                beneficiary,
                amount,
            },
        ) if terminal.body.zone == intent.destination
            && *pool == intent.destination_pool
            && *recipient == intent.recipient
            && *principal == intent.principal
            && *beneficiary == intent.reimbursement_account
            && *amount == total =>
        {
            Ok(())
        }
        (
            TransferOutcome::Rejected { .. },
            TransferOutcome::Refunded {
                beneficiary,
                amount,
            },
        ) if terminal.body.zone == intent.destination
            && *beneficiary == intent.refund_account
            && *amount == total =>
        {
            Ok(())
        }
        _ => Err(FastDrainError::IncompleteCommittedHistory),
    }
}

fn anchor_through(blocks: &[AppliedBlock], through: u64) -> Result<(u64, B256), FastDrainError> {
    let mut anchor = None;
    for block in blocks.iter().filter(|block| block.log_id.index <= through) {
        let attributes: ZonePayloadAttributes =
            bincode::deserialize(&block.input.l1_inputs).map_err(storage)?;
        match attributes.tempo_import {
            TempoImport::Full(prepared) => {
                let imported = prepared.header.num_hash();
                anchor = Some((imported.number, imported.hash));
            }
            TempoImport::CheckpointOnly(headers) => {
                if let Some(header) = headers.last() {
                    let imported = header.num_hash();
                    anchor = Some((imported.number, imported.hash));
                }
            }
            TempoImport::SameAnchor(opening) => {
                let expected = (opening.tempo_block_number, opening.tempo_block_hash);
                if anchor != Some(expected) {
                    return Err(FastDrainError::IncompleteCommittedHistory);
                }
            }
        }
    }
    anchor.ok_or(FastDrainError::IncompleteCommittedHistory)
}

fn assert_l1_import_entry(image: &ExactStateImage, id: LogId<u64>) -> Result<(), FastDrainError> {
    let applied = image
        .blocks
        .iter()
        .find(|block| block.log_id == id)
        .ok_or(FastDrainError::InvalidCommittedPoint)?;
    let attributes: ZonePayloadAttributes =
        bincode::deserialize(&applied.input.l1_inputs).map_err(storage)?;
    if !matches!(attributes.tempo_import, TempoImport::Full(_)) {
        return Err(FastDrainError::WrongBarrier);
    }
    Ok(())
}

fn assert_point(image: &ExactStateImage, point: CommittedDrainPoint) -> Result<(), FastDrainError> {
    let id = image
        .blocks
        .iter()
        .find(|block| {
            block.log_id.index == point.log_index && block.log_id.leader_id.term == point.log_term
        })
        .map(|block| block.log_id)
        .ok_or(FastDrainError::InvalidCommittedPoint)?;
    (ProductionPoint::from_image(image, id)? == point)
        .then_some(())
        .ok_or(FastDrainError::InvalidCommittedPoint)
}

struct ProductionPoint;
impl ProductionPoint {
    fn from_image(
        image: &ExactStateImage,
        id: LogId<u64>,
    ) -> Result<CommittedDrainPoint, FastDrainError> {
        let block = image
            .blocks
            .iter()
            .find(|block| block.log_id == id)
            .ok_or(FastDrainError::InvalidCommittedPoint)?;
        let (number, hash) = anchor_through(&image.blocks, id.index)?;
        Ok(CommittedDrainPoint {
            log_term: id.leader_id.term,
            log_index: id.index,
            block_height: block.output.block_height,
            block_hash: block.output.block_hash,
            state_root: block.output.state_root,
            imported_anchor_number: number,
            imported_anchor_hash: hash,
        })
    }
}

fn prefix_from(
    image: &ExactStateImage,
    resources: &CheckpointResourceImage,
) -> Result<FinalAcceptedPrefix, FastDrainError> {
    let point = ProductionPoint::from_image(image, image.last_applied)?;
    if resources.at != image.last_applied
        || (
            resources.imported_anchor_number,
            resources.imported_anchor_hash,
        ) != (point.imported_anchor_number, point.imported_anchor_hash)
    {
        return Err(FastDrainError::IncompleteCommittedHistory);
    }
    Ok(FinalAcceptedPrefix {
        zone_height: point.block_height,
        block_hash: point.block_hash,
        state_root: point.state_root,
        withdrawal_batch_index: resources.withdrawal_batch_index,
        imported_anchor_number: point.imported_anchor_number,
        imported_anchor_hash: point.imported_anchor_hash,
    })
}

fn drain_entries(
    resources: &CheckpointResourceImage,
) -> Result<Vec<(Address, B256, B256)>, FastDrainError> {
    let drain: DrainCheckpointResources =
        bincode::deserialize(&resources.drain_barriers).map_err(storage)?;
    drain
        .peers
        .into_iter()
        .map(|(portal, barrier, resolution)| {
            let barrier = BarrierInventory::decode_durable(&barrier)?;
            let resolution = BarrierResolutionInventory::decode_durable(&resolution)?;
            resolution.verify(&barrier)?;
            Ok((
                portal,
                barrier.statement.registry_digest(barrier.l1_chain_id),
                resolution.resolution.registry_digest(
                    barrier.l1_chain_id,
                    barrier.statement.destination_portal,
                    barrier.statement.destination_epoch,
                    barrier.statement.source_portal,
                ),
            ))
        })
        .collect()
}

fn state_error(error: impl ToString) -> FastDrainError {
    FastDrainError::Storage(error.to_string())
}
fn storage(error: impl ToString) -> FastDrainError {
    FastDrainError::Storage(error.to_string())
}
