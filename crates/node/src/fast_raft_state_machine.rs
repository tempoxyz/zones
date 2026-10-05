//! Persistent OpenRaft state machine and snapshot implementation.
//!
//! Applied entries are atomically replaced and fsynced before OpenRaft observes completion. The
//! same versioned, checksummed image is used for snapshot transfer, and installation delegates to
//! the node executor to replace/reconcile canonical execution state before publication.

#![allow(clippy::result_large_err)] // OpenRaft's required StorageError is intentionally rich.

use std::{
    fs::{self, File, OpenOptions},
    future::Future,
    io::{self, Cursor, Write},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
};

use alloy_consensus::{Transaction as _, transaction::TxHashRef as _};
use alloy_eips::eip2718::Decodable2718 as _;
use alloy_primitives::{B256, keccak256};
use openraft::{
    BasicNode, Entry, EntryPayload, ErrorSubject, ErrorVerb, LogId, RaftSnapshotBuilder, Snapshot,
    SnapshotMeta, StorageError, StoredMembership, storage::RaftStateMachine,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::fast_quorum::{CommittedBlock, FastRaftConfig, ReplicatedBlockInput};
use zone_primitives::fast_transfer::{CertificateBody, OutcomeCertificate, TransferIntent};

const STATE_FILE: &str = "raft-state-machine.bin";
const STATE_TEMP: &str = "raft-state-machine.tmp";
const IMAGE_MAGIC: &[u8; 8] = b"ZFSM0001";
const IMAGE_VERSION: u32 = 2;
const HEADER_BYTES: usize = 8 + 4 + 8 + 32;
const MAX_IMAGE_BYTES: usize = 1024 * 1024 * 1024;

/// A committed application record retained for deterministic snapshot recovery.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AppliedBlock {
    pub log_id: LogId<u64>,
    pub input: ReplicatedBlockInput,
    pub output: CommittedBlock,
}

/// Append-only certified execution evidence retained in the same fsynced image as the applied
/// Raft prefix. Raw protocol encodings are used deliberately: decoding and semantic validation is
/// performed by the drain projection, while the state machine enforces exact committed
/// coordinates and immutable byte identity.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CertifiedExecutionRecord {
    pub transfer_id: B256,
    pub log_id: LogId<u64>,
    pub block_height: u64,
    pub block_hash: B256,
    pub state_root: B256,
    pub transaction_hash: B256,
    pub canonical_intent: Vec<u8>,
    pub canonical_certificate: Vec<u8>,
    pub native_calldata: Vec<u8>,
    pub canonical_receipt: Vec<u8>,
    pub replay_witness: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(u8)]
pub enum CommittedProtocolKind {
    NoNewLocks,
    ImportedBarrier,
    InstalledCheckpoint,
    ObservedLocalClosure,
}

/// Canonical protocol-native transaction result retained at its committed Raft coordinate.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommittedProtocolRecord {
    pub log_id: LogId<u64>,
    pub block_height: u64,
    pub block_hash: B256,
    pub state_root: B256,
    pub transaction_hash: B256,
    pub kind: CommittedProtocolKind,
    pub canonical_payload: Vec<u8>,
    pub native_calldata: Vec<u8>,
    pub canonical_receipt: Vec<u8>,
}

impl CommittedProtocolRecord {
    fn immutable_identity(&self) -> (LogId<u64>, CommittedProtocolKind, B256) {
        (self.log_id, self.kind, keccak256(&self.canonical_payload))
    }

    fn sort_key(&self) -> (u64, u64, u8, B256) {
        (
            self.log_id.index,
            self.log_id.leader_id.term,
            self.kind as u8,
            keccak256(&self.canonical_payload),
        )
    }

    fn validate_against(&self, blocks: &[AppliedBlock]) -> io::Result<()> {
        let Some(applied) = blocks.iter().find(|block| block.log_id == self.log_id) else {
            return Err(invalid_data(
                "protocol record is outside the applied prefix",
            ));
        };
        if self.log_id.index == 0
            || self.block_height != applied.output.block_height
            || self.block_hash != applied.output.block_hash
            || self.state_root != applied.output.state_root
            || self.transaction_hash.is_zero()
            || self.canonical_payload.is_empty()
            || self.native_calldata.is_empty()
            || self.canonical_receipt.is_empty()
        {
            return Err(invalid_data(
                "protocol record does not match its applied block",
            ));
        }
        let transaction_matches = applied.input.transactions.iter().any(|encoded| {
            let mut bytes = encoded.as_ref();
            tempo_primitives::TempoTxEnvelope::decode_2718(&mut bytes)
                .ok()
                .filter(|_| bytes.is_empty())
                .is_some_and(|transaction| {
                    *transaction.tx_hash() == self.transaction_hash
                        && transaction.input().as_ref() == self.native_calldata
                })
        });
        if !transaction_matches {
            return Err(invalid_data(
                "protocol record transaction/calldata is absent from its applied block",
            ));
        }
        Ok(())
    }
}

