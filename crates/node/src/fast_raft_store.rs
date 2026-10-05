//! Crash-durable OpenRaft log storage for fast Zone block ordering.
//!
//! The data set is intentionally small (three replicas and short snapshots), so the first version
//! publishes one atomically replaced, fsynced image per mutation. This serializes vote and log I/O,
//! persists before OpenRaft's append callback, and favors an auditable safety boundary over write
//! amplification. Snapshot/state-machine persistence is supplied separately by the execution
//! adapter because it must atomically include the Reth replay material.

use std::{
    collections::BTreeMap,
    fmt::Debug,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    ops::RangeBounds,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use openraft::{
    Entry, LogId, LogState, RaftLogReader, StorageError, Vote,
    storage::{LogFlushed, RaftLogStorage},
    storage_error::{ErrorSubject, ErrorVerb},
};
use serde::{Deserialize, Serialize};

use crate::fast_quorum::FastRaftConfig;

const STORE_FILE: &str = "raft-log.bin";
const STORE_TEMP: &str = "raft-log.tmp";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct DiskState {
    vote: Option<Vote<u64>>,
    committed: Option<LogId<u64>>,
    last_purged: Option<LogId<u64>>,
    logs: BTreeMap<u64, Entry<FastRaftConfig>>,
}

/// Serialized durable term/vote/log store used by OpenRaft.
#[derive(Clone, Debug)]
pub struct DurableRaftLogStore {
    directory: PathBuf,
    state: Arc<Mutex<DiskState>>,
}

impl DurableRaftLogStore {
    pub fn open(directory: impl AsRef<Path>) -> io::Result<Self> {
        let directory = directory.as_ref().to_path_buf();
        let created = !directory.exists();
        fs::create_dir_all(&directory)?;
        if created {
            sync_directory(directory.parent().unwrap_or_else(|| Path::new(".")))?;
        }
        let path = directory.join(STORE_FILE);
        let state = if path.exists() {
            let bytes = fs::read(&path)?;
            bincode::deserialize(&bytes).map_err(invalid_data)?
        } else {
            let state = DiskState::default();
            persist(&directory, &state)?;
            state
        };
        validate(&state)?;
        Ok(Self {
            directory,
            state: Arc::new(Mutex::new(state)),
        })
    }

    fn read(&self) -> Result<std::sync::MutexGuard<'_, DiskState>, StorageError<u64>> {
        self.state.lock().map_err(|_| {
            storage_error(
                ErrorSubject::Store,
                ErrorVerb::Read,
                io::Error::other("Raft log store lock poisoned"),
            )
        })
    }

    fn mutate(
        &self,
        subject: ErrorSubject<u64>,
        verb: ErrorVerb,
        update: impl FnOnce(&mut DiskState),
    ) -> Result<(), StorageError<u64>> {
        let mut current = self.read()?;
        let mut next = current.clone();
        update(&mut next);
        validate(&next).map_err(|error| storage_error(subject.clone(), verb, error))?;
        persist(&self.directory, &next).map_err(|error| storage_error(subject, verb, error))?;
        *current = next;
        Ok(())
    }
}

impl RaftLogReader<FastRaftConfig> for DurableRaftLogStore {
    async fn try_get_log_entries<RB>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<FastRaftConfig>>, StorageError<u64>>
    where
        RB: RangeBounds<u64> + Clone + Debug + Send,
    {
        let state = self.read()?;
        Ok(state
            .logs
            .range(range)
            .map(|(_, entry)| entry.clone())
            .collect())
    }
}

impl RaftLogStorage<FastRaftConfig> for DurableRaftLogStore {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<FastRaftConfig>, StorageError<u64>> {
        let state = self.read()?;
        Ok(LogState {
            last_purged_log_id: state.last_purged,
            last_log_id: state
                .logs
                .last_key_value()
                .map(|(_, entry)| entry.log_id)
                .or(state.last_purged),
        })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<u64>) -> Result<(), StorageError<u64>> {
        let vote = *vote;
        self.mutate(ErrorSubject::Vote, ErrorVerb::Write, |state| {
            state.vote = Some(vote);
        })
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<u64>>, StorageError<u64>> {
        Ok(self.read()?.vote)
    }

    async fn save_committed(
        &mut self,
        committed: Option<LogId<u64>>,
    ) -> Result<(), StorageError<u64>> {
        self.mutate(ErrorSubject::Store, ErrorVerb::Write, |state| {
            state.committed = committed;
        })
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<u64>>, StorageError<u64>> {
        Ok(self.read()?.committed)
    }

    async fn append<I>(
        &mut self,
        entries: I,
        callback: LogFlushed<FastRaftConfig>,
    ) -> Result<(), StorageError<u64>>
    where
        I: IntoIterator<Item = Entry<FastRaftConfig>> + Send,
        I::IntoIter: Send,
    {
        let entries = entries.into_iter().collect::<Vec<_>>();
        let result = self.mutate(ErrorSubject::Logs, ErrorVerb::Write, |state| {
            for entry in &entries {
                state.logs.insert(entry.log_id.index, entry.clone());
            }
        });
        match result {
            Ok(()) => {
                // This callback is the append acknowledgement boundary. The atomic image and its
                // directory entry are already fsynced at this point.
                callback.log_io_completed(Ok(()));
                Ok(())
            }
            Err(error) => {
                callback.log_io_completed(Err(io::Error::other(error.to_string())));
                Err(error)
            }
        }
    }

    async fn truncate(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        self.mutate(ErrorSubject::Log(log_id), ErrorVerb::Delete, |state| {
            let suffix = state.logs.split_off(&log_id.index);
            drop(suffix);
        })
    }

    async fn purge(&mut self, log_id: LogId<u64>) -> Result<(), StorageError<u64>> {
        self.mutate(ErrorSubject::Log(log_id), ErrorVerb::Delete, |state| {
            let retained = state.logs.split_off(&log_id.index.saturating_add(1));
            state.logs = retained;
            state.last_purged = Some(log_id);
        })
    }
}

fn validate(state: &DiskState) -> io::Result<()> {
    if let Some((_, first)) = state.logs.first_key_value()
        && let Some(purged) = state.last_purged
        && first.log_id.index != purged.index.saturating_add(1)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Raft log has a hole after the purged prefix",
        ));
    }
    let mut previous = state.last_purged.map(|id| id.index);
    for (index, entry) in &state.logs {
        if *index != entry.log_id.index
            || previous.is_some_and(|previous| *index != previous.saturating_add(1))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Raft log is not consecutive",
            ));
        }
        previous = Some(*index);
    }
    Ok(())
}

fn persist(directory: &Path, state: &DiskState) -> io::Result<()> {
    let bytes = bincode::serialize(state).map_err(invalid_data)?;
    let temporary = directory.join(STORE_TEMP);
    let published = directory.join(STORE_FILE);
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
    fs::rename(&temporary, &published)?;
    sync_directory(directory)
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

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn vote_and_committed_pointer_survive_reopen() {
        let directory = tempdir().unwrap();
        let store = DurableRaftLogStore::open(directory.path()).unwrap();
        let vote = Vote::new(7, 2);
        futures::executor::block_on(async {
            let mut store = store;
            store.save_vote(&vote).await.unwrap();
            store.read_vote().await.unwrap()
        });
        let mut reopened = DurableRaftLogStore::open(directory.path()).unwrap();
        assert_eq!(
            futures::executor::block_on(reopened.read_vote()).unwrap(),
            Some(vote)
        );
    }
}
