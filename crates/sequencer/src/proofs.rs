//! Proactive, durable collection of per-block Zone and Tempo state proofs.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use alloy_primitives::B256;
use eyre::{Context as _, OptionExt as _, Result, bail, ensure};
use futures::StreamExt as _;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use zone_rpc::{ZoneDebugApi, types::ZoneExecutionWitness};

use crate::ZoneSequencerProvider;

const FORMAT_VERSION: u32 = 2;
const RETRY_INTERVAL: Duration = Duration::from_secs(1);
const FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Configuration for the proactive proof collector.
#[derive(Clone)]
pub struct ProofCollectorConfig {
    /// Directory containing immutable per-block JSON proof files.
    pub directory: PathBuf,
    /// In-process API used to replay an executed block and collect its Zone and Tempo witness.
    pub debug_api: Arc<dyn ZoneDebugApi>,
}

impl std::fmt::Debug for ProofCollectorConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProofCollectorConfig")
            .field("directory", &self.directory)
            .field("debug_api", &"<in-process>")
            .finish()
    }
}

/// Immutable proof material collected for one executed Zone block, possibly not yet canonical.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredBlockProof {
    pub format_version: u32,
    pub witness: ZoneExecutionWitness,
}

#[derive(Clone, Debug, Default)]
enum CollectorStatus {
    #[default]
    Reconciling,
    Ready,
    Failed(Arc<eyre::Report>),
}

#[derive(Debug)]
struct CollectionUnavailable(Arc<eyre::Report>);

impl std::fmt::Display for CollectionUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("proof collection is unavailable")
    }
}

impl std::error::Error for CollectionUnavailable {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}

#[derive(Debug)]
struct ProofStoreState {
    pruned_through: u64,
    proofs: BTreeMap<u64, Arc<StoredBlockProof>>,
}

/// Durable JSON spool plus an in-memory index of pending block proofs.
#[derive(Debug)]
struct ProofStore {
    directory: PathBuf,
    state: RwLock<ProofStoreState>,
}

impl ProofStore {
    fn open(directory: PathBuf, pruned_through: u64) -> Result<Self> {
        fs::create_dir_all(&directory)
            .wrap_err_with(|| format!("create proof directory {}", directory.display()))?;

        let mut proofs = BTreeMap::new();
        for entry in fs::read_dir(&directory)
            .wrap_err_with(|| format!("read proof directory {}", directory.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
                continue;
            }
            let file = File::open(&path)
                .wrap_err_with(|| format!("open stored proof {}", path.display()))?;
            let proof: StoredBlockProof = serde_json::from_reader(BufReader::new(file))
                .wrap_err_with(|| format!("decode stored proof {}", path.display()))?;
            proof.validate()?;
            ensure!(
                path.file_name() == Some(proof.file_name().as_ref()),
                "stored proof filename does not match contents: {}",
                path.display()
            );
            if proof.witness.block_number <= pruned_through {
                fs::remove_file(&path)
                    .wrap_err_with(|| format!("remove settled proof {}", path.display()))?;
                continue;
            }
            if proofs
                .insert(proof.witness.block_number, Arc::new(proof))
                .is_some()
            {
                bail!("duplicate stored proof height in {}", directory.display());
            }
        }
        sync_directory(&directory)?;

        Ok(Self {
            directory,
            state: RwLock::new(ProofStoreState {
                pruned_through,
                proofs,
            }),
        })
    }

    fn insert(&self, proof: StoredBlockProof) -> Result<Arc<StoredBlockProof>> {
        proof.validate()?;
        let mut state = self.state.write();
        {
            if proof.witness.block_number <= state.pruned_through {
                bail!(
                    "cannot store proof for settled Zone block {}",
                    proof.witness.block_number
                );
            }
            if let Some(existing) = state.proofs.get(&proof.witness.block_number) {
                ensure!(
                    existing.witness.block_hash == proof.witness.block_hash,
                    "conflicting proof already stored at Zone block {}",
                    proof.witness.block_number
                );
                return Ok(existing.clone());
            }
        }

        let path = self.directory.join(proof.file_name());
        let mut temporary = tempfile::NamedTempFile::new_in(&self.directory)
            .wrap_err_with(|| format!("create temporary proof in {}", self.directory.display()))?;
        {
            let mut writer = BufWriter::new(temporary.as_file_mut());
            serde_json::to_writer(&mut writer, &proof).context("encode block proof as JSON")?;
            writer.flush().context("flush block proof JSON")?;
        }
        temporary
            .as_file()
            .sync_all()
            .context("sync temporary block proof")?;
        fs::rename(temporary.path(), &path)
            .wrap_err_with(|| format!("publish stored proof {}", path.display()))?;
        sync_directory(&self.directory)?;

        let proof = Arc::new(proof);
        state
            .proofs
            .insert(proof.witness.block_number, proof.clone());
        Ok(proof)
    }

