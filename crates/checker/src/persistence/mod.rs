//! Row-oriented MDBX persistence with bounded canonical undo history.

mod codec;
mod model;
mod schema;
#[cfg(test)]
mod tests;

use std::{fs, path::Path, sync::Arc};

use reth_db::{
    Database, DatabaseEnv, DatabaseEnvKind,
    cursor::DbCursorRO,
    is_database_empty,
    mdbx::{DatabaseArguments, init_db_for},
    transaction::{DbTx, DbTxMut},
};

use crate::accounting::{AccountingError, Effect, State};

use model::DeltaRecord;
pub(crate) use model::{AppliedStatus, BlockRef, Checkpoint, Finding, Identity, Metadata, Status};
use schema::{
    AccountValue, Accounts, Deltas, Findings, Meta, MetaKey, MetaValue, Tables, TokenValue, Tokens,
};

const SCHEMA_VERSION: u32 = 1;
const RETAINED_DELTAS: u64 = 16_384;
const ACTIVE_FINDING: u8 = 0;

/// Loaded durable accounting state and its exact coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub(crate) metadata: Metadata,
    pub(crate) state: Arc<State>,
}

/// Post-block accounting state and its durable undo data.
pub(crate) struct CandidateTransition {
    state: State,
    record: DeltaRecord,
}

impl CandidateTransition {
    /// Consume the current snapshot and derive its next verified state.
    pub(crate) fn derive(
        prior: Snapshot,
        zone: BlockRef,
        parent: BlockRef,
        imported_tempo: BlockRef,
        effects: &[Effect],
    ) -> Result<Self, AccountingError> {
        let Snapshot { metadata, state } = prior;
        let mut state = Arc::unwrap_or_clone(state);
        let delta = state.apply(effects)?;
        Ok(Self {
            record: DeltaRecord {
                zone,
                parent,
                imported_tempo,
                imported_tempo_parent: metadata.imported_tempo,
                delta,
            },
            state,
        })
    }

    /// Return the derived post-block accounting state.
    pub(crate) const fn state(&self) -> &State {
        &self.state
    }
}

/// Sole-writer checker database.
pub(crate) struct Store {
    db: Arc<DatabaseEnv>,
    identity: Identity,
}

