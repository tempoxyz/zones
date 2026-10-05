//! Production C5 projection over the exact fsynced OpenRaft state image.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use alloy_consensus::BlockHeader as _;
use alloy_primitives::{Address, B256, U256, keccak256};
use openraft::LogId;
use reth_storage_api::{BlockNumReader, BlockReader, HeaderProvider, ReceiptProvider};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use zone_fast_transfer::{
    EpochRoster,
    drain::{
        BarrierInventory, BarrierResolutionInventory, CheckpointImage, CommittedDisposition,
        CommittedSourceLock, CommittedTerminal, DrainError, barriers_hash,
    },
};
use zone_payload::{TempoImport, ZonePayloadAttributes};
use zone_primitives::fast_transfer::{
    CanonicalEncode as _, FastCheckpointStatement, OutcomeCertificate, TransferIntent,
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
struct NoNewLocksPayload {
    epoch: u64,
    closure_hash: B256,
    imported_closures: Vec<(Address, u64, B256)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ImportedBarrierPayload {
    durable_inventory: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DrainCheckpointResources {
    /// Registry order, with canonical durable barrier/resolution encodings.
    peers: Vec<(Address, Vec<u8>, Vec<u8>)>,
}

/// Request consumed by the runtime's actual same-anchor producer. The consumer must enqueue the
/// protocol-native transaction through the ordinary fast transaction path and return only after
/// `CommittedStateHandle` observes the successful receipt.
pub enum FastDrainCommitRequest {
    NoNewLocks {
        canonical_payload: Vec<u8>,
        response: oneshot::Sender<Result<CommittedProtocolRecord, String>>,
    },
    ImportedBarrier {
        canonical_payload: Vec<u8>,
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
            CommittedProtocolKind::ImportedBarrier => FastDrainCommitRequest::ImportedBarrier {
                canonical_payload: payload.clone(),
                response,
            },
            CommittedProtocolKind::InstalledCheckpoint => {
                return Err(FastDrainError::Consensus(
                    "checkpoint installation uses the exact image request".to_owned(),
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
    peers: [DrainPeer; zone_fast_transfer::drain::DRAIN_PEER_COUNT],
}

impl<P> ProductionFastDrainState<P> {
    pub const fn new(
        committed: CommittedStateHandle<CanonicalFastExecution<P>>,
        producer: FastDrainCommitHandle,
        local_roster: EpochRoster,
        peers: [DrainPeer; zone_fast_transfer::drain::DRAIN_PEER_COUNT],
    ) -> Self {
        Self {
            committed,
            producer,
            local_roster,
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

    fn protocol_record(
        image: &ExactStateImage,
        kind: CommittedProtocolKind,
        through: u64,
    ) -> Result<&CommittedProtocolRecord, FastDrainError> {
        image
            .protocol_records
            .iter()
            .rev()
            .find(|record| record.kind == kind && record.log_id.index <= through)
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
        epoch: u64,
        closure_hash: B256,
    ) -> DrainFuture<'a, Result<CommittedDrainPoint, FastDrainError>> {
        Box::pin(async move {
            let payload = bincode::serialize(&NoNewLocksPayload {
                epoch,
                closure_hash,
                imported_closures: self
                    .peers
                    .iter()
                    .map(|peer| {
                        (
                            peer.roster.domain.portal,
                            peer.roster.domain.authority_epoch,
                            peer.closure_hash,
                        )
                    })
                    .collect(),
            })
            .map_err(storage)?;
            let record = self
                .producer
                .protocol(CommittedProtocolKind::NoNewLocks, payload)
                .await?;
            self.committed
                .persist_protocol_record(record.clone())
                .map_err(state_error)?;
            Self::point_at(&self.image()?, record.log_id)
        })
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
            Self::protocol_record(&image, CommittedProtocolKind::NoNewLocks, point.log_index)?;
        if record.log_id.index != point.log_index {
            return Err(FastDrainError::WrongBarrier);
        }
        let payload: NoNewLocksPayload =
            bincode::deserialize(&record.canonical_payload).map_err(storage)?;
        payload
            .imported_closures
            .iter()
            .any(|entry| {
                *entry
                    == (
                        destination.domain.portal,
                        destination.domain.authority_epoch,
                        closure_hash,
                    )
            })
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
    ) -> DrainFuture<'a, Result<CommittedDrainPoint, FastDrainError>> {
        Box::pin(async move {
            inventory.verify_complete()?;
            let payload = bincode::serialize(&ImportedBarrierPayload {
                durable_inventory: inventory.durable_bytes()?,
            })
            .map_err(storage)?;
            let record = self
                .producer
                .protocol(CommittedProtocolKind::ImportedBarrier, payload)
                .await?;
            self.committed
                .persist_protocol_record(record.clone())
                .map_err(state_error)?;
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
                let closure = Self::protocol_record(
                    &image,
                    CommittedProtocolKind::NoNewLocks,
                    point.log_index,
                )?;
                if closure.log_id.index != point.log_index {
                    return Err(FastDrainError::ConflictingCertificate);
                }
                let snapshot = self.snapshot_at(&image, point.log_index, &peer.roster)?;
                BarrierInventory::build(
                    self.local_roster.domain.l1_chain_id,
                    *destination,
                    peer.roster.domain.authority_epoch,
                    peer.closure_hash,
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
                let closure = Self::protocol_record(
                    &image,
                    CommittedProtocolKind::NoNewLocks,
                    point.log_index,
                )?;
                let closure: NoNewLocksPayload =
                    bincode::deserialize(&closure.canonical_payload).map_err(storage)?;
                final_settlement_digest(
                    self.local_roster.domain.l1_chain_id,
                    self.local_roster.domain.portal,
                    *epoch,
                    self.local_roster.domain.roster_hash,
                    closure.closure_hash,
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
                let next = EpochRoster {
                    domain: zone_primitives::fast_transfer::ZoneDomain {
                        authority_epoch: *next_epoch,
                        ..self.local_roster.domain
                    },
                    members: self.local_roster.members,
                };
                self.build_checkpoint(
                    &image,
                    Self::resources(&image)?.final_settlement_hash,
                    &next,
                )?
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

    fn checkpoint_image(
        &self,
        settlement_hash: B256,
        next: &EpochRoster,
    ) -> Result<CheckpointImage, FastDrainError> {
        self.build_checkpoint(&self.image()?, settlement_hash, next)
    }

    fn install_checkpoint<'a>(
        &'a self,
        image: &'a CheckpointImage,
    ) -> DrainFuture<'a, Result<CommittedDrainPoint, FastDrainError>> {
        Box::pin(async move {
            image.validate()?;
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
            let point = self.producer.install(image.clone()).await?;
            assert_point(&self.image()?, point)?;
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

fn anchor_through(blocks: &[AppliedBlock], through: u64) -> Result<(u64, B256), FastDrainError> {
    let mut anchor = None;
    for block in blocks.iter().filter(|block| block.log_id.index <= through) {
        let attributes: ZonePayloadAttributes =
            bincode::deserialize(&block.input.l1_inputs).map_err(storage)?;
        match attributes.tempo_import {
            TempoImport::Full(prepared) => {
                anchor = Some((prepared.header.number(), prepared.header.hash_slow()))
            }
            TempoImport::CheckpointOnly(headers) => {
                if let Some(header) = headers.last() {
                    anchor = Some((header.number(), header.hash_slow()));
                }
            }
            TempoImport::SameAnchor(opening) => {
                let expected = (opening.anchor_number, opening.anchor_hash);
                if anchor != Some(expected) {
                    return Err(FastDrainError::IncompleteCommittedHistory);
                }
            }
        }
    }
    anchor.ok_or(FastDrainError::IncompleteCommittedHistory)
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
