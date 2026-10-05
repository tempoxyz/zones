//! Crash-durable append journal and atomic snapshots for service-owned state.

use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use alloy_primitives::{Address, B256, U256, keccak256};
use zone_primitives::fast_transfer::SignatureBytes;

use crate::{
    delivery::{DeliveryRecord, DeliveryStore, DeliveryTransition},
    replenishment::{
        DepositRecord, InventoryContribution, ReplenishmentJob, ReplenishmentStage,
        WithdrawalRecord,
    },
};

const FRAME_MAGIC: &[u8; 4] = b"FTJ1";
const JOURNAL_FILE: &str = "journal.bin";
const SNAPSHOT_FILE: &str = "snapshot.bin";
const SNAPSHOT_TEMP: &str = "snapshot.tmp";
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;

/// Persisted proof that one replica signed one independently reconstructed committed body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SigningRecord {
    /// Signed EIP-712 digest.
    pub digest: B256,
    /// Authorized local signer.
    pub signer: Address,
    /// Exact signature returned for this digest.
    pub signature: SignatureBytes,
    /// Original committed log term.
    pub log_term: u64,
    /// Original committed log index.
    pub log_index: u64,
}

/// Opaque, complete replay material for one quorum-committed block entry.
///
/// The journal does not interpret executor-specific bytes. Node integration must encode them
/// deterministically and independently verify the reconstructed block hash and state root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplicatedBlockInput {
    /// Original Raft term of this committed entry.
    pub log_term: u64,
    /// Original Raft index; the durable ordering key.
    pub log_index: u64,
    /// Committed Zone block height.
    pub block_height: u64,
    /// Expected committed block hash.
    pub block_hash: B256,
    /// Expected post-execution state root.
    pub state_root: B256,
    /// Deterministic block attributes/system inputs.
    pub block_input: Vec<u8>,
    /// Complete ordered transaction byte vectors.
    pub transactions: Vec<Vec<u8>>,
    /// Complete finalized L1 execution input or same-anchor witness selector.
    pub l1_execution_input: Vec<u8>,
    /// Replay/proof witness bytes retained before certification.
    pub witness: Vec<u8>,
}

/// Permanent replay outcome retained after active transfer state is compacted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TombstoneOutcome {
    /// Destination paid.
    Paid,
    /// Destination permanently rejected.
    Rejected,
    /// Source released escrow.
    Released,
    /// Source refunded escrow.
    Refunded,
}

/// Durable transfer ID/body binding and terminal marker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Tombstone {
    /// Stable transfer ID.
    pub transfer_id: B256,
    /// Complete original intent hash.
    pub intent_hash: B256,
    /// Permanent terminal state.
    pub outcome: TombstoneOutcome,
}