impl CertifiedExecutionRecord {
    fn validate_against(&self, blocks: &[AppliedBlock]) -> io::Result<()> {
        if self.transfer_id.is_zero()
            || self.log_id.index == 0
            || self.block_hash.is_zero()
            || self.state_root.is_zero()
            || self.transaction_hash.is_zero()
            || self.canonical_intent.is_empty()
            || self.canonical_certificate.is_empty()
            || self.native_calldata.is_empty()
            || self.canonical_receipt.is_empty()
            || self.replay_witness.is_empty()
        {
            return Err(invalid_data(
                "incomplete certified committed execution record",
            ));
        }
        let Some(applied) = blocks.iter().find(|block| block.log_id == self.log_id) else {
            return Err(invalid_data(
                "certified execution record is outside the applied prefix",
            ));
        };
        if applied.output.block_height != self.block_height
            || applied.output.block_hash != self.block_hash
            || applied.output.state_root != self.state_root
            || applied.input.replay_witness.as_ref() != self.replay_witness
        {
            return Err(invalid_data(
                "certified execution record does not match its applied block",
            ));
        }
        let transaction_matches = applied.input.transactions.iter().any(|encoded| {
            let mut bytes = encoded.as_ref();
            tempo_primitives::TempoTxEnvelope::decode_2718(&mut bytes)
                .ok()
                .filter(|_| bytes.is_empty())
                .is_some_and(|transaction| {
                    *transaction.tx_hash() == self.transaction_hash
                        && transaction.input().as_ref() == self.native_calldata
                })
        });
        if !transaction_matches {
            return Err(invalid_data(
                "certified execution transaction/calldata is absent from its applied block",
            ));
        }
        Ok(())
    }
}

/// Exact checksummed state-machine image returned to checkpoint construction. This is the same
/// byte sequence OpenRaft snapshots and installs; it is not a summary or a freshly serialized
/// projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactStateImage {
    pub bytes: Vec<u8>,
    pub checksum: B256,
    pub last_applied: LogId<u64>,
    pub membership: StoredMembership<u64, BasicNode>,
    pub blocks: Vec<AppliedBlock>,
    pub certified_history: Vec<CertifiedExecutionRecord>,
    pub checkpoint_resources: Option<CheckpointResourceImage>,
    pub protocol_records: Vec<CommittedProtocolRecord>,
    pub installed_checkpoint_hash: Option<B256>,
}

/// Strictly decode the exact OpenRaft snapshot bytes without starting an executor. This is used by
/// a disjoint next-roster staging process to validate and fsync the accepted old prefix before it
/// is allowed to acknowledge the handoff.
pub fn inspect_exact_state_image(bytes: &[u8]) -> io::Result<ExactStateImage> {
    let state = decode_image(bytes)?;
    validate(&state)?;
    let last_applied = state
        .last_applied
        .ok_or_else(|| invalid_data("checkpoint snapshot has no applied prefix"))?;
    if state.blocks.is_empty() || state.membership.membership().voter_ids().count() != 3 {
        return Err(invalid_data(
            "checkpoint snapshot lacks blocks or exact three-member membership",
        ));
    }
    let canonical = encode_image(&state)?;
    if canonical != bytes {
        return Err(invalid_data("checkpoint snapshot is not canonical"));
    }
    Ok(ExactStateImage {
        checksum: keccak256(bytes),
        bytes: bytes.to_vec(),
        last_applied,
        membership: state.membership,
        blocks: state.blocks,
        certified_history: state.certified_history,
        checkpoint_resources: state.checkpoint_resources,
        protocol_records: state.protocol_records,
        installed_checkpoint_hash: state.installed_checkpoint_hash,
    })
}

/// Exact non-consensus resources required to resume the committed head.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CheckpointResourceImage {
    pub at: LogId<u64>,
    pub imported_anchor_number: u64,
    pub imported_anchor_hash: B256,
    pub withdrawal_batch_index: u64,
    pub local_closure_hash: B256,
    pub final_settlement_hash: B256,
    pub service_protocol_journal: Vec<u8>,
    pub drain_barriers: Vec<u8>,
    pub canonical_batch_boundary: Vec<u8>,
    pub replenishment_inventory: Vec<u8>,
}

impl CheckpointResourceImage {
    fn validate_against(&self, blocks: &[AppliedBlock]) -> io::Result<()> {
        let Some(head) = blocks.last() else {
            return Err(invalid_data(
                "checkpoint resources require a committed head",
            ));
        };
        if self.at != head.log_id
            || self.imported_anchor_hash.is_zero()
            || self.local_closure_hash.is_zero()
            || self.service_protocol_journal.is_empty()
            || self.drain_barriers.is_empty()
            || self.canonical_batch_boundary.is_empty()
            || self.replenishment_inventory.is_empty()
        {
            return Err(invalid_data(
                "checkpoint resources are incomplete or not at the committed head",
            ));
        }
        Ok(())
    }
}

/// Transfer-level data reconstructed from canonical committed execution for authenticated RPC.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedTransferRecord {
    pub intent: TransferIntent,
    pub body: CertificateBody,
    /// Present only after the two signatures over `body` have been durably assembled.
    pub certificate: Option<OutcomeCertificate>,
}

/// Canonical committed head exposed without consulting speculative OpenRaft metrics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedBlockRef {
    pub log_id: LogId<u64>,
    pub block: CommittedBlock,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
