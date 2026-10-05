//! Concrete fsynced storage and signer adapters for the T14 distributed drain.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use alloy_consensus::{BlockHeader as _, Sealable as _, crypto::secp256k1::recover_signer};
use alloy_contract::CallBuilder;
use alloy_eips::BlockNumberOrTag;
use alloy_network::{ReceiptResponse as _, TransactionBuilder as _};
use alloy_primitives::{Address, B256, Bytes, Signature, U256, keccak256};
use alloy_provider::{DynProvider, Provider as _};
use alloy_rpc_types_eth::{BlockId, TransactionRequest};
use alloy_signer::SignerSync as _;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall as _;
use openraft::{BasicNode, ChangeMembers, LogId, Raft};
use serde::{Deserialize, Serialize};
use tempo_alloy::{TempoNetwork, rpc::TempoTransactionRequest};
use tokio::sync::{mpsc, oneshot};
use zone_fast_transfer::{
    AuthenticatedPeerSession, DurableJournal, EpochRoster, ReplenishmentJob, SigningRecord,
    drain::{
        BarrierInventory, BarrierResolutionInventory, CheckpointImage, DrainCertificate,
        MAX_DRAIN_OBJECT_BYTES,
    },
};
use zone_p2p::{
    InterZoneServiceRequest, MAX_INTER_ZONE_MESSAGE_SIZE, NEXT_ROSTER_CHECKPOINT_STREAM_PREFIX,
    P2pPeerId,
};
use zone_payload::{TempoImport, ZonePayloadAttributes};
use zone_primitives::fast_transfer::{
    CanonicalEncode as _, FastBarrierResolution, FastBarrierStatement, FastCheckpointStatement,
    OutcomeCertificate, SignatureBytes, TransferIntent,
};

use crate::{
    fast_drain::{
        CommittedDrainPoint, DrainClosureObservation, DrainFuture, DrainObjectKey, DrainPeer,
        DrainSigningPurpose, FastDrainCommittedState, FastDrainCommittee, FastDrainConfig,
        FastDrainError, FastDrainJournal, FastDrainNative, FastDrainRegistry, FastDrainService,
        FastDrainTransport, FinalAcceptedPrefix, PreparedRegistryAction,
    },
    fast_drain_state::DrainCheckpointResources,
    fast_quorum::{CanonicalFastSettlementBoundary, FastRaftConfig},
    fast_raft_state_machine::{
        CheckpointResourceImage, CommittedStateHandle, DurableStateMachineExecution,
        inspect_exact_state_image,
    },
    fast_raft_store::DurableRaftLogStore,
};

const JOURNAL_MAGIC: &[u8; 4] = b"FDJ1";
const JOURNAL_FILE: &str = "drain-journal.bin";
const SNAPSHOT_FILE: &str = "drain-snapshot.bin";
const SNAPSHOT_TEMP: &str = "drain-snapshot.tmp";
const MAX_SUBMISSION_HASHES: usize = 64;
const ACTION_DOMAIN: &[u8] = b"TEMPO_ZONE_FAST_DRAIN_ACTION_T14_V1";
const DRAIN_WIRE_MAGIC: &[u8; 8] = b"TZDRN14!";
const DRAIN_WIRE_VERSION: u8 = 1;
const DRAIN_BARRIER_CLASS: u8 = 1;
const DRAIN_RESOLUTION_CLASS: u8 = 2;
const DRAIN_CHUNK_BYTES: usize = 12 * 1024;
const DRAIN_STREAM_NAMESPACE: u64 = 0xd5a1_0000_0000_0000;
const HANDOFF_WIRE_MAGIC: &[u8; 8] = b"TZHND14!";
const HANDOFF_WIRE_VERSION: u8 = 1;
const HANDOFF_SIGNATURE_REQUEST: u8 = 1;
const HANDOFF_IMAGE_CHUNK: u8 = 2;
const HANDOFF_CHUNK_BYTES: usize = 12 * 1024;
const MAX_INBOUND_DRAIN_OBJECTS: usize = zone_fast_transfer::drain::DRAIN_PEER_COUNT * 2;

pub fn is_checkpoint_handoff_payload(payload: &[u8]) -> bool {
    payload.starts_with(HANDOFF_WIRE_MAGIC)
}

mod factory_abi {
    alloy_sol_types::sol! {
        interface IT14ZonePortalRead {
            function fastEpochConfig(uint64 epoch) external view;
            function fastPeerBarrier(uint64 epoch, address peer) external view;
        }

        struct FastBarrierStatement {
            address destinationPortal;
            uint64 destinationEpoch;
            bytes32 closureHash;
            address sourcePortal;
            uint64 sourceEpoch;
            uint64 importedAnchorNumber;
            bytes32 importedAnchorHash;
            uint64 logTerm;
            uint64 logIndex;
            uint256 blockHeight;
            bytes32 blockHash;
            bytes32 stateRoot;
            uint64 lockLogWatermark;
            bytes32 completeLockRoot;
            bytes32 unresolvedRoot;
            uint64 unresolvedCount;
        }

        struct FastBarrierResolution {
            bytes32 barrierHash;
            bytes32 terminalRoot;
            bytes32 dispositionRoot;
            uint64 resolvedCount;
            bytes32 remainingUnresolvedRoot;
            uint64 remainingUnresolvedCount;
        }

        struct FastCheckpointStatement {
            address portal;
            uint64 oldEpoch;
            uint64 nextEpoch;
            bytes32 nextRosterHash;
            uint256 finalZoneHeight;
            bytes32 finalBlockHash;
            uint64 finalWithdrawalBatchIndex;
            bytes32 finalSettlementHash;
            uint64 checkpointLogTerm;
            uint64 checkpointLogIndex;
            uint256 checkpointHeight;
            bytes32 checkpointBlockHash;
            bytes32 checkpointStateRoot;
        }

        interface IT14ZoneFactory {
            function recordFastPeerBarrier(
                address portal,
                FastBarrierStatement statement,
                bytes[] signatures
            ) external;
            function finalizeFastPeerBarrier(
                address portal,
                uint64 epoch,
                address peerPortal,
                FastBarrierResolution resolution,
                bytes[] signatures
            ) external;
            function recordFastFinalSettlement(
                address portal,
                uint64 epoch,
                uint256 zoneHeight,
                bytes32 blockHash,
                uint64 withdrawalBatchIndex,
                bytes[] signatures
            ) external;
            function installFastCheckpoint(
                address portal,
                FastCheckpointStatement statement,
                address[] nextMembers,
                bytes[] signatures
            ) external;
            function retireFastEpoch(address portal, uint64 epoch) external;
        }
    }
}

#[derive(Clone, Debug, Default)]
struct DrainJournalState {
    closure: Option<(u64, B256, CommittedDrainPoint)>,
    barriers: BTreeMap<DrainObjectKey, BarrierInventory>,
    resolutions: BTreeMap<DrainObjectKey, BarrierResolutionInventory>,
    certificates: BTreeMap<(u8, DrainObjectKey), DrainCertificate>,
    resolved: BTreeSet<(DrainObjectKey, B256)>,
    actions: BTreeMap<DrainObjectKey, RegistryActionState>,
    checkpoints: BTreeMap<DrainObjectKey, CheckpointImage>,
    outbound_chunks: BTreeMap<DrainChunkKey, DrainChunkRecord>,
    inbound_chunks: BTreeMap<DrainChunkKey, InboundDrainChunk>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct DrainChunkKey {
    peer: Address,
    stream_class: u8,
    object: B256,
    index: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DrainChunkRecord {
    payload_hash: B256,
    acknowledged: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct InboundDrainChunk {
    certificate: DrainCertificate,
    full_hash: B256,
    total_bytes: u32,
    chunk_count: u32,
    payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RegistryActionState {
    action: PreparedRegistryAction,
    completed: Option<B256>,
}

#[derive(Debug)]
struct DrainJournalInner {
    append: File,
    state: DrainJournalState,
    next_nonce: u64,
}

/// Append-only, checksummed and fsynced C5 journal with atomic snapshots.
pub struct FileFastDrainJournal {
    directory: PathBuf,
    epoch: u64,
    expected_signer: Address,
    inner: Mutex<DrainJournalInner>,
}

impl std::fmt::Debug for FileFastDrainJournal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileFastDrainJournal")
            .field("directory", &self.directory)
            .field("epoch", &self.epoch)
            .field("expected_signer", &self.expected_signer)
            .finish_non_exhaustive()
    }
}

impl FileFastDrainJournal {
    /// Open and recover a journal. `first_nonce` is the canonical pending provider nonce observed
    /// during startup; recovered prepared actions monotonically advance it and are never replaced.
    pub fn open(
        directory: impl AsRef<Path>,
        epoch: u64,
        expected_signer: Address,
        first_nonce: u64,
    ) -> Result<Self, FastDrainError> {
        if epoch == 0 || expected_signer.is_zero() {
            return Err(FastDrainError::InvalidConfiguration);
        }
        let directory = directory.as_ref().to_path_buf();
        let created = !directory.exists();
        fs::create_dir_all(&directory).map_err(storage)?;
        if created {
            sync_directory(directory.parent().unwrap_or_else(|| Path::new(".")))?;
        }
        let mut state = DrainJournalState::default();
        let snapshot_path = directory.join(SNAPSHOT_FILE);
        if snapshot_path.exists() {
            let bytes = fs::read(snapshot_path).map_err(storage)?;
            let parsed = replay_frames(&bytes, &mut state, false)?;
            if parsed != bytes.len() {
                return Err(storage("partial drain snapshot frame"));
            }
        }
        let path = directory.join(JOURNAL_FILE);
        let new_file = !path.exists();
        let append = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .map_err(storage)?;
        if new_file {
            append.sync_all().map_err(storage)?;
            sync_directory(&directory)?;
        }
        let mut bytes = Vec::new();
        File::open(&path)
            .and_then(|mut file| file.read_to_end(&mut bytes))
            .map_err(storage)?;
        let parsed = replay_frames(&bytes, &mut state, true)?;
        if parsed != bytes.len() {
            append.set_len(parsed as u64).map_err(storage)?;
            append.sync_all().map_err(storage)?;
        }
        if state
            .closure
            .is_some_and(|(stored_epoch, _, _)| stored_epoch != epoch)
        {
            return Err(storage("drain journal belongs to a different epoch"));
        }
        let next_nonce = state
            .actions
            .values()
            .map(|record| record.action.nonce.saturating_add(1))
            .fold(first_nonce, u64::max);
        Ok(Self {
            directory,
            epoch,
            expected_signer,
            inner: Mutex::new(DrainJournalInner {
                append,
                state,
                next_nonce,
            }),
        })
    }

    /// Publish an atomic complete snapshot, then truncate and fsync the append log.
    pub fn snapshot(&self) -> Result<(), FastDrainError> {
        let inner = self.inner.lock().map_err(|_| storage("lock poisoned"))?;
        let operations = snapshot_operations(&inner.state)?;
        let temporary = self.directory.join(SNAPSHOT_TEMP);
        {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&temporary)
                .map_err(storage)?;
            for operation in &operations {
                write_frame(&mut file, operation)?;
            }
            file.flush().map_err(storage)?;
            file.sync_all().map_err(storage)?;
        }
        fs::rename(temporary, self.directory.join(SNAPSHOT_FILE)).map_err(storage)?;
        sync_directory(&self.directory)?;
        inner.append.set_len(0).map_err(storage)?;
        inner.append.sync_all().map_err(storage)
    }

    fn persist_outbound_chunk(
        &self,
        key: DrainChunkKey,
        payload_hash: B256,
    ) -> Result<bool, FastDrainError> {
        let existing = {
            let inner = self.inner.lock().map_err(|_| storage("lock poisoned"))?;
            inner.state.outbound_chunks.get(&key).cloned()
        };
        if let Some(existing) = existing {
            if existing.payload_hash != payload_hash {
                return Err(FastDrainError::ConflictingJournalObject);
            }
            return Ok(existing.acknowledged);
        }
        self.mutate(
            JournalOperation::OutboundChunk(key.clone(), payload_hash),
            |state, _| {
                state.outbound_chunks.insert(
                    key,
                    DrainChunkRecord {
                        payload_hash,
                        acknowledged: false,
                    },
                );
                Ok(false)
            },
        )
    }

    fn acknowledge_outbound_chunk(&self, key: DrainChunkKey) -> Result<(), FastDrainError> {
        self.mutate(
            JournalOperation::OutboundChunkAck(key.clone()),
            |state, _| {
                let record = state
                    .outbound_chunks
                    .get_mut(&key)
                    .ok_or(FastDrainError::ConflictingJournalObject)?;
                record.acknowledged = true;
                Ok(())
            },
        )
    }

    /// Fsync one authenticated inbound chunk. Exact replays are idempotent; conflicting metadata
    /// or bytes for the same object position fail closed.
    fn persist_inbound_chunk(
        &self,
        key: DrainChunkKey,
        chunk: InboundDrainChunk,
    ) -> Result<(), FastDrainError> {
        if chunk.payload.is_empty()
            || chunk.payload.len() > DRAIN_CHUNK_BYTES
            || chunk.chunk_count == 0
            || key.index >= chunk.chunk_count
            || usize::try_from(chunk.total_bytes)
                .ok()
                .is_none_or(|length| length > MAX_DRAIN_OBJECT_BYTES)
        {
            return Err(FastDrainError::InvalidConfiguration);
        }
        let inner = self.inner.lock().map_err(|_| storage("lock poisoned"))?;
        if inner.state.inbound_chunks.keys().any(|existing| {
            existing.peer == key.peer
                && existing.stream_class == key.stream_class
                && existing.object != key.object
        }) {
            return Err(FastDrainError::ConflictingJournalObject);
        }
        let object_count = inner
            .state
            .inbound_chunks
            .keys()
            .map(|existing| (existing.peer, existing.stream_class, existing.object))
            .collect::<BTreeSet<_>>()
            .len();
        if object_count >= MAX_INBOUND_DRAIN_OBJECTS
            && !inner.state.inbound_chunks.keys().any(|existing| {
                existing.peer == key.peer
                    && existing.stream_class == key.stream_class
                    && existing.object == key.object
            })
        {
            return Err(FastDrainError::InvalidConfiguration);
        }
        if let Some(existing) = inner.state.inbound_chunks.get(&key).cloned() {
            return (existing == chunk)
                .then_some(())
                .ok_or(FastDrainError::ConflictingJournalObject);
        }
        drop(inner);
        self.mutate(
            JournalOperation::InboundChunk(key.clone(), chunk.clone()),
            |state, _| insert_inbound_chunk(&mut state.inbound_chunks, key, chunk),
        )
    }

    fn assembled_inbound(
        &self,
        peer: Address,
        stream_class: u8,
        object: B256,
    ) -> Result<Option<(Vec<u8>, DrainCertificate)>, FastDrainError> {
        let inner = self.inner.lock().map_err(|_| storage("lock poisoned"))?;
        let first_key = DrainChunkKey {
            peer,
            stream_class,
            object,
            index: 0,
        };
        let Some(first) = inner.state.inbound_chunks.get(&first_key) else {
            return Ok(None);
        };
        let mut encoded = Vec::with_capacity(first.total_bytes as usize);
        for index in 0..first.chunk_count {
            let key = DrainChunkKey {
                peer,
                stream_class,
                object,
                index,
            };
            let Some(chunk) = inner.state.inbound_chunks.get(&key) else {
                return Ok(None);
            };
            if chunk.certificate != first.certificate
                || chunk.full_hash != first.full_hash
                || chunk.total_bytes != first.total_bytes
                || chunk.chunk_count != first.chunk_count
            {
                return Err(FastDrainError::ConflictingJournalObject);
            }
            encoded.extend_from_slice(&chunk.payload);
        }
        if encoded.len() != first.total_bytes as usize || keccak256(&encoded) != first.full_hash {
            return Err(FastDrainError::ConflictingJournalObject);
        }
        Ok(Some((encoded, first.certificate)))
    }

    fn mutate<T>(
        &self,
        operation: JournalOperation,
        apply: impl FnOnce(&mut DrainJournalState, &mut u64) -> Result<T, FastDrainError>,
    ) -> Result<T, FastDrainError> {
        let mut inner = self.inner.lock().map_err(|_| storage("lock poisoned"))?;
        let mut next = inner.state.clone();
        let mut next_nonce = inner.next_nonce;
        let result = apply(&mut next, &mut next_nonce)?;
        write_frame(&mut inner.append, &operation)?;
        inner.append.flush().map_err(storage)?;
        inner.append.sync_all().map_err(storage)?;
        inner.state = next;
        inner.next_nonce = next_nonce;
        Ok(result)
    }
}

impl FastDrainJournal for FileFastDrainJournal {
    fn persist_closure(
        &self,
        closure_hash: B256,
        point: CommittedDrainPoint,
    ) -> Result<(), FastDrainError> {
        self.mutate(
            JournalOperation::Closure(self.epoch, closure_hash, point),
            |state, _| match state.closure {
                Some(existing) if existing == (self.epoch, closure_hash, point) => Ok(()),
                Some(_) => Err(FastDrainError::ConflictingClosure),
                None => {
                    state.closure = Some((self.epoch, closure_hash, point));
                    Ok(())
                }
            },
        )
    }

    fn closure(&self, epoch: u64) -> Result<Option<(B256, CommittedDrainPoint)>, FastDrainError> {
        let inner = self.inner.lock().map_err(|_| storage("lock poisoned"))?;
        Ok(inner
            .state
            .closure
            .filter(|(stored, _, _)| *stored == epoch)
            .map(|(_, hash, point)| (hash, point)))
    }

    fn persist_barrier(
        &self,
        key: &DrainObjectKey,
        inventory: &BarrierInventory,
    ) -> Result<(), FastDrainError> {
        inventory.verify_complete()?;
        self.mutate(
            JournalOperation::Barrier(key.clone(), inventory.clone()),
            |state, _| insert_exact(&mut state.barriers, key.clone(), inventory.clone()),
        )
    }

    fn barrier(&self, key: &DrainObjectKey) -> Result<Option<BarrierInventory>, FastDrainError> {
        let inner = self.inner.lock().map_err(|_| storage("lock poisoned"))?;
        Ok(inner.state.barriers.get(key).cloned())
    }

    fn persist_resolution(
        &self,
        key: &DrainObjectKey,
        resolution: &BarrierResolutionInventory,
    ) -> Result<(), FastDrainError> {
        self.mutate(
            JournalOperation::Resolution(key.clone(), resolution.clone()),
            |state, _| insert_exact(&mut state.resolutions, key.clone(), resolution.clone()),
        )
    }

    fn resolution(
        &self,
        key: &DrainObjectKey,
    ) -> Result<Option<BarrierResolutionInventory>, FastDrainError> {
        let inner = self.inner.lock().map_err(|_| storage("lock poisoned"))?;
        Ok(inner.state.resolutions.get(key).cloned())
    }

    fn persist_certificate(
        &self,
        purpose: DrainSigningPurpose,
        key: &DrainObjectKey,
        certificate: DrainCertificate,
    ) -> Result<(), FastDrainError> {
        self.mutate(
            JournalOperation::Certificate(purpose, key.clone(), certificate),
            |state, _| {
                insert_exact(
                    &mut state.certificates,
                    (purpose_tag(purpose), key.clone()),
                    certificate,
                )
            },
        )
    }

    fn certificate(
        &self,
        purpose: DrainSigningPurpose,
        key: &DrainObjectKey,
    ) -> Result<Option<DrainCertificate>, FastDrainError> {
        let inner = self.inner.lock().map_err(|_| storage("lock poisoned"))?;
        Ok(inner
            .state
            .certificates
            .get(&(purpose_tag(purpose), key.clone()))
            .copied())
    }

    fn mark_lock_resolved(
        &self,
        key: &DrainObjectKey,
        transfer_id: B256,
    ) -> Result<(), FastDrainError> {
        self.mutate(
            JournalOperation::LockResolved(key.clone(), transfer_id),
            |state, _| {
                state.resolved.insert((key.clone(), transfer_id));
                Ok(())
            },
        )
    }

    fn lock_resolved(
        &self,
        key: &DrainObjectKey,
        transfer_id: B256,
    ) -> Result<bool, FastDrainError> {
        let inner = self.inner.lock().map_err(|_| storage("lock poisoned"))?;
        Ok(inner.state.resolved.contains(&(key.clone(), transfer_id)))
    }

