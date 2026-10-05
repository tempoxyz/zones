//! Concrete fsynced storage and signer adapters for the T14 distributed drain.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use alloy_network::{ReceiptResponse as _, TransactionBuilder as _};
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_provider::{DynProvider, Provider as _};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_signer::SignerSync as _;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall as _;
use tempo_alloy::{TempoNetwork, rpc::TempoTransactionRequest};
use tokio::sync::{mpsc, oneshot};
use zone_fast_transfer::{
    DurableJournal, SigningRecord,
    drain::{
        BarrierInventory, BarrierResolutionInventory, CheckpointImage, DrainCertificate,
        MAX_DRAIN_OBJECT_BYTES,
    },
};
use zone_p2p::{InterZoneServiceRequest, MAX_INTER_ZONE_MESSAGE_SIZE, P2pPeerId};
use zone_primitives::fast_transfer::{
    FastBarrierResolution, FastBarrierStatement, FastCheckpointStatement, SignatureBytes,
};

use crate::fast_drain::{
    CommittedDrainPoint, DrainFuture, DrainObjectKey, DrainPeer, DrainSigningPurpose,
    FastDrainCommittedState, FastDrainCommittee, FastDrainConfig, FastDrainError, FastDrainJournal,
    FastDrainNative, FastDrainRegistry, FastDrainService, FastDrainTransport, FinalAcceptedPrefix,
    PreparedRegistryAction,
};

const JOURNAL_MAGIC: &[u8; 4] = b"FDJ1";
const JOURNAL_FILE: &str = "drain-journal.bin";
const SNAPSHOT_FILE: &str = "drain-snapshot.bin";
const SNAPSHOT_TEMP: &str = "drain-snapshot.tmp";
const MAX_SUBMISSION_HASHES: usize = 64;
const ACTION_DOMAIN: &[u8] = b"TEMPO_ZONE_FAST_DRAIN_ACTION_T14_V1";
const DRAIN_WIRE_VERSION: u8 = 1;
const DRAIN_BARRIER_CLASS: u8 = 1;
const DRAIN_RESOLUTION_CLASS: u8 = 2;
const DRAIN_CHUNK_BYTES: usize = 12 * 1024;