    fn contains(&self, number: u64, hash: B256) -> bool {
        self.state
            .read()
            .proofs
            .get(&number)
            .is_some_and(|proof| proof.witness.block_hash == hash)
    }

    fn snapshot(&self, from: u64, to: u64) -> Result<Vec<Arc<StoredBlockProof>>> {
        ensure!(from <= to, "invalid proof range {from}..={to}");
        let state = self.state.read();
        (from..=to)
            .map(|number| {
                state
                    .proofs
                    .get(&number)
                    .cloned()
                    .ok_or_eyre(format!("proof for Zone block {number} is not collected"))
            })
            .collect()
    }

    fn invalidate_from(&self, from: u64) -> Result<()> {
        let mut state = self.state.write();
        // Keep the index until deletion is durable so a failed operation can be retried.
        self.remove_files(
            state
                .proofs
                .range(from..)
                .map(|(_, proof)| proof.clone())
                .collect(),
        )?;
        state.proofs.split_off(&from);
        Ok(())
    }

    /// Retain canonical proofs and at most one staged child of the canonical head.
    fn reconcile(
        &self,
        head: u64,
        mut block_hash: impl FnMut(u64) -> Result<Option<B256>>,
    ) -> Result<()> {
        let stored = self.state.read().proofs.clone();
        for (number, proof) in stored {
            let canonical = block_hash(number)?;
            let staged_next = head.checked_add(1) == Some(number)
                && block_hash(head)? == Some(proof.witness.parent_hash);
            if !staged_next && (number > head || canonical != Some(proof.witness.block_hash)) {
                self.invalidate_from(number)?;
                break;
            }
        }
        Ok(())
    }

    fn prune_through(&self, through: u64) -> Result<()> {
        let mut state = self.state.write();
        if through <= state.pruned_through {
            return Ok(());
        }
        self.remove_files(
            state
                .proofs
                .range(..=through)
                .map(|(_, proof)| proof.clone())
                .collect(),
        )?;
        state.proofs.retain(|number, _| *number > through);
        state.pruned_through = through;
        Ok(())
    }

    fn remove_files(&self, proofs: Vec<Arc<StoredBlockProof>>) -> Result<()> {
        if proofs.is_empty() {
            return Ok(());
        }
        for proof in proofs {
            let path = self.directory.join(proof.file_name());
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .wrap_err_with(|| format!("remove stored proof {}", path.display()));
                }
            }
        }
        sync_directory(&self.directory)
    }
}

impl StoredBlockProof {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == FORMAT_VERSION,
            "unsupported proof format version {}",
            self.format_version
        );
        ensure!(
            self.witness.block_number > 0,
            "cannot store a genesis block proof"
        );
        Ok(())
    }

    fn file_name(&self) -> std::ffi::OsString {
        format!(
            "{}-{:x}.json",
            self.witness.block_number, self.witness.block_hash
        )
        .into()
    }
}

fn sync_directory(directory: &Path) -> Result<()> {
    File::open(directory)
        .wrap_err_with(|| format!("open proof directory {}", directory.display()))?
        .sync_all()
        .wrap_err_with(|| format!("sync proof directory {}", directory.display()))
}

/// Read/prune handle shared with the shadow prover and settlement monitor.
#[derive(Clone, Debug)]
pub struct ProofCollectorHandle {
    store: Arc<ProofStore>,
    status: watch::Receiver<CollectorStatus>,
    requests: mpsc::Sender<CollectRequest>,
}

struct CollectRequest {
    number: u64,
    hash: B256,
    response: oneshot::Sender<Result<()>>,
}

impl ProofCollectorHandle {
    /// Collect one executed block before it becomes canonical.
    ///
    /// Success means the proof file and its directory have both been synced. Dropping this
    /// future does not cancel an already queued request or authorize canonicalization.
    pub async fn collect_and_persist(&self, number: u64, hash: B256) -> Result<()> {
        let (response, result) = oneshot::channel();
        let result = async {
            self.requests
                .send(CollectRequest {
                    number,
                    hash,
                    response,
                })
                .await
                .context("proof collector stopped")?;
            result.await.context("proof collector dropped request")?
        };
        let mut status = self.status.clone();
        tokio::pin!(result);
        loop {
            if let CollectorStatus::Failed(failure) = &*status.borrow() {
                return Err(CollectionUnavailable(failure.clone()).into());
            }
            tokio::select! {
                result = &mut result => return result,
                changed = status.changed() => { changed.context("proof collector stopped")?; }
            }
        }
    }