    fn prepare_registry_action(
        &self,
        key: &DrainObjectKey,
        signer: Address,
        target: Address,
        calldata: Vec<u8>,
    ) -> Result<PreparedRegistryAction, FastDrainError> {
        if signer != self.expected_signer
            || target.is_zero()
            || calldata.is_empty()
            || calldata.len() > MAX_DRAIN_OBJECT_BYTES
        {
            return Err(FastDrainError::InvalidConfiguration);
        }
        let existing = {
            let inner = self.inner.lock().map_err(|_| storage("lock poisoned"))?;
            inner.state.actions.get(key).cloned()
        };
        if let Some(existing) = existing {
            if existing.action.signer != signer
                || existing.action.target != target
                || existing.action.calldata != calldata
            {
                return Err(FastDrainError::ConflictingRegistryAction);
            }
            return Ok(existing.action);
        }
        let mut inner = self.inner.lock().map_err(|_| storage("lock poisoned"))?;
        let nonce = inner.next_nonce;
        let action = PreparedRegistryAction {
            action_id: action_id(key, signer, nonce, target, &calldata)?,
            signer,
            nonce,
            target,
            calldata,
            submission_hashes: Vec::new(),
        };
        let operation = JournalOperation::Action(key.clone(), action.clone());
        let mut next = inner.state.clone();
        next.actions.insert(
            key.clone(),
            RegistryActionState {
                action: action.clone(),
                completed: None,
            },
        );
        write_frame(&mut inner.append, &operation)?;
        inner.append.flush().map_err(storage)?;
        inner.append.sync_all().map_err(storage)?;
        inner.state = next;
        inner.next_nonce = nonce
            .checked_add(1)
            .ok_or_else(|| storage("nonce overflow"))?;
        Ok(action)
    }

    fn record_registry_submission(
        &self,
        action_id: B256,
        transaction_hash: B256,
    ) -> Result<(), FastDrainError> {
        self.mutate(
            JournalOperation::Submission(action_id, transaction_hash),
            |state, _| {
                let record = action_by_id_mut(state, action_id)?;
                if record.action.submission_hashes.contains(&transaction_hash) {
                    return Ok(());
                }
                if record.action.submission_hashes.len() >= MAX_SUBMISSION_HASHES
                    || record.completed.is_some()
                {
                    return Err(FastDrainError::ConflictingRegistryAction);
                }
                record.action.submission_hashes.push(transaction_hash);
                Ok(())
            },
        )
    }

    fn complete_registry_action(
        &self,
        action_id: B256,
        transaction_hash: B256,
    ) -> Result<(), FastDrainError> {
        self.mutate(
            JournalOperation::ActionCompleted(action_id, transaction_hash),
            |state, _| {
                let record = action_by_id_mut(state, action_id)?;
                if !record.action.submission_hashes.contains(&transaction_hash) {
                    return Err(FastDrainError::ConflictingRegistryAction);
                }
                match record.completed {
                    Some(existing) if existing != transaction_hash => {
                        Err(FastDrainError::ConflictingRegistryAction)
                    }
                    _ => {
                        record.completed = Some(transaction_hash);
                        Ok(())
                    }
                }
            },
        )
    }

    fn persist_checkpoint_image(
        &self,
        key: &DrainObjectKey,
        image: &CheckpointImage,
    ) -> Result<(), FastDrainError> {
        image.validate()?;
        self.mutate(
            JournalOperation::Checkpoint(key.clone(), image.clone()),
            |state, _| insert_exact(&mut state.checkpoints, key.clone(), image.clone()),
        )
    }

    fn checkpoint_image(
        &self,
        key: &DrainObjectKey,
    ) -> Result<Option<CheckpointImage>, FastDrainError> {
        let inner = self.inner.lock().map_err(|_| storage("lock poisoned"))?;
        Ok(inner.state.checkpoints.get(key).cloned())
    }
}

/// Actual local ECDSA signer. The signature record is fsynced in the shared protocol journal
/// before the signature is returned to the service or a requesting replica.
pub struct DurableLocalDrainCommittee {
    signer: PrivateKeySigner,
    signing_journal: Arc<DurableJournal>,
    requester: Arc<dyn DrainPeerSignatureRequester>,
}

impl DurableLocalDrainCommittee {
    pub fn new(
        signer: PrivateKeySigner,
        signing_journal: Arc<DurableJournal>,
        requester: Arc<dyn DrainPeerSignatureRequester>,
    ) -> Result<Self, FastDrainError> {
        if signer.address().is_zero() {
            return Err(FastDrainError::InvalidConfiguration);
        }
        Ok(Self {
            signer,
            signing_journal,
            requester,
        })
    }

    fn sign_and_persist(
        &self,
        digest: B256,
        point: CommittedDrainPoint,
    ) -> Result<SignatureBytes, FastDrainError> {
        if let Some(record) = self
            .signing_journal
            .signing_record(digest, self.signer.address())
            .map_err(storage)?
        {
            if record.log_term != point.log_term || record.log_index != point.log_index {
                return Err(FastDrainError::ConflictingCertificate);
            }
            return Ok(record.signature);
        }
        let signature = SignatureBytes(
            self.signer
                .sign_hash_sync(&digest)
                .map_err(|error| FastDrainError::Certificate(error.to_string()))?
                .as_bytes(),
        );
        self.signing_journal
            .persist_signing_record(SigningRecord {
                digest,
                signer: self.signer.address(),
                signature,
                log_term: point.log_term,
                log_index: point.log_index,
            })
            .map_err(storage)?;
        Ok(signature)
    }
}

pub trait DrainPeerSignatureRequester: Send + Sync + 'static {
    fn install_checkpoint<'a>(
        &'a self,
        member: Address,
        image: &'a CheckpointImage,
    ) -> DrainFuture<'a, Result<(), FastDrainError>>;
    fn request<'a>(
        &'a self,
        member: Address,
        purpose: DrainSigningPurpose,
        key: &'a DrainObjectKey,
        digest: B256,
        point: CommittedDrainPoint,
    ) -> DrainFuture<'a, Result<SignatureBytes, FastDrainError>>;
}

/// Exact command consumed by the authenticated intra-Zone fast-network runtime. The network must
/// route only to `target_member` in this finalized roster and return the remote handler's result;
/// callers never provide a precomputed signature body to the signer.
pub struct DrainSignatureNetworkRequest {
    pub target_member: Address,
    pub purpose: DrainSigningPurpose,
    pub key: DrainObjectKey,
    pub digest: B256,
    pub point: CommittedDrainPoint,
    pub response: oneshot::Sender<Result<SignatureBytes, String>>,
}

/// Production requester backed by the existing authenticated same-roster network command loop.
pub struct AuthenticatedDrainSignatureRequester {
    requests: mpsc::Sender<DrainSignatureNetworkRequest>,
    local_roster: EpochRoster,
    next_roster: Option<EpochRoster>,
    next_endpoints: Option<[DrainCommonwareEndpoint; 3]>,
    handoff_requests: mpsc::Sender<InterZoneServiceRequest>,
    response_timeout: Duration,
}

impl AuthenticatedDrainSignatureRequester {
    pub fn new(
        requests: mpsc::Sender<DrainSignatureNetworkRequest>,
        local_roster: &EpochRoster,
        next_roster: Option<&EpochRoster>,
        next_endpoints: Option<[DrainCommonwareEndpoint; 3]>,
        handoff_requests: mpsc::Sender<InterZoneServiceRequest>,
        response_timeout: Duration,
    ) -> Result<Self, FastDrainError> {
        if requests.max_capacity() == 0
            || handoff_requests.max_capacity() == 0
            || response_timeout.is_zero()
            || next_roster.is_some() != next_endpoints.is_some()
        {
            return Err(FastDrainError::InvalidConfiguration);
        }
        if let (Some(next), Some(endpoints)) = (next_roster, next_endpoints.as_ref()) {
            let members = endpoints
                .iter()
                .map(|endpoint| endpoint.member)
                .collect::<BTreeSet<_>>();
            let identities = endpoints
                .iter()
                .map(|endpoint| endpoint.identity.clone())
                .collect::<BTreeSet<_>>();
            if members != next.members.into_iter().collect() || identities.len() != 3 {
                return Err(FastDrainError::InvalidConfiguration);
            }
        }
        Ok(Self {
            requests,
            local_roster: local_roster.clone(),
            next_roster: next_roster.cloned(),
            next_endpoints,
            handoff_requests,
            response_timeout,
        })
    }
}

impl DrainPeerSignatureRequester for AuthenticatedDrainSignatureRequester {
    fn install_checkpoint<'a>(
        &'a self,
        member: Address,
        image: &'a CheckpointImage,
    ) -> DrainFuture<'a, Result<(), FastDrainError>> {
        Box::pin(async move {
            let next = self
                .next_roster
                .as_ref()
                .ok_or(FastDrainError::InvalidConfiguration)?;
            let endpoints = self
                .next_endpoints
                .as_ref()
                .ok_or(FastDrainError::InvalidConfiguration)?;
            let endpoint = endpoints
                .iter()
                .find(|endpoint| endpoint.member == member)
                .ok_or(FastDrainError::InvalidConfiguration)?;
            if !next.members.contains(&member)
                || image.statement.next_epoch != next.domain.authority_epoch
                || image.statement.next_roster_hash != next.domain.roster_hash
            {
                return Err(FastDrainError::InvalidConfiguration);
            }
            let encoded = image.durable_bytes()?;
            let image_hash = image.image_hash()?;
            let total_bytes =
                u32::try_from(encoded.len()).map_err(|_| FastDrainError::InvalidConfiguration)?;
            let count = u32::try_from(encoded.len().div_ceil(HANDOFF_CHUNK_BYTES))
                .map_err(|_| FastDrainError::InvalidConfiguration)?;
            for (index, chunk) in encoded.chunks(HANDOFF_CHUNK_BYTES).enumerate() {
                let index =
                    u32::try_from(index).map_err(|_| FastDrainError::InvalidConfiguration)?;
                let payload =
                    encode_checkpoint_image_chunk(image_hash, total_bytes, count, index, chunk)?;
                let (response, receive) = oneshot::channel();
                self.handoff_requests
                    .send(InterZoneServiceRequest {
                        target: endpoint.identity.clone(),
                        remote_zone_id: next.domain.zone_id,
                        stream: checkpoint_handoff_stream(image_hash),
                        sequence: u64::from(index),
                        payload,
                        response,
                    })
                    .await
                    .map_err(|_| {
                        FastDrainError::Transport(
                            "checkpoint image Commonware port stopped".to_owned(),
                        )
                    })?;
                let ack = tokio::time::timeout(self.response_timeout, receive)
                    .await
                    .map_err(|_| {
                        FastDrainError::Transport("checkpoint image chunk timed out".to_owned())
                    })?
                    .map_err(|_| {
                        FastDrainError::Transport(
                            "checkpoint image response channel dropped".to_owned(),
                        )
                    })?
                    .map_err(FastDrainError::Transport)?;
                if ack.authenticated_peer != endpoint.identity
                    || ack.remote_member != member
                    || ack.remote_domain != next.domain
                    || ack.stream != checkpoint_handoff_stream(image_hash)
                    || ack.sequence != u64::from(index)
                    || !ack.response_payload.is_empty()
                {
                    return Err(FastDrainError::Transport(
                        "checkpoint image acknowledgment mismatch".to_owned(),
                    ));
                }
            }
            Ok(())
        })
    }

    fn request<'a>(
        &'a self,
        member: Address,
        purpose: DrainSigningPurpose,
        key: &'a DrainObjectKey,
        digest: B256,
        point: CommittedDrainPoint,
    ) -> DrainFuture<'a, Result<SignatureBytes, FastDrainError>> {
        Box::pin(async move {
            if digest.is_zero() {
                return Err(FastDrainError::InvalidConfiguration);
            }
            if self.local_roster.members.contains(&member) {
                let (response, receive) = oneshot::channel();
                self.requests
                    .send(DrainSignatureNetworkRequest {
                        target_member: member,
                        purpose,
                        key: key.clone(),
                        digest,
                        point,
                        response,
                    })
                    .await
                    .map_err(|_| {
                        FastDrainError::Transport(
                            "authenticated drain signature network stopped".to_owned(),
                        )
                    })?;
                return receive
                    .await
                    .map_err(|_| {
                        FastDrainError::Transport(
                            "drain signature response channel dropped".to_owned(),
                        )
                    })?
                    .map_err(FastDrainError::Transport);
            }
            let next = self
                .next_roster
                .as_ref()
                .ok_or(FastDrainError::InvalidConfiguration)?;
            let endpoints = self
                .next_endpoints
                .as_ref()
                .ok_or(FastDrainError::InvalidConfiguration)?;
            if purpose != DrainSigningPurpose::Checkpoint
                || !next.members.contains(&member)
                || !matches!(key, DrainObjectKey::Checkpoint { old_epoch, next_epoch }
                    if *old_epoch == self.local_roster.domain.authority_epoch
                        && *next_epoch == next.domain.authority_epoch)
            {
                return Err(FastDrainError::InvalidConfiguration);
            }
            let payload = encode_checkpoint_handoff_request(key, digest, point)?;
            let stream = checkpoint_handoff_stream(digest);
            let endpoint = endpoints
                .iter()
                .find(|endpoint| endpoint.member == member)
                .ok_or(FastDrainError::InvalidConfiguration)?;
            let (response, receive) = oneshot::channel();
            self.handoff_requests
                .send(InterZoneServiceRequest {
                    target: endpoint.identity.clone(),
                    remote_zone_id: next.domain.zone_id,
                    stream,
                    sequence: point.log_index,
                    payload,
                    response,
                })
                .await
                .map_err(|_| {
                    FastDrainError::Transport(
                        "checkpoint handoff Commonware port stopped".to_owned(),
                    )
                })?;
            let ack = tokio::time::timeout(self.response_timeout, receive)
                .await
                .map_err(|_| {
                    FastDrainError::Transport("checkpoint handoff response timed out".to_owned())
                })?
                .map_err(|_| {
                    FastDrainError::Transport(
                        "checkpoint handoff response channel dropped".to_owned(),
                    )
                })?
                .map_err(FastDrainError::Transport)?;
            if ack.authenticated_peer != endpoint.identity
                || ack.remote_member != member
                || ack.remote_domain != next.domain
                || ack.stream != stream
                || ack.sequence != point.log_index
            {
                return Err(FastDrainError::Transport(
                    "checkpoint handoff acknowledgment binding mismatch".to_owned(),
                ));
            }
            decode_checkpoint_handoff_response(&ack.response_payload, digest, member)
        })
    }
}

/// Inbound callback for the authenticated same-roster network handler. The handler must pass the
/// ECDSA member bound to the authenticated Commonware peer. This function independently rebuilds
/// the phase body from the fsynced committed prefix before signing and persisting the record.
#[allow(clippy::too_many_arguments)]
pub fn sign_authenticated_drain_phase(
    authenticated_requester: Address,
    roster: &EpochRoster,
    local_signer: &PrivateKeySigner,
    signing_journal: &DurableJournal,
    committed: &dyn FastDrainCommittedState,
    purpose: DrainSigningPurpose,
    key: &DrainObjectKey,
    digest: B256,
    point: CommittedDrainPoint,
) -> Result<SignatureBytes, FastDrainError> {
    sign_authenticated_drain_phase_handoff(
        authenticated_requester,
        roster,
        roster,
        local_signer,
        signing_journal,
        committed,
        purpose,
        key,
        digest,
        point,
    )
}

/// Cross-roster form used only after exact checkpoint installation when the old authenticated
/// member requests an acknowledgment from a member of the finalized next roster.
#[allow(clippy::too_many_arguments)]
pub fn sign_authenticated_drain_phase_handoff(
    authenticated_requester: Address,
    requester_roster: &EpochRoster,
    signing_roster: &EpochRoster,
    local_signer: &PrivateKeySigner,
    signing_journal: &DurableJournal,
    committed: &dyn FastDrainCommittedState,
    purpose: DrainSigningPurpose,
    key: &DrainObjectKey,
    digest: B256,
    point: CommittedDrainPoint,
) -> Result<SignatureBytes, FastDrainError> {
    if authenticated_requester == local_signer.address()
        || !requester_roster.members.contains(&authenticated_requester)
        || !signing_roster.members.contains(&local_signer.address())
        || requester_roster.domain.portal != signing_roster.domain.portal
        || requester_roster.domain.l1_chain_id != signing_roster.domain.l1_chain_id
        || requester_roster.domain.zone_id != signing_roster.domain.zone_id
        || requester_roster.domain.chain_id != signing_roster.domain.chain_id
        || requester_roster.domain.protocol_version != signing_roster.domain.protocol_version
        || (requester_roster != signing_roster
            && signing_roster.domain.authority_epoch <= requester_roster.domain.authority_epoch)
        || digest.is_zero()
    {
        return Err(FastDrainError::InvalidConfiguration);
    }
    committed.assert_signing_body(purpose, key, digest, point)?;
    if let Some(record) = signing_journal
        .signing_record(digest, local_signer.address())
        .map_err(storage)?
    {
        if record.log_term != point.log_term || record.log_index != point.log_index {
            return Err(FastDrainError::ConflictingCertificate);
        }
        return Ok(record.signature);
    }
    let signature = SignatureBytes(
        local_signer
            .sign_hash_sync(&digest)
            .map_err(|error| FastDrainError::Certificate(error.to_string()))?
            .as_bytes(),
    );
    signing_journal
        .persist_signing_record(SigningRecord {
            digest,
            signer: local_signer.address(),
            signature,
            log_term: point.log_term,
            log_index: point.log_index,
        })
        .map_err(storage)?;
    Ok(signature)
}

fn checkpoint_handoff_stream(digest: B256) -> u64 {
    let mut suffix = [0u8; 8];
    suffix.copy_from_slice(&digest[..8]);
    NEXT_ROSTER_CHECKPOINT_STREAM_PREFIX | (u64::from_be_bytes(suffix) & 0x0000_ffff_ffff_ffff)
}

fn encode_checkpoint_handoff_request(
    key: &DrainObjectKey,
    digest: B256,
    point: CommittedDrainPoint,
) -> Result<Vec<u8>, FastDrainError> {
    let DrainObjectKey::Checkpoint {
        old_epoch,
        next_epoch,
    } = key
    else {
        return Err(FastDrainError::InvalidConfiguration);
    };
    if digest.is_zero()
        || point.log_index == 0
        || point.block_hash.is_zero()
        || point.state_root.is_zero()
    {
        return Err(FastDrainError::InvalidCommittedPoint);
    }
    let mut out = Vec::with_capacity(8 + 1 + 16 + 32 + 8 * 3 + 64);
    out.extend_from_slice(HANDOFF_WIRE_MAGIC);
    out.push(HANDOFF_WIRE_VERSION);
    out.push(HANDOFF_SIGNATURE_REQUEST);
    out.extend_from_slice(&old_epoch.to_be_bytes());
    out.extend_from_slice(&next_epoch.to_be_bytes());
    out.extend_from_slice(digest.as_slice());
    put_point(&mut out, point);
    Ok(out)
}

fn decode_checkpoint_handoff_request(
    payload: &[u8],
) -> Result<(DrainObjectKey, B256, CommittedDrainPoint), FastDrainError> {
    const LENGTH: usize = 8 + 1 + 1 + 8 + 8 + 32 + 8 + 8 + 8 + 32 + 32 + 8 + 32;
    if payload.len() != LENGTH
        || &payload[..8] != HANDOFF_WIRE_MAGIC
        || payload[8] != HANDOFF_WIRE_VERSION
        || payload[9] != HANDOFF_SIGNATURE_REQUEST
    {
        return Err(FastDrainError::InvalidConfiguration);
    }
    let mut offset = 10;
    fn take_u64(payload: &[u8], offset: &mut usize) -> u64 {
        let value = u64::from_be_bytes(
            payload[*offset..*offset + 8]
                .try_into()
                .expect("fixed handoff field"),
        );
        *offset += 8;
        value
    }
    let old_epoch = take_u64(payload, &mut offset);
    let next_epoch = take_u64(payload, &mut offset);
    let digest = B256::from_slice(&payload[offset..offset + 32]);
    offset += 32;
    let log_term = take_u64(payload, &mut offset);
    let log_index = take_u64(payload, &mut offset);
    let block_height = take_u64(payload, &mut offset);
    let block_hash = B256::from_slice(&payload[offset..offset + 32]);
    offset += 32;
    let state_root = B256::from_slice(&payload[offset..offset + 32]);
    offset += 32;
    let imported_anchor_number = take_u64(payload, &mut offset);
    let imported_anchor_hash = B256::from_slice(&payload[offset..offset + 32]);
    let point = CommittedDrainPoint {
        log_term,
        log_index,
        block_height,
        block_hash,
        state_root,
        imported_anchor_number,
        imported_anchor_hash,
    };
    if old_epoch == 0
        || next_epoch <= old_epoch
        || digest.is_zero()
        || log_index == 0
        || block_hash.is_zero()
        || state_root.is_zero()
        || imported_anchor_hash.is_zero()
    {
        return Err(FastDrainError::InvalidConfiguration);
    }
    Ok((
        DrainObjectKey::Checkpoint {
            old_epoch,
            next_epoch,
        },
        digest,
        point,
    ))
}