impl Store {
    /// Open an existing database or atomically create it from an authenticated checkpoint.
    pub(crate) fn open_or_create(
        path: &Path,
        checkpoint: &Checkpoint,
    ) -> Result<(Self, Snapshot), PersistenceError> {
        if path.exists() {
            return Self::open(path, checkpoint.identity);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| PersistenceError::Invalid(error.to_string()))?;
        }
        Self::create_atomic(path, checkpoint)?;
        Self::open(path, checkpoint.identity)
    }

    /// Create, verify, and atomically publish a genesis checkpoint.
    pub(crate) fn create_atomic(
        target: &Path,
        checkpoint: &Checkpoint,
    ) -> Result<Snapshot, PersistenceError> {
        if target.exists() {
            return Err(PersistenceError::Invalid("database already exists".into()));
        }
        let parent = target
            .parent()
            .ok_or_else(|| PersistenceError::Invalid("database has no parent directory".into()))?;
        let staging = tempfile::Builder::new()
            .prefix(".checker-staging-")
            .tempdir_in(parent)
            .map_err(|error| PersistenceError::Invalid(error.to_string()))?;
        let snapshot = {
            let (store, snapshot) = Self::create(staging.path(), checkpoint)?;
            drop(store);
            let (store, reopened) = Self::open(staging.path(), checkpoint.identity)?;
            drop(store);
            if reopened != snapshot {
                return Err(PersistenceError::Invalid(
                    "genesis state changed after reopen".into(),
                ));
            }
            snapshot
        };
        fs::rename(staging.path(), target)
            .map_err(|error| PersistenceError::Invalid(error.to_string()))?;
        Ok(snapshot)
    }

    fn create(path: &Path, checkpoint: &Checkpoint) -> Result<(Self, Snapshot), PersistenceError> {
        if !is_database_empty(path) {
            return Err(PersistenceError::Invalid(
                "fresh database is not empty".into(),
            ));
        }
        let db = init_db_for::<_, Tables>(path, DatabaseArguments::default())
            .map_err(PersistenceError::Open)?;
        let store = Self {
            db: Arc::new(db),
            identity: checkpoint.identity,
        };
        let metadata = Metadata {
            identity: checkpoint.identity,
            verified_zone: checkpoint.zone,
            imported_tempo: checkpoint.tempo,
            observed_zone: checkpoint.zone,
            status: Status::Verifying,
        };
        let tx = store.db.tx_mut()?;
        tx.put::<Meta>(MetaKey::Version, MetaValue::Version(SCHEMA_VERSION))?;
        tx.put::<Meta>(
            MetaKey::Metadata,
            MetaValue::Metadata(Box::new(metadata.clone())),
        )?;
        for (key, value) in checkpoint.state.accounts() {
            tx.put::<Accounts>(key, AccountValue(value))?;
        }
        for (token, value) in checkpoint.state.tokens() {
            tx.put::<Tokens>(token, TokenValue(value))?;
        }
        tx.commit()?;
        Ok((
            store,
            Snapshot {
                metadata,
                state: Arc::new(checkpoint.state.clone()),
            },
        ))
    }

    /// Open and validate an existing checker database.
    pub(crate) fn open(
        path: &Path,
        identity: Identity,
    ) -> Result<(Self, Snapshot), PersistenceError> {
        let db = DatabaseEnv::open(path, DatabaseEnvKind::RW, DatabaseArguments::default())?;
        let store = Self {
            db: Arc::new(db),
            identity,
        };
        let snapshot = store.load()?;
        Ok((store, snapshot))
    }

    /// Load the active row state directly without replaying retained deltas.
    pub(crate) fn load(&self) -> Result<Snapshot, PersistenceError> {
        let tx = self.db.tx()?;
        let version = match tx.get::<Meta>(MetaKey::Version)? {
            Some(MetaValue::Version(version)) => version,
            _ => {
                return Err(PersistenceError::Invalid(
                    "schema version is missing".into(),
                ));
            }
        };
        if version != SCHEMA_VERSION {
            return Err(PersistenceError::Schema {
                expected: SCHEMA_VERSION,
                actual: version,
            });
        }
        let metadata = read_metadata(&tx)?;
        if metadata.identity != self.identity {
            return Err(PersistenceError::Identity);
        }

        let mut accounts = Vec::new();
        let mut cursor = tx.cursor_read::<Accounts>()?;
        let mut row = cursor.first()?;
        while let Some((key, AccountValue(value))) = row {
            accounts.push((key, value));
            row = cursor.next()?;
        }
        let mut tokens = Vec::new();
        let mut cursor = tx.cursor_read::<Tokens>()?;
        let mut row = cursor.first()?;
        while let Some((token, TokenValue(value))) = row {
            tokens.push((token, value));
            row = cursor.next()?;
        }
        let state = State::from_rows(accounts, tokens)?;
        tx.commit()?;
        Ok(Snapshot {
            metadata,
            state: Arc::new(state),
        })
    }

    /// Atomically apply one contiguous verified block.
    ///
    /// Requires the candidate's parent coordinates to still be current and verifying.
    pub(crate) fn apply(
        &self,
        candidate: CandidateTransition,
    ) -> Result<Snapshot, PersistenceError> {
        let CandidateTransition { state, record } = candidate;
        codec::validate(&record).map_err(PersistenceError::Invalid)?;
        let zone = record.zone;
        let imported_tempo = record.imported_tempo;

        let tx = self.db.tx_mut()?;
        let mut metadata = read_metadata(&tx)?;
        if metadata.status != Status::Verifying {
            return Err(PersistenceError::Invalid("checker is not verifying".into()));
        }
        if record.parent != metadata.verified_zone
            || record.imported_tempo_parent != metadata.imported_tempo
            || zone.number != record.parent.number.saturating_add(1)
        {
            return Err(PersistenceError::StaleSnapshot);
        }
        write_changed_rows(&tx, &state, &record.delta)?;
        tx.put::<Deltas>(zone.number, record)?;
        metadata.verified_zone = zone;
        metadata.imported_tempo = imported_tempo;
        if metadata.observed_zone.number < zone.number {
            metadata.observed_zone = zone;
        }
        tx.put::<Meta>(
            MetaKey::Metadata,
            MetaValue::Metadata(Box::new(metadata.clone())),
        )?;
        if let Some(height) = zone.number.checked_sub(RETAINED_DELTAS) {
            tx.delete::<Deltas>(height, None)?;
        }
        tx.commit()?;
        Ok(Snapshot {
            metadata,
            state: Arc::new(state),
        })
    }

    /// Unwind verified state to one retained exact ancestor.
    pub(crate) fn reorg(
        &self,
        prior: &Snapshot,
        ancestor: BlockRef,
    ) -> Result<Snapshot, PersistenceError> {
        if ancestor.number > prior.metadata.verified_zone.number {
            return Err(PersistenceError::Invalid(
                "reorg ancestor exceeds verified tip".into(),
            ));
        }
        let tx = self.db.tx_mut()?;
        ensure_current(&tx, &prior.metadata)?;
        let mut state = prior.state.as_ref().clone();
        let mut metadata = prior.metadata.clone();
        while metadata.verified_zone.number > ancestor.number {
            let height = metadata.verified_zone.number;
            let record = tx
                .get::<Deltas>(height)?
                .ok_or(PersistenceError::ReorgBeyondRetention { ancestor })?;
            if record.zone != metadata.verified_zone
                || record.imported_tempo != metadata.imported_tempo
            {
                return Err(PersistenceError::Invalid(
                    "delta chain is inconsistent".into(),
                ));
            }
            let delta = record.delta.clone();
            state.unwind(record.delta)?;
            write_changed_rows(&tx, &state, &delta)?;
            tx.delete::<Deltas>(height, None)?;
            metadata.verified_zone = record.parent;
            metadata.imported_tempo = record.imported_tempo_parent;
        }
        if metadata.verified_zone != ancestor {
            return Err(PersistenceError::Invalid(
                "reorg ancestor hash mismatch".into(),
            ));
        }
        metadata.observed_zone = ancestor;
        metadata.status = Status::Verifying;
        tx.delete::<Findings>(ACTIVE_FINDING, None)?;
        tx.put::<Meta>(
            MetaKey::Metadata,
            MetaValue::Metadata(Box::new(metadata.clone())),
        )?;
        tx.commit()?;
        Ok(Snapshot {
            metadata,
            state: Arc::new(state),
        })
    }

    /// Atomically discard derived rows and restart from authenticated genesis.
    pub(crate) fn reset(&self, checkpoint: &Checkpoint) -> Result<Snapshot, PersistenceError> {
        if checkpoint.identity != self.identity {
            return Err(PersistenceError::Identity);
        }
        let metadata = Metadata {
            identity: self.identity,
            verified_zone: checkpoint.zone,
            imported_tempo: checkpoint.tempo,
            observed_zone: checkpoint.zone,
            status: Status::Verifying,
        };
        let tx = self.db.tx_mut()?;
        tx.clear::<Accounts>()?;
        tx.clear::<Tokens>()?;
        tx.clear::<Deltas>()?;
        tx.clear::<Findings>()?;
        for (key, value) in checkpoint.state.accounts() {
            tx.put::<Accounts>(key, AccountValue(value))?;
        }
        for (token, value) in checkpoint.state.tokens() {
            tx.put::<Tokens>(token, TokenValue(value))?;
        }
        tx.put::<Meta>(
            MetaKey::Metadata,
            MetaValue::Metadata(Box::new(metadata.clone())),
        )?;
        tx.commit()?;
        Ok(Snapshot {
            metadata,
            state: Arc::new(checkpoint.state.clone()),
        })
    }

    /// Persist a terminal finding without advancing verified accounting state.
    pub(crate) fn record_finding(
        &self,
        prior: &Snapshot,
        finding: Finding,
    ) -> Result<Snapshot, PersistenceError> {
        codec::validate(&finding).map_err(PersistenceError::Invalid)?;
        let tx = self.db.tx_mut()?;
        ensure_current(&tx, &prior.metadata)?;
        let mut metadata = prior.metadata.clone();
        let observed_through = if metadata.observed_zone.number >= finding.zone.number {
            metadata.observed_zone
        } else {
            finding.zone
        };
        metadata.observed_zone = observed_through;
        metadata.status = Status::Diverged {
            first_unchecked: finding.zone,
            observed_through,
        };
        tx.put::<Findings>(ACTIVE_FINDING, finding)?;
        tx.put::<Meta>(
            MetaKey::Metadata,
            MetaValue::Metadata(Box::new(metadata.clone())),
        )?;
        tx.commit()?;
        Ok(Snapshot {
            metadata,
            state: Arc::clone(&prior.state),
        })
    }

    /// Extend the durable unchecked range while verification remains stopped.
    pub(crate) fn observe_diverged(
        &self,
        prior: &Snapshot,
        observed: BlockRef,
    ) -> Result<Snapshot, PersistenceError> {
        let Status::Diverged {
            first_unchecked, ..
        } = prior.metadata.status
        else {
            return Err(PersistenceError::Invalid("checker has not diverged".into()));
        };
        let mut metadata = prior.metadata.clone();
        metadata.observed_zone = observed;
        metadata.status = Status::Diverged {
            first_unchecked,
            observed_through: observed,
        };
        let tx = self.db.tx_mut()?;
        ensure_current(&tx, &prior.metadata)?;
        tx.put::<Meta>(
            MetaKey::Metadata,
            MetaValue::Metadata(Box::new(metadata.clone())),
        )?;
        tx.commit()?;
        Ok(Snapshot {
            metadata,
            state: Arc::clone(&prior.state),
        })
    }

    /// Persist the latest delivered canonical tip before potentially slow acquisition.
    ///
    /// Requires `prior` to still be verifying; errors if the checker has diverged.
    pub(crate) fn observe(
        &self,
        prior: &Snapshot,
        observed: BlockRef,
    ) -> Result<Snapshot, PersistenceError> {
        if prior.metadata.status != Status::Verifying {
            return Err(PersistenceError::Invalid("checker is not verifying".into()));
        }
        let mut metadata = prior.metadata.clone();
        metadata.observed_zone = observed;
        let tx = self.db.tx_mut()?;
        ensure_current(&tx, &prior.metadata)?;
        tx.put::<Meta>(
            MetaKey::Metadata,
            MetaValue::Metadata(Box::new(metadata.clone())),
        )?;
        tx.commit()?;
        Ok(Snapshot {
            metadata,
            state: Arc::clone(&prior.state),
        })
    }
}

