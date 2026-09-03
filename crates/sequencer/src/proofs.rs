//! Proactive, durable collection of per-block Zone and Tempo state proofs.

use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use alloy_consensus::{BlockHeader as _, Sealable as _};
use alloy_primitives::B256;
use alloy_provider::DynProvider;
use eyre::{Context as _, OptionExt as _, Result, bail, ensure};
use futures::StreamExt as _;
use parking_lot::RwLock;
use reth_provider::TransactionVariant;
use serde::{Deserialize, Serialize};
use tempo_alloy::TempoNetwork;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use zone_rpc::ZoneDebugApi;
use zone_spf::{TempoStateWitness, ZoneStateWitness};

use crate::{
    ZoneSequencerProvider,
    prover::{build_zone_inputs_for_block, collect_l1_reads, tempo_header, tempo_state_witness},
};

const FORMAT_VERSION: u32 = 1;
const RETRY_INTERVAL: Duration = Duration::from_secs(1);
const FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Configuration for the proactive proof collector.
#[derive(Clone)]
pub struct ProofCollectorConfig {
    /// Directory containing immutable per-block JSON proof files.
    pub directory: PathBuf,
    /// In-process API used to replay an executed block and collect its Zone reads.
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

/// Immutable proof material collected for one canonical Zone block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredBlockProof {
    pub format_version: u32,
    pub block_number: u64,
    pub block_hash: B256,
    pub parent_hash: B256,
    pub zone_state_witness: ZoneStateWitness,
    pub tempo_state_witness: TempoStateWitness,
}