fn encode_checkpoint_image_chunk(
    image_hash: B256,
    total_bytes: u32,
    chunk_count: u32,
    index: u32,
    chunk: &[u8],
) -> Result<Vec<u8>, FastDrainError> {
    let expected = usize::try_from(total_bytes)
        .ok()
        .filter(|total| *total > 0 && *total <= MAX_DRAIN_OBJECT_BYTES)
        .map(|total| total.div_ceil(HANDOFF_CHUNK_BYTES))
        .and_then(|count| u32::try_from(count).ok());
    if image_hash.is_zero()
        || expected != Some(chunk_count)
        || index >= chunk_count
        || chunk.is_empty()
        || chunk.len() > HANDOFF_CHUNK_BYTES
        || (index + 1 < chunk_count && chunk.len() != HANDOFF_CHUNK_BYTES)
    {
        return Err(FastDrainError::InvalidConfiguration);
    }
    let mut out = Vec::with_capacity(8 + 1 + 1 + 32 + 12 + chunk.len());
    out.extend_from_slice(HANDOFF_WIRE_MAGIC);
    out.push(HANDOFF_WIRE_VERSION);
    out.push(HANDOFF_IMAGE_CHUNK);
    out.extend_from_slice(image_hash.as_slice());
    out.extend_from_slice(&total_bytes.to_be_bytes());
    out.extend_from_slice(&chunk_count.to_be_bytes());
    out.extend_from_slice(&index.to_be_bytes());
    out.extend_from_slice(chunk);
    Ok(out)
}

fn decode_checkpoint_image_chunk(
    payload: &[u8],
) -> Result<(B256, u32, u32, u32, &[u8]), FastDrainError> {
    const HEADER: usize = 8 + 1 + 1 + 32 + 12;
    if payload.len() <= HEADER
        || payload.len() > HEADER + HANDOFF_CHUNK_BYTES
        || &payload[..8] != HANDOFF_WIRE_MAGIC
        || payload[8] != HANDOFF_WIRE_VERSION
        || payload[9] != HANDOFF_IMAGE_CHUNK
    {
        return Err(FastDrainError::InvalidConfiguration);
    }
    let image_hash = B256::from_slice(&payload[10..42]);
    let total = u32::from_be_bytes(payload[42..46].try_into().expect("fixed"));
    let count = u32::from_be_bytes(payload[46..50].try_into().expect("fixed"));
    let index = u32::from_be_bytes(payload[50..54].try_into().expect("fixed"));
    let chunk = &payload[HEADER..];
    encode_checkpoint_image_chunk(image_hash, total, count, index, chunk)?;
    Ok((image_hash, total, count, index, chunk))
}

fn encode_checkpoint_handoff_response(
    digest: B256,
    member: Address,
    signature: SignatureBytes,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 1 + 1 + 32 + 20 + 65);
    out.extend_from_slice(HANDOFF_WIRE_MAGIC);
    out.push(HANDOFF_WIRE_VERSION);
    out.push(HANDOFF_SIGNATURE_REQUEST);
    out.extend_from_slice(digest.as_slice());
    out.extend_from_slice(member.as_slice());
    out.extend_from_slice(&signature.0);
    out
}

fn decode_checkpoint_handoff_response(
    payload: &[u8],
    digest: B256,
    member: Address,
) -> Result<SignatureBytes, FastDrainError> {
    if payload.len() != 8 + 1 + 1 + 32 + 20 + 65
        || &payload[..8] != HANDOFF_WIRE_MAGIC
        || payload[8] != HANDOFF_WIRE_VERSION
        || payload[9] != HANDOFF_SIGNATURE_REQUEST
        || B256::from_slice(&payload[10..42]) != digest
        || Address::from_slice(&payload[42..62]) != member
    {
        return Err(FastDrainError::Transport(
            "invalid checkpoint handoff signature response".to_owned(),
        ));
    }
    let signature = SignatureBytes(payload[62..].try_into().expect("fixed signature"));
    let alloy_signature = Signature::try_from(signature.0.as_slice())
        .map_err(|error| FastDrainError::Certificate(error.to_string()))?;
    if recover_signer(&alloy_signature, digest)
        .map_err(|error| FastDrainError::Certificate(error.to_string()))?
        != member
    {
        return Err(FastDrainError::Certificate(
            "checkpoint handoff signer mismatch".to_owned(),
        ));
    }
    Ok(signature)
}

/// Next-roster endpoint callback. It signs only the exact checkpoint body reconstructed from the
/// already-installed OpenRaft image and binds the authenticated requester to the old roster.
pub struct AuthenticatedNextRosterCheckpointSigner {
    requester_roster: EpochRoster,
    signing_roster: EpochRoster,
    signer: PrivateKeySigner,
    signing_journal: Arc<DurableJournal>,
    committed: RwLock<Option<Arc<dyn FastDrainCommittedState>>>,
    install_config: Option<NextRosterCheckpointInstallConfig>,
}

impl AuthenticatedNextRosterCheckpointSigner {
    pub fn new(
        requester_roster: EpochRoster,
        signing_roster: EpochRoster,
        signer: PrivateKeySigner,
        signing_journal: Arc<DurableJournal>,
        committed: Arc<dyn FastDrainCommittedState>,
    ) -> Result<Self, FastDrainError> {
        if signer.address().is_zero()
            || !signing_roster.members.contains(&signer.address())
            || signing_roster.domain.authority_epoch <= requester_roster.domain.authority_epoch
        {
            return Err(FastDrainError::InvalidConfiguration);
        }
        Ok(Self {
            requester_roster,
            signing_roster,
            signer,
            signing_journal,
            committed: RwLock::new(Some(committed)),
            install_config: None,
        })
    }

    pub fn new_installing(
        config: NextRosterCheckpointInstallConfig,
        signer: PrivateKeySigner,
        signing_journal: Arc<DurableJournal>,
    ) -> Result<Self, FastDrainError> {
        if signer.address() != config.local_next_member
            || !config.next_roster.members.contains(&signer.address())
        {
            return Err(FastDrainError::InvalidConfiguration);
        }
        Ok(Self {
            requester_roster: config.old_roster.clone(),
            signing_roster: config.next_roster.clone(),
            signer,
            signing_journal,
            committed: RwLock::new(None),
            install_config: Some(config),
        })
    }