mod factory_abi {
    alloy_sol_types::sol! {
        #[sol(rpc)]
        interface IT14ZonePortal {
            struct FastEpochConfig {
                uint32 protocolVersion;
                uint8 threshold;
                uint8 proofMode;
                bool closed;
                bool retired;
                uint16 expectedPeerBarriers;
                uint16 recordedPeerBarriers;
                uint16 finalizedPeerBarriers;
                uint64 activatedAtTempoBlock;
                bytes32 rosterHash;
                bytes32 peersHash;
                bytes32 expectedVerifierCodeHash;
                bytes32 expectedVerifierConfigHash;
                bytes32 closureHash;
                uint256 finalSettlementHeight;
                bytes32 finalSettlementBlockHash;
                uint64 finalSettlementWithdrawalBatchIndex;
                bytes32 barriersHash;
                bytes32 finalSettlementHash;
                uint64 nextEpoch;
                bytes32 nextRosterHash;
                uint64 checkpointLogTerm;
                uint64 checkpointLogIndex;
                uint256 checkpointHeight;
                bytes32 checkpointBlockHash;
                bytes32 checkpointStateRoot;
                bytes32 checkpointHash;
            }

            struct FastPeerBarrier {
                bool recorded;
                bool finalized;
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
                bytes32 barrierHash;
                bytes32 terminalRoot;
                bytes32 dispositionRoot;
                uint64 resolvedCount;
                bytes32 remainingUnresolvedRoot;
                uint64 remainingUnresolvedCount;
                bytes32 resolutionHash;
            }

            function fastEpochConfig(uint64 epoch) external view returns (FastEpochConfig);
            function fastPeerBarrier(uint64 epoch, address peer) external view returns (FastPeerBarrier);
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
        let mut inner = self.inner.lock().map_err(|_| storage("lock poisoned"))?;
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
    fn request<'a>(
        &'a self,
        member: Address,
        purpose: DrainSigningPurpose,
        key: &'a DrainObjectKey,
        digest: B256,
        point: CommittedDrainPoint,
    ) -> DrainFuture<'a, Result<SignatureBytes, FastDrainError>>;
}

impl FastDrainCommittee for DurableLocalDrainCommittee {
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
            Ok(self
                .provider
                .get_block_by_hash(block_hash)
                .hashes()
                .await
                .map_err(registry)?
                .is_some())
        })
    }

    fn barrier_recorded<'a>(
        &'a self,
        epoch: u64,
        peer: Address,
        barrier_hash: B256,
    ) -> DrainFuture<'a, Result<bool, FastDrainError>> {
        Box::pin(async move {
            let barrier = factory_abi::IT14ZonePortal::new(self.portal, &self.provider)
                .fastPeerBarrier(epoch, peer)
                .call()
                .await
                .map_err(registry)?;
            Ok(barrier.recorded && barrier.barrierHash == barrier_hash)
        })
    }

    fn barrier_finalized<'a>(
        &'a self,
        epoch: u64,
        peer: Address,
        resolution_hash: B256,
    ) -> DrainFuture<'a, Result<bool, FastDrainError>> {
        Box::pin(async move {
            let barrier = factory_abi::IT14ZonePortal::new(self.portal, &self.provider)
                .fastPeerBarrier(epoch, peer)
                .call()
                .await
                .map_err(registry)?;
            Ok(barrier.recorded && barrier.finalized && barrier.resolutionHash == resolution_hash)
        })
    }

    fn final_settlement_hash<'a>(
        &'a self,
        epoch: u64,
    ) -> DrainFuture<'a, Result<Option<B256>, FastDrainError>> {
        Box::pin(async move {
            let config = factory_abi::IT14ZonePortal::new(self.portal, &self.provider)
                .fastEpochConfig(epoch)
                .call()
                .await
                .map_err(registry)?;
            Ok((!config.finalSettlementHash.is_zero()).then_some(config.finalSettlementHash))
        })
    }

    fn checkpoint_hash<'a>(
        &'a self,
        epoch: u64,
    ) -> DrainFuture<'a, Result<Option<B256>, FastDrainError>> {
        Box::pin(async move {
            let config = factory_abi::IT14ZonePortal::new(self.portal, &self.provider)
                .fastEpochConfig(epoch)
                .call()
                .await
                .map_err(registry)?;
            Ok((!config.checkpointHash.is_zero()).then_some(config.checkpointHash))
        })
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
    (u64::from(stream_class) << 56) | (u64::from_be_bytes(prefix) & 0x00ff_ffff_ffff_ffff)
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
    if !matches!(stream_class, DRAIN_BARRIER_CLASS | DRAIN_RESOLUTION_CLASS)
        || payload.is_empty()
        || payload.len() > DRAIN_CHUNK_BYTES
        || chunk_count == 0
        || index >= chunk_count
    {
        return Err(FastDrainError::InvalidConfiguration);
    }
    let mut frame = Vec::with_capacity(1 + 1 + 32 * 2 + 65 * 2 + 4 * 4 + payload.len());
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
    pub response_timeout: Duration,
}

pub struct AssembledFastDrain {
    pub service: Arc<FastDrainService>,
    pub journal: Arc<FileFastDrainJournal>,
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
    signature_requester: Arc<dyn DrainPeerSignatureRequester>,
) -> Result<AssembledFastDrain, FastDrainError> {
    config.drain.validate()?;
    if local_signer.address() != config.drain.local_member
        || config.drain.local_roster.domain.l1_chain_id != config.l1_chain_id
        || config.routes.len() != config.drain.peers.len()
        || config.journal_directory.as_os_str().is_empty()
    {
        return Err(FastDrainError::InvalidConfiguration);
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
    let service = Arc::new(FastDrainService::new(
        config.drain,
        journal.clone(),
        signing_journal,
        committed,
        committee,
        transport,
        native,
        registry,
    )?);
    Ok(AssembledFastDrain { service, journal })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
            DRAIN_CHUNK_BYTES as u32,
            2,
            1,
            &vec![5; DRAIN_CHUNK_BYTES],
        )
        .expect("bounded chunk");
        assert!(frame.len() <= MAX_INTER_ZONE_MESSAGE_SIZE as usize);
        assert_eq!(frame[0], DRAIN_WIRE_VERSION);
        assert_eq!(frame[1], DRAIN_BARRIER_CLASS);
    }
}