/// Journal I/O, corruption, or monotonicity failure.
#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    /// Filesystem operation failed.
    #[error("durable journal I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// A complete record failed its checksum or canonical decoding.
    #[error("durable journal is corrupt: {0}")]
    Corrupt(&'static str),
    /// Existing durable state conflicts with the requested idempotency key.
    #[error("durable record conflicts with existing state")]
    Conflict,
    /// Requested delivery transition moves backward or references missing work.
    #[error("invalid durable delivery transition")]
    InvalidTransition,
    /// Cursor would skip a non-durable incoming sequence.
    #[error("durable cursor would skip an unresolved gap")]
    CursorGap,
    /// Mutex was poisoned by an earlier panic.
    #[error("durable journal lock poisoned")]
    Poisoned,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct DeliveryKey {
    peer_zone: u32,
    stream: u64,
    sequence: u64,
}

#[derive(Clone, Debug, Default)]
struct JournalState {
    outgoing: BTreeMap<DeliveryKey, DeliveryRecord>,
    incoming: BTreeMap<DeliveryKey, Vec<u8>>,
    cursors: HashMap<(u32, u64), u64>,
    signing: HashMap<(B256, Address), SigningRecord>,
    tombstones: HashMap<B256, Tombstone>,
    jobs: HashMap<B256, ReplenishmentJob>,
    replay: BTreeMap<u64, ReplicatedBlockInput>,
}

#[derive(Debug)]
struct Inner {
    journal: File,
    state: JournalState,
}

/// Concrete fsync-backed service journal.
///
/// Every mutation is appended, flushed, and `fsync`ed before it becomes visible in memory.
/// Snapshots are written to a new file, `fsync`ed, atomically renamed, and followed by a
/// directory `fsync`; journal truncation happens only after snapshot publication.
#[derive(Debug)]
pub struct DurableJournal {
    directory: PathBuf,
    inner: Mutex<Inner>,
}

impl DurableJournal {
    /// Open or create a journal, replaying the atomic snapshot and valid journal prefix.
    /// An incomplete final append is truncated; corruption in a complete frame fails closed.
    pub fn open(directory: impl AsRef<Path>) -> Result<Self, JournalError> {
        let directory = directory.as_ref().to_path_buf();
        let created = !directory.exists();
        fs::create_dir_all(&directory)?;
        if created {
            sync_directory(directory.parent().unwrap_or_else(|| Path::new(".")))?;
        }

        let mut state = JournalState::default();
        let snapshot_path = directory.join(SNAPSHOT_FILE);
        if snapshot_path.exists() {
            let snapshot = fs::read(&snapshot_path)?;
            let parsed = replay_frames(&snapshot, &mut state, false)?;
            if parsed != snapshot.len() {
                return Err(JournalError::Corrupt("partial snapshot frame"));
            }
        }

        let journal_path = directory.join(JOURNAL_FILE);
        let journal_created = !journal_path.exists();
        let journal = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&journal_path)?;
        if journal_created {
            journal.sync_all()?;
            sync_directory(&directory)?;
        }
        let mut journal_bytes = Vec::new();
        File::open(&journal_path)?.read_to_end(&mut journal_bytes)?;
        let parsed = replay_frames(&journal_bytes, &mut state, true)?;
        if parsed != journal_bytes.len() {
            journal.set_len(parsed as u64)?;
            journal.sync_all()?;
        }

        Ok(Self {
            directory,
            inner: Mutex::new(Inner { journal, state }),
        })
    }

    /// Persist a replica signing record before exposing its signature.
    pub fn persist_signing_record(&self, record: SigningRecord) -> Result<bool, JournalError> {
        self.mutate(Operation::Signing(record.clone()), |state| {
            let key = (record.digest, record.signer);
            match state.signing.get(&key) {
                Some(existing) if existing == &record => Ok(false),
                Some(_) => Err(JournalError::Conflict),
                None => {
                    state.signing.insert(key, record);
                    Ok(true)
                }
            }
        })
    }

    /// Read the exact durable signing record, if present.
    pub fn signing_record(
        &self,
        digest: B256,
        signer: Address,
    ) -> Result<Option<SigningRecord>, JournalError> {
        let inner = self.inner.lock().map_err(|_| JournalError::Poisoned)?;
        Ok(inner.state.signing.get(&(digest, signer)).cloned())
    }

    /// Persist all replay material for a committed block before any outcome is signed.
    pub fn persist_replicated_block(
        &self,
        record: ReplicatedBlockInput,
    ) -> Result<bool, JournalError> {
        self.mutate(
            Operation::Replay(Box::new(record.clone())),
            |state| match state.replay.get(&record.log_index) {
                Some(existing) if existing == &record => Ok(false),
                Some(_) => Err(JournalError::Conflict),
                None => {
                    state.replay.insert(record.log_index, record);
                    Ok(true)
                }
            },
        )
    }

    /// Load the committed replay suffix in original log-index order.
    pub fn replicated_blocks_from(
        &self,
        first_log_index: u64,
    ) -> Result<Vec<ReplicatedBlockInput>, JournalError> {
        let inner = self.inner.lock().map_err(|_| JournalError::Poisoned)?;
        Ok(inner
            .state
            .replay
            .range(first_log_index..)
            .map(|(_, record)| record.clone())
            .collect())
    }

    /// Persist a permanent transfer tombstone and full intent binding.
    pub fn persist_tombstone(&self, tombstone: Tombstone) -> Result<bool, JournalError> {
        self.mutate(Operation::Tombstone(tombstone), |state| {
            match state.tombstones.get(&tombstone.transfer_id) {
                Some(existing) if existing == &tombstone => Ok(false),
                Some(_) => Err(JournalError::Conflict),
                None => {
                    state.tombstones.insert(tombstone.transfer_id, tombstone);
                    Ok(true)
                }
            }
        })
    }

    /// Look up a permanent replay marker.
    pub fn tombstone(&self, transfer_id: B256) -> Result<Option<Tombstone>, JournalError> {
        let inner = self.inner.lock().map_err(|_| JournalError::Poisoned)?;
        Ok(inner.state.tombstones.get(&transfer_id).copied())
    }

    /// Persist the complete latest replenishment job record.
    pub fn persist_replenishment_job(&self, job: ReplenishmentJob) -> Result<bool, JournalError> {
        self.mutate(Operation::Job(Box::new(job.clone())), |state| {
            match state.jobs.get(&job.job_id) {
                Some(existing) if existing == &job => Ok(false),
                Some(existing) => {
                    validate_job_update(existing, &job)?;
                    state.jobs.insert(job.job_id, job);
                    Ok(true)
                }
                _ => {
                    state.jobs.insert(job.job_id, job);
                    Ok(true)
                }
            }
        })
    }

    /// Reconstruct all unfinished replenishment jobs after restart.
    pub fn unfinished_replenishment_jobs(&self) -> Result<Vec<ReplenishmentJob>, JournalError> {
        let inner = self.inner.lock().map_err(|_| JournalError::Poisoned)?;
        let mut jobs: Vec<_> = inner
            .state
            .jobs
            .values()
            .filter(|job| job.stage != ReplenishmentStage::PoolCredited)
            .cloned()
            .collect();
        jobs.sort_by_key(|job| job.job_id);
        Ok(jobs)
    }

    /// Publish an atomic snapshot of deliveries, cursors, signing records, tombstones,
    /// and replenishment jobs, then start a fresh journal.
    pub fn snapshot(&self) -> Result<(), JournalError> {
        let inner = self.inner.lock().map_err(|_| JournalError::Poisoned)?;
        let operations = snapshot_operations(&inner.state);
        let temporary = self.directory.join(SNAPSHOT_TEMP);
        let snapshot = self.directory.join(SNAPSHOT_FILE);
        {
            let mut file = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&temporary)?;
            for operation in operations {
                write_frame(&mut file, &operation)?;
            }
            file.flush()?;
            file.sync_all()?;
        }
        fs::rename(&temporary, &snapshot)?;
        sync_directory(&self.directory)?;
        inner.journal.set_len(0)?;
        inner.journal.sync_all()?;
        Ok(())
    }

    fn mutate<T>(
        &self,
        operation: Operation,
        apply: impl FnOnce(&mut JournalState) -> Result<T, JournalError>,
    ) -> Result<T, JournalError> {
        let mut inner = self.inner.lock().map_err(|_| JournalError::Poisoned)?;
        // Validate against a clone so conflicts never reach disk.
        let mut next = inner.state.clone();
        let result = apply(&mut next)?;
        write_frame(&mut inner.journal, &operation)?;
        inner.journal.flush()?;
        inner.journal.sync_data()?;
        inner.state = next;
        Ok(result)
    }
}