    fn receive_checkpoint_chunk(&self, payload: &[u8]) -> Result<(), FastDrainError> {
        let config = self
            .install_config
            .as_ref()
            .ok_or(FastDrainError::InvalidConfiguration)?;
        let (image_hash, total, count, index, _) = decode_checkpoint_image_chunk(payload)?;
        let incoming_root = config.storage_directory.join("incoming-checkpoint");
        create_synced_directory(&incoming_root)?;
        let object = incoming_root.join(format!("{image_hash:x}"));
        if let Ok(entries) = fs::read_dir(&incoming_root) {
            for entry in entries {
                let entry = entry.map_err(storage)?;
                if entry.path() != object {
                    return Err(FastDrainError::CheckpointInstallationMismatch);
                }
            }
        }
        create_synced_directory(&object)?;
        install_exact_file(&object.join(format!("{index:08}.chunk")), payload)?;
        let mut encoded = Vec::with_capacity(total as usize);
        for current in 0..count {
            let path = object.join(format!("{current:08}.chunk"));
            if !path.exists() {
                return Ok(());
            }
            let frame = fs::read(path).map_err(storage)?;
            let (actual_hash, actual_total, actual_count, actual_index, chunk) =
                decode_checkpoint_image_chunk(&frame)?;
            if actual_hash != image_hash
                || actual_total != total
                || actual_count != count
                || actual_index != current
            {
                return Err(FastDrainError::CheckpointInstallationMismatch);
            }
            encoded.extend_from_slice(chunk);
        }
        if encoded.len() != total as usize {
            return Err(FastDrainError::CheckpointInstallationMismatch);
        }
        let installed = install_next_roster_checkpoint_state(config.clone(), &encoded)?;
        if installed.image_hash != image_hash {
            return Err(FastDrainError::CheckpointInstallationMismatch);
        }
        let mut committed = self
            .committed
            .write()
            .map_err(|_| storage("checkpoint state lock poisoned"))?;
        match committed.as_ref() {
            Some(existing) if existing.installed_checkpoint_hash()? != image_hash => {
                return Err(FastDrainError::CheckpointInstallationMismatch);
            }
            Some(_) => {}
            None => *committed = Some(installed),
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct NextRosterCheckpointInstallConfig {
    pub storage_directory: PathBuf,
    pub old_roster: EpochRoster,
    pub next_roster: EpochRoster,
    pub local_next_member: Address,
    /// Exact finalized imported Tempo anchor that supplied both rosters and checkpoint authority.
    pub expected_imported_anchor_number: u64,
    pub expected_imported_anchor_hash: B256,
    /// Old roster's finalized accepted prefix. These values are read independently by the
    /// install-only successor and must match the received image before it can acknowledge.
    pub expected_final_zone_height: U256,
    pub expected_final_block_hash: B256,
    pub expected_final_withdrawal_batch_index: u64,
    pub expected_final_settlement_hash: B256,
    /// Finalized next-roster node metadata in registry order, using node IDs 1, 2 and 3.
    pub next_membership: BTreeMap<u64, BasicNode>,
}

const SUCCESSOR_BOOTSTRAP_FILE: &str = "successor-bootstrap.bin";
const SUCCESSOR_AUTHORIZED_FILE: &str = "successor-transition-authorized.bin";
const SUCCESSOR_COMPLETE_FILE: &str = "successor-transition-complete.bin";
const SUCCESSOR_CATCHUP_TARGET_FILE: &str = "successor-catchup-target.bin";
const SUCCESSOR_CATCHUP_COMPLETE_FILE: &str = "successor-catchup-complete.bin";
const BATCH_BOUNDARY_MAGIC: &[u8; 8] = b"ZFBND001";
const REPLENISHMENT_MAGIC: &[u8; 8] = b"ZFRPL001";
const EXECUTION_OUTCOMES_MAGIC: &[u8; 8] = b"ZFOUT001";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SuccessorBootstrapArtifact {
    pub image_hash: B256,
    pub old_epoch: u64,
    pub next_epoch: u64,
    pub old_roster_hash: B256,
    pub next_roster_hash: B256,
    pub accepted_prefix: LogId<u64>,
    pub next_membership: BTreeMap<u64, BasicNode>,
}

#[derive(Clone, Debug)]
pub struct SuccessorResourceRestoreConfig {
    pub successor_storage_directory: PathBuf,
    /// Directory passed to `CanonicalFastExecution::open`, before it is opened.
    pub execution_directory: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RestoredSuccessorResources {
    pub batch_boundary: CanonicalFastSettlementBoundary,
    pub replenishment_jobs: Vec<ReplenishmentJob>,
}

#[derive(Clone, Debug)]
pub struct CheckpointResourcePublishConfig {
    pub imported_anchor_number: u64,
    pub imported_anchor_hash: B256,
    pub withdrawal_batch_index: u64,
    pub local_closure_hash: B256,
    pub drain_barriers: DrainCheckpointResources,
    pub batch_boundary: CanonicalFastSettlementBoundary,
}

/// Publish the complete pre-settlement resource image at the exact committed head. The final
/// settlement hash deliberately starts at zero and is sealed one-way by
/// `record_final_settlement_hash` only after the registry observation succeeds.
pub fn publish_checkpoint_resources<E: DurableStateMachineExecution>(
    committed: &CommittedStateHandle<E>,
    protocol_journal: &DurableJournal,
    config: CheckpointResourcePublishConfig,
) -> Result<bool, FastDrainError> {
    if config.imported_anchor_number == 0
        || config.imported_anchor_hash.is_zero()
        || config.local_closure_hash.is_zero()
        || config.drain_barriers.peers.len() != zone_fast_transfer::drain::DRAIN_PEER_COUNT
    {
        return Err(FastDrainError::InvalidConfiguration);
    }
    let exact = committed
        .exact_state_image()
        .map_err(|error| FastDrainError::Storage(error.to_string()))?;
    let mut peer_portals = BTreeSet::new();
    for (portal, barrier_bytes, resolution_bytes) in &config.drain_barriers.peers {
        let barrier = BarrierInventory::decode_durable(barrier_bytes)?;
        let resolution = BarrierResolutionInventory::decode_durable(resolution_bytes)?;
        if *portal != barrier.statement.source_portal
            || !peer_portals.insert(*portal)
            || resolution.verify(&barrier).is_err()
        {
            return Err(FastDrainError::InvalidConfiguration);
        }
    }
    let jobs = protocol_journal
        .unfinished_replenishment_jobs()
        .map_err(storage)?;
    let resources = CheckpointResourceImage {
        at: exact.last_applied,
        imported_anchor_number: config.imported_anchor_number,
        imported_anchor_hash: config.imported_anchor_hash,
        withdrawal_batch_index: config.withdrawal_batch_index,
        local_closure_hash: config.local_closure_hash,
        final_settlement_hash: B256::ZERO,
        service_protocol_journal: protocol_journal
            .checkpoint_snapshot_bytes()
            .map_err(storage)?,
        drain_barriers: bincode::serialize(&config.drain_barriers).map_err(storage)?,
        canonical_batch_boundary: encode_successor_batch_boundary(config.batch_boundary)?,
        replenishment_inventory: encode_successor_replenishment_inventory(&jobs)?,
    };
    committed
        .persist_checkpoint_resources(resources)
        .map_err(|error| FastDrainError::Storage(error.to_string()))
}

fn encode_checked_resource<T: Serialize>(
    magic: &[u8; 8],
    value: &T,
) -> Result<Vec<u8>, FastDrainError> {
    let payload = bincode::serialize(value).map_err(storage)?;
    let mut encoded = Vec::with_capacity(40 + payload.len());
    encoded.extend_from_slice(magic);
    encoded.extend_from_slice(keccak256(&payload).as_slice());
    encoded.extend_from_slice(&payload);
    Ok(encoded)
}

fn decode_checked_resource<T: for<'de> Deserialize<'de>>(
    magic: &[u8; 8],
    encoded: &[u8],
) -> Result<T, FastDrainError> {
    if encoded.len() <= 40 || &encoded[..8] != magic || keccak256(&encoded[40..]) != encoded[8..40]
    {
        return Err(FastDrainError::CheckpointInstallationMismatch);
    }
    bincode::deserialize(&encoded[40..]).map_err(storage)
}

pub fn encode_successor_batch_boundary(
    boundary: CanonicalFastSettlementBoundary,
) -> Result<Vec<u8>, FastDrainError> {
    encode_checked_resource(
        BATCH_BOUNDARY_MAGIC,
        &(
            boundary.block_height,
            boundary.block_hash,
            boundary.timestamp_millis,
        ),
    )
}

pub fn encode_successor_replenishment_inventory(
    jobs: &[ReplenishmentJob],
) -> Result<Vec<u8>, FastDrainError> {
    let mut job_ids = jobs.iter().map(|job| job.job_id).collect::<Vec<_>>();
    job_ids.sort_unstable();
    if job_ids.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(FastDrainError::InvalidConfiguration);
    }
    encode_checked_resource(REPLENISHMENT_MAGIC, &job_ids)
}

/// Restore executor-owned outcomes and the shared C4/replenishment protocol journal before
/// `CanonicalFastExecution::open`. Every target is exact-install-only: preexisting different bytes
/// fail closed. The returned boundary is the checkpoint's canonical scheduler predecessor.
pub fn restore_successor_runtime_resources(
    config: SuccessorResourceRestoreConfig,
) -> Result<RestoredSuccessorResources, FastDrainError> {
    if config.successor_storage_directory.as_os_str().is_empty()
        || config.execution_directory.as_os_str().is_empty()
    {
        return Err(FastDrainError::InvalidConfiguration);
    }
    let artifact = load_successor_bootstrap(&config.successor_storage_directory)?;
    let encoded = fs::read(
        config
            .successor_storage_directory
            .join("checkpoint-image.bin"),
    )
    .map_err(storage)?;
    let image = CheckpointImage::decode_durable(&encoded)?;
    if image.image_hash()? != artifact.image_hash {
        return Err(FastDrainError::CheckpointInstallationMismatch);
    }
    let exact = inspect_exact_state_image(&image.consensus_snapshot)
        .map_err(|error| FastDrainError::Storage(error.to_string()))?;

    #[derive(Serialize)]
    struct ExecutionDiskRecord {
        intent: Vec<u8>,
        body: Vec<u8>,
        certificate: Option<Vec<u8>>,
    }
    let mut outcomes = Vec::with_capacity(exact.certified_history.len());
    for record in &exact.certified_history {
        let intent = TransferIntent::decode(&record.canonical_intent)
            .map_err(|error| FastDrainError::Storage(error.to_string()))?;
        let certificate = OutcomeCertificate::decode(&record.canonical_certificate)
            .map_err(|error| FastDrainError::Storage(error.to_string()))?;
        if intent.transfer_id() != record.transfer_id
            || certificate.body.transfer_id != record.transfer_id
            || certificate.body.intent_hash != intent.intent_hash()
            || certificate.body.log_term != record.log_id.leader_id.term
            || certificate.body.log_index != record.log_id.index
            || certificate.body.block_height != record.block_height
            || certificate.body.block_hash != record.block_hash
            || certificate.body.state_root != record.state_root
        {
            return Err(FastDrainError::CheckpointInstallationMismatch);
        }
        outcomes.push(ExecutionDiskRecord {
            intent: record.canonical_intent.clone(),
            body: certificate.body.canonical_bytes(),
            certificate: Some(record.canonical_certificate.clone()),
        });
    }
    let payload = bincode::serialize(&outcomes).map_err(storage)?;
    let mut outcome_bytes = Vec::with_capacity(40 + payload.len());
    outcome_bytes.extend_from_slice(EXECUTION_OUTCOMES_MAGIC);
    outcome_bytes.extend_from_slice(keccak256(&payload).as_slice());
    outcome_bytes.extend_from_slice(&payload);

    create_synced_directory(&config.execution_directory)?;
    let outcomes_directory = config.execution_directory.join("outcomes");
    let journal_directory = config.execution_directory.join("journal");
    create_synced_directory(&outcomes_directory)?;
    create_synced_directory(&journal_directory)?;
    install_exact_file(
        &outcomes_directory.join("committed-outcomes.bin"),
        &outcome_bytes,
    )?;
    install_exact_file(
        &journal_directory.join("snapshot.bin"),
        &image.fast_service_journal,
    )?;
    let journal_path = journal_directory.join("journal.bin");
    if journal_path.exists() && fs::metadata(&journal_path).map_err(storage)?.len() != 0 {
        return Err(FastDrainError::CheckpointInstallationMismatch);
    }
    let journal = DurableJournal::open(&journal_directory).map_err(storage)?;
    let mut journal_jobs = journal.unfinished_replenishment_jobs().map_err(storage)?;
    journal_jobs.sort_by_key(|job| job.job_id);
    let mut replenishment_job_ids: Vec<B256> =
        decode_checked_resource(REPLENISHMENT_MAGIC, &image.replenishment_state)?;
    replenishment_job_ids.sort_unstable();
    if replenishment_job_ids
        .windows(2)
        .any(|pair| pair[0] == pair[1])
        || journal_jobs
            .iter()
            .map(|job| job.job_id)
            .collect::<Vec<_>>()
            != replenishment_job_ids
    {
        return Err(FastDrainError::CheckpointInstallationMismatch);
    }
    let (block_height, block_hash, timestamp_millis): (u64, B256, u64) =
        decode_checked_resource(BATCH_BOUNDARY_MAGIC, &image.batch_boundaries)?;
    if block_hash.is_zero()
        || timestamp_millis == 0
        || block_height > image.statement.final_zone_height.to::<u64>()
    {
        return Err(FastDrainError::CheckpointInstallationMismatch);
    }
    sync_directory(&outcomes_directory)?;
    sync_directory(&journal_directory)?;
    sync_directory(&config.execution_directory)?;
    Ok(RestoredSuccessorResources {
        batch_boundary: CanonicalFastSettlementBoundary {
            block_height,
            block_hash,
            timestamp_millis,
        },
        replenishment_jobs: journal_jobs,
    })
}

/// Seed the real successor C5 journal with the exact old-epoch replay barriers retained by the
/// checkpoint. Call this before `assemble_production_fast_drain` opens the same directory.
pub fn restore_successor_drain_resources(
    successor_storage_directory: impl AsRef<Path>,
    drain_journal_directory: impl AsRef<Path>,
    next_epoch: u64,
    next_signer: Address,
    first_nonce: u64,
) -> Result<(), FastDrainError> {
    let root = successor_storage_directory.as_ref();
    let artifact = decode_successor_artifact(root)?;
    if artifact.next_epoch != next_epoch || next_signer.is_zero() {
        return Err(FastDrainError::InvalidConfiguration);
    }
    let image = CheckpointImage::decode_durable(
        &fs::read(root.join("checkpoint-image.bin")).map_err(storage)?,
    )?;
    let resources: DrainCheckpointResources =
        bincode::deserialize(&image.replay_barriers).map_err(storage)?;
    let journal = FileFastDrainJournal::open(
        drain_journal_directory,
        next_epoch,
        next_signer,
        first_nonce,
    )?;
    for (_, barrier_bytes, resolution_bytes) in resources.peers {
        let barrier = BarrierInventory::decode_durable(&barrier_bytes)?;
        let resolution = BarrierResolutionInventory::decode_durable(&resolution_bytes)?;
        resolution.verify(&barrier)?;
        let barrier_key = DrainObjectKey::InboundBarrier {
            epoch: barrier.statement.destination_epoch,
            source: barrier.statement.source_portal,
        };
        let resolution_key = DrainObjectKey::Resolution {
            destination_epoch: barrier.statement.destination_epoch,
            destination: barrier.statement.destination_portal,
            source: barrier.statement.source_portal,
        };
        journal.persist_barrier(&barrier_key, &barrier)?;
        journal.persist_resolution(&resolution_key, &resolution)?;
    }
    journal.snapshot()
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct SuccessorTransitionAuthorization {
    image_hash: B256,
    checkpoint_digest: B256,
    accepted_prefix: LogId<u64>,
    signing_member: Address,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct SuccessorTransitionComplete {
    image_hash: B256,
    accepted_prefix: LogId<u64>,
    membership_log: LogId<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct SuccessorCatchupTarget {
    image_hash: B256,
    anchor_number: u64,
    anchor_hash: B256,
}

pub fn persist_successor_catchup_target(
    storage_directory: impl AsRef<Path>,
    anchor_number: u64,
    anchor_hash: B256,
) -> Result<(), FastDrainError> {
    let root = storage_directory.as_ref();
    let artifact = decode_successor_artifact(root)?;
    if anchor_number == 0 || anchor_hash.is_zero() {
        return Err(FastDrainError::InvalidConfiguration);
    }
    let target = SuccessorCatchupTarget {
        image_hash: artifact.image_hash,
        anchor_number,
        anchor_hash,
    };
    install_exact_file(
        &root.join(SUCCESSOR_CATCHUP_TARGET_FILE),
        &bincode::serialize(&target).map_err(storage)?,
    )?;
    sync_directory(root)
}

pub fn pending_successor_catchup_target(
    storage_directory: impl AsRef<Path>,
) -> Result<Option<(u64, B256)>, FastDrainError> {
    let root = storage_directory.as_ref();
    let artifact = decode_successor_artifact(root)?;
    let target_bytes = match fs::read(root.join(SUCCESSOR_CATCHUP_TARGET_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(storage(error)),
    };
    let target: SuccessorCatchupTarget = bincode::deserialize(&target_bytes).map_err(storage)?;
    if target.image_hash != artifact.image_hash
        || target.anchor_number == 0
        || target.anchor_hash.is_zero()
    {
        return Err(FastDrainError::CheckpointInstallationMismatch);
    }
    match fs::read(root.join(SUCCESSOR_CATCHUP_COMPLETE_FILE)) {
        Ok(bytes) => {
            let complete: SuccessorCatchupTarget = bincode::deserialize(&bytes).map_err(storage)?;
            if complete != target {
                return Err(FastDrainError::CheckpointInstallationMismatch);
            }
            Ok(None)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(Some((target.anchor_number, target.anchor_hash)))
        }
        Err(error) => Err(storage(error)),
    }
}

pub fn complete_successor_catchup(
    storage_directory: impl AsRef<Path>,
    anchor_number: u64,
    anchor_hash: B256,
) -> Result<(), FastDrainError> {
    let root = storage_directory.as_ref();
    let Some((expected_number, expected_hash)) = pending_successor_catchup_target(root)? else {
        return Ok(());
    };
    if (anchor_number, anchor_hash) != (expected_number, expected_hash) {
        return Err(FastDrainError::CheckpointInstallationMismatch);
    }
    let artifact = decode_successor_artifact(root)?;
    let complete = SuccessorCatchupTarget {
        image_hash: artifact.image_hash,
        anchor_number,
        anchor_hash,
    };
    install_exact_file(
        &root.join(SUCCESSOR_CATCHUP_COMPLETE_FILE),
        &bincode::serialize(&complete).map_err(storage)?,
    )?;
    sync_directory(root)
}

fn decode_successor_artifact(
    storage_directory: &Path,
) -> Result<SuccessorBootstrapArtifact, FastDrainError> {
    let bytes = fs::read(storage_directory.join(SUCCESSOR_BOOTSTRAP_FILE)).map_err(storage)?;
    let artifact: SuccessorBootstrapArtifact = bincode::deserialize(&bytes).map_err(storage)?;
    if artifact.image_hash.is_zero()
        || artifact.old_epoch == 0
        || artifact.next_epoch <= artifact.old_epoch
        || artifact.old_roster_hash.is_zero()
        || artifact.next_roster_hash.is_zero()
        || artifact.accepted_prefix.index == 0
        || artifact
            .next_membership
            .keys()
            .copied()
            .collect::<BTreeSet<_>>()
            != BTreeSet::from([1, 2, 3])
        || artifact
            .next_membership
            .values()
            .any(|node| node.addr.is_empty())
    {
        return Err(FastDrainError::CheckpointInstallationMismatch);
    }
    Ok(artifact)
}

/// Validate and return the exact successor bootstrap installed by the authenticated checkpoint
/// receiver. Call this before opening the Raft stores: it rejects a missing/conflicting snapshot,
/// checkpoint image, or purged/committed log prefix.
pub fn load_successor_bootstrap(
    storage_directory: impl AsRef<Path>,
) -> Result<SuccessorBootstrapArtifact, FastDrainError> {
    let root = storage_directory.as_ref();
    let artifact = decode_successor_artifact(root)?;
    let encoded = fs::read(root.join("checkpoint-image.bin")).map_err(storage)?;
    let image = CheckpointImage::decode_durable(&encoded)?;
    if image.image_hash()? != artifact.image_hash
        || image.statement.old_epoch != artifact.old_epoch
        || image.statement.next_epoch != artifact.next_epoch
        || image.statement.next_roster_hash != artifact.next_roster_hash
    {
        return Err(FastDrainError::CheckpointInstallationMismatch);
    }
    let installed_exact = inspect_exact_state_image(&image.consensus_snapshot)
        .map_err(|error| FastDrainError::Storage(error.to_string()))?;
    if installed_exact.last_applied != artifact.accepted_prefix {
        return Err(FastDrainError::CheckpointInstallationMismatch);
    }
    let snapshot = fs::read(root.join("state-machine/raft-state-machine.bin")).map_err(storage)?;
    let exact = inspect_exact_state_image(&snapshot)
        .map_err(|error| FastDrainError::Storage(error.to_string()))?;
    let accepted = exact
        .blocks
        .iter()
        .find(|block| block.log_id == artifact.accepted_prefix)
        .ok_or(FastDrainError::CheckpointInstallationMismatch)?;
    let installed_head = installed_exact
        .blocks
        .last()
        .ok_or(FastDrainError::CheckpointInstallationMismatch)?;
    let transition_complete = fs::read(root.join(SUCCESSOR_COMPLETE_FILE))
        .ok()
        .and_then(|bytes| bincode::deserialize::<SuccessorTransitionComplete>(&bytes).ok())
        .is_some_and(|complete| {
            complete.image_hash == artifact.image_hash
                && complete.accepted_prefix == artifact.accepted_prefix
                && complete.membership_log.index > artifact.accepted_prefix.index
                && exact.last_applied.index >= complete.membership_log.index
        });
    if accepted.output != installed_head.output
        || exact.last_applied.index < artifact.accepted_prefix.index
        || (snapshot != image.consensus_snapshot && !transition_complete)
    {
        return Err(FastDrainError::CheckpointInstallationMismatch);
    }
    if exact.last_applied == artifact.accepted_prefix {
        DurableRaftLogStore::install_snapshot_prefix(root.join("log"), artifact.accepted_prefix)
            .map_err(storage)?;
    }
    Ok(artifact)
}

/// Return the exact imported Tempo anchor retained in the installed old-prefix image. This is
/// used only to start an authenticated successor catch-up before its execution database has been
/// replayed; it does not grant SameAnchor admission.
pub fn load_successor_checkpoint_anchor(
    storage_directory: impl AsRef<Path>,
) -> Result<(u64, B256, u64), FastDrainError> {
    let root = storage_directory.as_ref();
    let artifact = decode_successor_artifact(root)?;
    let image = CheckpointImage::decode_durable(
        &fs::read(root.join("checkpoint-image.bin")).map_err(storage)?,
    )?;
    if image.image_hash()? != artifact.image_hash {
        return Err(FastDrainError::CheckpointInstallationMismatch);
    }
    let exact = inspect_exact_state_image(&image.consensus_snapshot).map_err(storage)?;
    let resources = exact
        .checkpoint_resources
        .ok_or(FastDrainError::CheckpointInstallationMismatch)?;
    if exact.last_applied != artifact.accepted_prefix
        || resources.at != artifact.accepted_prefix
        || resources.imported_anchor_number == 0
        || resources.imported_anchor_hash.is_zero()
    {
        return Err(FastDrainError::CheckpointInstallationMismatch);
    }
    let last_operational_anchor = exact
        .blocks
        .iter()
        .rev()
        .find_map(|block| {
            let attributes: ZonePayloadAttributes =
                bincode::deserialize(&block.input.l1_inputs).ok()?;
            match attributes.tempo_import {
                TempoImport::Full(prepared) => Some(prepared.header.number()),
                TempoImport::CheckpointOnly(_) | TempoImport::SameAnchor(_) => None,
            }
        })
        .ok_or(FastDrainError::CheckpointInstallationMismatch)?;
    if last_operational_anchor > resources.imported_anchor_number {
        return Err(FastDrainError::CheckpointInstallationMismatch);
    }
    Ok((
        resources.imported_anchor_number,
        resources.imported_anchor_hash,
        last_operational_anchor,
    ))
}

fn authorize_successor_transition(
    config: &NextRosterCheckpointInstallConfig,
    committed: &dyn FastDrainCommittedState,
    checkpoint_digest: B256,
    point: CommittedDrainPoint,
) -> Result<(), FastDrainError> {
    let artifact = load_successor_bootstrap(&config.storage_directory)?;
    let image_hash = committed.installed_checkpoint_hash()?;
    if image_hash != artifact.image_hash
        || point.log_term != artifact.accepted_prefix.leader_id.term
        || point.log_index != artifact.accepted_prefix.index
        || checkpoint_digest.is_zero()
    {
        return Err(FastDrainError::CheckpointInstallationMismatch);
    }
    let authorization = SuccessorTransitionAuthorization {
        image_hash,
        checkpoint_digest,
        accepted_prefix: artifact.accepted_prefix,
        signing_member: config.local_next_member,
    };
    install_exact_file(
        &config.storage_directory.join(SUCCESSOR_AUTHORIZED_FILE),
        &bincode::serialize(&authorization).map_err(storage)?,
    )?;
    sync_directory(&config.storage_directory)
}

fn membership_matches(
    metrics: &openraft::RaftMetrics<u64, BasicNode>,
    expected: &BTreeMap<u64, BasicNode>,
) -> bool {
    metrics
        .membership_config
        .membership()
        .nodes()
        .map(|(id, node)| (*id, node.clone()))
        .collect::<BTreeMap<_, _>>()
        == *expected
        && metrics
            .membership_config
            .membership()
            .voter_ids()
            .collect::<BTreeSet<_>>()
            == BTreeSet::from([1, 2, 3])
}

/// Complete the authorized successor membership transition. Any member that produced a local
/// next-roster checkpoint signature may elect and write the idempotent `SetNodes` entry. A member
/// without that marker only waits for an authorized peer's entry to replicate and apply.
pub async fn drive_successor_membership_transition(
    raft: &Raft<FastRaftConfig>,
    storage_directory: impl AsRef<Path>,
    local_node_id: u64,
    timeout: Duration,
) -> Result<LogId<u64>, FastDrainError> {
    if !matches!(local_node_id, 1..=3) || timeout.is_zero() {
        return Err(FastDrainError::InvalidConfiguration);
    }
    let root = storage_directory.as_ref();
    let artifact = decode_successor_artifact(root)?;
    if let Ok(bytes) = fs::read(root.join(SUCCESSOR_COMPLETE_FILE)) {
        let complete: SuccessorTransitionComplete =
            bincode::deserialize(&bytes).map_err(storage)?;
        if complete.image_hash != artifact.image_hash
            || complete.accepted_prefix != artifact.accepted_prefix
            || complete.membership_log.index <= artifact.accepted_prefix.index
        {
            return Err(FastDrainError::CheckpointInstallationMismatch);
        }
        return Ok(complete.membership_log);
    }
    let authorized = match fs::read(root.join(SUCCESSOR_AUTHORIZED_FILE)) {
        Ok(bytes) => {
            let authorization: SuccessorTransitionAuthorization =
                bincode::deserialize(&bytes).map_err(storage)?;
            if authorization.image_hash != artifact.image_hash
                || authorization.accepted_prefix != artifact.accepted_prefix
                || authorization.checkpoint_digest.is_zero()
            {
                return Err(FastDrainError::CheckpointInstallationMismatch);
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(storage(error)),
    };

    let mut metrics = raft.metrics();
    if authorized {
        raft.trigger()
            .elect()
            .await
            .map_err(|error| FastDrainError::Consensus(error.to_string()))?;
    }
    let transition = async {
        let mut attempted_term = None;
        loop {
            let current = metrics.borrow().clone();
            if let Err(error) = current.running_state {
                return Err(FastDrainError::Consensus(error.to_string()));
            }
            if membership_matches(&current, &artifact.next_membership)
                && current
                    .last_applied
                    .is_some_and(|log| log.index > artifact.accepted_prefix.index)
            {
                return Ok::<LogId<u64>, FastDrainError>(
                    current.last_applied.expect("checked above"),
                );
            }
            if authorized
                && current.current_leader == Some(local_node_id)
                && attempted_term != Some(current.current_term)
            {
                attempted_term = Some(current.current_term);
                // Losing leadership races another authorized member. Its identical SetNodes entry
                // is sufficient, so observe/retry in a later term instead of failing this node.
                let _ = raft
                    .change_membership(
                        ChangeMembers::SetNodes(artifact.next_membership.clone()),
                        false,
                    )
                    .await;
                continue;
            }
            tokio::select! {
                changed = metrics.changed() => {
                    changed.map_err(|error| FastDrainError::Consensus(error.to_string()))?;
                }
                () = tokio::time::sleep(Duration::from_secs(2)), if authorized => {
                    raft.trigger().elect().await
                        .map_err(|error| FastDrainError::Consensus(error.to_string()))?;
                }
            }
        }
    };
    let membership_log = tokio::time::timeout(timeout, transition)
        .await
        .map_err(|_| {
            FastDrainError::Consensus("successor membership transition timed out".into())
        })??;
    let complete = SuccessorTransitionComplete {
        image_hash: artifact.image_hash,
        accepted_prefix: artifact.accepted_prefix,
        membership_log,
    };
    install_exact_file(
        &root.join(SUCCESSOR_COMPLETE_FILE),
        &bincode::serialize(&complete).map_err(storage)?,
    )?;
    sync_directory(root)?;
    Ok(membership_log)
}

/// Read-only committed-state projection owned by a standalone next member. Only checkpoint body
/// reconstruction is supported; every mutating/old-epoch drain method fails closed.
pub struct InstalledNextRosterCheckpointState {
    image: CheckpointImage,
    image_hash: B256,
    point: CommittedDrainPoint,
    state_machine_directory: PathBuf,
}

impl InstalledNextRosterCheckpointState {
    pub fn state_machine_directory(&self) -> &Path {
        &self.state_machine_directory
    }
}

/// Atomically install the exact canonical OpenRaft image and all checkpoint resources into a
/// next-member directory. Existing bytes must be identical; a partial or conflicting prefix is
/// never replaced. The returned projection may be given directly to the checkpoint signer.
pub fn install_next_roster_checkpoint_state(
    config: NextRosterCheckpointInstallConfig,
    encoded_checkpoint: &[u8],
) -> Result<Arc<InstalledNextRosterCheckpointState>, FastDrainError> {
    if config.storage_directory.as_os_str().is_empty()
        || !config
            .next_roster
            .members
            .contains(&config.local_next_member)
        || config.old_roster.domain.portal != config.next_roster.domain.portal
        || config.old_roster.domain.zone_id != config.next_roster.domain.zone_id
        || config.old_roster.domain.l1_chain_id != config.next_roster.domain.l1_chain_id
        || config.old_roster.domain.chain_id != config.next_roster.domain.chain_id
        || config.old_roster.domain.protocol_version != config.next_roster.domain.protocol_version
        || config.next_roster.domain.authority_epoch <= config.old_roster.domain.authority_epoch
        || config.expected_imported_anchor_number == 0
        || config.expected_imported_anchor_hash.is_zero()
        || config.expected_final_zone_height.is_zero()
        || config.expected_final_block_hash.is_zero()
        || config.expected_final_settlement_hash.is_zero()
        || config
            .next_membership
            .keys()
            .copied()
            .collect::<BTreeSet<_>>()
            != BTreeSet::from([1, 2, 3])
        || config
            .next_membership
            .values()
            .any(|node| node.addr.is_empty())
    {
        return Err(FastDrainError::InvalidConfiguration);
    }
    let image = CheckpointImage::decode_durable(encoded_checkpoint)?;
    image.validate()?;
    if image.statement.old_epoch != config.old_roster.domain.authority_epoch
        || image.statement.next_epoch != config.next_roster.domain.authority_epoch
        || image.statement.next_roster_hash != config.next_roster.domain.roster_hash
        || image.statement.portal != config.old_roster.domain.portal
        || image.statement.final_zone_height != config.expected_final_zone_height
        || image.statement.final_block_hash != config.expected_final_block_hash
        || image.statement.final_withdrawal_batch_index
            != config.expected_final_withdrawal_batch_index
        || image.statement.final_settlement_hash != config.expected_final_settlement_hash
    {
        return Err(FastDrainError::CheckpointInstallationMismatch);
    }
    let exact = inspect_exact_state_image(&image.consensus_snapshot)
        .map_err(|error| FastDrainError::Storage(error.to_string()))?;
    let head = exact
        .blocks
        .last()
        .ok_or(FastDrainError::InvalidCommittedPoint)?;
    let resources = exact
        .checkpoint_resources
        .as_ref()
        .ok_or(FastDrainError::CheckpointInstallationMismatch)?;
    let expected_transfer_history =
        bincode::serialize(&exact.certified_history).map_err(storage)?;
    let expected_replay_witnesses = bincode::serialize(
        &exact
            .blocks
            .iter()
            .map(|block| block.input.replay_witness.clone())
            .collect::<Vec<_>>(),
    )
    .map_err(storage)?;
    let expected_raft_prefix = exact
        .blocks
        .iter()
        .map(|block| block.output.block_hash)
        .collect::<Vec<_>>();
    if exact.last_applied.leader_id.term != image.statement.checkpoint_log_term
        || exact.last_applied.index != image.statement.checkpoint_log_index
        || head.output.block_height != image.statement.checkpoint_height.to::<u64>()
        || head.output.block_hash != image.statement.checkpoint_block_hash
        || head.output.state_root != image.statement.checkpoint_state_root
        || image.canonical_head_hash != head.output.block_hash
        || image.canonical_state_root != head.output.state_root
        || resources.at != exact.last_applied
        || resources.imported_anchor_hash.is_zero()
        || resources.imported_anchor_number != config.expected_imported_anchor_number
        || resources.imported_anchor_hash != config.expected_imported_anchor_hash
        || resources.service_protocol_journal != image.fast_service_journal
        || resources.drain_barriers != image.replay_barriers
        || resources.canonical_batch_boundary != image.batch_boundaries
        || resources.replenishment_inventory != image.replenishment_state
        || image.raft_prefix != expected_raft_prefix
        || image.transfer_history != expected_transfer_history
        || image.replay_witnesses != expected_replay_witnesses
        || image.witness_root != keccak256(&expected_replay_witnesses)
        || image.outcomes_root != keccak256(&expected_transfer_history)
        || image.replay_barriers_root != keccak256(&image.replay_barriers)
    {
        return Err(FastDrainError::CheckpointInstallationMismatch);
    }
    let root = config.storage_directory;
    let state_machine_directory = root.join("state-machine");
    let resources_directory = root.join("checkpoint-resources");
    create_synced_directory(&root)?;
    create_synced_directory(&state_machine_directory)?;
    create_synced_directory(&resources_directory)?;
    install_exact_file(&root.join("checkpoint-image.bin"), encoded_checkpoint)?;
    install_exact_file(
        &state_machine_directory.join("raft-state-machine.bin"),
        &image.consensus_snapshot,
    )?;
    for (name, bytes) in [
        ("transfer-history.bin", image.transfer_history.as_slice()),
        ("replay-witnesses.bin", image.replay_witnesses.as_slice()),
        ("replay-barriers.bin", image.replay_barriers.as_slice()),
        (
            "fast-service-journal.bin",
            image.fast_service_journal.as_slice(),
        ),
        ("batch-boundaries.bin", image.batch_boundaries.as_slice()),
        (
            "replenishment-state.bin",
            image.replenishment_state.as_slice(),
        ),
    ] {
        install_exact_file(&resources_directory.join(name), bytes)?;
    }
    sync_directory(&resources_directory)?;
    sync_directory(&state_machine_directory)?;
    sync_directory(&root)?;
    let image_hash = image.image_hash()?;
    DurableRaftLogStore::install_snapshot_prefix(root.join("log"), exact.last_applied)
        .map_err(storage)?;
    let artifact = SuccessorBootstrapArtifact {
        image_hash,
        old_epoch: config.old_roster.domain.authority_epoch,
        next_epoch: config.next_roster.domain.authority_epoch,
        old_roster_hash: config.old_roster.domain.roster_hash,
        next_roster_hash: config.next_roster.domain.roster_hash,
        accepted_prefix: exact.last_applied,
        next_membership: config.next_membership.clone(),
    };
    install_exact_file(
        &root.join(SUCCESSOR_BOOTSTRAP_FILE),
        &bincode::serialize(&artifact).map_err(storage)?,
    )?;
    sync_directory(&root)?;
    let point = CommittedDrainPoint {
        log_term: exact.last_applied.leader_id.term,
        log_index: exact.last_applied.index,
        block_height: head.output.block_height,
        block_hash: head.output.block_hash,
        state_root: head.output.state_root,
        imported_anchor_number: resources.imported_anchor_number,
        imported_anchor_hash: resources.imported_anchor_hash,
    };
    Ok(Arc::new(InstalledNextRosterCheckpointState {
        image,
        image_hash,
        point,
        state_machine_directory,
    }))
}

fn create_synced_directory(path: &Path) -> Result<(), FastDrainError> {
    if path.exists() {
        if !path.is_dir() {
            return Err(storage("checkpoint install path is not a directory"));
        }
        return Ok(());
    }
    fs::create_dir_all(path).map_err(storage)?;
    sync_directory(path.parent().unwrap_or_else(|| Path::new(".")))
}

fn install_exact_file(path: &Path, bytes: &[u8]) -> Result<(), FastDrainError> {
    if bytes.is_empty() {
        return Err(FastDrainError::CheckpointInstallationMismatch);
    }
    if path.exists() {
        return (fs::read(path).map_err(storage)? == bytes)
            .then_some(())
            .ok_or(FastDrainError::CheckpointInstallationMismatch);
    }
    let temporary = path.with_extension("installing");
    if temporary.exists() {
        fs::remove_file(&temporary).map_err(storage)?;
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(storage)?;
    file.write_all(bytes).map_err(storage)?;
    file.sync_all().map_err(storage)?;
    fs::rename(&temporary, path).map_err(storage)?;
    sync_directory(path.parent().unwrap_or_else(|| Path::new(".")))
}

impl FastDrainCommittedState for InstalledNextRosterCheckpointState {
    fn commit_no_new_locks<'a>(
        &'a self,
        _destination: &'a EpochRoster,
        _closure_hash: B256,
    ) -> DrainFuture<'a, Result<CommittedDrainPoint, FastDrainError>> {
        Box::pin(async { Err(FastDrainError::InvalidConfiguration) })
    }

    fn local_destination_closure_point(
        &self,
        _epoch: u64,
        _closure_hash: B256,
    ) -> Result<CommittedDrainPoint, FastDrainError> {
        Err(FastDrainError::InvalidConfiguration)
    }

    fn source_snapshot(
        &self,
        _point: CommittedDrainPoint,
        _destination: &EpochRoster,
    ) -> Result<crate::fast_drain::SourceDrainSnapshot, FastDrainError> {
        Err(FastDrainError::InvalidConfiguration)
    }

    fn assert_imported_destination_closure(
        &self,
        _point: CommittedDrainPoint,
        _destination: &EpochRoster,
        _closure_hash: B256,
    ) -> Result<(), FastDrainError> {
        Err(FastDrainError::InvalidConfiguration)
    }

    fn current_source_snapshot(
        &self,
        _point: CommittedDrainPoint,
        _destination: &EpochRoster,
    ) -> Result<(crate::fast_drain::SourceDrainSnapshot, CommittedDrainPoint), FastDrainError> {
        Err(FastDrainError::InvalidConfiguration)
    }

    fn commit_imported_barrier<'a>(
        &'a self,
        _inventory: &'a BarrierInventory,
        _certificate: DrainCertificate,
    ) -> DrainFuture<'a, Result<CommittedDrainPoint, FastDrainError>> {
        Box::pin(async { Err(FastDrainError::InvalidConfiguration) })
    }

    fn assert_signing_body(
        &self,
        purpose: DrainSigningPurpose,
        key: &DrainObjectKey,
        digest: B256,
        point: CommittedDrainPoint,
    ) -> Result<(), FastDrainError> {
        if purpose != DrainSigningPurpose::Checkpoint
            || key
                != &(DrainObjectKey::Checkpoint {
                    old_epoch: self.image.statement.old_epoch,
                    next_epoch: self.image.statement.next_epoch,
                })
            || digest != self.image.statement.registry_digest(self.image.l1_chain_id)
            || point != self.point
        {
            return Err(FastDrainError::CheckpointInstallationMismatch);
        }
        Ok(())
    }

    fn final_accepted_prefix(
        &self,
    ) -> Result<(FinalAcceptedPrefix, CommittedDrainPoint), FastDrainError> {
        Err(FastDrainError::InvalidConfiguration)
    }

    fn assert_exposure_retirement_complete(&self, _epoch: u64) -> Result<(), FastDrainError> {
        Err(FastDrainError::InvalidConfiguration)
    }

    fn assert_source_dispositions_complete(&self, _epoch: u64) -> Result<(), FastDrainError> {
        Err(FastDrainError::InvalidConfiguration)
    }

    fn checkpoint_image(
        &self,
        settlement_hash: B256,
        next: &EpochRoster,
    ) -> Result<CheckpointImage, FastDrainError> {
        if settlement_hash != self.image.statement.final_settlement_hash
            || next.domain.authority_epoch != self.image.statement.next_epoch
            || next.domain.roster_hash != self.image.statement.next_roster_hash
        {
            return Err(FastDrainError::CheckpointInstallationMismatch);
        }
        Ok(self.image.clone())
    }

    fn install_checkpoint<'a>(
        &'a self,
        image: &'a CheckpointImage,
    ) -> DrainFuture<'a, Result<CommittedDrainPoint, FastDrainError>> {
        Box::pin(async move {
            if image.image_hash()? != self.image_hash {
                return Err(FastDrainError::CheckpointInstallationMismatch);
            }
            Ok(self.point)
        })
    }

    fn installed_checkpoint_hash(&self) -> Result<B256, FastDrainError> {
        Ok(self.image_hash)
    }
}

impl crate::fast_service_adapters::FastNextRosterCheckpointHandler
    for AuthenticatedNextRosterCheckpointSigner
{
    fn receive_authenticated<'a>(
        &'a self,
        session: &'a AuthenticatedPeerSession,
        stream: u64,
        sequence: u64,
        payload: &'a [u8],
    ) -> crate::fast_service::ServiceFuture<
        'a,
        Result<Vec<u8>, crate::fast_service::FastServiceError>,
    > {
        Box::pin(async move {
            if session.local_roster() != &self.signing_roster
                || session.remote_roster() != &self.requester_roster
                || !session.authenticates_delivery(stream, sequence)
            {
                return Err(crate::fast_service::FastServiceError::UnauthenticatedPeer);
            }
            if payload.get(9) == Some(&HANDOFF_IMAGE_CHUNK) {
                let (image_hash, _, _, index, _) =
                    decode_checkpoint_image_chunk(payload).map_err(|error| {
                        crate::fast_service::FastServiceError::Transport(error.to_string())
                    })?;
                if stream != checkpoint_handoff_stream(image_hash) || sequence != u64::from(index) {
                    return Err(crate::fast_service::FastServiceError::UnauthenticatedPeer);
                }
                self.receive_checkpoint_chunk(payload).map_err(|error| {
                    crate::fast_service::FastServiceError::Transport(error.to_string())
                })?;
                return Ok(Vec::new());
            }
            let (key, digest, point) =
                decode_checkpoint_handoff_request(payload).map_err(|error| {
                    crate::fast_service::FastServiceError::Transport(error.to_string())
                })?;
            if stream != checkpoint_handoff_stream(digest) || sequence != point.log_index {
                return Err(crate::fast_service::FastServiceError::UnauthenticatedPeer);
            }
            let committed = self
                .committed
                .read()
                .map_err(|_| crate::fast_service::FastServiceError::Poisoned)?
                .clone()
                .ok_or_else(|| {
                    crate::fast_service::FastServiceError::Transport(
                        "checkpoint image is not installed".to_owned(),
                    )
                })?;
            let signature = sign_authenticated_drain_phase_handoff(
                session.remote_member(),
                &self.requester_roster,
                &self.signing_roster,
                &self.signer,
                &self.signing_journal,
                committed.as_ref(),
                DrainSigningPurpose::Checkpoint,
                &key,
                digest,
                point,
            )
            .map_err(|error| crate::fast_service::FastServiceError::Transport(error.to_string()))?;
            if let Some(config) = self.install_config.as_ref() {
                authorize_successor_transition(config, committed.as_ref(), digest, point).map_err(
                    |error| crate::fast_service::FastServiceError::Transport(error.to_string()),
                )?;
            }
            Ok(encode_checkpoint_handoff_response(
                digest,
                self.signer.address(),
                signature,
            ))
        })
    }
}

impl FastDrainCommittee for DurableLocalDrainCommittee {
    fn install_next_checkpoint<'a>(
        &'a self,
        member: Address,
        image: &'a CheckpointImage,
    ) -> DrainFuture<'a, Result<(), FastDrainError>> {
        self.requester.install_checkpoint(member, image)
    }

    fn sign_local<'a>(
        &'a self,
        _purpose: DrainSigningPurpose,
        _key: &'a DrainObjectKey,
        digest: B256,
        point: CommittedDrainPoint,
    ) -> DrainFuture<'a, Result<SignatureBytes, FastDrainError>> {
        Box::pin(async move { self.sign_and_persist(digest, point) })
    }

    fn request_signature<'a>(
        &'a self,
        member: Address,
        purpose: DrainSigningPurpose,
        key: &'a DrainObjectKey,
        digest: B256,
        point: CommittedDrainPoint,
    ) -> DrainFuture<'a, Result<SignatureBytes, FastDrainError>> {
        self.requester.request(member, purpose, key, digest, point)
    }
}

