//! Durable snapshots, journals, and findings for the Zone checker.

mod codec;
mod model;
mod schema;
#[cfg(test)]
mod tests;
mod validation;

pub(crate) use validation::make_finding;
use validation::{
    validate_checkpoint, validate_coverage_advance, validate_finding, validate_metadata,
    validate_state,
};

pub(crate) use model::{
    BlockNumHash, ChainCut, Checkpoint, CheckpointChunk, CheckpointChunkKey, CheckpointId,
    CheckpointManifest, Coverage, Finding, FindingKey, Identity, JournalEntry, MetaValue, Metadata,
    Snapshot,
};

use crate::{CheckerBlockedReason, kernel::State};
use reth_db::{
    Database, DatabaseEnv, DatabaseEnvKind,
    cursor::{DbCursorRO, DbCursorRW},
    is_database_empty,
    mdbx::{DatabaseArguments, init_db_for},
    open_db_read_only,
    transaction::{DbTx, DbTxMut},
};
use schema::{CheckpointChunks, Checkpoints, Findings, Journal, Meta, MetaKey, PersistenceTables};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

pub(crate) const SCHEMA_VERSION: u32 = 9;
const CHECKPOINT_INTERVAL: u64 = 64;
const CHECKPOINT_CHUNK_SIZE: usize = 1024 * 1024;
/// Minimum Zone history retained for local reorg recovery.
///
/// This is an availability horizon, not a Zone finality claim.
const MIN_RETAINED_JOURNAL_BLOCKS: u64 = 16_384;
/// Result returned by durable persistence operations.
pub(crate) type Result<T> = std::result::Result<T, PersistenceError>;

/// Failure while opening, validating, or updating durable checker state.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PersistenceError {
    #[error("database open failed: {0}")]
    Open(#[from] eyre::Report),
    #[error("database error: {0}")]
    Database(#[from] reth_db::DatabaseError),
    #[error("database codec error: {0}")]
    Codec(#[from] codec::CodecError),
    #[error(
        "incompatible checker schema: expected {expected}, found {actual}; rebuild at {rebuild_path}"
    )]
    Schema {
        expected: u32,
        actual: u32,
        rebuild_path: PathBuf,
    },
    #[error("database identity mismatch")]
    Identity,
    #[error("stale database snapshot")]
    StaleSnapshot,
    #[error("reorg ancestor {ancestor:?} is outside retained history at {recovery:?}")]
    ReorgBeyondRetention {
        ancestor: BlockNumHash,
        recovery: BlockNumHash,
    },
    #[error("invalid checker database: {0}")]
    Invalid(String),
    #[cfg(test)]
    #[error("injected transaction abort")]
    InjectedAbort,
}

/// Sole-writer handle for one durable checker database.
pub(crate) struct Persistence {
    db: Arc<DatabaseEnv>,
    identity: Identity,
    retention: RetentionPolicy,
    #[cfg(test)]
    abort_next_write: AtomicBool,
}

/// Fixed persistence cadence and local reorg-recovery horizon.
#[derive(Debug, Clone, Copy)]
struct RetentionPolicy {
    checkpoint_interval: u64,
    minimum_journal_blocks: u64,
}

impl RetentionPolicy {
    const PRODUCTION: Self = Self {
        checkpoint_interval: CHECKPOINT_INTERVAL,
        minimum_journal_blocks: MIN_RETAINED_JOURNAL_BLOCKS,
    };
}

impl Persistence {
    /// Create, verify, and atomically publish an initial checker database.
    pub(crate) fn create_atomic(
        target: &Path,
        identity: Identity,
        cut: ChainCut,
        state: State,
    ) -> Result<Snapshot> {
        if target.exists() {
            return Err(invalid("checkpoint target already exists"));
        }
        let parent = target
            .parent()
            .ok_or_else(|| invalid("checkpoint target has no sibling directory"))?;
        let staging = tempfile::Builder::new()
            .prefix(".checker-staging-")
            .tempdir_in(parent)
            .map_err(|error| invalid(format!("cannot create checkpoint staging: {error}")))?;

        let result = (|| {
            let (store, snapshot) = Self::create(staging.path(), identity, cut, state)?;
            drop(store);
            let (reopened, verified) = Self::open(staging.path(), identity)?;
            drop(reopened);
            if snapshot != verified {
                return Err(invalid("genesis checkpoint changed across final reopen"));
            }
            Ok(verified)
        })();
        match result {
            Ok(snapshot) => {
                fs::rename(staging.path(), target).map_err(|error| {
                    invalid(format!("cannot atomically publish checkpoint: {error}"))
                })?;
                Ok(snapshot)
            }
            Err(error) => Err(error),
        }
    }