fn read_metadata<T: DbTx>(tx: &T) -> Result<Metadata, PersistenceError> {
    match tx.get::<Meta>(MetaKey::Metadata)? {
        Some(MetaValue::Metadata(metadata)) => Ok(*metadata),
        _ => Err(PersistenceError::Invalid("metadata is missing".into())),
    }
}

fn ensure_current<T: DbTx>(tx: &T, prior: &Metadata) -> Result<(), PersistenceError> {
    if read_metadata(tx)? != *prior {
        return Err(PersistenceError::StaleSnapshot);
    }
    Ok(())
}

fn write_changed_rows<T: DbTxMut>(
    tx: &T,
    state: &State,
    delta: &crate::accounting::BlockDelta,
) -> Result<(), PersistenceError> {
    for (key, _) in &delta.accounts {
        match state.account(*key) {
            Some(value) => tx.put::<Accounts>(*key, AccountValue(value))?,
            None => {
                tx.delete::<Accounts>(*key, None)?;
            }
        }
    }
    for (token, _) in &delta.tokens {
        match state.token(*token) {
            Some(value) => tx.put::<Tokens>(*token, TokenValue(value))?,
            None => {
                tx.delete::<Tokens>(*token, None)?;
            }
        }
    }
    Ok(())
}

/// Durable checker database failure.
#[derive(Debug, thiserror::Error)]
pub(crate) enum PersistenceError {
    #[error("database open failed: {0}")]
    Open(eyre::Report),
    #[error("database error: {0}")]
    Database(#[from] reth_db::DatabaseError),
    #[error("accounting error: {0}")]
    Accounting(#[from] crate::accounting::AccountingError),
    #[error("checker identity does not match the database")]
    Identity,
    #[error("checker schema mismatch: expected {expected}, found {actual}")]
    Schema { expected: u32, actual: u32 },
    #[error("stale checker snapshot")]
    StaleSnapshot,
    #[error("reorg ancestor {ancestor:?} is outside retained history")]
    ReorgBeyondRetention { ancestor: BlockRef },
    #[error("invalid checker database: {0}")]
    Invalid(String),
}