impl DeliveryStore for DurableJournal {
    type Error = JournalError;

    fn persist_outgoing(&self, peer_zone: u32, record: &DeliveryRecord) -> Result<(), Self::Error> {
        let key = DeliveryKey {
            peer_zone,
            stream: record.stream,
            sequence: record.sequence,
        };
        self.mutate(
            Operation::Outgoing(key, record.clone()),
            |state| match state.outgoing.get(&key) {
                Some(existing) if existing == record => Ok(()),
                Some(_) => Err(JournalError::Conflict),
                None => {
                    state.outgoing.insert(key, record.clone());
                    Ok(())
                }
            },
        )
    }

    fn persist_incoming(
        &self,
        peer_zone: u32,
        stream: u64,
        sequence: u64,
        payload: &[u8],
    ) -> Result<(), Self::Error> {
        let key = DeliveryKey {
            peer_zone,
            stream,
            sequence,
        };
        let payload = payload.to_vec();
        self.mutate(
            Operation::Incoming(key, payload.clone()),
            |state| match state.incoming.get(&key) {
                Some(existing) if existing == &payload => Ok(()),
                Some(_) => Err(JournalError::Conflict),
                None => {
                    state.incoming.insert(key, payload);
                    Ok(())
                }
            },
        )
    }

    fn transition(
        &self,
        peer_zone: u32,
        stream: u64,
        sequence: u64,
        next: DeliveryTransition,
    ) -> Result<(), Self::Error> {
        let key = DeliveryKey {
            peer_zone,
            stream,
            sequence,
        };
        self.mutate(Operation::Transition(key, next), |state| {
            let record = state
                .outgoing
                .get_mut(&key)
                .ok_or(JournalError::InvalidTransition)?;
            record
                .advance(next)
                .map_err(|_| JournalError::InvalidTransition)?;
            Ok(())
        })
    }

    fn advance_contiguous_cursor(
        &self,
        peer_zone: u32,
        stream: u64,
        through_sequence: u64,
    ) -> Result<(), Self::Error> {
        self.mutate(
            Operation::Cursor(peer_zone, stream, through_sequence),
            |state| {
                let previous = state
                    .cursors
                    .get(&(peer_zone, stream))
                    .copied()
                    .unwrap_or(0);
                if through_sequence < previous {
                    return Err(JournalError::InvalidTransition);
                }
                for sequence in previous.saturating_add(1)..=through_sequence {
                    let key = DeliveryKey {
                        peer_zone,
                        stream,
                        sequence,
                    };
                    if !state.incoming.contains_key(&key) {
                        return Err(JournalError::CursorGap);
                    }
                }
                state.cursors.insert((peer_zone, stream), through_sequence);
                Ok(())
            },
        )
    }

    fn load_unfinished(&self, peer_zone: u32) -> Result<Vec<DeliveryRecord>, Self::Error> {
        let inner = self.inner.lock().map_err(|_| JournalError::Poisoned)?;
        Ok(inner
            .state
            .outgoing
            .iter()
            .filter(|(key, record)| peer_zone == key.peer_zone && !record.compactable())
            .map(|(_, record)| record.clone())
            .collect())
    }
}

#[derive(Clone, Debug)]
enum Operation {
    Outgoing(DeliveryKey, DeliveryRecord),
    Incoming(DeliveryKey, Vec<u8>),
    Transition(DeliveryKey, DeliveryTransition),
    Cursor(u32, u64, u64),
    Signing(SigningRecord),
    Tombstone(Tombstone),
    Job(Box<ReplenishmentJob>),
    Replay(Box<ReplicatedBlockInput>),
}

fn snapshot_operations(state: &JournalState) -> Vec<Operation> {
    let mut operations = Vec::new();
    operations.extend(
        state
            .outgoing
            .iter()
            .map(|(key, record)| Operation::Outgoing(*key, record.clone())),
    );
    operations.extend(
        state
            .incoming
            .iter()
            .map(|(key, payload)| Operation::Incoming(*key, payload.clone())),
    );
    let mut cursors: Vec<_> = state.cursors.iter().collect();
    cursors.sort_by_key(|((peer, stream), _)| (*peer, *stream));
    operations.extend(
        cursors
            .into_iter()
            .map(|((peer, stream), sequence)| Operation::Cursor(*peer, *stream, *sequence)),
    );
    let mut signing: Vec<_> = state.signing.values().cloned().collect();
    signing.sort_by_key(|record| (record.digest, record.signer));
    operations.extend(signing.into_iter().map(Operation::Signing));
    let mut tombstones: Vec<_> = state.tombstones.values().copied().collect();
    tombstones.sort_by_key(|record| record.transfer_id);
    operations.extend(tombstones.into_iter().map(Operation::Tombstone));
    let mut jobs: Vec<_> = state.jobs.values().cloned().collect();
    jobs.sort_by_key(|job| job.job_id);
    operations.extend(jobs.into_iter().map(|job| Operation::Job(Box::new(job))));
    operations.extend(
        state
            .replay
            .values()
            .cloned()
            .map(|record| Operation::Replay(Box::new(record))),
    );
    operations
}