    /// Read the authenticated identity from an existing checker database.
    /// This never creates or repairs a database and is intended for runtime
    /// preflight before opening the sole-writer handle.
    pub(crate) fn inspect_identity(path: impl AsRef<Path>) -> Result<Identity> {
        let path = path.as_ref();
        probe(path)?;
        let db = open_db_read_only(path, DatabaseArguments::default())?;
        let tx = db.tx()?;
        let meta = read_metadata(&tx)?;
        validate_metadata(&meta)?;
        tx.commit()?;
        Ok(meta.identity)
    }

    /// Read and reconstruct an existing database without opening a writer.
    pub(crate) fn inspect_snapshot(path: impl AsRef<Path>) -> Result<Snapshot> {
        let path = path.as_ref();
        probe(path)?;
        let db = open_db_read_only(path, DatabaseArguments::default())?;
        let tx = db.tx()?;
        let meta = read_metadata(&tx)?;
        validate_metadata(&meta)?;
        tx.commit()?;
        Self {
            db: Arc::new(db),
            identity: meta.identity,
            retention: RetentionPolicy::PRODUCTION,
            #[cfg(test)]
            abort_next_write: AtomicBool::new(false),
        }
        .load()
    }

    /// Create a fresh database initialized at the supplied chain cut.
    pub(crate) fn create(
        path: impl AsRef<Path>,
        identity: Identity,
        cut: ChainCut,
        state: State,
    ) -> Result<(Self, Snapshot)> {
        let path = path.as_ref().to_path_buf();
        if !is_database_empty(&path) {
            return Err(PersistenceError::Invalid("fresh path is not empty".into()));
        }
        validate_state(&state, identity)?;
        let db = init_db_for::<_, PersistenceTables>(&path, DatabaseArguments::default())?;
        let this = Self {
            db: Arc::new(db),
            identity,
            retention: RetentionPolicy::PRODUCTION,
            #[cfg(test)]
            abort_next_write: AtomicBool::new(false),
        };
        let id = CheckpointId::from(cut.zone);
        let meta = Metadata {
            identity,
            recovery_checkpoint: id,
            active_checkpoint: id,
            verified_zone_tip: cut.zone,
            imported_tempo_tip: cut.tempo,
            observed_zone_tip: cut.zone,
            active_finding: None,
            cleared_findings: 0,
            last_cleared_finding: None,
            coverage: Coverage::Complete,
            blocked: None,
        };
        let checkpoint = Checkpoint {
            cut,
            state: state.clone(),
        };
        codec::encode(&meta)?;
        let tx = this.db.tx_mut()?;
        Self::write_checkpoint(&tx, id, checkpoint)?;
        tx.put::<Meta>(MetaKey::Version, MetaValue::Version(SCHEMA_VERSION))?;
        tx.put::<Meta>(
            MetaKey::Metadata,
            MetaValue::Metadata(Box::new(meta.clone())),
        )?;
        tx.commit()?;
        Ok((
            this,
            Snapshot {
                meta,
                state: Arc::new(state),
            },
        ))
    }

    /// Open the sole-writer database handle and reconstruct its active snapshot.
    pub(crate) fn open(path: impl AsRef<Path>, identity: Identity) -> Result<(Self, Snapshot)> {
        let path = path.as_ref().to_path_buf();
        probe(&path)?;
        let db = DatabaseEnv::open(&path, DatabaseEnvKind::RW, DatabaseArguments::default())?;
        let this = Self {
            db: Arc::new(db),
            identity,
            retention: RetentionPolicy::PRODUCTION,
            #[cfg(test)]
            abort_next_write: AtomicBool::new(false),
        };
        this.load().map(|snapshot| (this, snapshot))
    }