struct StateImage {
    last_applied: Option<LogId<u64>>,
    membership: StoredMembership<u64, BasicNode>,
    blocks: Vec<AppliedBlock>,
    certified_history: Vec<CertifiedExecutionRecord>,
    checkpoint_resources: Option<CheckpointResourceImage>,
    protocol_records: Vec<CommittedProtocolRecord>,
    installed_checkpoint_hash: Option<B256>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
struct LegacyStateImageV1 {
    last_applied: Option<LogId<u64>>,
    membership: StoredMembership<u64, BasicNode>,
    blocks: Vec<AppliedBlock>,
}

/// Node execution boundary required by the durable Raft state machine.
///
/// Both operations must be idempotent by `(term, index)`. `apply_committed` must durably retain
/// complete replay data and canonical execution before returning. `restore_committed` must replace
/// or reconcile local canonical state to exactly the supplied committed prefix.
pub trait DurableStateMachineExecution: Send + Sync + 'static {
    type Error: std::error::Error + Send + Sync + 'static;

    fn apply_committed<'a>(
        &'a self,
        log_id: LogId<u64>,
        input: &'a ReplicatedBlockInput,
    ) -> Pin<Box<dyn Future<Output = Result<CommittedBlock, Self::Error>> + Send + 'a>>;

    fn restore_committed<'a>(
        &'a self,
        blocks: &'a [AppliedBlock],
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>>;

    /// Read an executor-owned record. The state-machine handle independently fences it against
    /// the fsynced applied prefix before returning it to RPC.
    fn committed_transfer(
        &self,
        transfer_id: alloy_primitives::B256,
    ) -> Result<Option<CommittedTransferRecord>, Self::Error>;
}

/// Cloneable, read-only handle for RPC and receipt publication.
pub struct CommittedStateHandle<E> {
    directory: PathBuf,
    state: Arc<Mutex<StateImage>>,
    executor: Arc<E>,
}

impl<E> Clone for CommittedStateHandle<E> {
    fn clone(&self) -> Self {
        Self {
            directory: self.directory.clone(),
            state: self.state.clone(),
            executor: self.executor.clone(),
        }
    }
}

impl<E: DurableStateMachineExecution> CommittedStateHandle<E> {
    /// Return the exact versioned/checksummed image used by OpenRaft snapshot transfer together
    /// with its decoded fields. Empty images are rejected: roster replacement may never bootstrap
    /// from an empty committed chain.
    pub fn exact_state_image(&self) -> Result<ExactStateImage, CommittedReadError<E::Error>> {
        let state = self
            .state
            .lock()
            .map_err(|_| CommittedReadError::Poisoned)?
            .clone();
        validate(&state).map_err(CommittedReadError::InvalidImage)?;
        let last_applied = state
            .last_applied
            .ok_or(CommittedReadError::EmptyCommittedPrefix)?;
        if state.blocks.is_empty() || state.membership.membership().voter_ids().count() != 3 {
            return Err(CommittedReadError::EmptyCommittedPrefix);
        }
        let bytes = encode_image(&state).map_err(CommittedReadError::InvalidImage)?;
        Ok(ExactStateImage {
            checksum: keccak256(&bytes),
            bytes,
            last_applied,
            membership: state.membership,
            blocks: state.blocks,
            certified_history: state.certified_history,
            checkpoint_resources: state.checkpoint_resources,
            protocol_records: state.protocol_records,
            installed_checkpoint_hash: state.installed_checkpoint_hash,
        })
    }