fn write_frame(file: &mut File, operation: &Operation) -> Result<(), JournalError> {
    let body = encode_operation(operation);
    if body.len() > MAX_RECORD_BYTES {
        return Err(JournalError::Corrupt("record exceeds maximum"));
    }
    file.write_all(FRAME_MAGIC)?;
    file.write_all(&(body.len() as u32).to_be_bytes())?;
    file.write_all(&body)?;
    file.write_all(keccak256(&body).as_slice())?;
    Ok(())
}

fn replay_frames(
    bytes: &[u8],
    state: &mut JournalState,
    allow_partial_tail: bool,
) -> Result<usize, JournalError> {
    let mut offset = 0usize;
    while offset < bytes.len() {
        let frame_start = offset;
        if bytes.len() - offset < 8 {
            return if allow_partial_tail {
                Ok(frame_start)
            } else {
                Err(JournalError::Corrupt("partial frame header"))
            };
        }
        if &bytes[offset..offset + 4] != FRAME_MAGIC {
            return Err(JournalError::Corrupt("bad frame magic"));
        }
        offset += 4;
        let length =
            u32::from_be_bytes(bytes[offset..offset + 4].try_into().expect("exact")) as usize;
        offset += 4;
        if length > MAX_RECORD_BYTES {
            return Err(JournalError::Corrupt("record exceeds maximum"));
        }
        let needed = length
            .checked_add(32)
            .ok_or(JournalError::Corrupt("record length overflow"))?;
        if bytes.len() - offset < needed {
            return if allow_partial_tail {
                Ok(frame_start)
            } else {
                Err(JournalError::Corrupt("partial frame body"))
            };
        }
        let body = &bytes[offset..offset + length];
        offset += length;
        let checksum = B256::from_slice(&bytes[offset..offset + 32]);
        offset += 32;
        if keccak256(body) != checksum {
            return Err(JournalError::Corrupt("checksum mismatch"));
        }
        apply_replay(state, decode_operation(body)?)?;
    }
    Ok(offset)
}

fn apply_replay(state: &mut JournalState, operation: Operation) -> Result<(), JournalError> {
    match operation {
        Operation::Outgoing(key, record) => match state.outgoing.get(&key) {
            Some(existing) if existing != &record => return Err(JournalError::Conflict),
            Some(_) => {}
            None => {
                state.outgoing.insert(key, record);
            }
        },
        Operation::Incoming(key, payload) => match state.incoming.get(&key) {
            Some(existing) if existing != &payload => return Err(JournalError::Conflict),
            Some(_) => {}
            None => {
                state.incoming.insert(key, payload);
            }
        },
        Operation::Transition(key, next) => {
            state
                .outgoing
                .get_mut(&key)
                .ok_or(JournalError::InvalidTransition)?
                .advance(next)
                .map_err(|_| JournalError::InvalidTransition)?;
        }
        Operation::Cursor(peer, stream, through) => {
            let previous = state.cursors.get(&(peer, stream)).copied().unwrap_or(0);
            if through < previous {
                return Err(JournalError::InvalidTransition);
            }
            for sequence in previous.saturating_add(1)..=through {
                if !state.incoming.contains_key(&DeliveryKey {
                    peer_zone: peer,
                    stream,
                    sequence,
                }) {
                    return Err(JournalError::CursorGap);
                }
            }
            state.cursors.insert((peer, stream), through);
        }
        Operation::Signing(record) => {
            let key = (record.digest, record.signer);
            match state.signing.get(&key) {
                Some(existing) if existing != &record => return Err(JournalError::Conflict),
                _ => {
                    state.signing.insert(key, record);
                }
            }
        }
        Operation::Tombstone(tombstone) => match state.tombstones.get(&tombstone.transfer_id) {
            Some(existing) if existing != &tombstone => return Err(JournalError::Conflict),
            _ => {
                state.tombstones.insert(tombstone.transfer_id, tombstone);
            }
        },
        Operation::Job(job) => {
            if let Some(existing) = state.jobs.get(&job.job_id) {
                validate_job_update(existing, &job)?;
            }
            state.jobs.insert(job.job_id, *job);
        }
        Operation::Replay(record) => match state.replay.get(&record.log_index) {
            Some(existing) if existing != record.as_ref() => return Err(JournalError::Conflict),
            Some(_) => {}
            None => {
                state.replay.insert(record.log_index, *record);
            }
        },
    }
    Ok(())
}