    /// Reconstruct and validate the active durable snapshot.
    pub(crate) fn load(&self) -> Result<Snapshot> {
        let identity = self.identity;
        let tx = self.db.tx()?;
        let meta = read_metadata(&tx)?;
        if meta.identity != identity {
            return Err(PersistenceError::Identity);
        }
        validate_metadata(&meta)?;
        let recovery = Self::read_checkpoint(&tx, meta.recovery_checkpoint)?;
        validate_checkpoint(meta.recovery_checkpoint, &recovery, identity)?;
        let active = Self::read_checkpoint(&tx, meta.active_checkpoint)?;
        validate_checkpoint(meta.active_checkpoint, &active, identity)?;
        let mut checkpoints = tx.cursor_read::<Checkpoints>()?;
        let bootstrap_id = checkpoints
            .first()?
            .map(|(id, _)| id)
            .ok_or_else(|| invalid("bootstrap checkpoint is missing"))?;
        if recovery.cut.zone.number > meta.verified_zone_tip.number {
            return Err(invalid("recovery checkpoint exceeds verified tip"));
        }
        let mut retained_tip = recovery.cut.zone;
        let mut retained_imported = recovery.cut.tempo;
        for height in retained_tip.number.saturating_add(1)..=active.cut.zone.number {
            let entry = tx
                .get::<Journal>(height)?
                .ok_or_else(|| invalid(format!("missing journal height {height}")))?;
            Self::validate_journal_entry(retained_tip, retained_imported, height, &entry)?;
            retained_tip = entry.zone;
            retained_imported = entry.imported_tempo;
        }
        if retained_tip != active.cut.zone || retained_imported != active.cut.tempo {
            return Err(invalid(
                "active checkpoint is not on retained journal history",
            ));
        }
        let mut state = active.state.clone();
        validate_state(&state, identity)?;
        let mut tip = active.cut.zone;
        let mut imported = active.cut.tempo;
        for height in tip.number.saturating_add(1)..=meta.verified_zone_tip.number {
            let entry = tx
                .get::<Journal>(height)?
                .ok_or_else(|| invalid(format!("missing journal height {height}")))?;
            Self::apply_journal_entry(&mut state, &mut tip, &mut imported, height, entry)?;
        }
        if tip != meta.verified_zone_tip || imported != meta.imported_tempo_tip {
            return Err(invalid("journal does not reach verified tip"));
        }
        let mut journal = tx.cursor_read::<Journal>()?;
        if meta.recovery_checkpoint != bootstrap_id
            && meta.recovery_checkpoint.height < meta.verified_zone_tip.number
            && journal
                .first()?
                .is_none_or(|(height, _)| height != meta.recovery_checkpoint.height + 1)
        {
            return Err(invalid("journal does not begin after recovery checkpoint"));
        }
        if journal
            .last()?
            .is_some_and(|(height, _)| height > meta.verified_zone_tip.number)
        {
            return Err(invalid("journal extends beyond verified tip"));
        }
        validate_state(&state, identity)?;
        if let Some(key) = meta.active_finding {
            let finding = tx
                .get::<Findings>(key)?
                .ok_or_else(|| invalid("active finding row is missing"))?;
            validate_finding(key, &finding, Some(&meta))?;
        }
        tx.commit()?;
        Ok(Snapshot {
            meta,
            state: Arc::new(state),
        })
    }

    /// Persist one contiguous verified transition and its resulting state.
    pub(crate) fn apply(
        &self,
        prior: &Snapshot,
        entry: JournalEntry,
        observed: BlockNumHash,
        coverage: Coverage,
    ) -> Result<Snapshot> {
        let mut candidate = prior.state.as_ref().clone();
        candidate
            .apply(&entry.delta)
            .map_err(|error| invalid(error.to_string()))?;
        validate_state(&candidate, self.identity)?;
        codec::encode(&entry)?;
        self.write(prior, candidate, |tx, meta, candidate| {
            if entry.zone.number
                != meta
                    .verified_zone_tip
                    .number
                    .checked_add(1)
                    .ok_or_else(|| invalid("height overflow"))?
                || entry.parent != meta.verified_zone_tip
                || entry.imported_tempo.number <= meta.imported_tempo_tip.number
                || entry.imported_tempo_parent != meta.imported_tempo_tip
            {
                return Err(invalid("journal parent or height mismatch"));
            }
            validate_coverage_advance(meta, entry.zone, observed, &coverage)?;
            if tx.get::<Journal>(entry.zone.number)?.is_some() {
                return Err(invalid("journal height conflict"));
            }
            tx.put::<Journal>(entry.zone.number, entry.clone())?;
            meta.verified_zone_tip = entry.zone;
            meta.imported_tempo_tip = entry.imported_tempo;
            meta.coverage = coverage;
            meta.observed_zone_tip = observed;
            if entry
                .zone
                .number
                .saturating_sub(meta.active_checkpoint.height)
                >= self.retention.checkpoint_interval
            {
                self.checkpoint_in_tx(tx, meta, candidate)?;
            }
            Ok(())
        })
    }