    /// Append immutable certified history only after its exact term/index/block commitment is in
    /// the fsynced applied image. The update is itself fsynced before returning. Runtime
    /// certification must call this before publishing the certificate.
    pub fn persist_certified_execution(
        &self,
        record: CertifiedExecutionRecord,
    ) -> Result<bool, CommittedReadError<E::Error>> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| CommittedReadError::Poisoned)?;
        record
            .validate_against(&guard.blocks)
            .map_err(CommittedReadError::InvalidImage)?;
        let identity = (record.log_id, record.transfer_id, record.transaction_hash);
        if let Some(existing) = guard
            .certified_history
            .iter()
            .find(|entry| (entry.log_id, entry.transfer_id, entry.transaction_hash) == identity)
        {
            return if existing == &record {
                Ok(false)
            } else {
                Err(CommittedReadError::ConflictingHistory)
            };
        }
        let mut next = guard.clone();
        next.certified_history.push(record);
        next.certified_history.sort_by_key(|entry| {
            (
                entry.log_id.index,
                entry.log_id.leader_id.term,
                entry.transfer_id,
                entry.transaction_hash,
            )
        });
        validate(&next).map_err(CommittedReadError::InvalidImage)?;
        persist(&self.directory, &next).map_err(CommittedReadError::InvalidImage)?;
        *guard = next;
        Ok(true)
    }

    pub fn certified_history(
        &self,
    ) -> Result<Vec<CertifiedExecutionRecord>, CommittedReadError<E::Error>> {
        self.state
            .lock()
            .map(|state| state.certified_history.clone())
            .map_err(|_| CommittedReadError::Poisoned)
    }

    /// Fsync a complete service/protocol resource image at the current committed head.
    pub fn persist_checkpoint_resources(
        &self,
        resources: CheckpointResourceImage,
    ) -> Result<bool, CommittedReadError<E::Error>> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| CommittedReadError::Poisoned)?;
        resources
            .validate_against(&guard.blocks)
            .map_err(CommittedReadError::InvalidImage)?;
        if let Some(existing) = &guard.checkpoint_resources {
            if existing == &resources {
                return Ok(false);
            }
            if existing.at.index >= resources.at.index {
                return Err(CommittedReadError::ConflictingResources);
            }
        }
        let mut next = guard.clone();
        next.checkpoint_resources = Some(resources);
        validate(&next).map_err(CommittedReadError::InvalidImage)?;
        persist(&self.directory, &next).map_err(CommittedReadError::InvalidImage)?;
        *guard = next;
        Ok(true)
    }

    /// Seal a previously-published resource image with the finalized on-chain settlement hash.
    /// This is the only permitted same-coordinate mutation and is one-way from zero to exact.
    pub fn finalize_checkpoint_resources(
        &self,
        settlement_hash: B256,
    ) -> Result<bool, CommittedReadError<E::Error>> {
        if settlement_hash.is_zero() {
            return Err(CommittedReadError::ConflictingResources);
        }
        let mut guard = self
            .state
            .lock()
            .map_err(|_| CommittedReadError::Poisoned)?;
        let resources = guard
            .checkpoint_resources
            .as_ref()
            .ok_or(CommittedReadError::ConflictingResources)?;
        if resources.final_settlement_hash == settlement_hash {
            return Ok(false);
        }
        if !resources.final_settlement_hash.is_zero() {
            return Err(CommittedReadError::ConflictingResources);
        }
        let mut next = guard.clone();
        next.checkpoint_resources
            .as_mut()
            .expect("checked above")
            .final_settlement_hash = settlement_hash;
        validate(&next).map_err(CommittedReadError::InvalidImage)?;
        persist(&self.directory, &next).map_err(CommittedReadError::InvalidImage)?;
        *guard = next;
        Ok(true)
    }

    /// Fsync one immutable protocol-native transaction record after same-Raft commitment.
    pub fn persist_protocol_record(
        &self,
        record: CommittedProtocolRecord,
    ) -> Result<bool, CommittedReadError<E::Error>> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| CommittedReadError::Poisoned)?;
        record
            .validate_against(&guard.blocks)
            .map_err(CommittedReadError::InvalidImage)?;
        let identity = record.immutable_identity();
        if let Some(existing) = guard
            .protocol_records
            .iter()
            .find(|entry| entry.immutable_identity() == identity)
        {
            return if existing == &record {
                Ok(false)
            } else {
                Err(CommittedReadError::ConflictingProtocolRecord)
            };
        }
        let mut next = guard.clone();
        next.protocol_records.push(record);
        next.protocol_records
            .sort_by_key(CommittedProtocolRecord::sort_key);
        validate(&next).map_err(CommittedReadError::InvalidImage)?;
        persist(&self.directory, &next).map_err(CommittedReadError::InvalidImage)?;
        *guard = next;
        Ok(true)
    }

    pub fn protocol_records(
        &self,
    ) -> Result<Vec<CommittedProtocolRecord>, CommittedReadError<E::Error>> {
        self.state
            .lock()
            .map(|state| state.protocol_records.clone())
            .map_err(|_| CommittedReadError::Poisoned)
    }

    /// Fsync the checkpoint identity only after the caller has installed and compared the exact
    /// consensus image. A conflicting identity is never overwritten.
    pub fn persist_installed_checkpoint_hash(
        &self,
        hash: B256,
    ) -> Result<bool, CommittedReadError<E::Error>> {
        if hash.is_zero() {
            return Err(CommittedReadError::ConflictingInstalledCheckpoint);
        }
        let mut guard = self
            .state
            .lock()
            .map_err(|_| CommittedReadError::Poisoned)?;
        match guard.installed_checkpoint_hash {
            Some(existing) if existing == hash => return Ok(false),
            Some(_) => return Err(CommittedReadError::ConflictingInstalledCheckpoint),
            None => {}
        }
        let mut next = guard.clone();
        next.installed_checkpoint_hash = Some(hash);
        persist(&self.directory, &next).map_err(CommittedReadError::InvalidImage)?;
        *guard = next;
        Ok(true)
    }

    /// Clone the exact fsynced applied block prefix for canonical schedulers and checkpoint
    /// export. This never consults executor sidecars or speculative OpenRaft metrics.
    pub fn applied_blocks(&self) -> Result<Vec<AppliedBlock>, CommittedReadError<E::Error>> {
        self.state
            .lock()
            .map(|state| state.blocks.clone())
            .map_err(|_| CommittedReadError::Poisoned)
    }

    pub fn committed_head(
        &self,
    ) -> Result<Option<CommittedBlockRef>, CommittedReadError<E::Error>> {
        let state = self
            .state
            .lock()
            .map_err(|_| CommittedReadError::Poisoned)?;
        Ok(state.blocks.last().map(|applied| CommittedBlockRef {
            log_id: applied.log_id,
            block: applied.output.clone(),
        }))
    }

    pub fn committed_transfer(
        &self,
        transfer_id: alloy_primitives::B256,
    ) -> Result<Option<CommittedTransferRecord>, CommittedReadError<E::Error>> {
        let Some(record) = self
            .executor
            .committed_transfer(transfer_id)
            .map_err(CommittedReadError::Executor)?
        else {
            return Ok(None);
        };
        if record.body.transfer_id != transfer_id
            || record.body.intent_hash != record.intent.intent_hash()
            || record
                .certificate
                .as_ref()
                .is_some_and(|certificate| certificate.body != record.body)
        {
            return Err(CommittedReadError::InconsistentRecord);
        }
        let state = self
            .state
            .lock()
            .map_err(|_| CommittedReadError::Poisoned)?;
        let committed = state.blocks.iter().any(|applied| {
            applied.log_id.leader_id.term == record.body.log_term
                && applied.log_id.index == record.body.log_index
                && applied.output.block_height == record.body.block_height
                && applied.output.block_hash == record.body.block_hash
                && applied.output.state_root == record.body.state_root
        });
        if !committed {
            return Err(CommittedReadError::NotInCommittedPrefix);
        }
        Ok(Some(record))
    }
}