/// Wallet-backed Tempo provider using the exact signed T14 ZoneFactory ABI. Every transaction
/// reuses the nonce and calldata already fsynced by [`FileFastDrainJournal`].
pub struct AlloyTempoDrainRegistry {
    provider: DynProvider<TempoNetwork>,
    portal: Address,
    factory: Address,
    signer: Address,
    fee_token: Address,
    l1_chain_id: u64,
}

impl AlloyTempoDrainRegistry {
    pub async fn new(
        provider: DynProvider<TempoNetwork>,
        portal: Address,
        factory: Address,
        signer: Address,
        fee_token: Address,
        l1_chain_id: u64,
    ) -> Result<Self, FastDrainError> {
        if portal.is_zero()
            || factory.is_zero()
            || signer.is_zero()
            || fee_token.is_zero()
            || l1_chain_id == 0
            || provider.get_chain_id().await.map_err(registry)? != l1_chain_id
        {
            return Err(FastDrainError::InvalidConfiguration);
        }
        Ok(Self {
            provider,
            portal,
            factory,
            signer,
            fee_token,
            l1_chain_id,
        })
    }

    async fn finalized_block(&self) -> Result<BlockId, FastDrainError> {
        let finalized = self
            .provider
            .get_header_by_number(BlockNumberOrTag::Finalized)
            .await
            .map_err(registry)?
            .ok_or(FastDrainError::RegistryObservationMismatch)?;
        Ok(BlockId::hash_canonical(finalized.hash_slow()))
    }

    async fn portal_words_at(
        &self,
        calldata: Vec<u8>,
        expected_words: usize,
        block: BlockId,
    ) -> Result<Vec<B256>, FastDrainError> {
        let output = CallBuilder::new_raw(&self.provider, Bytes::from(calldata))
            .to(self.portal)
            .block(block)
            .call()
            .await
            .map_err(registry)?;
        if output.len() != expected_words * 32 {
            return Err(FastDrainError::RegistryObservationMismatch);
        }
        Ok(output.chunks_exact(32).map(B256::from_slice).collect())
    }

    async fn portal_words(
        &self,
        calldata: Vec<u8>,
        expected_words: usize,
    ) -> Result<Vec<B256>, FastDrainError> {
        let block = self.finalized_block().await?;
        self.portal_words_at(calldata, expected_words, block).await
    }

    async fn epoch_config_words(&self, epoch: u64) -> Result<Vec<B256>, FastDrainError> {
        let block = self.finalized_block().await?;
        let historical = zone_sequencer::attestation::read_historical_fast_epoch_config(
            &self.provider,
            self.portal,
            epoch,
            block.clone(),
        )
        .await
        .map_err(registry)?;
        let words = self
            .portal_words_at(
                factory_abi::IT14ZonePortalRead::fastEpochConfigCall { epoch }.abi_encode(),
                27,
                block,
            )
            .await?;
        if words[18] != historical.finalSettlementHash {
            return Err(FastDrainError::RegistryObservationMismatch);
        }
        Ok(words)
    }
}

impl FastDrainRegistry for AlloyTempoDrainRegistry {
    fn portal(&self) -> Address {
        self.portal
    }

    fn target(&self) -> Address {
        self.factory
    }

    fn encode_record_barrier(
        &self,
        statement: &FastBarrierStatement,
        certificate: DrainCertificate,
    ) -> Result<Vec<u8>, FastDrainError> {
        if certificate.digest != statement.registry_digest(self.l1_chain_id) {
            return Err(FastDrainError::ConflictingCertificate);
        }
        Ok(factory_abi::IT14ZoneFactory::recordFastPeerBarrierCall {
            portal: self.portal,
            statement: abi_barrier(statement),
            signatures: abi_signatures(certificate),
        }
        .abi_encode())
    }

    fn encode_finalize_barrier(
        &self,
        epoch: u64,
        peer: Address,
        resolution: &FastBarrierResolution,
        certificate: DrainCertificate,
    ) -> Result<Vec<u8>, FastDrainError> {
        if certificate.digest
            != resolution.registry_digest(self.l1_chain_id, self.portal, epoch, peer)
        {
            return Err(FastDrainError::ConflictingCertificate);
        }
        Ok(factory_abi::IT14ZoneFactory::finalizeFastPeerBarrierCall {
            portal: self.portal,
            epoch,
            peerPortal: peer,
            resolution: abi_resolution(resolution),
            signatures: abi_signatures(certificate),
        }
        .abi_encode())
    }

    fn encode_final_settlement(
        &self,
        epoch: u64,
        prefix: FinalAcceptedPrefix,
        certificate: DrainCertificate,
    ) -> Result<Vec<u8>, FastDrainError> {
        if certificate.digest.is_zero()
            || prefix.block_hash.is_zero()
            || prefix.state_root.is_zero()
            || prefix.imported_anchor_hash.is_zero()
        {
            return Err(FastDrainError::InvalidCommittedPoint);
        }
        Ok(
            factory_abi::IT14ZoneFactory::recordFastFinalSettlementCall {
                portal: self.portal,
                epoch,
                zoneHeight: U256::from(prefix.zone_height),
                blockHash: prefix.block_hash,
                withdrawalBatchIndex: prefix.withdrawal_batch_index,
                signatures: abi_signatures(certificate),
            }
            .abi_encode(),
        )
    }

    fn encode_install_checkpoint(
        &self,
        statement: &FastCheckpointStatement,
        next_members: [Address; 3],
        certificate: DrainCertificate,
    ) -> Result<Vec<u8>, FastDrainError> {
        if statement.portal != self.portal
            || certificate.digest != statement.registry_digest(self.l1_chain_id)
        {
            return Err(FastDrainError::ConflictingCertificate);
        }
        Ok(factory_abi::IT14ZoneFactory::installFastCheckpointCall {
            portal: self.portal,
            statement: abi_checkpoint(statement),
            nextMembers: next_members.to_vec(),
            signatures: abi_signatures(certificate),
        }
        .abi_encode())
    }

    fn encode_retire_epoch(&self, epoch: u64) -> Result<Vec<u8>, FastDrainError> {
        if epoch == 0 {
            return Err(FastDrainError::InvalidConfiguration);
        }
        Ok(factory_abi::IT14ZoneFactory::retireFastEpochCall {
            portal: self.portal,
            epoch,
        }
        .abi_encode())
    }

    fn submit_prepared<'a>(
        &'a self,
        action: &'a PreparedRegistryAction,
    ) -> DrainFuture<'a, Result<B256, FastDrainError>> {
        Box::pin(async move {
            if action.signer != self.signer
                || action.target != self.factory
                || action.calldata.is_empty()
            {
                return Err(FastDrainError::ConflictingRegistryAction);
            }
            let request = TempoTransactionRequest {
                inner: TransactionRequest::default()
                    .with_from(action.signer)
                    .with_to(action.target)
                    .with_nonce(action.nonce)
                    .with_input(Bytes::from(action.calldata.clone())),
                fee_token: Some(self.fee_token),
                ..Default::default()
            };
            let pending = self
                .provider
                .send_transaction(request)
                .await
                .map_err(registry)?;
            Ok(*pending.tx_hash())
        })
    }

    fn transaction_committed<'a>(
        &'a self,
        transaction_hash: B256,
    ) -> DrainFuture<'a, Result<bool, FastDrainError>> {
        Box::pin(async move {
            let Some(receipt) = self
                .provider
                .get_transaction_receipt(transaction_hash)
                .await
                .map_err(registry)?
            else {
                return Ok(false);
            };
            if !receipt.status() {
                return Err(FastDrainError::RegistryObservationMismatch);
            }
            let Some(block_hash) = receipt.block_hash() else {
                return Ok(false);
            };
            let Some(block_number) = receipt.block_number() else {
                return Ok(false);
            };
            let Some(finalized) = self
                .provider
                .get_header_by_number(BlockNumberOrTag::Finalized)
                .await
                .map_err(registry)?
            else {
                return Ok(false);
            };
            if finalized.number() < block_number {
                return Ok(false);
            }
            let Some(included) = self
                .provider
                .get_header_by_number(block_number.into())
                .await
                .map_err(registry)?
            else {
                return Ok(false);
            };
            Ok(included.hash_slow() == block_hash)
        })
    }

    fn barrier_recorded<'a>(
        &'a self,
        epoch: u64,
        peer: Address,
        barrier_hash: B256,
    ) -> DrainFuture<'a, Result<bool, FastDrainError>> {
        Box::pin(async move {
            let words = self
                .portal_words(
                    factory_abi::IT14ZonePortalRead::fastPeerBarrierCall { epoch, peer }
                        .abi_encode(),
                    21,
                )
                .await?;
            Ok(word_bool(words[0])? && words[14] == barrier_hash)
        })
    }

    fn barrier_finalized<'a>(
        &'a self,
        epoch: u64,
        peer: Address,
        resolution_hash: B256,
    ) -> DrainFuture<'a, Result<bool, FastDrainError>> {
        Box::pin(async move {
            let words = self
                .portal_words(
                    factory_abi::IT14ZonePortalRead::fastPeerBarrierCall { epoch, peer }
                        .abi_encode(),
                    21,
                )
                .await?;
            Ok(word_bool(words[0])? && word_bool(words[1])? && words[20] == resolution_hash)
        })
    }

    fn final_settlement_hash<'a>(
        &'a self,
        epoch: u64,
    ) -> DrainFuture<'a, Result<Option<B256>, FastDrainError>> {
        Box::pin(async move {
            let words = self.epoch_config_words(epoch).await?;
            Ok((!words[18].is_zero()).then_some(words[18]))
        })
    }

    fn checkpoint_hash<'a>(
        &'a self,
        epoch: u64,
    ) -> DrainFuture<'a, Result<Option<B256>, FastDrainError>> {
        Box::pin(async move {
            let words = self.epoch_config_words(epoch).await?;
            Ok((!words[26].is_zero()).then_some(words[26]))
        })
    }

    fn epoch_retired<'a>(&'a self, epoch: u64) -> DrainFuture<'a, Result<bool, FastDrainError>> {
        Box::pin(async move {
            let words = self.epoch_config_words(epoch).await?;
            word_bool(words[4])
        })
    }
}