    /// Record the latest canonical Zone block retained by the local node.
    pub(crate) fn record_observed_tip(
        &self,
        prior: &Snapshot,
        observed: BlockNumHash,
    ) -> Result<Metadata> {
        if observed == prior.meta.observed_zone_tip {
            return Ok(prior.meta.clone());
        }
        if observed.number < prior.meta.verified_zone_tip.number
            || (observed.number == prior.meta.verified_zone_tip.number
                && observed != prior.meta.verified_zone_tip)
        {
            return Err(invalid("observed Zone tip precedes verified tip"));
        }
        self.write_metadata(prior, |_tx, meta| {
            let coverage = match meta.coverage {
                Coverage::Gap {
                    first_unchecked, ..
                } => {
                    if observed.number < first_unchecked.number
                        || (observed.number == first_unchecked.number
                            && observed != first_unchecked)
                    {
                        return Err(invalid("observed tip precedes the active finding"));
                    }
                    Coverage::Gap {
                        first_unchecked,
                        observed_through: observed,
                    }
                }
                Coverage::Complete | Coverage::Recovering => {
                    if observed == meta.verified_zone_tip {
                        Coverage::Complete
                    } else {
                        Coverage::Recovering
                    }
                }
            };
            meta.observed_zone_tip = observed;
            meta.coverage = coverage;
            Ok(())
        })
    }

    /// Return retained verified coordinates used to locate a reorg ancestor.
    pub(crate) fn retained_zone_coordinates(&self) -> Result<Vec<BlockNumHash>> {
        let tx = self.db.tx()?;
        let meta = read_metadata(&tx)?;
        let recovery = Self::read_checkpoint(&tx, meta.recovery_checkpoint)?;
        let mut coordinates = Vec::new();
        coordinates.push(recovery.cut.zone);
        for height in recovery.cut.zone.number.saturating_add(1)..=meta.verified_zone_tip.number {
            let entry = tx
                .get::<Journal>(height)?
                .ok_or_else(|| invalid("retained journal is incomplete"))?;
            coordinates.push(entry.zone);
        }
        tx.commit()?;
        Ok(coordinates)
    }

    /// Persist a checkpoint for the already active snapshot.
    #[cfg(test)]
    pub(crate) fn checkpoint_current(&self, prior: &Snapshot) -> Result<Snapshot> {
        validate_state(&prior.state, self.identity)?;
        self.write(prior, prior.state.as_ref().clone(), |tx, meta, state| {
            self.checkpoint_in_tx(tx, meta, state)?;
            Ok(())
        })
    }

    /// Assert a supplied snapshot is active, then checkpoint it.
    #[cfg(test)]
    pub(crate) fn checkpoint(
        &self,
        identity: Identity,
        cut: ChainCut,
        state: State,
    ) -> Result<Snapshot> {
        if identity != self.identity {
            return Err(PersistenceError::Identity);
        }
        let prior = self.load()?;
        if cut.zone != prior.meta.verified_zone_tip
            || cut.tempo != prior.meta.imported_tempo_tip
            || state != *prior.state
        {
            return Err(invalid("checkpoint is not current"));
        }
        self.checkpoint_current(&prior)
    }

    /// Atomically persist a divergence finding and its unchecked suffix.
    pub(crate) fn record_divergence(
        &self,
        prior: &Snapshot,
        key: FindingKey,
        finding: Finding,
        observed_through: BlockNumHash,
    ) -> Result<Snapshot> {
        self.write(prior, prior.state.as_ref().clone(), |tx, meta, _state| {
            let first_unchecked = finding.zone;
            if observed_through.number < first_unchecked.number {
                return Err(invalid("divergence suffix precedes its finding"));
            }
            let observed_through = match meta.coverage {
                Coverage::Complete | Coverage::Recovering => observed_through,
                Coverage::Gap {
                    first_unchecked: existing_first,
                    observed_through: existing_through,
                } => {
                    if existing_first.number != first_unchecked.number {
                        return Err(invalid("divergence does not begin at the coverage gap"));
                    }
                    if existing_first != first_unchecked
                        || observed_through.number >= existing_through.number
                    {
                        observed_through
                    } else {
                        existing_through
                    }
                }
            };
            Self::record_finding_tx(tx, meta, key, finding)?;
            meta.observed_zone_tip = observed_through;
            meta.coverage = Coverage::Gap {
                first_unchecked,
                observed_through,
            };
            Ok(())
        })
    }

    /// Persist that the checker cannot safely advance verification.
    pub(crate) fn record_blocked_current(&self, reason: CheckerBlockedReason) -> Result<Snapshot> {
        let current = self.load()?;
        self.write(
            &current,
            current.state.as_ref().clone(),
            |_tx, meta, _state| {
                meta.blocked = Some(reason);
                Ok(())
            },
        )
    }