    /// Wait for a reconciled, complete inclusive range of retained proofs.
    pub async fn wait_for_range(&self, from: u64, to: u64) -> Result<Vec<Arc<StoredBlockProof>>> {
        ensure!(from <= to, "invalid proof range {from}..={to}");
        let mut status = self.status.clone();
        loop {
            if matches!(*status.borrow(), CollectorStatus::Ready)
                && let Ok(proofs) = self.store.snapshot(from, to)
            {
                return Ok(proofs);
            }
            if let CollectorStatus::Failed(failure) = &*status.borrow() {
                return Err(CollectionUnavailable(failure.clone()).into());
            }
            status
                .changed()
                .await
                .context("proof collector stopped before the requested range was available")?;
        }
    }

    pub(crate) async fn prune_through(&self, through: u64) -> Result<()> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || store.prune_through(through))
            .await
            .context("proof pruning task panicked")?
    }
}

struct ProofCollector<P> {
    config: ProofCollectorConfig,
    provider: P,
    store: Arc<ProofStore>,
    status: watch::Sender<CollectorStatus>,
    requests: mpsc::Receiver<CollectRequest>,
}

/// Reload the durable spool on a blocking task, then start collection until shutdown.
pub async fn spawn_proof_collector<P: ZoneSequencerProvider>(
    config: ProofCollectorConfig,
    provider: P,
    pruned_through: u64,
    shutdown: CancellationToken,
) -> Result<(ProofCollectorHandle, tokio::task::JoinHandle<()>)> {
    let directory = config.directory.clone();
    let store = Arc::new(
        tokio::task::spawn_blocking(move || ProofStore::open(directory, pruned_through))
            .await
            .context("proof store opening task panicked")??,
    );
    let (status_tx, status_rx) = watch::channel(CollectorStatus::default());
    let (requests_tx, requests_rx) = mpsc::channel(16);
    let collector = ProofCollector {
        config,
        provider,
        store: store.clone(),
        status: status_tx,
        requests: requests_rx,
    };
    let handle = ProofCollectorHandle {
        store,
        status: status_rx,
        requests: requests_tx,
    };
    let task = tokio::spawn(collector.run(shutdown));
    Ok((handle, task))
}

impl<P: ZoneSequencerProvider> ProofCollector<P> {
    async fn run(mut self, shutdown: CancellationToken) {
        info!(
            target: "zone::sequencer::proofs",
            directory = %self.config.directory.display(),
            "Proof collector started"
        );
        let mut canonical = self.provider.canonical_state_stream();
        let mut fallback = tokio::time::interval(FALLBACK_POLL_INTERVAL);

        loop {
            match self.reconcile_and_collect().await {
                Ok(()) => {
                    self.status.send_replace(CollectorStatus::Ready);
                }
                Err(error) => {
                    error!(target: "zone::sequencer::proofs", %error, "Proof collection failed");
                    self.status
                        .send_replace(CollectorStatus::Failed(Arc::new(error)));
                    if shutdown
                        .run_until_cancelled(tokio::time::sleep(RETRY_INTERVAL))
                        .await
                        .is_none()
                    {
                        return;
                    }
                    continue;
                }
            }

            tokio::select! {
                biased;
                () = shutdown.cancelled() => return,
                request = self.requests.recv() => {
                    let Some(request) = request else {
                        warn!(target: "zone::sequencer::proofs", "Proof collection request channel closed");
                        return;
                    };
                    let result = self.collect_and_persist(request.number, request.hash).await;
                    let _ = request.response.send(result);
                }
                _ = fallback.tick() => {}
                notification = canonical.next() => {
                    let Some(notification) = notification else {
                        warn!(target: "zone::sequencer::proofs", "Canonical state stream closed");
                        return;
                    };
                    if notification.reverted().is_some() {
                        self.status.send_replace(CollectorStatus::Reconciling);
                        info!(target: "zone::sequencer::proofs", "Reconciling proof spool after canonical reorg");
                    }
                }
            }
        }
    }