impl<P> CommittedStateHandle<crate::fast_execution::CanonicalFastExecution<P>>
where
    P: reth_storage_api::BlockNumReader
        + reth_storage_api::BlockReader<Block = tempo_primitives::Block>
        + reth_storage_api::HeaderProvider<Header = tempo_primitives::TempoHeader>
        + reth_storage_api::ReceiptProvider<Receipt = tempo_primitives::TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    /// Return only executor records whose exact term/index/block commitment is present in the
    /// fsynced applied state-machine image. Speculative sidecar records are not an authority.
    pub fn committed_transfers(
        &self,
    ) -> Result<
        Vec<CommittedTransferRecord>,
        CommittedReadError<crate::fast_execution::FastExecutionError>,
    > {
        let records = self
            .executor
            .committed_transfers_snapshot()
            .map_err(CommittedReadError::Executor)?;
        let state = self
            .state
            .lock()
            .map_err(|_| CommittedReadError::Poisoned)?;
        let mut committed = Vec::new();
        for record in records {
            let included = state.blocks.iter().any(|applied| {
                applied.log_id.leader_id.term == record.body.log_term
                    && applied.log_id.index == record.body.log_index
                    && applied.output.block_height == record.body.block_height
                    && applied.output.block_hash == record.body.block_hash
                    && applied.output.state_root == record.body.state_root
            });
            if !included {
                continue;
            }
            if record.body.transfer_id != record.intent.transfer_id()
                || record.body.intent_hash != record.intent.intent_hash()
                || record
                    .certificate
                    .as_ref()
                    .is_some_and(|certificate| certificate.body != record.body)
            {
                return Err(CommittedReadError::InconsistentRecord);
            }
            committed.push(record);
        }
        committed.sort_by_key(|record| {
            (
                record.body.log_term,
                record.body.log_index,
                record.body.transfer_id,
            )
        });
        Ok(committed)
    }

    /// Return the actual canonical receipt status only when its containing term/index is inside
    /// the fsynced applied prefix. An executor-side speculative result remains indistinguishable
    /// from an unknown transaction at this boundary.
    pub fn committed_transaction_result(
        &self,
        transaction_hash: alloy_primitives::B256,
    ) -> Result<Option<bool>, CommittedReadError<crate::fast_execution::FastExecutionError>> {
        let Some((term, index, succeeded)) = self
            .executor
            .committed_transaction_result(transaction_hash)
            .map_err(CommittedReadError::Executor)?
        else {
            return Ok(None);
        };
        let state = self
            .state
            .lock()
            .map_err(|_| CommittedReadError::Poisoned)?;
        Ok(state
            .blocks
            .iter()
            .any(|applied| applied.log_id.leader_id.term == term && applied.log_id.index == index)
            .then_some(succeeded))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CommittedReadError<E: std::error::Error + 'static> {
    #[error("committed state-machine lock poisoned")]
    Poisoned,
    #[error("canonical executor read failed: {0}")]
    Executor(E),
    #[error("executor returned an internally inconsistent transfer record")]
    InconsistentRecord,
    #[error("transfer record is not in the fsynced committed prefix")]
    NotInCommittedPrefix,
    #[error("committed state-machine image is empty or lacks the exact three-member roster")]
    EmptyCommittedPrefix,
    #[error("certified committed history conflicts with an existing immutable record")]
    ConflictingHistory,
    #[error("checkpoint resources conflict with an existing image at the same or newer prefix")]
    ConflictingResources,
    #[error("protocol-native record conflicts with an immutable committed record")]
    ConflictingProtocolRecord,
    #[error("installed checkpoint conflicts with the fsynced checkpoint identity")]
    ConflictingInstalledCheckpoint,
    #[error("invalid committed state-machine image: {0}")]
    InvalidImage(io::Error),
}

/// Fsync-backed state machine passed directly to `openraft::Raft::new`.
pub struct DurableRaftStateMachine<E> {
    directory: PathBuf,
    state: Arc<Mutex<StateImage>>,
    executor: Arc<E>,
}

impl<E> Clone for DurableRaftStateMachine<E> {
    fn clone(&self) -> Self {
        Self {
            directory: self.directory.clone(),
            state: self.state.clone(),
            executor: self.executor.clone(),
        }
    }
}

impl<E: DurableStateMachineExecution> DurableRaftStateMachine<E> {
    /// Open and fully restore the committed prefix before the Raft runtime starts serving.
    pub async fn open(directory: impl AsRef<Path>, executor: Arc<E>) -> io::Result<Self> {
        let directory = directory.as_ref().to_path_buf();
        let created = !directory.exists();
        fs::create_dir_all(&directory)?;
        if created {
            sync_directory(directory.parent().unwrap_or_else(|| Path::new(".")))?;
        }
        let path = directory.join(STATE_FILE);
        let state = if path.exists() {
            decode_image(&fs::read(path)?)?
        } else {
            let state = StateImage::default();
            persist(&directory, &state)?;
            state
        };
        validate(&state)?;
        executor
            .restore_committed(&state.blocks)
            .await
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(Self {
            directory,
            state: Arc::new(Mutex::new(state)),
            executor,
        })
    }