    /// Rewind durable journal and coverage state to a retained ancestor.
    pub(crate) fn reorg(&self, prior: &Snapshot, ancestor: BlockNumHash) -> Result<Snapshot> {
        if ancestor.number > prior.meta.verified_zone_tip.number {
            return Err(invalid("reorg ancestor exceeds verified history"));
        }
        self.reorg_verified(prior, ancestor)
    }

    /// Reconstruct verified state and remove journal entries after the ancestor.
    fn reorg_verified(&self, prior: &Snapshot, ancestor: BlockNumHash) -> Result<Snapshot> {
        let snapshot = self.reconstruct_at(ancestor)?;
        self.write(prior, snapshot.state.as_ref().clone(), |tx, meta, state| {
            let previous_active = meta.active_checkpoint;
            let previous_recovery = meta.recovery_checkpoint;
            let checkpoint = Checkpoint {
                cut: ChainCut {
                    zone: ancestor,
                    tempo: snapshot.meta.imported_tempo_tip,
                },
                state: state.clone(),
            };
            let active_checkpoint = CheckpointId::from(ancestor);
            Self::write_checkpoint(tx, active_checkpoint, checkpoint)?;
            let mut cursor = tx.cursor_write::<Journal>()?;
            while let Some((height, _)) = cursor.last()? {
                if height <= ancestor.number {
                    break;
                }
                cursor.delete_current()?;
            }
            meta.verified_zone_tip = ancestor;
            meta.imported_tempo_tip = snapshot.meta.imported_tempo_tip;
            meta.active_checkpoint = active_checkpoint;
            if meta.observed_zone_tip.number > ancestor.number {
                meta.observed_zone_tip = ancestor;
            }
            if let Some(cleared) = meta.active_finding.take() {
                meta.cleared_findings = meta.cleared_findings.saturating_add(1);
                meta.last_cleared_finding = Some(cleared);
            }
            meta.coverage = Coverage::Complete;
            Self::remove_obsolete_checkpoints(tx, meta, previous_active, previous_recovery)?;
            Ok(())
        })
    }

    /// Reconstruct a canonical snapshot at a verified reorg ancestor.
    fn reconstruct_at(&self, ancestor: BlockNumHash) -> Result<Snapshot> {
        let identity = self.identity;
        let tx = self.db.tx()?;
        let meta = read_metadata(&tx)?;
        if meta.identity != identity || ancestor.number > meta.verified_zone_tip.number {
            return Err(invalid("invalid reorg ancestor"));
        }
        let recovery = Self::read_checkpoint(&tx, meta.recovery_checkpoint)?;
        validate_checkpoint(meta.recovery_checkpoint, &recovery, identity)?;
        if ancestor.number < recovery.cut.zone.number
            || (ancestor.number == recovery.cut.zone.number && ancestor != recovery.cut.zone)
        {
            return Err(PersistenceError::ReorgBeyondRetention {
                ancestor,
                recovery: recovery.cut.zone,
            });
        }

        let (checkpoint_id, checkpoint) = if meta.active_checkpoint.height <= ancestor.number {
            let checkpoint = Self::read_checkpoint(&tx, meta.active_checkpoint)?;
            validate_checkpoint(meta.active_checkpoint, &checkpoint, identity)?;
            (meta.active_checkpoint, checkpoint)
        } else {
            (meta.recovery_checkpoint, recovery)
        };
        let mut state = checkpoint.state;
        let mut tip = checkpoint.cut.zone;
        let mut imported = checkpoint.cut.tempo;
        for height in tip.number.saturating_add(1)..=ancestor.number {
            let entry = tx
                .get::<Journal>(height)?
                .ok_or_else(|| invalid("missing reorg journal"))?;
            Self::apply_journal_entry(&mut state, &mut tip, &mut imported, height, entry)
                .map_err(|_| invalid("non-contiguous reorg journal"))?;
        }
        if tip != ancestor {
            return Err(invalid("ancestor hash conflict"));
        }
        validate_state(&state, identity)?;
        let mut out = meta;
        out.active_checkpoint = checkpoint_id;
        out.verified_zone_tip = ancestor;
        out.imported_tempo_tip = imported;
        Ok(Snapshot {
            meta: out,
            state: Arc::new(state),
        })
    }