fn word_bool(word: B256) -> Result<bool, FastDrainError> {
    if word.is_zero() {
        Ok(false)
    } else if word[..31].iter().all(|byte| *byte == 0) && word[31] == 1 {
        Ok(true)
    } else {
        Err(FastDrainError::RegistryObservationMismatch)
    }
}

fn abi_barrier(value: &FastBarrierStatement) -> factory_abi::FastBarrierStatement {
    factory_abi::FastBarrierStatement {
        destinationPortal: value.destination_portal,
        destinationEpoch: value.destination_epoch,
        closureHash: value.closure_hash,
        sourcePortal: value.source_portal,
        sourceEpoch: value.source_epoch,
        importedAnchorNumber: value.imported_anchor_number,
        importedAnchorHash: value.imported_anchor_hash,
        logTerm: value.log_term,
        logIndex: value.log_index,
        blockHeight: value.block_height,
        blockHash: value.block_hash,
        stateRoot: value.state_root,
        lockLogWatermark: value.lock_log_watermark,
        completeLockRoot: value.complete_lock_root,
        unresolvedRoot: value.unresolved_root,
        unresolvedCount: value.unresolved_count,
    }
}

fn abi_resolution(value: &FastBarrierResolution) -> factory_abi::FastBarrierResolution {
    factory_abi::FastBarrierResolution {
        barrierHash: value.barrier_hash,
        terminalRoot: value.terminal_root,
        dispositionRoot: value.disposition_root,
        resolvedCount: value.resolved_count,
        remainingUnresolvedRoot: value.remaining_unresolved_root,
        remainingUnresolvedCount: value.remaining_unresolved_count,
    }
}

fn abi_checkpoint(value: &FastCheckpointStatement) -> factory_abi::FastCheckpointStatement {
    factory_abi::FastCheckpointStatement {
        portal: value.portal,
        oldEpoch: value.old_epoch,
        nextEpoch: value.next_epoch,
        nextRosterHash: value.next_roster_hash,
        finalZoneHeight: value.final_zone_height,
        finalBlockHash: value.final_block_hash,
        finalWithdrawalBatchIndex: value.final_withdrawal_batch_index,
        finalSettlementHash: value.final_settlement_hash,
        checkpointLogTerm: value.checkpoint_log_term,
        checkpointLogIndex: value.checkpoint_log_index,
        checkpointHeight: value.checkpoint_height,
        checkpointBlockHash: value.checkpoint_block_hash,
        checkpointStateRoot: value.checkpoint_state_root,
    }
}

fn abi_signatures(certificate: DrainCertificate) -> Vec<Bytes> {
    certificate
        .signatures
        .into_iter()
        .map(|signature| Bytes::copy_from_slice(&signature.0))
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DrainCommonwareEndpoint {
    pub member: Address,
    pub identity: P2pPeerId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DrainCommonwareRoute {
    pub zone_id: u32,
    pub portal: Address,
    pub endpoints: [DrainCommonwareEndpoint; 3],
}

/// Chunked private drain transport over the actual dedicated Commonware inter-Zone request port.
/// Every chunk is fsynced before send and marked complete only after the authenticated remote ACK.
pub struct CommonwareFastDrainTransport {
    requests: mpsc::Sender<InterZoneServiceRequest>,
    routes: BTreeMap<Address, DrainCommonwareRoute>,
    journal: Arc<FileFastDrainJournal>,
    response_timeout: Duration,
}

impl CommonwareFastDrainTransport {
    pub fn new(
        requests: mpsc::Sender<InterZoneServiceRequest>,
        routes: Vec<DrainCommonwareRoute>,
        journal: Arc<FileFastDrainJournal>,
        response_timeout: Duration,
    ) -> Result<Self, FastDrainError> {
        if requests.max_capacity() == 0 || routes.len() != 9 || response_timeout.is_zero() {
            return Err(FastDrainError::InvalidConfiguration);
        }
        let mut indexed = BTreeMap::new();
        for route in routes {
            let members = route
                .endpoints
                .iter()
                .map(|endpoint| endpoint.member)
                .collect::<BTreeSet<_>>();
            let identities = route
                .endpoints
                .iter()
                .map(|endpoint| endpoint.identity.clone())
                .collect::<BTreeSet<_>>();
            if route.zone_id == 0
                || route.portal.is_zero()
                || members.len() != 3
                || identities.len() != 3
                || indexed.insert(route.portal, route).is_some()
            {
                return Err(FastDrainError::InvalidConfiguration);
            }
        }
        Ok(Self {
            requests,
            routes: indexed,
            journal,
            response_timeout,
        })
    }

    async fn send_object(
        &self,
        peer: &DrainPeer,
        stream_class: u8,
        encoded: Vec<u8>,
        certificate: DrainCertificate,
    ) -> Result<(), FastDrainError> {
        let route = self
            .routes
            .get(&peer.roster.domain.portal)
            .ok_or(FastDrainError::InvalidConfiguration)?;
        let route_members = route
            .endpoints
            .iter()
            .map(|endpoint| endpoint.member)
            .collect::<BTreeSet<_>>();
        if route.zone_id != peer.zone_id
            || route.portal != peer.roster.domain.portal
            || route_members != peer.roster.members.into_iter().collect()
            || encoded.is_empty()
            || encoded.len() > MAX_DRAIN_OBJECT_BYTES
        {
            return Err(FastDrainError::InvalidConfiguration);
        }
        let full_hash = keccak256(&encoded);
        let chunk_count = encoded.len().div_ceil(DRAIN_CHUNK_BYTES);
        let chunk_count =
            u32::try_from(chunk_count).map_err(|_| FastDrainError::InvalidConfiguration)?;
        let stream = drain_stream(stream_class, certificate.digest);
        for (index, payload) in encoded.chunks(DRAIN_CHUNK_BYTES).enumerate() {
            let index = u32::try_from(index).expect("bounded drain chunk count");
            let frame = encode_drain_chunk(
                stream_class,
                certificate,
                full_hash,
                encoded.len() as u32,
                chunk_count,
                index,
                payload,
            )?;
            let key = DrainChunkKey {
                peer: route.portal,
                stream_class,
                object: certificate.digest,
                index,
            };
            if self
                .journal
                .persist_outbound_chunk(key.clone(), keccak256(&frame))?
            {
                continue;
            }
            let mut acknowledged = false;
            for endpoint in &route.endpoints {
                let (response, receiver) = oneshot::channel();
                self.requests
                    .send(InterZoneServiceRequest {
                        target: endpoint.identity.clone(),
                        remote_zone_id: route.zone_id,
                        stream,
                        sequence: u64::from(index),
                        payload: frame.clone(),
                        response,
                    })
                    .await
                    .map_err(|_| {
                        FastDrainError::Transport("Commonware drain port closed".to_owned())
                    })?;
                if let Ok(Ok(Ok(ack))) = tokio::time::timeout(self.response_timeout, receiver).await
                    && ack.authenticated_peer == endpoint.identity
                    && ack.remote_member == endpoint.member
                    && ack.remote_domain == peer.roster.domain
                    && ack.stream == stream
                    && ack.sequence == u64::from(index)
                    && ack.response_payload.is_empty()
                {
                    acknowledged = true;
                    break;
                }
            }
            if !acknowledged {
                return Err(FastDrainError::Transport(
                    "no finalized Commonware peer durably acknowledged drain chunk".to_owned(),
                ));
            }
            self.journal.acknowledge_outbound_chunk(key)?;
        }
        Ok(())
    }
}

impl FastDrainTransport for CommonwareFastDrainTransport {
    fn send_barrier<'a>(
        &'a self,
        peer: &'a DrainPeer,
        inventory: &'a BarrierInventory,
        certificate: DrainCertificate,
    ) -> DrainFuture<'a, Result<(), FastDrainError>> {
        Box::pin(async move {
            self.send_object(
                peer,
                DRAIN_BARRIER_CLASS,
                inventory.durable_bytes()?,
                certificate,
            )
            .await
        })
    }

    fn send_resolution<'a>(
        &'a self,
        peer: &'a DrainPeer,
        inventory: &'a BarrierResolutionInventory,
        certificate: DrainCertificate,
    ) -> DrainFuture<'a, Result<(), FastDrainError>> {
        Box::pin(async move {
            self.send_object(
                peer,
                DRAIN_RESOLUTION_CLASS,
                inventory.durable_bytes()?,
                certificate,
            )
            .await
        })
    }
}

fn drain_stream(stream_class: u8, object: B256) -> u64 {
    let mut prefix = [0u8; 8];
    prefix.copy_from_slice(&object[..8]);
    DRAIN_STREAM_NAMESPACE
        | (u64::from(stream_class) << 48)
        | (u64::from_be_bytes(prefix) & 0x0000_ffff_ffff_ffff)
}