fn replenishment_rank(stage: ReplenishmentStage) -> u8 {
    match stage {
        ReplenishmentStage::InventoryAllocated => 0,
        ReplenishmentStage::WithdrawalRequested => 1,
        ReplenishmentStage::WithdrawalBounced => 2,
        ReplenishmentStage::TreasuryFunded => 3,
        ReplenishmentStage::DepositSubmitted => 4,
        ReplenishmentStage::DepositRefundPending => 5,
        ReplenishmentStage::PoolCredited => 6,
    }
}

fn validate_job_update(
    existing: &ReplenishmentJob,
    next: &ReplenishmentJob,
) -> Result<(), JournalError> {
    if existing.job_id != next.job_id
        || existing.source_inventory != next.source_inventory
        || existing.source_fallback != next.source_fallback
        || existing.treasury != next.treasury
        || existing.destination_pool != next.destination_pool
        || existing.deposit_refund != next.deposit_refund
        || existing.contributions != next.contributions
        || existing.gross_amount != next.gross_amount
        || replenishment_rank(next.stage) < replenishment_rank(existing.stage)
        || !option_extends(&existing.withdrawal, &next.withdrawal, |old, new| {
            old.transaction_intent_hash == new.transaction_intent_hash
                && old.signer_nonce == new.signer_nonce
                && old.fallback_nonce == new.fallback_nonce
                && option_preserved(old.transaction_hash, new.transaction_hash)
                && option_preserved(old.accepted_batch_hash, new.accepted_batch_hash)
                && option_preserved(old.queue_index, new.queue_index)
        })
        || !option_preserved(existing.treasury_credit, next.treasury_credit)
        || !option_extends(&existing.deposit, &next.deposit, |old, new| {
            old.transaction_intent_hash == new.transaction_intent_hash
                && old.signer_nonce == new.signer_nonce
                && option_preserved(old.queue_index, new.queue_index)
                && option_preserved(old.transaction_hash, new.transaction_hash)
        })
        || !option_preserved(existing.pool_credit, next.pool_credit)
    {
        return Err(JournalError::Conflict);
    }
    Ok(())
}

fn option_preserved<T: Copy + Eq>(existing: Option<T>, next: Option<T>) -> bool {
    existing.is_none_or(|value| next == Some(value))
}

fn option_extends<T>(
    existing: &Option<T>,
    next: &Option<T>,
    compatible: impl FnOnce(&T, &T) -> bool,
) -> bool {
    match (existing, next) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(existing), Some(next)) => compatible(existing, next),
    }
}

fn encode_operation(operation: &Operation) -> Vec<u8> {
    let mut out = Vec::new();
    match operation {
        Operation::Outgoing(key, record) => {
            out.push(0);
            put_key(&mut out, *key);
            put_delivery(&mut out, record);
        }
        Operation::Incoming(key, payload) => {
            out.push(1);
            put_key(&mut out, *key);
            put_bytes(&mut out, payload);
        }
        Operation::Transition(key, transition) => {
            out.push(2);
            put_key(&mut out, *key);
            out.push(delivery_tag(*transition));
        }
        Operation::Cursor(peer, stream, sequence) => {
            out.push(3);
            out.extend_from_slice(&peer.to_be_bytes());
            out.extend_from_slice(&stream.to_be_bytes());
            out.extend_from_slice(&sequence.to_be_bytes());
        }
        Operation::Signing(record) => {
            out.push(4);
            out.extend_from_slice(record.digest.as_slice());
            out.extend_from_slice(record.signer.as_slice());
            out.extend_from_slice(&record.signature.0);
            out.extend_from_slice(&record.log_term.to_be_bytes());
            out.extend_from_slice(&record.log_index.to_be_bytes());
        }
        Operation::Tombstone(record) => {
            out.push(5);
            out.extend_from_slice(record.transfer_id.as_slice());
            out.extend_from_slice(record.intent_hash.as_slice());
            out.push(tombstone_tag(record.outcome));
        }
        Operation::Job(job) => {
            out.push(6);
            put_job(&mut out, job);
        }
        Operation::Replay(record) => {
            out.push(7);
            put_replay(&mut out, record);
        }
    }
    out
}

fn decode_operation(bytes: &[u8]) -> Result<Operation, JournalError> {
    let mut reader = BinaryReader::new(bytes);
    let operation = match reader.u8()? {
        0 => Operation::Outgoing(reader.key()?, reader.delivery()?),
        1 => Operation::Incoming(reader.key()?, reader.bytes(MAX_RECORD_BYTES)?),
        2 => Operation::Transition(reader.key()?, reader.delivery_transition()?),
        3 => Operation::Cursor(reader.u32()?, reader.u64()?, reader.u64()?),
        4 => Operation::Signing(SigningRecord {
            digest: reader.b256()?,
            signer: reader.address()?,
            signature: SignatureBytes(reader.array::<65>()?),
            log_term: reader.u64()?,
            log_index: reader.u64()?,
        }),
        5 => Operation::Tombstone(Tombstone {
            transfer_id: reader.b256()?,
            intent_hash: reader.b256()?,
            outcome: reader.tombstone_outcome()?,
        }),
        6 => Operation::Job(Box::new(reader.job()?)),
        7 => Operation::Replay(Box::new(reader.replay()?)),
        _ => return Err(JournalError::Corrupt("unknown operation tag")),
    };
    if !reader.finished() {
        return Err(JournalError::Corrupt("operation trailing bytes"));
    }
    Ok(operation)
}