    /// Write or verify the immutable checkpoint at the current metadata tips.
    fn checkpoint_in_tx(
        &self,
        tx: &<DatabaseEnv as Database>::TXMut,
        meta: &mut Metadata,
        state: &State,
    ) -> Result<()> {
        let previous_active = meta.active_checkpoint;
        let previous_recovery = meta.recovery_checkpoint;
        let cut = ChainCut {
            zone: meta.verified_zone_tip,
            tempo: meta.imported_tempo_tip,
        };
        let id = CheckpointId::from(cut.zone);
        let checkpoint = Checkpoint {
            cut,
            state: state.clone(),
        };
        Self::write_checkpoint(tx, id, checkpoint)?;
        meta.active_checkpoint = id;
        self.advance_recovery_checkpoint(tx, meta)?;
        Self::remove_obsolete_checkpoints(tx, meta, previous_active, previous_recovery)?;
        Ok(())
    }

    /// Advance the durable local reorg floor and prune history before it.
    fn advance_recovery_checkpoint(
        &self,
        tx: &<DatabaseEnv as Database>::TXMut,
        meta: &mut Metadata,
    ) -> Result<()> {
        let Some(target_height) = meta
            .verified_zone_tip
            .number
            .checked_sub(self.retention.minimum_journal_blocks)
        else {
            return Ok(());
        };
        if target_height <= meta.recovery_checkpoint.height {
            return Ok(());
        }

        let checkpoint = Self::reconstruct_from_checkpoint(
            tx,
            meta,
            meta.recovery_checkpoint,
            target_height,
            self.identity,
        )?;
        let id = CheckpointId::from(checkpoint.cut.zone);
        Self::write_checkpoint(tx, id, checkpoint)?;
        meta.recovery_checkpoint = id;
        Self::prune_journal_through(tx, target_height)?;
        Ok(())
    }

    /// Delete journal entries included in the recovery checkpoint.
    fn prune_journal_through(tx: &<DatabaseEnv as Database>::TXMut, through: u64) -> Result<()> {
        let mut cursor = tx.cursor_write::<Journal>()?;
        while let Some((height, _)) = cursor.first()? {
            if height > through {
                break;
            }
            cursor.delete_current()?;
        }
        Ok(())
    }

    /// Write an immutable checkpoint, rejecting a conflicting existing cut.
    fn write_checkpoint(
        tx: &<DatabaseEnv as Database>::TXMut,
        id: CheckpointId,
        checkpoint: Checkpoint,
    ) -> Result<()> {
        let encoded = codec::encode_unbounded(&checkpoint)?;
        let chunk_count = encoded.len().div_ceil(CHECKPOINT_CHUNK_SIZE);
        let chunk_count = u32::try_from(chunk_count)
            .map_err(|_| invalid("checkpoint requires too many chunks"))?;
        let encoded_len = u64::try_from(encoded.len())
            .map_err(|_| invalid("checkpoint encoded length exceeds u64"))?;
        let manifest = CheckpointManifest {
            cut: checkpoint.cut,
            chunk_count,
            encoded_len,
            commitment: alloy_primitives::keccak256(&encoded),
        };
        codec::encode(&manifest)?;
        if tx.get::<Checkpoints>(id)?.is_some() {
            if Self::read_checkpoint(tx, id)? != checkpoint {
                return Err(invalid("checkpoint identity is immutable"));
            }
        } else {
            for (index, bytes) in encoded.chunks(CHECKPOINT_CHUNK_SIZE).enumerate() {
                let index = u32::try_from(index)
                    .map_err(|_| invalid("checkpoint chunk index exceeds u32"))?;
                tx.put::<CheckpointChunks>(
                    CheckpointChunkKey {
                        checkpoint: id,
                        index,
                    },
                    CheckpointChunk(bytes.to_vec()),
                )?;
            }
            tx.put::<Checkpoints>(id, manifest)?;
        }
        Ok(())
    }

    /// Authenticate and reconstruct one chunked checkpoint.
    fn read_checkpoint<T: DbTx>(tx: &T, id: CheckpointId) -> Result<Checkpoint> {
        let manifest = tx
            .get::<Checkpoints>(id)?
            .ok_or_else(|| invalid("checkpoint manifest is missing"))?;
        if manifest.chunk_count == 0 {
            return Err(invalid("checkpoint manifest has no chunks"));
        }
        let capacity = usize::try_from(manifest.encoded_len)
            .map_err(|_| invalid("checkpoint encoded length exceeds usize"))?;
        let mut encoded = Vec::with_capacity(capacity);
        for index in 0..manifest.chunk_count {
            let chunk = tx
                .get::<CheckpointChunks>(CheckpointChunkKey {
                    checkpoint: id,
                    index,
                })?
                .ok_or_else(|| invalid("checkpoint chunk is missing"))?;
            if chunk.0.is_empty() || chunk.0.len() > CHECKPOINT_CHUNK_SIZE {
                return Err(invalid("checkpoint chunk has invalid size"));
            }
            encoded.extend_from_slice(&chunk.0);
        }
        if encoded.len() != capacity || alloy_primitives::keccak256(&encoded) != manifest.commitment
        {
            return Err(invalid("checkpoint chunk commitment mismatch"));
        }
        let checkpoint: Checkpoint = codec::decode_unbounded(&encoded)?;
        if checkpoint.cut != manifest.cut {
            return Err(invalid("checkpoint manifest cut mismatch"));
        }
        Ok(checkpoint)
    }