#[derive(Clone, Debug, Default)]
struct CollectorStatus {
    ready: bool,
    failure: Option<Arc<str>>,
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
            if proof.block_number <= pruned_through {
                fs::remove_file(&path)
                    .wrap_err_with(|| format!("remove settled proof {}", path.display()))?;
                continue;
            }
            if proofs.insert(proof.block_number, Arc::new(proof)).is_some() {
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
        {
            let state = self.state.read();
            if proof.block_number <= state.pruned_through {
                bail!(
                    "cannot store proof for settled Zone block {}",
                    proof.block_number
                );
            }
            if let Some(existing) = state.proofs.get(&proof.block_number) {
                ensure!(
                    existing.block_hash == proof.block_hash,
                    "conflicting proof already stored at Zone block {}",
                    proof.block_number
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
        self.state
            .write()
            .proofs
            .insert(proof.block_number, proof.clone());
        Ok(proof)
    }

    fn contains(&self, number: u64, hash: B256) -> bool {
        self.state
            .read()
            .proofs
            .get(&number)
            .is_some_and(|proof| proof.block_hash == hash)
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
        let removed = {
            let mut state = self.state.write();
            let retained = state.proofs.split_off(&from);
            retained.into_values().collect::<Vec<_>>()
        };
        self.remove_files(removed)
    }

    fn prune_through(&self, through: u64) -> Result<()> {
        let removed = {
            let mut state = self.state.write();
            if through <= state.pruned_through {
                return Ok(());
            }
            state.pruned_through = through;
            let retained = state.proofs.split_off(&through.saturating_add(1));
            let removed = std::mem::replace(&mut state.proofs, retained);
            removed.into_values().collect::<Vec<_>>()
        };
        self.remove_files(removed)
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
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.format_version == FORMAT_VERSION,
            "unsupported proof format version {}",
            self.format_version
        );
        ensure!(self.block_number > 0, "cannot store a genesis block proof");
        Ok(())
    }

    pub fn file_name(&self) -> std::ffi::OsString {
        format!("{}-{:x}.json", self.block_number, self.block_hash).into()
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
}

impl ProofCollectorHandle {
    pub async fn wait_for_range(&self, from: u64, to: u64) -> Result<Vec<Arc<StoredBlockProof>>> {
        let mut status = self.status.clone();
        loop {
            if status.borrow().ready
                && let Ok(proofs) = self.store.snapshot(from, to)
            {
                return Ok(proofs);
            }
            if let Some(failure) = status.borrow().failure.clone() {
                bail!("proof collection is unavailable: {failure}");
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
    l1_provider: DynProvider<TempoNetwork>,
    store: Arc<ProofStore>,
    status: watch::Sender<CollectorStatus>,
}

pub(crate) fn spawn_proof_collector<P: ZoneSequencerProvider>(
    config: ProofCollectorConfig,
    provider: P,
    l1_provider: DynProvider<TempoNetwork>,
    pruned_through: u64,
    shutdown: CancellationToken,
) -> Result<(ProofCollectorHandle, tokio::task::JoinHandle<()>)> {
    let store = Arc::new(ProofStore::open(config.directory.clone(), pruned_through)?);
    let (status_tx, status_rx) = watch::channel(CollectorStatus::default());
    let collector = ProofCollector {
        config,
        provider,
        l1_provider,
        store: store.clone(),
        status: status_tx,
    };
    let handle = ProofCollectorHandle {
        store,
        status: status_rx,
    };
    let task = tokio::spawn(collector.run(shutdown));
    Ok((handle, task))
}

impl<P: ZoneSequencerProvider> ProofCollector<P> {
    async fn run(self, shutdown: CancellationToken) {
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
                    self.status.send_modify(|status| {
                        status.ready = true;
                        status.failure = None;
                    });
                }
                Err(error) => {
                    error!(target: "zone::sequencer::proofs", %error, "Proof collection failed");
                    self.status.send_modify(|status| {
                        status.ready = false;
                        status.failure = Some(Arc::from(error.to_string()));
                    });
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
                _ = fallback.tick() => {}
                notification = canonical.next() => {
                    let Some(notification) = notification else {
                        warn!(target: "zone::sequencer::proofs", "Canonical state stream closed");
                        return;
                    };
                    if notification.reverted().is_some() {
                        self.status.send_modify(|status| status.ready = false);
                        info!(target: "zone::sequencer::proofs", "Reconciling proof spool after canonical reorg");
                    }
                }
            }
        }
    }

    async fn reconcile_and_collect(&self) -> Result<()> {
        let head = self.provider.best_block_number()?;
        let stored = self.store.state.read().proofs.clone();
        for (number, proof) in stored {
            let canonical = self.provider.block_hash(number)?;
            if number > head || canonical != Some(proof.block_hash) {
                self.store.invalidate_from(number)?;
                break;
            }
        }

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
            self.store.invalidate_from(number)?;
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

    async fn collect_block(&self, number: u64, block_hash: B256) -> Result<StoredBlockProof> {
        let block = self
            .provider
            .recovered_block(block_hash.into(), TransactionVariant::WithHash)?
            .ok_or_eyre(format!("executed Zone block {block_hash} not found"))?;
        ensure!(
            block.number() == number && block.hash() == block_hash,
            "Zone block {number} changed"
        );
        let parent_hash = block.parent_hash();
        let inputs = build_zone_inputs_for_block(&self.provider, &block)?;
        let witness = self
            .config
            .debug_api
            .zone_execution_witness_by_hash(block_hash)
            .await
            .map_err(|error| eyre::eyre!(error.to_string()))
            .wrap_err_with(|| format!("collect Zone witness for block {number}"))?;
        ensure!(
            witness.execution_witness.headers.len() <= 1,
            "Zone block {number} reads an older BLOCKHASH"
        );
        let zone_state_witness = ZoneStateWitness {
            node_pool: witness.execution_witness.state,
            bytecodes: witness.execution_witness.codes,
        };
        let tempo_reads = witness
            .tempo_reads
            .into_iter()
            .map(|read| (number, read))
            .collect();
        let initial_tempo_header = tempo_header(&self.l1_provider, inputs.initial_tempo_number)
            .await
            .context("fetch initial Tempo checkpoint for stored proof")?;
        ensure!(
            initial_tempo_header.hash_slow() == inputs.initial_tempo_hash,
            "Tempo checkpoint changed while collecting Zone block {number}"
        );
        let reads = collect_l1_reads(tempo_reads, &inputs.checkpoint_by_zone_block)?;
        let tempo_state_witness =
            tempo_state_witness(&self.l1_provider, &initial_tempo_header, reads).await?;

        Ok(StoredBlockProof {
            format_version: FORMAT_VERSION,
            block_number: number,
            block_hash,
            parent_hash,
            zone_state_witness,
            tempo_state_witness,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, b256};

    use super::*;

    fn proof(number: u64, hash: B256) -> StoredBlockProof {
        StoredBlockProof {
            format_version: FORMAT_VERSION,
            block_number: number,
            block_hash: hash,
            parent_hash: B256::ZERO,
            zone_state_witness: ZoneStateWitness {
                node_pool: vec![Bytes::from_static(b"zone")],
                bytecodes: vec![Bytes::from_static(b"code")],
            },
            tempo_state_witness: TempoStateWitness {
                initial_tempo_header_rlp: Bytes::from_static(b"header"),
                node_pool: vec![Bytes::from_static(b"tempo")],
            },
        }
    }

    #[test]
    fn persists_loads_and_prunes_json_proofs() {
        let directory = tempfile::tempdir().unwrap();
        let hash = b256!("0101010101010101010101010101010101010101010101010101010101010101");
        let store = ProofStore::open(directory.path().to_path_buf(), 0).unwrap();
        store.insert(proof(1, hash)).unwrap();
        assert_eq!(store.snapshot(1, 1).unwrap()[0].block_hash, hash);
        let json = fs::read_to_string(directory.path().join(proof(1, hash).file_name())).unwrap();
        assert!(json.starts_with("{\"formatVersion\":1,"));

        let reopened = ProofStore::open(directory.path().to_path_buf(), 0).unwrap();
        assert_eq!(reopened.snapshot(1, 1).unwrap()[0].block_hash, hash);
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
}