fn put_key(out: &mut Vec<u8>, key: DeliveryKey) {
    out.extend_from_slice(&key.peer_zone.to_be_bytes());
    out.extend_from_slice(&key.stream.to_be_bytes());
    out.extend_from_slice(&key.sequence.to_be_bytes());
}

fn put_delivery(out: &mut Vec<u8>, record: &DeliveryRecord) {
    out.extend_from_slice(&record.stream.to_be_bytes());
    out.extend_from_slice(&record.sequence.to_be_bytes());
    out.extend_from_slice(record.transfer_id.as_slice());
    put_bytes(out, &record.payload);
    out.push(delivery_tag(record.transition));
    out.extend_from_slice(&record.attempts.to_be_bytes());
    let millis = u64::try_from(record.retry_after.as_millis()).unwrap_or(u64::MAX);
    out.extend_from_slice(&millis.to_be_bytes());
}

fn delivery_tag(transition: DeliveryTransition) -> u8 {
    match transition {
        DeliveryTransition::Queued => 0,
        DeliveryTransition::TransportAcknowledged => 1,
        DeliveryTransition::TerminalReceived => 2,
        DeliveryTransition::SourceDisposed => 3,
    }
}

fn tombstone_tag(outcome: TombstoneOutcome) -> u8 {
    match outcome {
        TombstoneOutcome::Paid => 0,
        TombstoneOutcome::Rejected => 1,
        TombstoneOutcome::Released => 2,
        TombstoneOutcome::Refunded => 3,
    }
}

fn put_job(out: &mut Vec<u8>, job: &ReplenishmentJob) {
    out.extend_from_slice(job.job_id.as_slice());
    out.extend_from_slice(job.source_inventory.as_slice());
    out.extend_from_slice(job.source_fallback.as_slice());
    out.extend_from_slice(job.treasury.as_slice());
    out.extend_from_slice(job.destination_pool.as_slice());
    out.extend_from_slice(job.deposit_refund.as_slice());
    out.extend_from_slice(&(job.contributions.len() as u32).to_be_bytes());
    for contribution in &job.contributions {
        out.extend_from_slice(contribution.transfer_id.as_slice());
        put_u256(out, contribution.amount);
    }
    put_u256(out, job.gross_amount);
    out.push(replenishment_rank(job.stage));
    put_option(out, job.withdrawal.as_ref(), |out, record| {
        out.extend_from_slice(record.transaction_intent_hash.as_slice());
        out.extend_from_slice(&record.signer_nonce.to_be_bytes());
        out.extend_from_slice(&record.fallback_nonce.to_be_bytes());
        put_option(out, record.transaction_hash.as_ref(), |out, value| {
            out.extend_from_slice(value.as_slice())
        });
        put_option(out, record.accepted_batch_hash.as_ref(), |out, value| {
            out.extend_from_slice(value.as_slice())
        });
        put_option(out, record.queue_index.as_ref(), |out, value| {
            out.extend_from_slice(&value.to_be_bytes())
        });
    });
    put_option(out, job.treasury_credit.as_ref(), |out, value| {
        put_u256(out, *value)
    });
    put_option(out, job.deposit.as_ref(), |out, record| {
        out.extend_from_slice(record.transaction_intent_hash.as_slice());
        out.extend_from_slice(&record.signer_nonce.to_be_bytes());
        put_option(out, record.queue_index.as_ref(), |out, value| {
            out.extend_from_slice(&value.to_be_bytes())
        });
        put_option(out, record.transaction_hash.as_ref(), |out, value| {
            out.extend_from_slice(value.as_slice())
        });
    });
    put_option(out, job.pool_credit.as_ref(), |out, value| {
        put_u256(out, *value)
    });
}

fn put_replay(out: &mut Vec<u8>, record: &ReplicatedBlockInput) {
    out.extend_from_slice(&record.log_term.to_be_bytes());
    out.extend_from_slice(&record.log_index.to_be_bytes());
    out.extend_from_slice(&record.block_height.to_be_bytes());
    out.extend_from_slice(record.block_hash.as_slice());
    out.extend_from_slice(record.state_root.as_slice());
    put_bytes(out, &record.block_input);
    out.extend_from_slice(&(record.transactions.len() as u32).to_be_bytes());
    for transaction in &record.transactions {
        put_bytes(out, transaction);
    }
    put_bytes(out, &record.l1_execution_input);
    put_bytes(out, &record.witness);
}

fn put_option<T>(out: &mut Vec<u8>, value: Option<&T>, put: impl FnOnce(&mut Vec<u8>, &T)) {
    match value {
        Some(value) => {
            out.push(1);
            put(out, value);
        }
        None => out.push(0),
    }
}

fn put_u256(out: &mut Vec<u8>, value: U256) {
    out.extend_from_slice(&value.to_be_bytes::<32>());
}

fn put_bytes(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value);
}