    /// Retain only bootstrap, recovery, and active checkpoints.
    fn remove_obsolete_checkpoints(
        tx: &<DatabaseEnv as Database>::TXMut,
        meta: &Metadata,
        previous_active: CheckpointId,
        previous_recovery: CheckpointId,
    ) -> Result<()> {
        let bootstrap = Self::bootstrap_checkpoint_id(tx)?;
        for id in [previous_active, previous_recovery] {
            if id != bootstrap && id != meta.active_checkpoint && id != meta.recovery_checkpoint {
                Self::delete_checkpoint(tx, id)?;
            }
        }
        Ok(())
    }

    /// Delete one checkpoint manifest and all of its chunks.
    fn delete_checkpoint(tx: &<DatabaseEnv as Database>::TXMut, id: CheckpointId) -> Result<()> {
        if let Some(manifest) = tx.get::<Checkpoints>(id)? {
            for index in 0..manifest.chunk_count {
                tx.delete::<CheckpointChunks>(
                    CheckpointChunkKey {
                        checkpoint: id,
                        index,
                    },
                    None,
                )?;
            }
            tx.delete::<Checkpoints>(id, None)?;
        }
        Ok(())
    }

    /// Read the immutable bootstrap checkpoint identity.
    fn bootstrap_checkpoint_id(tx: &<DatabaseEnv as Database>::TXMut) -> Result<CheckpointId> {
        tx.cursor_read::<Checkpoints>()?
            .first()?
            .map(|(id, _)| id)
            .ok_or_else(|| invalid("bootstrap checkpoint is missing"))
    }

    /// Reconstruct a checkpoint state by applying journal entries through `height`.
    fn reconstruct_from_checkpoint(
        tx: &<DatabaseEnv as Database>::TXMut,
        meta: &Metadata,
        checkpoint_id: CheckpointId,
        height: u64,
        identity: Identity,
    ) -> Result<Checkpoint> {
        let checkpoint = Self::read_checkpoint(tx, checkpoint_id)?;
        validate_checkpoint(checkpoint_id, &checkpoint, identity)?;
        if height < checkpoint.cut.zone.number || height > meta.verified_zone_tip.number {
            return Err(invalid("recovery checkpoint target is out of range"));
        }
        let mut state = checkpoint.state;
        let mut zone = checkpoint.cut.zone;
        let mut tempo = checkpoint.cut.tempo;
        for number in zone.number.saturating_add(1)..=height {
            let entry = tx
                .get::<Journal>(number)?
                .ok_or_else(|| invalid("missing recovery journal"))?;
            Self::apply_journal_entry(&mut state, &mut zone, &mut tempo, number, entry)?;
        }
        validate_state(&state, identity)?;
        Ok(Checkpoint {
            cut: ChainCut { zone, tempo },
            state,
        })
    }

    /// Validate and apply one contiguous journal transition.
    fn apply_journal_entry(
        state: &mut State,
        zone: &mut BlockNumHash,
        tempo: &mut BlockNumHash,
        height: u64,
        entry: JournalEntry,
    ) -> Result<()> {
        Self::validate_journal_entry(*zone, *tempo, height, &entry)?;
        state
            .apply(&entry.delta)
            .map_err(|error| invalid(error.to_string()))?;
        *zone = entry.zone;
        *tempo = entry.imported_tempo;
        Ok(())
    }

    /// Validate one journal transition without replaying its state delta.
    fn validate_journal_entry(
        zone: BlockNumHash,
        tempo: BlockNumHash,
        height: u64,
        entry: &JournalEntry,
    ) -> Result<()> {
        if entry.zone.number != height
            || entry.parent != zone
            || entry.delta.validate().is_err()
            || entry.imported_tempo.number <= tempo.number
            || entry.imported_tempo_parent != tempo
        {
            return Err(invalid(format!("conflicting journal height {height}")));
        }
        Ok(())
    }