fn encode_drain_chunk(
    stream_class: u8,
    certificate: DrainCertificate,
    full_hash: B256,
    total_bytes: u32,
    chunk_count: u32,
    index: u32,
    payload: &[u8],
) -> Result<Vec<u8>, FastDrainError> {
    let expected_count = usize::try_from(total_bytes)
        .ok()
        .filter(|length| *length > 0 && *length <= MAX_DRAIN_OBJECT_BYTES)
        .map(|length| length.div_ceil(DRAIN_CHUNK_BYTES))
        .and_then(|count| u32::try_from(count).ok());
    if !matches!(stream_class, DRAIN_BARRIER_CLASS | DRAIN_RESOLUTION_CLASS)
        || payload.is_empty()
        || payload.len() > DRAIN_CHUNK_BYTES
        || chunk_count == 0
        || expected_count != Some(chunk_count)
        || index >= chunk_count
        || (index + 1 < chunk_count && payload.len() != DRAIN_CHUNK_BYTES)
        || (index + 1 == chunk_count
            && payload.len()
                != total_bytes as usize - DRAIN_CHUNK_BYTES * (chunk_count as usize - 1))
    {
        return Err(FastDrainError::InvalidConfiguration);
    }
    let mut frame = Vec::with_capacity(8 + 1 + 1 + 32 * 2 + 65 * 2 + 4 * 4 + payload.len());
    frame.extend_from_slice(DRAIN_WIRE_MAGIC);
    frame.push(DRAIN_WIRE_VERSION);
    frame.push(stream_class);
    put_certificate(&mut frame, certificate);
    frame.extend_from_slice(full_hash.as_slice());
    frame.extend_from_slice(&total_bytes.to_be_bytes());
    frame.extend_from_slice(&chunk_count.to_be_bytes());
    frame.extend_from_slice(&index.to_be_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    if frame.len() > MAX_INTER_ZONE_MESSAGE_SIZE as usize {
        return Err(FastDrainError::InvalidConfiguration);
    }
    Ok(frame)
}

/// Exact multiplex discriminator for C5 frames. C4 service envelopes never share this prefix, so
/// `(version,class)` pairs such as `(1,1)` and `(1,2)` cannot be misrouted by header coincidence.
pub fn is_fast_drain_payload(payload: &[u8]) -> bool {
    payload.starts_with(DRAIN_WIRE_MAGIC)
}

fn decode_drain_chunk(
    stream: u64,
    sequence: u64,
    frame: &[u8],
) -> Result<(DrainChunkKey, InboundDrainChunk), FastDrainError> {
    if !is_fast_drain_payload(frame) || frame.len() > MAX_INTER_ZONE_MESSAGE_SIZE as usize {
        return Err(FastDrainError::Transport("not a C5 drain frame".to_owned()));
    }
    let mut reader = Reader::new(&frame[DRAIN_WIRE_MAGIC.len()..]);
    if reader.u8()? != DRAIN_WIRE_VERSION {
        return Err(FastDrainError::Transport(
            "unsupported C5 drain wire version".to_owned(),
        ));
    }
    let stream_class = reader.u8()?;
    if !matches!(stream_class, DRAIN_BARRIER_CLASS | DRAIN_RESOLUTION_CLASS) {
        return Err(FastDrainError::Transport(
            "unsupported C5 drain object class".to_owned(),
        ));
    }
    let certificate = reader.certificate()?;
    let full_hash = reader.b256()?;
    let total_bytes = reader.u32()?;
    let chunk_count = reader.u32()?;
    let index = reader.u32()?;
    let payload = reader.bytes()?.to_vec();
    reader.finish()?;
    let expected_count = usize::try_from(total_bytes)
        .ok()
        .filter(|length| *length > 0 && *length <= MAX_DRAIN_OBJECT_BYTES)
        .map(|length| length.div_ceil(DRAIN_CHUNK_BYTES))
        .and_then(|count| u32::try_from(count).ok())
        .ok_or(FastDrainError::InvalidConfiguration)?;
    if certificate.digest.is_zero()
        || chunk_count != expected_count
        || index >= chunk_count
        || sequence != u64::from(index)
        || stream != drain_stream(stream_class, certificate.digest)
        || payload.is_empty()
        || payload.len() > DRAIN_CHUNK_BYTES
        || (index + 1 < chunk_count && payload.len() != DRAIN_CHUNK_BYTES)
        || (index + 1 == chunk_count
            && payload.len()
                != total_bytes as usize - DRAIN_CHUNK_BYTES * (chunk_count as usize - 1))
    {
        return Err(FastDrainError::InvalidConfiguration);
    }
    Ok((
        DrainChunkKey {
            peer: Address::ZERO,
            stream_class,
            object: certificate.digest,
            index,
        },
        InboundDrainChunk {
            certificate,
            full_hash,
            total_bytes,
            chunk_count,
            payload,
        },
    ))
}

/// C5 consumer called by the single C4/C5 inter-Zone multiplex dispatcher. It fsyncs each chunk;
/// the final chunk is acknowledged only after full reassembly, canonical decode, certificate/root
/// verification, committed import/native resolution, and registry observation all succeed.
pub struct FastDrainIncoming {
    config: FastDrainConfig,
    service: Arc<FastDrainService>,
    journal: Arc<FileFastDrainJournal>,
}

impl FastDrainIncoming {
    pub fn new(
        config: FastDrainConfig,
        service: Arc<FastDrainService>,
        journal: Arc<FileFastDrainJournal>,
    ) -> Result<Self, FastDrainError> {
        config.validate()?;
        Ok(Self {
            config,
            service,
            journal,
        })
    }

    pub async fn receive_authenticated(
        &self,
        session: &AuthenticatedPeerSession,
        stream: u64,
        sequence: u64,
        payload: &[u8],
    ) -> Result<(), FastDrainError> {
        if !session.authenticates_delivery(stream, sequence)
            || session.local_roster() != &self.config.local_roster
        {
            return Err(FastDrainError::Transport(
                "C5 authenticated delivery transcript mismatch".to_owned(),
            ));
        }
        let peer = self
            .config
            .peers
            .iter()
            .find(|peer| &peer.roster == session.remote_roster())
            .ok_or_else(|| FastDrainError::Transport("unknown C5 source roster".to_owned()))?;
        if !peer.roster.members.contains(&session.remote_member()) {
            return Err(FastDrainError::Transport(
                "C5 source member is outside finalized roster".to_owned(),
            ));
        }
        let (mut key, chunk) = decode_drain_chunk(stream, sequence, payload)?;
        key.peer = peer.roster.domain.portal;
        self.journal.persist_inbound_chunk(key.clone(), chunk)?;
        let Some((encoded, certificate)) =
            self.journal
                .assembled_inbound(key.peer, key.stream_class, key.object)?
        else {
            return Ok(());
        };
        if certificate.digest != key.object {
            return Err(FastDrainError::ConflictingCertificate);
        }
        match key.stream_class {
            DRAIN_BARRIER_CLASS => {
                let inventory = BarrierInventory::decode_durable(&encoded)?;
                let closure_hash = self
                    .journal
                    .closure(self.config.local_roster.domain.authority_epoch)?
                    .ok_or(FastDrainError::MissingClosure)?
                    .0;
                self.service
                    .receive_barrier(peer, inventory, certificate, closure_hash)
                    .await
            }
            DRAIN_RESOLUTION_CLASS => {
                let resolution = BarrierResolutionInventory::decode_durable(&encoded)?;
                self.service
                    .receive_resolution(peer, resolution, certificate)
                    .await
            }
            _ => Err(FastDrainError::InvalidConfiguration),
        }
    }
}

fn insert_exact<K: Ord, V: Eq>(
    map: &mut BTreeMap<K, V>,
    key: K,
    value: V,
) -> Result<(), FastDrainError> {
    match map.get(&key) {
        Some(existing) if existing == &value => Ok(()),
        Some(_) => Err(FastDrainError::ConflictingJournalObject),
        None => {
            map.insert(key, value);
            Ok(())
        }
    }
}

fn insert_inbound_chunk(
    chunks: &mut BTreeMap<DrainChunkKey, InboundDrainChunk>,
    key: DrainChunkKey,
    chunk: InboundDrainChunk,
) -> Result<(), FastDrainError> {
    if chunks.keys().any(|existing| {
        existing.peer == key.peer
            && existing.stream_class == key.stream_class
            && existing.object != key.object
    }) {
        return Err(FastDrainError::ConflictingJournalObject);
    }
    let objects = chunks
        .keys()
        .map(|existing| (existing.peer, existing.stream_class, existing.object))
        .collect::<BTreeSet<_>>();
    if objects.len() >= MAX_INBOUND_DRAIN_OBJECTS
        && !objects.contains(&(key.peer, key.stream_class, key.object))
    {
        return Err(FastDrainError::InvalidConfiguration);
    }
    insert_exact(chunks, key, chunk)
}

fn action_by_id_mut(
    state: &mut DrainJournalState,
    action_id: B256,
) -> Result<&mut RegistryActionState, FastDrainError> {
    state
        .actions
        .values_mut()
        .find(|record| record.action.action_id == action_id)
        .ok_or(FastDrainError::ConflictingRegistryAction)
}

#[derive(Clone, Debug)]
enum JournalOperation {
    Closure(u64, B256, CommittedDrainPoint),
    Barrier(DrainObjectKey, BarrierInventory),
    Resolution(DrainObjectKey, BarrierResolutionInventory),
    Certificate(DrainSigningPurpose, DrainObjectKey, DrainCertificate),
    LockResolved(DrainObjectKey, B256),
    Action(DrainObjectKey, PreparedRegistryAction),
    Submission(B256, B256),
    ActionCompleted(B256, B256),
    Checkpoint(DrainObjectKey, CheckpointImage),
    OutboundChunk(DrainChunkKey, B256),
    OutboundChunkAck(DrainChunkKey),
    InboundChunk(DrainChunkKey, InboundDrainChunk),
}

fn snapshot_operations(state: &DrainJournalState) -> Result<Vec<JournalOperation>, FastDrainError> {
    let mut operations = Vec::new();
    if let Some((epoch, hash, point)) = state.closure {
        operations.push(JournalOperation::Closure(epoch, hash, point));
    }
    operations.extend(
        state
            .barriers
            .iter()
            .map(|(key, value)| JournalOperation::Barrier(key.clone(), value.clone())),
    );
    operations.extend(
        state
            .resolutions
            .iter()
            .map(|(key, value)| JournalOperation::Resolution(key.clone(), value.clone())),
    );
    for ((purpose, key), certificate) in &state.certificates {
        operations.push(JournalOperation::Certificate(
            purpose_from_tag(*purpose)?,
            key.clone(),
            *certificate,
        ));
    }
    operations.extend(
        state
            .resolved
            .iter()
            .map(|(key, id)| JournalOperation::LockResolved(key.clone(), *id)),
    );
    for (key, record) in &state.actions {
        let mut initial = record.action.clone();
        initial.submission_hashes.clear();
        operations.push(JournalOperation::Action(key.clone(), initial));
        operations.extend(
            record
                .action
                .submission_hashes
                .iter()
                .map(|hash| JournalOperation::Submission(record.action.action_id, *hash)),
        );
        if let Some(hash) = record.completed {
            operations.push(JournalOperation::ActionCompleted(
                record.action.action_id,
                hash,
            ));
        }
    }
    operations.extend(
        state
            .checkpoints
            .iter()
            .map(|(key, value)| JournalOperation::Checkpoint(key.clone(), value.clone())),
    );
    for (key, record) in &state.outbound_chunks {
        operations.push(JournalOperation::OutboundChunk(
            key.clone(),
            record.payload_hash,
        ));
        if record.acknowledged {
            operations.push(JournalOperation::OutboundChunkAck(key.clone()));
        }
    }
    operations.extend(
        state
            .inbound_chunks
            .iter()
            .map(|(key, chunk)| JournalOperation::InboundChunk(key.clone(), chunk.clone())),
    );
    Ok(operations)
}

fn write_frame(file: &mut File, operation: &JournalOperation) -> Result<(), FastDrainError> {
    let body = encode_operation(operation)?;
    if body.len() > MAX_DRAIN_OBJECT_BYTES {
        return Err(storage("drain journal record exceeds bound"));
    }
    file.write_all(JOURNAL_MAGIC).map_err(storage)?;
    file.write_all(&(body.len() as u32).to_be_bytes())
        .map_err(storage)?;
    file.write_all(&body).map_err(storage)?;
    file.write_all(keccak256(&body).as_slice()).map_err(storage)
}

fn replay_frames(
    bytes: &[u8],
    state: &mut DrainJournalState,
    allow_partial_tail: bool,
) -> Result<usize, FastDrainError> {
    let mut offset = 0usize;
    while offset < bytes.len() {
        let start = offset;
        if bytes.len() - offset < 8 {
            return if allow_partial_tail {
                Ok(start)
            } else {
                Err(storage("partial drain frame header"))
            };
        }
        if &bytes[offset..offset + 4] != JOURNAL_MAGIC {
            return Err(storage("invalid drain frame magic"));
        }
        let length =
            u32::from_be_bytes(bytes[offset + 4..offset + 8].try_into().expect("fixed")) as usize;
        if length > MAX_DRAIN_OBJECT_BYTES {
            return Err(storage("oversized drain frame"));
        }
        let end = offset
            .checked_add(8 + length + 32)
            .ok_or_else(|| storage("drain frame overflow"))?;
        if end > bytes.len() {
            return if allow_partial_tail {
                Ok(start)
            } else {
                Err(storage("partial drain frame"))
            };
        }
        let body = &bytes[offset + 8..offset + 8 + length];
        if keccak256(body).as_slice() != &bytes[offset + 8 + length..end] {
            return Err(storage("drain frame checksum mismatch"));
        }
        apply_recovered(state, decode_operation(body)?)?;
        offset = end;
    }
    Ok(offset)
}

fn apply_recovered(
    state: &mut DrainJournalState,
    operation: JournalOperation,
) -> Result<(), FastDrainError> {
    match operation {
        JournalOperation::Closure(epoch, hash, point) => match state.closure {
            Some(existing) if existing != (epoch, hash, point) => {
                Err(FastDrainError::ConflictingClosure)
            }
            _ => {
                state.closure = Some((epoch, hash, point));
                Ok(())
            }
        },
        JournalOperation::Barrier(key, value) => insert_exact(&mut state.barriers, key, value),
        JournalOperation::Resolution(key, value) => {
            insert_exact(&mut state.resolutions, key, value)
        }
        JournalOperation::Certificate(purpose, key, value) => {
            insert_exact(&mut state.certificates, (purpose_tag(purpose), key), value)
        }
        JournalOperation::LockResolved(key, id) => {
            state.resolved.insert((key, id));
            Ok(())
        }
        JournalOperation::Action(key, action) => insert_exact(
            &mut state.actions,
            key,
            RegistryActionState {
                action,
                completed: None,
            },
        ),
        JournalOperation::Submission(id, hash) => {
            let action = action_by_id_mut(state, id)?;
            if !action.action.submission_hashes.contains(&hash) {
                if action.action.submission_hashes.len() >= MAX_SUBMISSION_HASHES {
                    return Err(FastDrainError::ConflictingRegistryAction);
                }
                action.action.submission_hashes.push(hash);
            }
            Ok(())
        }
        JournalOperation::ActionCompleted(id, hash) => {
            let action = action_by_id_mut(state, id)?;
            if !action.action.submission_hashes.contains(&hash)
                || action.completed.is_some_and(|existing| existing != hash)
            {
                return Err(FastDrainError::ConflictingRegistryAction);
            }
            action.completed = Some(hash);
            Ok(())
        }
        JournalOperation::Checkpoint(key, image) => {
            insert_exact(&mut state.checkpoints, key, image)
        }
        JournalOperation::OutboundChunk(key, payload_hash) => insert_exact(
            &mut state.outbound_chunks,
            key,
            DrainChunkRecord {
                payload_hash,
                acknowledged: false,
            },
        ),
        JournalOperation::OutboundChunkAck(key) => {
            let record = state
                .outbound_chunks
                .get_mut(&key)
                .ok_or(FastDrainError::ConflictingJournalObject)?;
            record.acknowledged = true;
            Ok(())
        }
        JournalOperation::InboundChunk(key, chunk) => {
            insert_inbound_chunk(&mut state.inbound_chunks, key, chunk)
        }
    }
}

fn encode_operation(operation: &JournalOperation) -> Result<Vec<u8>, FastDrainError> {
    let mut out = Vec::new();
    match operation {
        JournalOperation::Closure(epoch, hash, point) => {
            out.push(1);
            out.extend_from_slice(&epoch.to_be_bytes());
            out.extend_from_slice(hash.as_slice());
            put_point(&mut out, *point);
        }
        JournalOperation::Barrier(key, inventory) => {
            out.push(2);
            put_key(&mut out, key);
            put_bytes(&mut out, &inventory.durable_bytes()?)?;
        }
        JournalOperation::Resolution(key, resolution) => {
            out.push(3);
            put_key(&mut out, key);
            put_bytes(&mut out, &resolution.durable_bytes()?)?;
        }
        JournalOperation::Certificate(purpose, key, certificate) => {
            out.push(4);
            out.push(purpose_tag(*purpose));
            put_key(&mut out, key);
            put_certificate(&mut out, *certificate);
        }
        JournalOperation::LockResolved(key, id) => {
            out.push(5);
            put_key(&mut out, key);
            out.extend_from_slice(id.as_slice());
        }
        JournalOperation::Action(key, action) => {
            out.push(6);
            put_key(&mut out, key);
            put_action(&mut out, action)?;
        }
        JournalOperation::Submission(id, hash) => {
            out.push(7);
            out.extend_from_slice(id.as_slice());
            out.extend_from_slice(hash.as_slice());
        }
        JournalOperation::ActionCompleted(id, hash) => {
            out.push(8);
            out.extend_from_slice(id.as_slice());
            out.extend_from_slice(hash.as_slice());
        }
        JournalOperation::Checkpoint(key, image) => {
            out.push(9);
            put_key(&mut out, key);
            put_bytes(&mut out, &image.durable_bytes()?)?;
        }
        JournalOperation::OutboundChunk(key, payload_hash) => {
            out.push(10);
            put_chunk_key(&mut out, key);
            out.extend_from_slice(payload_hash.as_slice());
        }
        JournalOperation::OutboundChunkAck(key) => {
            out.push(11);
            put_chunk_key(&mut out, key);
        }
        JournalOperation::InboundChunk(key, chunk) => {
            out.push(12);
            put_chunk_key(&mut out, key);
            put_certificate(&mut out, chunk.certificate);
            out.extend_from_slice(chunk.full_hash.as_slice());
            out.extend_from_slice(&chunk.total_bytes.to_be_bytes());
            out.extend_from_slice(&chunk.chunk_count.to_be_bytes());
            put_bytes(&mut out, &chunk.payload)?;
        }
    }
    Ok(out)
}

fn decode_operation(bytes: &[u8]) -> Result<JournalOperation, FastDrainError> {
    let mut reader = Reader::new(bytes);
    let operation = match reader.u8()? {
        1 => JournalOperation::Closure(reader.u64()?, reader.b256()?, reader.point()?),
        2 => JournalOperation::Barrier(
            reader.key()?,
            BarrierInventory::decode_durable(reader.bytes()?)?,
        ),
        3 => JournalOperation::Resolution(
            reader.key()?,
            BarrierResolutionInventory::decode_durable(reader.bytes()?)?,
        ),
        4 => JournalOperation::Certificate(
            purpose_from_tag(reader.u8()?)?,
            reader.key()?,
            reader.certificate()?,
        ),
        5 => JournalOperation::LockResolved(reader.key()?, reader.b256()?),
        6 => JournalOperation::Action(reader.key()?, reader.action()?),
        7 => JournalOperation::Submission(reader.b256()?, reader.b256()?),
        8 => JournalOperation::ActionCompleted(reader.b256()?, reader.b256()?),
        9 => JournalOperation::Checkpoint(
            reader.key()?,
            CheckpointImage::decode_durable(reader.bytes()?)?,
        ),
        10 => JournalOperation::OutboundChunk(reader.chunk_key()?, reader.b256()?),
        11 => JournalOperation::OutboundChunkAck(reader.chunk_key()?),
        12 => JournalOperation::InboundChunk(
            reader.chunk_key()?,
            InboundDrainChunk {
                certificate: reader.certificate()?,
                full_hash: reader.b256()?,
                total_bytes: reader.u32()?,
                chunk_count: reader.u32()?,
                payload: reader.bytes()?.to_vec(),
            },
        ),
        _ => return Err(storage("unknown drain journal operation")),
    };
    reader.finish()?;
    Ok(operation)
}

fn put_point(out: &mut Vec<u8>, point: CommittedDrainPoint) {
    out.extend_from_slice(&point.log_term.to_be_bytes());
    out.extend_from_slice(&point.log_index.to_be_bytes());
    out.extend_from_slice(&point.block_height.to_be_bytes());
    out.extend_from_slice(point.block_hash.as_slice());
    out.extend_from_slice(point.state_root.as_slice());
    out.extend_from_slice(&point.imported_anchor_number.to_be_bytes());
    out.extend_from_slice(point.imported_anchor_hash.as_slice());
}

fn put_key(out: &mut Vec<u8>, key: &DrainObjectKey) {
    match key {
        DrainObjectKey::Closure { epoch } => {
            out.push(1);
            out.extend_from_slice(&epoch.to_be_bytes());
        }
        DrainObjectKey::OutboundBarrier { epoch, destination } => {
            out.push(2);
            out.extend_from_slice(&epoch.to_be_bytes());
            out.extend_from_slice(destination.as_slice());
        }
        DrainObjectKey::InboundBarrier { epoch, source } => {
            out.push(3);
            out.extend_from_slice(&epoch.to_be_bytes());
            out.extend_from_slice(source.as_slice());
        }
        DrainObjectKey::Resolution {
            destination_epoch,
            destination,
            source,
        } => {
            out.push(4);
            out.extend_from_slice(&destination_epoch.to_be_bytes());
            out.extend_from_slice(destination.as_slice());
            out.extend_from_slice(source.as_slice());
        }
        DrainObjectKey::FinalSettlement { epoch } => {
            out.push(5);
            out.extend_from_slice(&epoch.to_be_bytes());
        }
        DrainObjectKey::Checkpoint {
            old_epoch,
            next_epoch,
        } => {
            out.push(6);
            out.extend_from_slice(&old_epoch.to_be_bytes());
            out.extend_from_slice(&next_epoch.to_be_bytes());
        }
        DrainObjectKey::Retirement { epoch } => {
            out.push(7);
            out.extend_from_slice(&epoch.to_be_bytes());
        }
    }
}

fn put_certificate(out: &mut Vec<u8>, certificate: DrainCertificate) {
    out.extend_from_slice(certificate.digest.as_slice());
    out.extend_from_slice(&certificate.signatures[0].0);
    out.extend_from_slice(&certificate.signatures[1].0);
}

fn put_chunk_key(out: &mut Vec<u8>, key: &DrainChunkKey) {
    out.extend_from_slice(key.peer.as_slice());
    out.push(key.stream_class);
    out.extend_from_slice(key.object.as_slice());
    out.extend_from_slice(&key.index.to_be_bytes());
}

fn put_action(out: &mut Vec<u8>, action: &PreparedRegistryAction) -> Result<(), FastDrainError> {
    out.extend_from_slice(action.action_id.as_slice());
    out.extend_from_slice(action.signer.as_slice());
    out.extend_from_slice(&action.nonce.to_be_bytes());
    out.extend_from_slice(action.target.as_slice());
    put_bytes(out, &action.calldata)?;
    if action.submission_hashes.len() > MAX_SUBMISSION_HASHES {
        return Err(FastDrainError::ConflictingRegistryAction);
    }
    out.extend_from_slice(&(action.submission_hashes.len() as u32).to_be_bytes());
    for hash in &action.submission_hashes {
        out.extend_from_slice(hash.as_slice());
    }
    Ok(())
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), FastDrainError> {
    if bytes.len() > MAX_DRAIN_OBJECT_BYTES {
        return Err(storage("drain field exceeds bound"));
    }
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], FastDrainError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| storage("drain decode overflow"))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| storage("truncated drain operation"))?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, FastDrainError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, FastDrainError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().expect("fixed")))
    }

    fn u64(&mut self) -> Result<u64, FastDrainError> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().expect("fixed")))
    }

    fn b256(&mut self) -> Result<B256, FastDrainError> {
        Ok(B256::from_slice(self.take(32)?))
    }

    fn address(&mut self) -> Result<Address, FastDrainError> {
        Ok(Address::from_slice(self.take(20)?))
    }

    fn bytes(&mut self) -> Result<&'a [u8], FastDrainError> {
        let length = self.u32()? as usize;
        if length > MAX_DRAIN_OBJECT_BYTES {
            return Err(storage("oversized drain field"));
        }
        self.take(length)
    }

    fn point(&mut self) -> Result<CommittedDrainPoint, FastDrainError> {
        Ok(CommittedDrainPoint {
            log_term: self.u64()?,
            log_index: self.u64()?,
            block_height: self.u64()?,
            block_hash: self.b256()?,
            state_root: self.b256()?,
            imported_anchor_number: self.u64()?,
            imported_anchor_hash: self.b256()?,
        })
    }

    fn key(&mut self) -> Result<DrainObjectKey, FastDrainError> {
        match self.u8()? {
            1 => Ok(DrainObjectKey::Closure { epoch: self.u64()? }),
            2 => Ok(DrainObjectKey::OutboundBarrier {
                epoch: self.u64()?,
                destination: self.address()?,
            }),
            3 => Ok(DrainObjectKey::InboundBarrier {
                epoch: self.u64()?,
                source: self.address()?,
            }),
            4 => Ok(DrainObjectKey::Resolution {
                destination_epoch: self.u64()?,
                destination: self.address()?,
                source: self.address()?,
            }),
            5 => Ok(DrainObjectKey::FinalSettlement { epoch: self.u64()? }),
            6 => Ok(DrainObjectKey::Checkpoint {
                old_epoch: self.u64()?,
                next_epoch: self.u64()?,
            }),
            7 => Ok(DrainObjectKey::Retirement { epoch: self.u64()? }),
            _ => Err(storage("invalid drain object key")),
        }
    }

    fn certificate(&mut self) -> Result<DrainCertificate, FastDrainError> {
        Ok(DrainCertificate {
            digest: self.b256()?,
            signatures: [
                SignatureBytes(self.take(65)?.try_into().expect("fixed")),
                SignatureBytes(self.take(65)?.try_into().expect("fixed")),
            ],
        })
    }

    fn chunk_key(&mut self) -> Result<DrainChunkKey, FastDrainError> {
        Ok(DrainChunkKey {
            peer: self.address()?,
            stream_class: self.u8()?,
            object: self.b256()?,
            index: self.u32()?,
        })
    }

    fn action(&mut self) -> Result<PreparedRegistryAction, FastDrainError> {
        let action_id = self.b256()?;
        let signer = self.address()?;
        let nonce = self.u64()?;
        let target = self.address()?;
        let calldata = self.bytes()?.to_vec();
        let count = self.u32()? as usize;
        if count > MAX_SUBMISSION_HASHES {
            return Err(storage("too many registry submission hashes"));
        }
        let mut submission_hashes = Vec::with_capacity(count);
        for _ in 0..count {
            submission_hashes.push(self.b256()?);
        }
        Ok(PreparedRegistryAction {
            action_id,
            signer,
            nonce,
            target,
            calldata,
            submission_hashes,
        })
    }

    fn finish(self) -> Result<(), FastDrainError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(storage("drain operation has trailing bytes"))
        }
    }
}

fn action_id(
    key: &DrainObjectKey,
    signer: Address,
    nonce: u64,
    target: Address,
    calldata: &[u8],
) -> Result<B256, FastDrainError> {
    let mut encoded = Vec::new();
    encoded.extend_from_slice(ACTION_DOMAIN);
    put_key(&mut encoded, key);
    encoded.extend_from_slice(signer.as_slice());
    encoded.extend_from_slice(&nonce.to_be_bytes());
    encoded.extend_from_slice(target.as_slice());
    put_bytes(&mut encoded, calldata)?;
    Ok(keccak256(encoded))
}

const fn purpose_tag(purpose: DrainSigningPurpose) -> u8 {
    match purpose {
        DrainSigningPurpose::Barrier => 1,
        DrainSigningPurpose::Resolution => 2,
        DrainSigningPurpose::FinalSettlement => 3,
        DrainSigningPurpose::Checkpoint => 4,
    }
}

fn purpose_from_tag(tag: u8) -> Result<DrainSigningPurpose, FastDrainError> {
    match tag {
        1 => Ok(DrainSigningPurpose::Barrier),
        2 => Ok(DrainSigningPurpose::Resolution),
        3 => Ok(DrainSigningPurpose::FinalSettlement),
        4 => Ok(DrainSigningPurpose::Checkpoint),
        _ => Err(storage("invalid drain signing purpose")),
    }
}

fn sync_directory(path: &Path) -> Result<(), FastDrainError> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(storage)
}

fn storage(error: impl ToString) -> FastDrainError {
    FastDrainError::Storage(error.to_string())
}

fn registry(error: impl ToString) -> FastDrainError {
    FastDrainError::Registry(error.to_string())
}

pub struct ProductionFastDrainConfig {
    pub drain: FastDrainConfig,
    pub journal_directory: PathBuf,
    pub factory: Address,
    pub fee_token: Address,
    pub l1_chain_id: u64,
    pub routes: Vec<DrainCommonwareRoute>,
    /// Explicit same-Zone endpoints for the finalized disjoint next roster. Required iff
    /// `drain.next_roster` is present; these identities must also be present in the 33-peer P2P
    /// routing config and installed through `NextRosterHandoffAuthoritySet`.
    pub next_roster_route: Option<DrainCommonwareRoute>,
    pub response_timeout: Duration,
}

pub struct AssembledFastDrain {
    pub service: Arc<FastDrainService>,
    pub journal: Arc<FileFastDrainJournal>,
    pub incoming: Arc<FastDrainIncoming>,
}