    async fn reconcile_and_collect(&self) -> Result<()> {
        let head = self.provider.best_block_number()?;
        let store = self.store.clone();
        let provider = self.provider.clone();
        tokio::task::spawn_blocking(move || {
            store.reconcile(head, |number| Ok(provider.block_hash(number)?))
        })
        .await
        .context("proof reconciliation task panicked")??;

        let start = self.store.state.read().pruned_through.saturating_add(1);
        for number in start..=head {
            let block_hash = self
                .provider
                .block_hash(number)?
                .ok_or_eyre(format!("canonical Zone block {number} has no hash"))?;
            if self.store.contains(number, block_hash) {
                continue;
            }
            self.collect_and_persist(number, block_hash).await?;
        }
        Ok(())
    }

    async fn collect_and_persist(&self, number: u64, block_hash: B256) -> Result<()> {
        if self.store.contains(number, block_hash) {
            return Ok(());
        }
        if self.store.state.read().proofs.contains_key(&number) {
            self.invalidate_from(number).await?;
        }
        let proof = self.collect_block(number, block_hash).await?;
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || store.insert(proof))
            .await
            .context("proof persistence task panicked")??;
        self.status.send_modify(|_| {});
        info!(
            target: "zone::sequencer::proofs",
            zone_block = number,
            %block_hash,
            "Collected and persisted block proofs"
        );
        Ok(())
    }

    async fn invalidate_from(&self, number: u64) -> Result<()> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || store.invalidate_from(number))
            .await
            .context("proof invalidation task panicked")?
    }

    async fn collect_block(&self, number: u64, block_hash: B256) -> Result<StoredBlockProof> {
        let witness = self
            .config
            .debug_api
            .zone_execution_witness(block_hash.into())
            .await
            .map_err(|error| eyre::eyre!(error.to_string()))
            .wrap_err_with(|| format!("collect witness for Zone block {number}"))?;
        ensure!(
            witness.block_hash == block_hash,
            "collected witness does not match Zone block {number} ({block_hash})"
        );
        ensure!(
            witness.execution_witness.headers.len() <= 1,
            "Zone block {number} reads an older BLOCKHASH"
        );
        Ok(StoredBlockProof {
            format_version: FORMAT_VERSION,
            witness,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, b256};

    use super::*;

    fn proof(number: u64, hash: B256) -> StoredBlockProof {
        let mut execution_witness = ZoneExecutionWitness::default().execution_witness;
        execution_witness.state = vec![Bytes::from_static(b"zone")];
        execution_witness.codes = vec![Bytes::from_static(b"code")];
        execution_witness.headers = vec![Bytes::from_static(b"ancestor")];
        StoredBlockProof {
            format_version: FORMAT_VERSION,
            witness: ZoneExecutionWitness {
                block_number: number,
                block_hash: hash,
                execution_witness,
                tempo_state: vec![Bytes::from_static(b"tempo")],
                ..Default::default()
            },
        }
    }

    #[test]
    fn persists_loads_and_prunes_json_proofs() {
        let directory = tempfile::tempdir().unwrap();
        let hash = b256!("0101010101010101010101010101010101010101010101010101010101010101");
        let store = ProofStore::open(directory.path().to_path_buf(), 0).unwrap();
        store.insert(proof(1, hash)).unwrap();
        assert_eq!(store.snapshot(1, 1).unwrap()[0].witness.block_hash, hash);
        let json = fs::read_to_string(directory.path().join(proof(1, hash).file_name())).unwrap();
        let stored_json: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(stored_json["formatVersion"], FORMAT_VERSION);
        assert_eq!(
            stored_json["witness"],
            serde_json::to_value(proof(1, hash).witness).unwrap()
        );

        let reopened = ProofStore::open(directory.path().to_path_buf(), 0).unwrap();
        assert_eq!(*reopened.snapshot(1, 1).unwrap()[0], proof(1, hash));
        reopened.prune_through(1).unwrap();
        assert!(reopened.snapshot(1, 1).is_err());
        assert!(fs::read_dir(directory.path()).unwrap().next().is_none());
    }

    #[test]
    fn rejects_conflicting_proof_at_the_same_height() {
        let directory = tempfile::tempdir().unwrap();
        let store = ProofStore::open(directory.path().to_path_buf(), 0).unwrap();
        store.insert(proof(1, B256::repeat_byte(1))).unwrap();

        let error = store.insert(proof(1, B256::repeat_byte(2))).unwrap_err();
        assert!(error.to_string().contains("conflicting proof"));
    }

    #[test]
    fn retries_pruning_after_file_deletion_fails() {
        let directory = tempfile::tempdir().unwrap();
        let store = ProofStore::open(directory.path().to_path_buf(), 0).unwrap();
        let proof = proof(1, B256::repeat_byte(1));
        let path = directory.path().join(proof.file_name());
        store.insert(proof).unwrap();
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();

        let error = store.prune_through(1).unwrap_err();
        assert!(error.downcast_ref::<std::io::Error>().is_some());
        assert_eq!(store.state.read().pruned_through, 0);
        assert_eq!(store.snapshot(1, 1).unwrap().len(), 1);

        fs::remove_dir(&path).unwrap();
        store.prune_through(1).unwrap();
        assert_eq!(store.state.read().pruned_through, 1);
        assert!(store.state.read().proofs.is_empty());
    }

    #[test]
    fn restart_retains_staged_child_but_reorg_invalidates_it() {
        let directory = tempfile::tempdir().unwrap();
        let parent_hash = B256::repeat_byte(1);
        let staged_hash = B256::repeat_byte(2);
        let store = ProofStore::open(directory.path().to_path_buf(), 0).unwrap();
        let mut staged = proof(2, staged_hash);
        staged.witness.parent_hash = parent_hash;
        store.insert(staged).unwrap();
        drop(store);

        let store = ProofStore::open(directory.path().to_path_buf(), 0).unwrap();
        store
            .reconcile(1, |number| Ok((number == 1).then_some(parent_hash)))
            .unwrap();
        assert!(store.contains(2, staged_hash));
        store
            .reconcile(
                1,
                |number| Ok((number == 1).then_some(B256::repeat_byte(3))),
            )
            .unwrap();
        assert!(store.state.read().proofs.is_empty());
        assert!(fs::read_dir(directory.path()).unwrap().next().is_none());
    }

    #[test]
    fn reconciliation_discards_descendants_beyond_the_staged_child() {
        let directory = tempfile::tempdir().unwrap();
        let store = ProofStore::open(directory.path().to_path_buf(), 0).unwrap();
        let parent_hash = B256::repeat_byte(1);
        let mut staged = proof(2, B256::repeat_byte(2));
        staged.witness.parent_hash = parent_hash;
        store.insert(staged).unwrap();
        store.insert(proof(3, B256::repeat_byte(3))).unwrap();
        store
            .reconcile(1, |number| Ok((number == 1).then_some(parent_hash)))
            .unwrap();
        assert_eq!(
            store
                .state
                .read()
                .proofs
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn retries_invalidation_after_partial_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let store = ProofStore::open(directory.path().to_path_buf(), 0).unwrap();
        store.insert(proof(1, B256::repeat_byte(1))).unwrap();
        let second = proof(2, B256::repeat_byte(2));
        let path = directory.path().join(second.file_name());
        store.insert(second).unwrap();
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();

        let error = store.invalidate_from(1).unwrap_err();
        assert!(error.downcast_ref::<std::io::Error>().is_some());
        assert_eq!(store.snapshot(1, 2).unwrap().len(), 2);
        fs::remove_dir(&path).unwrap();
        store.invalidate_from(1).unwrap();
        assert!(store.state.read().proofs.is_empty());
        assert!(fs::read_dir(directory.path()).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn reconciliation_failure_preserves_the_error_chain() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(ProofStore::open(directory.path().to_path_buf(), 0).unwrap());
        let error = eyre::Report::from(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            .wrap_err("open proof file");
        let (_status_tx, status) = watch::channel(CollectorStatus::Failed(Arc::new(error)));
        let (requests, _receiver) = mpsc::channel(1);
        let handle = ProofCollectorHandle {
            store,
            status,
            requests,
        };
        let error = handle.wait_for_range(1, 1).await.unwrap_err();
        assert!(error.chain().any(|source| {
            source
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
        }));
    }

    #[tokio::test]
    async fn collection_request_returns_the_persistence_error() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(ProofStore::open(directory.path().to_path_buf(), 0).unwrap());
        let (_status_tx, status) = watch::channel(CollectorStatus::Ready);
        let (requests, mut receiver) = mpsc::channel::<CollectRequest>(1);
        let handle = ProofCollectorHandle {
            store,
            status,
            requests,
        };
        let responder = async {
            let request = receiver.recv().await.unwrap();
            assert_eq!(request.number, 1);
            request
                .response
                .send(Err(std::io::Error::from(
                    std::io::ErrorKind::PermissionDenied,
                )
                .into()))
                .unwrap();
        };
        let (result, ()) = tokio::join!(handle.collect_and_persist(1, B256::ZERO), responder);
        assert_eq!(
            result
                .unwrap_err()
                .downcast_ref::<std::io::Error>()
                .unwrap()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }
}