    pub fn committed_handle(&self) -> CommittedStateHandle<E> {
        CommittedStateHandle {
            directory: self.directory.clone(),
            state: self.state.clone(),
            executor: self.executor.clone(),
        }
    }

    fn snapshot(&self) -> Result<StateImage, StorageError<u64>> {
        self.state.lock().map(|state| state.clone()).map_err(|_| {
            storage_error(
                ErrorSubject::StateMachine,
                ErrorVerb::Read,
                io::Error::other("Raft state-machine lock poisoned"),
            )
        })
    }

    fn publish(
        &self,
        mut state: StateImage,
        preserve_concurrent_history: bool,
    ) -> Result<(), StorageError<u64>> {
        let mut guard = self.state.lock().map_err(|_| {
            storage_error(
                ErrorSubject::StateMachine,
                ErrorVerb::Write,
                io::Error::other("Raft state-machine lock poisoned"),
            )
        })?;
        if preserve_concurrent_history {
            for record in &guard.certified_history {
                if !state.certified_history.contains(record) {
                    state.certified_history.push(record.clone());
                }
            }
            for record in &guard.protocol_records {
                if !state.protocol_records.contains(record) {
                    state.protocol_records.push(record.clone());
                }
            }
            state
                .protocol_records
                .sort_by_key(CommittedProtocolRecord::sort_key);
            state.certified_history.sort_by_key(|entry| {
                (
                    entry.log_id.index,
                    entry.log_id.leader_id.term,
                    entry.transfer_id,
                    entry.transaction_hash,
                )
            });
            state.checkpoint_resources = guard
                .checkpoint_resources
                .as_ref()
                .filter(|resources| {
                    state
                        .blocks
                        .last()
                        .is_some_and(|head| resources.at == head.log_id)
                })
                .cloned();
            state.installed_checkpoint_hash = guard.installed_checkpoint_hash;
        }
        validate(&state)
            .map_err(|error| storage_error(ErrorSubject::StateMachine, ErrorVerb::Write, error))?;
        persist(&self.directory, &state)
            .map_err(|error| storage_error(ErrorSubject::StateMachine, ErrorVerb::Write, error))?;
        *guard = state;
        Ok(())
    }
}

impl<E: DurableStateMachineExecution> RaftStateMachine<FastRaftConfig>
    for DurableRaftStateMachine<E>
{
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogId<u64>>, StoredMembership<u64, BasicNode>), StorageError<u64>> {
        let state = self.snapshot()?;
        Ok((state.last_applied, state.membership))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<CommittedBlock>, StorageError<u64>>
    where
        I: IntoIterator<Item = Entry<FastRaftConfig>> + Send,
        I::IntoIter: Send,
    {
        let mut state = self.snapshot()?;
        let mut responses = Vec::new();
        for entry in entries {
            if state
                .last_applied
                .is_some_and(|applied| entry.log_id <= applied)
            {
                return Err(storage_error(
                    ErrorSubject::Log(entry.log_id),
                    ErrorVerb::Write,
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Raft entry applied out of order",
                    ),
                ));
            }
            let response = match entry.payload {
                EntryPayload::Blank => CommittedBlock::default(),
                EntryPayload::Membership(membership) => {
                    state.membership = StoredMembership::new(Some(entry.log_id), membership);
                    CommittedBlock::default()
                }
                EntryPayload::Normal(input) => {
                    let output = self
                        .executor
                        .apply_committed(entry.log_id, &input)
                        .await
                        .map_err(|error| {
                            storage_error(
                                ErrorSubject::Log(entry.log_id),
                                ErrorVerb::Write,
                                io::Error::other(error.to_string()),
                            )
                        })?;
                    if output.input_digest != input.digest() {
                        return Err(storage_error(
                            ErrorSubject::Log(entry.log_id),
                            ErrorVerb::Write,
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "executor returned a mismatched input digest",
                            ),
                        ));
                    }
                    state.blocks.push(AppliedBlock {
                        log_id: entry.log_id,
                        input,
                        output: output.clone(),
                    });
                    output
                }
            };
            state.last_applied = Some(entry.log_id);
            // Publish each entry separately: a later failure must not hide an earlier application
            // from OpenRaft recovery.
            self.publish(state.clone(), true)?;
            responses.push(response);
        }
        Ok(responses)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<Cursor<Vec<u8>>>, StorageError<u64>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<u64, BasicNode>,
        mut snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError<u64>> {
        snapshot.rewind().await.map_err(snapshot_write_error)?;
        let mut bytes = Vec::new();
        snapshot
            .read_to_end(&mut bytes)
            .await
            .map_err(snapshot_write_error)?;
        let state = decode_image(&bytes).map_err(snapshot_write_error)?;
        if state.last_applied != meta.last_log_id || state.membership != meta.last_membership {
            return Err(snapshot_write_error(io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot metadata does not match its state image",
            )));
        }
        validate(&state).map_err(snapshot_write_error)?;
        self.executor
            .restore_committed(&state.blocks)
            .await
            .map_err(|error| snapshot_write_error(io::Error::other(error.to_string())))?;
        self.publish(state, false)
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<FastRaftConfig>>, StorageError<u64>> {
        let state = self.snapshot()?;
        if state.last_applied.is_none() {
            return Ok(None);
        }
        Ok(Some(
            snapshot_from_state(&state).map_err(snapshot_read_error)?,
        ))
    }
}