    /// Insert or update one finding and install its active latch in `tx`.
    fn record_finding_tx(
        tx: &<DatabaseEnv as Database>::TXMut,
        meta: &mut Metadata,
        key: FindingKey,
        finding: Finding,
    ) -> Result<()> {
        validate_finding(key, &finding, Some(meta))?;
        codec::encode(&finding)?;
        if let Some(existing) = tx.get::<Findings>(key)? {
            if !same_finding_evidence(&existing, &finding) {
                return Err(invalid("conflicting same-height finding evidence"));
            }
            if existing.summary != finding.summary {
                tx.put::<Findings>(key, finding)?;
            }
        } else {
            tx.put::<Findings>(key, finding)?;
        }
        meta.active_finding = Some(key);
        Ok(())
    }

    /// Apply one metadata mutation against the expected prior snapshot atomically.
    fn write<F>(&self, prior: &Snapshot, state: State, f: F) -> Result<Snapshot>
    where
        F: FnOnce(&<DatabaseEnv as Database>::TXMut, &mut Metadata, &State) -> Result<()>,
    {
        let meta = self.write_metadata(prior, |tx, meta| f(tx, meta, &state))?;
        Ok(Snapshot {
            meta,
            state: Arc::new(state),
        })
    }

    /// Apply a metadata mutation without cloning the unchanged checker state.
    fn write_metadata<F>(&self, prior: &Snapshot, f: F) -> Result<Metadata>
    where
        F: FnOnce(&<DatabaseEnv as Database>::TXMut, &mut Metadata) -> Result<()>,
    {
        let tx = self.db.tx_mut()?;
        let mut meta = read_metadata(&tx)?;
        if meta.identity != self.identity {
            return Err(PersistenceError::Identity);
        }
        if meta != prior.meta {
            return Err(PersistenceError::StaleSnapshot);
        }
        f(&tx, &mut meta)?;
        validate_metadata(&meta)?;
        tx.put::<Meta>(
            MetaKey::Metadata,
            MetaValue::Metadata(Box::new(meta.clone())),
        )?;
        #[cfg(test)]
        if self.abort_next_write.swap(false, Ordering::SeqCst) {
            return Err(PersistenceError::InjectedAbort);
        }
        tx.commit()?;
        Ok(meta)
    }

    /// Make the next write transaction abort before commit.
    #[cfg(test)]
    pub(crate) fn inject_abort(&self) {
        self.abort_next_write.store(true, Ordering::SeqCst);
    }

    /// Use a compact retention window in focused persistence and runtime tests.
    #[cfg(test)]
    pub(crate) fn set_retention_for_tests(
        &mut self,
        checkpoint_interval: u64,
        minimum_journal_blocks: u64,
    ) {
        self.retention = RetentionPolicy {
            checkpoint_interval,
            minimum_journal_blocks,
        };
    }
}

/// Verify the on-disk schema version without opening a writer.
fn probe(path: &Path) -> Result<()> {
    let db = open_db_read_only(path, DatabaseArguments::default())?;
    let tx = db.tx()?;
    let raw = tx
        .get::<Meta>(MetaKey::Version)?
        .ok_or_else(|| invalid("schema version is missing"))?;
    let MetaValue::Version(actual) = raw else {
        return Err(invalid("schema version type mismatch"));
    };
    tx.commit()?;
    if actual != SCHEMA_VERSION {
        return Err(schema_error(actual, path));
    }
    Ok(())
}

/// Read and type-check the singleton metadata value in an open transaction.
fn read_metadata<T: DbTx>(tx: &T) -> Result<Metadata> {
    let value = tx
        .get::<Meta>(MetaKey::Metadata)?
        .ok_or_else(|| invalid("metadata row is missing"))?;
    let MetaValue::Metadata(meta) = value else {
        return Err(invalid("metadata type mismatch"));
    };
    Ok(*meta)
}

/// Construct the rebuild instruction for an incompatible schema version.
fn schema_error(actual: u32, path: &Path) -> PersistenceError {
    PersistenceError::Schema {
        expected: SCHEMA_VERSION,
        actual,
        rebuild_path: path.with_extension("rebuild"),
    }
}
/// Construct a durable-data validation error.
pub(super) fn invalid(message: impl Into<String>) -> PersistenceError {
    PersistenceError::Invalid(message.into())
}

/// Return whether two findings retain identical immutable evidence.
fn same_finding_evidence(existing: &Finding, candidate: &Finding) -> bool {
    existing.zone == candidate.zone
        && existing.parent == candidate.parent
        && existing.imported_tempo == candidate.imported_tempo
        && existing.imported_tempo_parent == candidate.imported_tempo_parent
        && existing.details == candidate.details
        && existing.evidence_len == candidate.evidence_len
        && existing.evidence_digest == candidate.evidence_digest
}