impl AssembledFastDrain {
    /// Start the process-lifetime recovery driver after runtime has installed the C4/C5 incoming
    /// multiplex callback and the canonical committed producer/native resources.
    pub fn spawn_recovery_driver(
        &self,
        retry_interval: Duration,
        closures: tokio::sync::watch::Receiver<DrainClosureObservation>,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<Result<(), FastDrainError>> {
        let service = self.service.clone();
        tokio::spawn(async move {
            service
                .run_recovery_driver(retry_interval, closures, shutdown)
                .await
        })
    }
}

/// Assemble all C5-owned production adapters from explicit runtime resources. Provider wallets,
/// signer keys, finalized rosters/routes, storage, Commonware ports and canonical state/native
/// handles are mandatory; the function has no development fallback or embedded key.
pub async fn assemble_production_fast_drain(
    config: ProductionFastDrainConfig,
    local_signer: PrivateKeySigner,
    tempo_provider: DynProvider<TempoNetwork>,
    commonware_requests: mpsc::Sender<InterZoneServiceRequest>,
    signing_journal: Arc<DurableJournal>,
    committed: Arc<dyn FastDrainCommittedState>,
    native: Arc<dyn FastDrainNative>,
    signature_requests: mpsc::Sender<DrainSignatureNetworkRequest>,
) -> Result<AssembledFastDrain, FastDrainError> {
    config.drain.validate()?;
    if local_signer.address() != config.drain.local_member
        || config.drain.local_roster.domain.l1_chain_id != config.l1_chain_id
        || config.routes.len() != config.drain.peers.len()
        || config.journal_directory.as_os_str().is_empty()
    {
        return Err(FastDrainError::InvalidConfiguration);
    }
    match (&config.drain.next_roster, &config.next_roster_route) {
        (Some(next), Some(route)) => {
            let members = route
                .endpoints
                .iter()
                .map(|endpoint| endpoint.member)
                .collect::<BTreeSet<_>>();
            let identities = route
                .endpoints
                .iter()
                .map(|endpoint| endpoint.identity.clone())
                .collect::<BTreeSet<_>>();
            if route.zone_id != next.domain.zone_id
                || route.portal != next.domain.portal
                || members != next.members.into_iter().collect()
                || identities.len() != 3
            {
                return Err(FastDrainError::InvalidConfiguration);
            }
        }
        (None, None) => {}
        _ => return Err(FastDrainError::InvalidConfiguration),
    }
    let configured_portals = config
        .routes
        .iter()
        .map(|route| route.portal)
        .collect::<BTreeSet<_>>();
    let roster_portals = config
        .drain
        .peers
        .iter()
        .map(|peer| peer.roster.domain.portal)
        .collect::<BTreeSet<_>>();
    if configured_portals != roster_portals {
        return Err(FastDrainError::InvalidConfiguration);
    }
    let first_nonce = tempo_provider
        .get_transaction_count(local_signer.address())
        .pending()
        .await
        .map_err(registry)?;
    let journal = Arc::new(FileFastDrainJournal::open(
        config.journal_directory,
        config.drain.local_roster.domain.authority_epoch,
        local_signer.address(),
        first_nonce,
    )?);
    let signature_requester = Arc::new(AuthenticatedDrainSignatureRequester::new(
        signature_requests,
        &config.drain.local_roster,
        config.drain.next_roster.as_ref(),
        config
            .next_roster_route
            .as_ref()
            .map(|route| route.endpoints.clone()),
        commonware_requests.clone(),
        config.response_timeout,
    )?);
    let committee = Arc::new(DurableLocalDrainCommittee::new(
        local_signer,
        signing_journal.clone(),
        signature_requester,
    )?);
    let transport = Arc::new(CommonwareFastDrainTransport::new(
        commonware_requests,
        config.routes,
        journal.clone(),
        config.response_timeout,
    )?);
    let registry = Arc::new(
        AlloyTempoDrainRegistry::new(
            tempo_provider,
            config.drain.local_roster.domain.portal,
            config.factory,
            config.drain.local_member,
            config.fee_token,
            config.l1_chain_id,
        )
        .await?,
    );
    let drain_config = config.drain;
    let service = Arc::new(FastDrainService::new(
        drain_config.clone(),
        journal.clone(),
        signing_journal,
        committed,
        committee,
        transport,
        native,
        registry,
    )?);
    let incoming = Arc::new(FastDrainIncoming::new(
        drain_config,
        service.clone(),
        journal.clone(),
    )?);
    Ok(AssembledFastDrain {
        service,
        journal,
        incoming,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;
    use openraft::{CommittedLeaderId, Entry, EntryPayload, Membership, storage::RaftStateMachine};
    use std::{collections::BTreeMap, convert::Infallible, future::Future, pin::Pin};
    use tempfile::TempDir;
    use zone_primitives::fast_transfer::ZoneDomain;

    #[derive(Default)]
    struct CheckpointFixtureExecution;

    impl DurableStateMachineExecution for CheckpointFixtureExecution {
        type Error = Infallible;

        fn apply_committed<'a>(
            &'a self,
            _log_id: LogId<u64>,
            input: &'a crate::fast_quorum::ReplicatedBlockInput,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<crate::fast_quorum::CommittedBlock, Self::Error>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                Ok(crate::fast_quorum::CommittedBlock {
                    input_digest: input.digest(),
                    block_height: 17,
                    block_hash: B256::repeat_byte(0x17),
                    state_root: B256::repeat_byte(0x18),
                    receipts_root: B256::repeat_byte(0x19),
                })
            })
        }

        fn restore_committed<'a>(
            &'a self,
            _blocks: &'a [crate::fast_raft_state_machine::AppliedBlock],
        ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
            Box::pin(async { Ok(()) })
        }

        fn committed_transfer(
            &self,
            _transfer_id: B256,
        ) -> Result<Option<crate::fast_raft_state_machine::CommittedTransferRecord>, Self::Error>
        {
            Ok(None)
        }
    }

    fn point() -> CommittedDrainPoint {
        CommittedDrainPoint {
            log_term: 2,
            log_index: 3,
            block_height: 4,
            block_hash: B256::repeat_byte(5),
            state_root: B256::repeat_byte(6),
            imported_anchor_number: 7,
            imported_anchor_hash: B256::repeat_byte(8),
        }
    }

    #[test]
    fn checkpoint_handoff_wire_binds_exact_installed_coordinate_and_signer() {
        let key = DrainObjectKey::Checkpoint {
            old_epoch: 9,
            next_epoch: 10,
        };
        let digest = B256::repeat_byte(0x31);
        let encoded = encode_checkpoint_handoff_request(&key, digest, point()).unwrap();
        assert_eq!(
            decode_checkpoint_handoff_request(&encoded).unwrap(),
            (key, digest, point())
        );
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(decode_checkpoint_handoff_request(&trailing).is_err());
        let mut changed = encoded;
        *changed.last_mut().unwrap() ^= 1;
        assert_ne!(
            decode_checkpoint_handoff_request(&changed).unwrap().2,
            point()
        );

        let signer = PrivateKeySigner::random();
        let signature = SignatureBytes(signer.sign_hash_sync(&digest).unwrap().as_bytes());
        let response = encode_checkpoint_handoff_response(digest, signer.address(), signature);
        assert_eq!(
            decode_checkpoint_handoff_response(&response, digest, signer.address()).unwrap(),
            signature
        );
        assert!(
            decode_checkpoint_handoff_response(
                &response,
                B256::repeat_byte(0x32),
                signer.address()
            )
            .is_err()
        );

        let chunk = vec![7u8; HANDOFF_CHUNK_BYTES];
        let image_hash = B256::repeat_byte(0x44);
        let frame = encode_checkpoint_image_chunk(
            image_hash,
            (HANDOFF_CHUNK_BYTES * 2) as u32,
            2,
            0,
            &chunk,
        )
        .unwrap();
        let decoded = decode_checkpoint_image_chunk(&frame).unwrap();
        assert_eq!(decoded.0, image_hash);
        assert_eq!(decoded.1, (HANDOFF_CHUNK_BYTES * 2) as u32);
        assert_eq!(decoded.2, 2);
        assert_eq!(decoded.3, 0);
        assert_eq!(decoded.4, chunk);
    }

    #[tokio::test]
    async fn disjoint_successors_install_ack_restart_and_reject_prefix_mutation() {
        // This fixture exercises the production durable image format and fsync/install path. It
        // intentionally has no native token authority or transaction signer.
        let source = TempDir::new().unwrap();
        let mut state_machine = crate::fast_raft_state_machine::DurableRaftStateMachine::open(
            source.path(),
            Arc::new(CheckpointFixtureExecution),
        )
        .await
        .unwrap();
        let old_members = BTreeMap::from([
            (1, BasicNode::new("old-1")),
            (2, BasicNode::new("old-2")),
            (3, BasicNode::new("old-3")),
        ]);
        let membership_log = LogId::new(CommittedLeaderId::new(3, 1), 1);
        let block_log = LogId::new(CommittedLeaderId::new(3, 1), 2);
        state_machine
            .apply(vec![
                Entry {
                    log_id: membership_log,
                    payload: EntryPayload::Membership(Membership::from(old_members)),
                },
                Entry {
                    log_id: block_log,
                    payload: EntryPayload::Normal(crate::fast_quorum::ReplicatedBlockInput {
                        epoch: 4,
                        parent_hash: B256::repeat_byte(0x16),
                        block_input: Bytes::from_static(b"accepted-old-prefix"),
                        transactions: Vec::new(),
                        l1_inputs: Bytes::from_static(b"ordered-l1-imports"),
                        replay_witness: Bytes::from_static(b"replay-witness"),
                    }),
                },
            ])
            .await
            .unwrap();
        let committed = state_machine.committed_handle();
        let replay_barriers = bincode::serialize(&vec![B256::repeat_byte(0x31)]).unwrap();
        let fast_service_journal = bincode::serialize(&vec![B256::repeat_byte(0x32)]).unwrap();
        let batch_boundaries = bincode::serialize(&vec![B256::repeat_byte(0x33)]).unwrap();
        let replenishment_state = bincode::serialize(&vec![B256::repeat_byte(0x34)]).unwrap();
        committed
            .persist_checkpoint_resources(CheckpointResourceImage {
                at: block_log,
                imported_anchor_number: 7,
                imported_anchor_hash: B256::repeat_byte(0x70),
                withdrawal_batch_index: 9,
                local_closure_hash: B256::repeat_byte(0x35),
                final_settlement_hash: B256::repeat_byte(0x36),
                service_protocol_journal: fast_service_journal.clone(),
                drain_barriers: replay_barriers.clone(),
                canonical_batch_boundary: batch_boundaries.clone(),
                replenishment_inventory: replenishment_state.clone(),
            })
            .unwrap();
        let exact = committed.exact_state_image().unwrap();
        let head = exact.blocks.last().unwrap();
        let transfer_history = bincode::serialize(&exact.certified_history).unwrap();
        let replay_witnesses = bincode::serialize(
            &exact
                .blocks
                .iter()
                .map(|block| block.input.replay_witness.clone())
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let old_roster = EpochRoster::from_finalized_registry(
            ZoneDomain {
                l1_chain_id: 1,
                zone_id: 7,
                chain_id: 7007,
                portal: Address::repeat_byte(0x70),
                authority_epoch: 4,
                roster_hash: B256::repeat_byte(0x40),
                protocol_version: 14,
            },
            [
                Address::repeat_byte(1),
                Address::repeat_byte(2),
                Address::repeat_byte(3),
            ],
        )
        .unwrap();
        let next_signers = [
            PrivateKeySigner::random(),
            PrivateKeySigner::random(),
            PrivateKeySigner::random(),
        ];
        let next_roster = EpochRoster::from_finalized_registry(
            ZoneDomain {
                authority_epoch: 5,
                roster_hash: B256::repeat_byte(0x50),
                ..old_roster.domain
            },
            next_signers.each_ref().map(|signer| signer.address()),
        )
        .unwrap();
        assert!(
            old_roster
                .members
                .iter()
                .all(|member| !next_roster.members.contains(member))
        );
        let statement = FastCheckpointStatement {
            portal: old_roster.domain.portal,
            old_epoch: 4,
            next_epoch: 5,
            next_roster_hash: next_roster.domain.roster_hash,
            final_zone_height: U256::from(17),
            final_block_hash: head.output.block_hash,
            final_withdrawal_batch_index: 9,
            final_settlement_hash: B256::repeat_byte(0x36),
            checkpoint_log_term: block_log.leader_id.term,
            checkpoint_log_index: block_log.index,
            checkpoint_height: U256::from(17),
            checkpoint_block_hash: head.output.block_hash,
            checkpoint_state_root: head.output.state_root,
        };
        let image = CheckpointImage {
            l1_chain_id: 1,
            statement: statement.clone(),
            canonical_head_hash: head.output.block_hash,
            canonical_state_root: head.output.state_root,
            witness_root: keccak256(&replay_witnesses),
            outcomes_root: keccak256(&transfer_history),
            replay_barriers_root: keccak256(&replay_barriers),
            raft_prefix: exact
                .blocks
                .iter()
                .map(|block| block.output.block_hash)
                .collect(),
            transfer_history,
            replay_witnesses,
            replay_barriers,
            fast_service_journal,
            batch_boundaries,
            replenishment_state,
            consensus_snapshot: exact.bytes,
        };
        let encoded = image.durable_bytes().unwrap();
        let membership = BTreeMap::from([
            (1, BasicNode::new("next-1")),
            (2, BasicNode::new("next-2")),
            (3, BasicNode::new("next-3")),
        ]);
        let installs = [TempDir::new().unwrap(), TempDir::new().unwrap()];
        let mut acknowledgments = Vec::new();
        for (index, directory) in installs.iter().enumerate() {
            let config = NextRosterCheckpointInstallConfig {
                storage_directory: directory.path().to_path_buf(),
                old_roster: old_roster.clone(),
                next_roster: next_roster.clone(),
                local_next_member: next_signers[index].address(),
                expected_imported_anchor_number: 7,
                expected_imported_anchor_hash: B256::repeat_byte(0x70),
                expected_final_zone_height: U256::from(17),
                expected_final_block_hash: head.output.block_hash,
                expected_final_withdrawal_batch_index: 9,
                expected_final_settlement_hash: B256::repeat_byte(0x36),
                next_membership: membership.clone(),
            };
            let installed = install_next_roster_checkpoint_state(config.clone(), &encoded).unwrap();
            assert_eq!(
                load_successor_bootstrap(directory.path())
                    .unwrap()
                    .accepted_prefix,
                block_log
            );
            persist_successor_catchup_target(directory.path(), 77, B256::repeat_byte(0x77))
                .unwrap();
            assert_eq!(
                pending_successor_catchup_target(directory.path()).unwrap(),
                Some((77, B256::repeat_byte(0x77)))
            );
            complete_successor_catchup(directory.path(), 77, B256::repeat_byte(0x77)).unwrap();
            assert_eq!(
                pending_successor_catchup_target(directory.path()).unwrap(),
                None
            );
            // Reopening and reinstalling the same bytes is idempotent and never resets history.
            install_next_roster_checkpoint_state(config.clone(), &encoded).unwrap();
            let digest = statement.registry_digest(1);
            let signature = sign_authenticated_drain_phase_handoff(
                old_roster.members[0],
                &old_roster,
                &next_roster,
                &next_signers[index],
                &DurableJournal::open(directory.path().join("signing")).unwrap(),
                installed.as_ref(),
                DrainSigningPurpose::Checkpoint,
                &DrainObjectKey::Checkpoint {
                    old_epoch: 4,
                    next_epoch: 5,
                },
                digest,
                installed.point,
            )
            .unwrap();
            authorize_successor_transition(&config, installed.as_ref(), digest, installed.point)
                .unwrap();
            acknowledgments.push(signature);
        }
        assert_ne!(acknowledgments[0], acknowledgments[1]);
        QuorumVerifier::verify_next_roster_checkpoint(
            1,
            &statement,
            next_roster.members,
            &[acknowledgments[0], acknowledgments[1]],
        )
        .unwrap();

        let mut mutated = image;
        mutated.statement.checkpoint_state_root = B256::repeat_byte(0xee);
        mutated.canonical_state_root = mutated.statement.checkpoint_state_root;
        assert!(
            install_next_roster_checkpoint_state(
                NextRosterCheckpointInstallConfig {
                    storage_directory: TempDir::new().unwrap().path().to_path_buf(),
                    old_roster,
                    next_roster,
                    local_next_member: next_signers[2].address(),
                    expected_imported_anchor_number: 7,
                    expected_imported_anchor_hash: B256::repeat_byte(0x70),
                    expected_final_zone_height: U256::from(17),
                    expected_final_block_hash: head.output.block_hash,
                    expected_final_withdrawal_batch_index: 9,
                    expected_final_settlement_hash: B256::repeat_byte(0x36),
                    next_membership: membership,
                },
                &mutated.durable_bytes().unwrap(),
            )
            .is_err()
        );
    }

    #[test]
    fn closure_action_snapshot_and_recovery_are_exact() {
        let directory = TempDir::new().expect("temporary directory");
        let signer = Address::repeat_byte(1);
        let journal = FileFastDrainJournal::open(directory.path(), 9, signer, 12)
            .expect("open drain journal");
        journal
            .persist_closure(B256::repeat_byte(2), point())
            .expect("persist closure");
        let key = DrainObjectKey::FinalSettlement { epoch: 9 };
        let action = journal
            .prepare_registry_action(&key, signer, Address::repeat_byte(3), vec![4, 5])
            .expect("prepare action");
        assert_eq!(action.nonce, 12);
        let transaction = B256::repeat_byte(6);
        journal
            .record_registry_submission(action.action_id, transaction)
            .expect("submission");
        journal
            .complete_registry_action(action.action_id, transaction)
            .expect("completion");
        journal.snapshot().expect("snapshot");
        drop(journal);

        let recovered = FileFastDrainJournal::open(directory.path(), 9, signer, 1)
            .expect("recover drain journal");
        assert_eq!(
            recovered.closure(9).expect("closure"),
            Some((B256::repeat_byte(2), point()))
        );
        let duplicate = recovered
            .prepare_registry_action(&key, signer, Address::repeat_byte(3), vec![4, 5])
            .expect("duplicate action");
        assert_eq!(duplicate.nonce, 12);
        assert_eq!(duplicate.submission_hashes, vec![transaction]);
    }

    #[test]
    fn conflicting_action_is_rejected_without_nonce_consumption() {
        let directory = TempDir::new().expect("temporary directory");
        let signer = Address::repeat_byte(1);
        let journal = FileFastDrainJournal::open(directory.path(), 9, signer, 20)
            .expect("open drain journal");
        let key = DrainObjectKey::FinalSettlement { epoch: 9 };
        journal
            .prepare_registry_action(&key, signer, Address::repeat_byte(3), vec![1])
            .expect("prepare action");
        assert!(matches!(
            journal.prepare_registry_action(&key, signer, Address::repeat_byte(3), vec![2]),
            Err(FastDrainError::ConflictingRegistryAction)
        ));
    }

    #[test]
    fn partial_tail_is_truncated_and_complete_state_recovers() {
        let directory = TempDir::new().expect("temporary directory");
        let signer = Address::repeat_byte(1);
        let journal =
            FileFastDrainJournal::open(directory.path(), 9, signer, 1).expect("open drain journal");
        journal
            .persist_closure(B256::repeat_byte(2), point())
            .expect("persist closure");
        drop(journal);
        OpenOptions::new()
            .append(true)
            .open(directory.path().join(JOURNAL_FILE))
            .and_then(|mut file| file.write_all(JOURNAL_MAGIC))
            .expect("append torn frame");
        let recovered = FileFastDrainJournal::open(directory.path(), 9, signer, 1)
            .expect("truncate partial tail");
        assert_eq!(
            recovered.closure(9).expect("closure"),
            Some((B256::repeat_byte(2), point()))
        );
    }

    #[test]
    fn drain_chunk_fits_carrier_and_binds_order_and_certificate() {
        let certificate = DrainCertificate {
            digest: B256::repeat_byte(1),
            signatures: [SignatureBytes([2; 65]), SignatureBytes([3; 65])],
        };
        let frame = encode_drain_chunk(
            DRAIN_BARRIER_CLASS,
            certificate,
            B256::repeat_byte(4),
            (DRAIN_CHUNK_BYTES * 2) as u32,
            2,
            1,
            &vec![5; DRAIN_CHUNK_BYTES],
        )
        .expect("bounded chunk");
        assert!(frame.len() <= MAX_INTER_ZONE_MESSAGE_SIZE as usize);
        assert_eq!(&frame[..DRAIN_WIRE_MAGIC.len()], DRAIN_WIRE_MAGIC);
        assert_eq!(frame[DRAIN_WIRE_MAGIC.len()], DRAIN_WIRE_VERSION);
        assert_eq!(frame[DRAIN_WIRE_MAGIC.len() + 1], DRAIN_BARRIER_CLASS);
        let (key, chunk) = decode_drain_chunk(
            drain_stream(DRAIN_BARRIER_CLASS, certificate.digest),
            1,
            &frame,
        )
        .expect("decode exact drain frame");
        assert_eq!(key.object, certificate.digest);
        assert_eq!(key.index, 1);
        assert_eq!(chunk.certificate, certificate);
    }

    #[test]
    fn inbound_chunks_reassemble_exactly_across_restart() {
        let directory = TempDir::new().expect("temporary directory");
        let signer = Address::repeat_byte(1);
        let peer = Address::repeat_byte(9);
        let certificate = DrainCertificate {
            digest: B256::repeat_byte(2),
            signatures: [SignatureBytes([3; 65]), SignatureBytes([4; 65])],
        };
        let mut encoded = vec![5; DRAIN_CHUNK_BYTES];
        encoded.extend_from_slice(&[6, 7, 8]);
        let full_hash = keccak256(&encoded);
        let first = InboundDrainChunk {
            certificate,
            full_hash,
            total_bytes: encoded.len() as u32,
            chunk_count: 2,
            payload: encoded[..DRAIN_CHUNK_BYTES].to_vec(),
        };
        let second = InboundDrainChunk {
            payload: encoded[DRAIN_CHUNK_BYTES..].to_vec(),
            ..first.clone()
        };
        let key = |index| DrainChunkKey {
            peer,
            stream_class: DRAIN_RESOLUTION_CLASS,
            object: certificate.digest,
            index,
        };

        let journal =
            FileFastDrainJournal::open(directory.path(), 9, signer, 1).expect("open drain journal");
        journal
            .persist_inbound_chunk(key(0), first)
            .expect("fsync first chunk");
        assert_eq!(
            journal
                .assembled_inbound(peer, DRAIN_RESOLUTION_CLASS, certificate.digest)
                .expect("partial object"),
            None
        );
        drop(journal);

        let recovered = FileFastDrainJournal::open(directory.path(), 9, signer, 1)
            .expect("recover first chunk");
        recovered
            .persist_inbound_chunk(key(1), second)
            .expect("fsync final chunk");
        assert_eq!(
            recovered
                .assembled_inbound(peer, DRAIN_RESOLUTION_CLASS, certificate.digest)
                .expect("complete object"),
            Some((encoded, certificate))
        );
    }
}