impl<E: DurableStateMachineExecution> RaftSnapshotBuilder<FastRaftConfig>
    for DurableRaftStateMachine<E>
{
    async fn build_snapshot(&mut self) -> Result<Snapshot<FastRaftConfig>, StorageError<u64>> {
        snapshot_from_state(&self.snapshot()?).map_err(snapshot_read_error)
    }
}

fn snapshot_from_state(state: &StateImage) -> io::Result<Snapshot<FastRaftConfig>> {
    let bytes = encode_image(state)?;
    let checksum = keccak256(&bytes);
    let index = state.last_applied.map_or(0, |id| id.index);
    Ok(Snapshot {
        meta: SnapshotMeta {
            last_log_id: state.last_applied,
            last_membership: state.membership.clone(),
            snapshot_id: format!("{index}-{checksum}"),
        },
        snapshot: Box::new(Cursor::new(bytes)),
    })
}

fn validate(state: &StateImage) -> io::Result<()> {
    let mut previous = None;
    for block in &state.blocks {
        if previous.is_some_and(|index| block.log_id.index <= index)
            || block.output.input_digest != block.input.digest()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid committed block sequence in Raft state machine",
            ));
        }
        previous = Some(block.log_id.index);
    }
    if let (Some(block), Some(applied)) = (state.blocks.last(), state.last_applied)
        && block.log_id.index > applied.index
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "committed block follows last applied Raft entry",
        ));
    }
    let mut previous_history = None;
    for record in &state.certified_history {
        record.validate_against(&state.blocks)?;
        let identity = (
            record.log_id.index,
            record.log_id.leader_id.term,
            record.transfer_id,
            record.transaction_hash,
        );
        if previous_history.is_some_and(|previous| previous >= identity) {
            return Err(invalid_data(
                "certified committed history is duplicated or non-canonical",
            ));
        }
        previous_history = Some(identity);
    }
    if let Some(resources) = &state.checkpoint_resources {
        resources.validate_against(&state.blocks)?;
    }
    let mut previous_protocol = None;
    for record in &state.protocol_records {
        record.validate_against(&state.blocks)?;
        let identity = record.sort_key();
        if previous_protocol.is_some_and(|previous| previous >= identity) {
            return Err(invalid_data(
                "committed protocol records are duplicated or non-canonical",
            ));
        }
        previous_protocol = Some(identity);
    }
    Ok(())
}

fn persist(directory: &Path, state: &StateImage) -> io::Result<()> {
    let bytes = encode_image(state)?;
    let temporary = directory.join(STATE_TEMP);
    let published = directory.join(STATE_FILE);
    {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
    }
    fs::rename(temporary, published)?;
    sync_directory(directory)
}

fn encode_image(state: &StateImage) -> io::Result<Vec<u8>> {
    let payload = bincode::serialize(state).map_err(invalid_data)?;
    if payload.len() > MAX_IMAGE_BYTES {
        return Err(invalid_data("Raft state-machine image exceeds maximum"));
    }
    let mut bytes = Vec::with_capacity(HEADER_BYTES + payload.len());
    bytes.extend_from_slice(IMAGE_MAGIC);
    bytes.extend_from_slice(&IMAGE_VERSION.to_be_bytes());
    bytes.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    bytes.extend_from_slice(keccak256(&payload).as_slice());
    bytes.extend_from_slice(&payload);
    Ok(bytes)
}

fn decode_image(bytes: &[u8]) -> io::Result<StateImage> {
    if bytes.len() < HEADER_BYTES || &bytes[..8] != IMAGE_MAGIC {
        return Err(invalid_data("invalid Raft state-machine image magic"));
    }
    let version = u32::from_be_bytes(bytes[8..12].try_into().expect("fixed header"));
    if !matches!(version, 1 | IMAGE_VERSION) {
        return Err(invalid_data("unsupported Raft state-machine image version"));
    }
    let length = usize::try_from(u64::from_be_bytes(
        bytes[12..20].try_into().expect("fixed header"),
    ))
    .map_err(invalid_data)?;
    if length > MAX_IMAGE_BYTES || bytes.len() != HEADER_BYTES + length {
        return Err(invalid_data("invalid Raft state-machine image length"));
    }
    let payload = &bytes[HEADER_BYTES..];
    if keccak256(payload).as_slice() != &bytes[20..52] {
        return Err(invalid_data("Raft state-machine checksum mismatch"));
    }
    if version == 1 {
        let legacy: LegacyStateImageV1 = bincode::deserialize(payload).map_err(invalid_data)?;
        Ok(StateImage {
            last_applied: legacy.last_applied,
            membership: legacy.membership,
            blocks: legacy.blocks,
            certified_history: Vec::new(),
            checkpoint_resources: None,
            protocol_records: Vec::new(),
            installed_checkpoint_hash: None,
        })
    } else {
        bincode::deserialize(payload).map_err(invalid_data)
    }
}

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn storage_error(
    subject: ErrorSubject<u64>,
    verb: ErrorVerb,
    error: io::Error,
) -> StorageError<u64> {
    StorageError::from_io_error(subject, verb, error)
}

fn snapshot_write_error(error: io::Error) -> StorageError<u64> {
    storage_error(ErrorSubject::Snapshot(None), ErrorVerb::Write, error)
}