struct BinaryReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> BinaryReader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn finished(&self) -> bool {
        self.offset == self.bytes.len()
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], JournalError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(JournalError::Corrupt("length overflow"))?;
        let result = self
            .bytes
            .get(self.offset..end)
            .ok_or(JournalError::Corrupt("truncated operation"))?;
        self.offset = end;
        Ok(result)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], JournalError> {
        Ok(self.take(N)?.try_into().expect("exact length"))
    }

    fn u8(&mut self) -> Result<u8, JournalError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, JournalError> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, JournalError> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn address(&mut self) -> Result<Address, JournalError> {
        Ok(Address::from_slice(self.take(20)?))
    }

    fn b256(&mut self) -> Result<B256, JournalError> {
        Ok(B256::from_slice(self.take(32)?))
    }

    fn u256(&mut self) -> Result<U256, JournalError> {
        Ok(U256::from_be_slice(self.take(32)?))
    }

    fn bytes(&mut self, maximum: usize) -> Result<Vec<u8>, JournalError> {
        let length = self.u32()? as usize;
        if length > maximum {
            return Err(JournalError::Corrupt("byte field exceeds maximum"));
        }
        Ok(self.take(length)?.to_vec())
    }

    fn option<T>(
        &mut self,
        get: impl FnOnce(&mut Self) -> Result<T, JournalError>,
    ) -> Result<Option<T>, JournalError> {
        match self.u8()? {
            0 => Ok(None),
            1 => get(self).map(Some),
            _ => Err(JournalError::Corrupt("invalid option tag")),
        }
    }

    fn key(&mut self) -> Result<DeliveryKey, JournalError> {
        Ok(DeliveryKey {
            peer_zone: self.u32()?,
            stream: self.u64()?,
            sequence: self.u64()?,
        })
    }

    fn delivery_transition(&mut self) -> Result<DeliveryTransition, JournalError> {
        match self.u8()? {
            0 => Ok(DeliveryTransition::Queued),
            1 => Ok(DeliveryTransition::TransportAcknowledged),
            2 => Ok(DeliveryTransition::TerminalReceived),
            3 => Ok(DeliveryTransition::SourceDisposed),
            _ => Err(JournalError::Corrupt("invalid delivery transition")),
        }
    }

    fn delivery(&mut self) -> Result<DeliveryRecord, JournalError> {
        Ok(DeliveryRecord {
            stream: self.u64()?,
            sequence: self.u64()?,
            transfer_id: self.b256()?,
            payload: self.bytes(MAX_RECORD_BYTES)?,
            transition: self.delivery_transition()?,
            attempts: self.u32()?,
            retry_after: Duration::from_millis(self.u64()?),
        })
    }

    fn tombstone_outcome(&mut self) -> Result<TombstoneOutcome, JournalError> {
        match self.u8()? {
            0 => Ok(TombstoneOutcome::Paid),
            1 => Ok(TombstoneOutcome::Rejected),
            2 => Ok(TombstoneOutcome::Released),
            3 => Ok(TombstoneOutcome::Refunded),
            _ => Err(JournalError::Corrupt("invalid tombstone outcome")),
        }
    }

    fn replenishment_stage(&mut self) -> Result<ReplenishmentStage, JournalError> {
        match self.u8()? {
            0 => Ok(ReplenishmentStage::InventoryAllocated),
            1 => Ok(ReplenishmentStage::WithdrawalRequested),
            2 => Ok(ReplenishmentStage::WithdrawalBounced),
            3 => Ok(ReplenishmentStage::TreasuryFunded),
            4 => Ok(ReplenishmentStage::DepositSubmitted),
            5 => Ok(ReplenishmentStage::DepositRefundPending),
            6 => Ok(ReplenishmentStage::PoolCredited),
            _ => Err(JournalError::Corrupt("invalid replenishment stage")),
        }
    }

    fn job(&mut self) -> Result<ReplenishmentJob, JournalError> {
        let job_id = self.b256()?;
        let source_inventory = self.address()?;
        let source_fallback = self.address()?;
        let treasury = self.address()?;
        let destination_pool = self.address()?;
        let deposit_refund = self.address()?;
        let count = self.u32()? as usize;
        if count > 10_000 {
            return Err(JournalError::Corrupt("too many contributions"));
        }
        let mut contributions = Vec::with_capacity(count);
        for _ in 0..count {
            contributions.push(InventoryContribution {
                transfer_id: self.b256()?,
                amount: self.u256()?,
            });
        }
        let gross_amount = self.u256()?;
        let stage = self.replenishment_stage()?;
        let withdrawal = self.option(|reader| {
            Ok(WithdrawalRecord {
                transaction_intent_hash: reader.b256()?,
                signer_nonce: reader.u64()?,
                fallback_nonce: reader.u64()?,
                transaction_hash: reader.option(Self::b256)?,
                accepted_batch_hash: reader.option(Self::b256)?,
                queue_index: reader.option(Self::u64)?,
            })
        })?;
        let treasury_credit = self.option(Self::u256)?;
        let deposit = self.option(|reader| {
            Ok(DepositRecord {
                transaction_intent_hash: reader.b256()?,
                signer_nonce: reader.u64()?,
                queue_index: reader.option(Self::u64)?,
                transaction_hash: reader.option(Self::b256)?,
            })
        })?;
        let pool_credit = self.option(Self::u256)?;
        Ok(ReplenishmentJob {
            job_id,
            source_inventory,
            source_fallback,
            treasury,
            destination_pool,
            deposit_refund,
            contributions,
            gross_amount,
            stage,
            withdrawal,
            treasury_credit,
            deposit,
            pool_credit,
        })
    }

    fn replay(&mut self) -> Result<ReplicatedBlockInput, JournalError> {
        let log_term = self.u64()?;
        let log_index = self.u64()?;
        let block_height = self.u64()?;
        let block_hash = self.b256()?;
        let state_root = self.b256()?;
        let block_input = self.bytes(MAX_RECORD_BYTES)?;
        let transaction_count = self.u32()? as usize;
        if transaction_count > 100_000 {
            return Err(JournalError::Corrupt("too many block transactions"));
        }
        let mut transactions = Vec::with_capacity(transaction_count);
        for _ in 0..transaction_count {
            transactions.push(self.bytes(MAX_RECORD_BYTES)?);
        }
        Ok(ReplicatedBlockInput {
            log_term,
            log_index,
            block_height,
            block_hash,
            state_root,
            block_input,
            transactions,
            l1_execution_input: self.bytes(MAX_RECORD_BYTES)?,
            witness: self.bytes(MAX_RECORD_BYTES)?,
        })
    }
}

fn sync_directory(path: &Path) -> Result<(), std::io::Error> {
    File::open(path)?.sync_all()
}

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;

    use super::*;

    fn temporary_directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "zone-fast-transfer-{name}-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = fs::remove_dir_all(&directory);
        directory
    }

    #[test]
    fn restart_reconstructs_unfinished_signatures_and_tombstones() {
        let directory = temporary_directory("replay");
        {
            let store = DurableJournal::open(&directory).unwrap();
            let record = DeliveryRecord::queued(1, 1, B256::repeat_byte(1), vec![1, 2, 3]);
            store.persist_outgoing(2, &record).unwrap();
            store.persist_incoming(2, 1, 1, &[4, 5]).unwrap();
            store.advance_contiguous_cursor(2, 1, 1).unwrap();
            store
                .persist_signing_record(SigningRecord {
                    digest: B256::repeat_byte(6),
                    signer: Address::repeat_byte(7),
                    signature: SignatureBytes([8; 65]),
                    log_term: 2,
                    log_index: 3,
                })
                .unwrap();
            store
                .persist_tombstone(Tombstone {
                    transfer_id: B256::repeat_byte(1),
                    intent_hash: B256::repeat_byte(9),
                    outcome: TombstoneOutcome::Paid,
                })
                .unwrap();
            store.snapshot().unwrap();
        }
        let store = DurableJournal::open(&directory).unwrap();
        assert_eq!(store.load_unfinished(2).unwrap().len(), 1);
        assert!(
            store
                .signing_record(B256::repeat_byte(6), Address::repeat_byte(7))
                .unwrap()
                .is_some()
        );
        assert!(store.tombstone(B256::repeat_byte(1)).unwrap().is_some());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn incomplete_tail_is_truncated_without_losing_acknowledged_prefix() {
        let directory = temporary_directory("partial");
        {
            let store = DurableJournal::open(&directory).unwrap();
            store
                .persist_outgoing(
                    2,
                    &DeliveryRecord::queued(1, 1, B256::repeat_byte(1), vec![1]),
                )
                .unwrap();
        }
        let path = directory.join(JOURNAL_FILE);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"FTJ1\0")
            .unwrap();
        let store = DurableJournal::open(&directory).unwrap();
        assert_eq!(store.load_unfinished(2).unwrap().len(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn cursor_cannot_skip_an_incoming_gap() {
        let directory = temporary_directory("cursor");
        let store = DurableJournal::open(&directory).unwrap();
        store.persist_incoming(2, 1, 2, &[1]).unwrap();
        assert!(matches!(
            store.advance_contiguous_cursor(2, 1, 2),
            Err(JournalError::CursorGap)
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn replenishment_identity_cannot_change_at_the_same_stage() {
        let directory = temporary_directory("job-conflict");
        let store = DurableJournal::open(&directory).unwrap();
        let mut job = ReplenishmentJob::allocate(
            B256::repeat_byte(1),
            Address::repeat_byte(2),
            Address::repeat_byte(3),
            Address::repeat_byte(4),
            Address::repeat_byte(5),
            Address::repeat_byte(6),
            vec![InventoryContribution {
                transfer_id: B256::repeat_byte(7),
                amount: U256::from(10),
            }],
        )
        .unwrap();
        store.persist_replenishment_job(job.clone()).unwrap();
        job.treasury = Address::repeat_byte(9);
        assert!(matches!(
            store.persist_replenishment_job(job),
            Err(JournalError::Conflict)
        ));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn committed_block_replay_material_survives_snapshot() {
        let directory = temporary_directory("replay-block");
        let record = ReplicatedBlockInput {
            log_term: 4,
            log_index: 9,
            block_height: 12,
            block_hash: B256::repeat_byte(1),
            state_root: B256::repeat_byte(2),
            block_input: vec![3, 4],
            transactions: vec![vec![5], vec![6, 7]],
            l1_execution_input: vec![8, 9],
            witness: vec![10, 11],
        };
        {
            let store = DurableJournal::open(&directory).unwrap();
            assert!(store.persist_replicated_block(record.clone()).unwrap());
            assert!(!store.persist_replicated_block(record.clone()).unwrap());
            store.snapshot().unwrap();
        }
        let store = DurableJournal::open(&directory).unwrap();
        assert_eq!(store.replicated_blocks_from(9).unwrap(), vec![record]);
        assert!(store.replicated_blocks_from(10).unwrap().is_empty());
        fs::remove_dir_all(directory).unwrap();
    }
}