fn snapshot_read_error(error: io::Error) -> StorageError<u64> {
    storage_error(ErrorSubject::Snapshot(None), ErrorVerb::Read, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Signed, TxLegacy};
    use alloy_eips::eip2718::Encodable2718 as _;
    use alloy_primitives::{B256, Bytes, U256};
    use openraft::CommittedLeaderId;

    fn applied(index: u64, witness: u8) -> AppliedBlock {
        let transaction = tempo_primitives::TempoTxEnvelope::Legacy(Signed::new_unhashed(
            TxLegacy {
                chain_id: Some(1),
                nonce: index,
                gas_price: 0,
                gas_limit: 100_000,
                to: alloy_primitives::Address::repeat_byte(7).into(),
                value: U256::ZERO,
                input: Bytes::from(vec![3]),
            },
            tempo_primitives::transaction::envelope::TEMPO_SYSTEM_TX_SIGNATURE,
        ));
        let input = ReplicatedBlockInput {
            epoch: 7,
            parent_hash: B256::repeat_byte(1),
            block_input: Bytes::from(vec![2]),
            transactions: vec![transaction.encoded_2718().into()],
            l1_inputs: Bytes::from(vec![4]),
            replay_witness: Bytes::from(vec![witness]),
        };
        AppliedBlock {
            log_id: LogId::new(CommittedLeaderId::new(2, 1), index),
            output: CommittedBlock {
                input_digest: input.digest(),
                block_height: 100 + index,
                block_hash: B256::repeat_byte(index as u8),
                state_root: B256::repeat_byte(index as u8 + 20),
                receipts_root: B256::repeat_byte(index as u8 + 40),
            },
            input,
        }
    }

    fn history(block: &AppliedBlock) -> CertifiedExecutionRecord {
        let mut encoded = block.input.transactions[0].as_ref();
        let transaction = tempo_primitives::TempoTxEnvelope::decode_2718(&mut encoded).unwrap();
        CertifiedExecutionRecord {
            transfer_id: B256::repeat_byte(9),
            log_id: block.log_id,
            block_height: block.output.block_height,
            block_hash: block.output.block_hash,
            state_root: block.output.state_root,
            transaction_hash: *transaction.tx_hash(),
            canonical_intent: vec![1],
            canonical_certificate: vec![2],
            native_calldata: transaction.input().to_vec(),
            canonical_receipt: vec![4],
            replay_witness: block.input.replay_witness.to_vec(),
        }
    }

    fn protocol(block: &AppliedBlock, payload: u8) -> CommittedProtocolRecord {
        let mut encoded = block.input.transactions[0].as_ref();
        let transaction = tempo_primitives::TempoTxEnvelope::decode_2718(&mut encoded).unwrap();
        CommittedProtocolRecord {
            log_id: block.log_id,
            block_height: block.output.block_height,
            block_hash: block.output.block_hash,
            state_root: block.output.state_root,
            transaction_hash: *transaction.tx_hash(),
            kind: CommittedProtocolKind::NoNewLocks,
            canonical_payload: vec![payload],
            native_calldata: transaction.input().to_vec(),
            canonical_receipt: vec![4],
        }
    }

    #[test]
    fn certified_lock_history_survives_later_disposition_and_image_restart() {
        let first = applied(1, 31);
        let second = applied(2, 32);
        let state = StateImage {
            last_applied: Some(second.log_id),
            blocks: vec![first.clone(), second.clone()],
            certified_history: vec![history(&first), history(&second)],
            ..StateImage::default()
        };
        validate(&state).unwrap();
        let recovered = decode_image(&encode_image(&state).unwrap()).unwrap();
        assert_eq!(recovered.certified_history, state.certified_history);
        assert_eq!(recovered.certified_history[0].log_id, first.log_id);
        assert_eq!(recovered.certified_history[1].log_id, second.log_id);
    }

    #[test]
    fn state_image_rejects_checksum_gap_witness_and_root_mismatch() {
        let block = applied(3, 51);
        let valid = StateImage {
            last_applied: Some(block.log_id),
            blocks: vec![block.clone()],
            certified_history: vec![history(&block)],
            ..StateImage::default()
        };
        let mut corrupt = encode_image(&valid).unwrap();
        *corrupt.last_mut().unwrap() ^= 1;
        assert!(decode_image(&corrupt).is_err());

        let mut gap = valid.clone();
        gap.certified_history[0].log_id.index += 1;
        assert!(validate(&gap).is_err());

        let mut witness = valid.clone();
        witness.certified_history[0].replay_witness[0] ^= 1;
        assert!(validate(&witness).is_err());

        let mut root = valid;
        root.certified_history[0].state_root = B256::repeat_byte(99);
        assert!(validate(&root).is_err());
    }

    #[test]
    fn one_import_entry_retains_multiple_destination_closures() {
        let block = applied(4, 61);
        let mut records = vec![protocol(&block, 2), protocol(&block, 1)];
        records.sort_by_key(CommittedProtocolRecord::sort_key);
        let state = StateImage {
            last_applied: Some(block.log_id),
            blocks: vec![block],
            protocol_records: records,
            ..StateImage::default()
        };
        validate(&state).unwrap();
        let recovered = decode_image(&encode_image(&state).unwrap()).unwrap();
        assert_eq!(recovered.protocol_records, state.protocol_records);
    }
}
