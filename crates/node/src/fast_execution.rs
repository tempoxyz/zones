//! Canonical Reth execution and receipt-derived outcome reconstruction for committed T14 entries.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    future::Future,
    io::{self, Write},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use alloy_consensus::{
    BlockHeader as _, Transaction as _, TxReceipt as _, transaction::TxHashRef as _,
};
use alloy_eips::{BlockHashOrNumber, NumHash};
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_rlp::Decodable as _;
use alloy_signer::SignerSync as _;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall as _;
use openraft::LogId;
use reth_node_api::PayloadTypes as _;
use reth_node_builder::ConsensusEngineHandle;
use reth_primitives_traits::SealedBlock;
use reth_storage_api::{BlockNumReader, BlockReader, HeaderProvider, ReceiptProvider};
use serde::{Deserialize, Serialize};
use tempo_primitives::{Block, TempoHeader};
use tempo_zone_contracts::{FAST_TRANSFER_ADDRESS, IFastTransfer};
use tracing::error;
use zone_fast_transfer::{DurableJournal, ReplicatedBlockInput as JournaledBlockInput};
use zone_l1::{DepositQueue, L1BlockTracker};
use zone_payload::{TempoImport, ZonePayloadAttributes, ZonePayloadTypes};
use zone_primitives::fast_transfer::{
    CanonicalEncode as _, CertificateBody, MAX_CERTIFICATE_BYTES, OutcomeCertificate,
    RejectionReason, SignatureBytes, TransferIntent, TransferOutcome, decode_exact,
};
use zone_sequencer::ProofCollectorHandle;

use crate::{
    engine::FastActivationRefresh,
    fast_batch::FastBatchScheduler,
    fast_quorum::{
        CommittedBlock, FastActivation, FastAdmissionClock, RaftCommit, ReplicatedBlockInput,
        sign_committed_outcome,
    },
    fast_raft_state_machine::{
        AppliedBlock, CommittedTransferRecord, DurableStateMachineExecution,
    },
};

const OUTCOMES_FILE: &str = "committed-outcomes.bin";
const OUTCOMES_TEMP: &str = "committed-outcomes.tmp";
const OUTCOMES_MAGIC: &[u8; 8] = b"ZFOUT001";
const TRANSACTION_RESULTS_FILE: &str = "committed-transaction-results.bin";
const TRANSACTION_RESULTS_TEMP: &str = "committed-transaction-results.tmp";
const TRANSACTION_RESULTS_MAGIC: &[u8; 8] = b"ZFTXR001";
const MAX_OUTCOMES_BYTES: usize = 1024 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct CommittedTransactionResult {
    log_term: u64,
    log_index: u64,
    succeeded: bool,
}

/// Reth adapter shared by OpenRaft application, snapshot restore, and committed RPC reads.
pub struct CanonicalFastExecution<P> {
    runtime: tokio::runtime::Handle,
    engine: ConsensusEngineHandle<ZonePayloadTypes>,
    provider: P,
    proof_collector: Option<ProofCollectorHandle>,
    journal: Arc<DurableJournal>,
    deposit_queue: DepositQueue,
    l1_block_tracker: L1BlockTracker,
    outcomes_directory: PathBuf,
    outcomes: Mutex<BTreeMap<B256, CommittedTransferRecord>>,
    transaction_results: Mutex<BTreeMap<B256, CommittedTransactionResult>>,
    expected_epoch: u64,
    authority_anchor: Arc<RwLock<zone_evm::same_anchor::SameAnchorOpening>>,
    authority_admission: FastAdmissionClock,
    authority_active: Arc<AtomicBool>,
    admission_open: Arc<AtomicBool>,
    activation_refresh: Arc<dyn FastActivationRefresh>,
    batch: OnceLock<FastBatchScheduler>,
}

impl<P> CanonicalFastExecution<P>
where
    P: BlockNumReader
        + BlockReader<Block = Block>
        + HeaderProvider<Header = TempoHeader>
        + ReceiptProvider<Receipt = tempo_primitives::TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    pub fn open(
        runtime: tokio::runtime::Handle,
        engine: ConsensusEngineHandle<ZonePayloadTypes>,
        provider: P,
        proof_collector: Option<ProofCollectorHandle>,
        deposit_queue: DepositQueue,
        l1_block_tracker: L1BlockTracker,
        expected_epoch: u64,
        authority_anchor: Arc<RwLock<zone_evm::same_anchor::SameAnchorOpening>>,
        authority_admission: FastAdmissionClock,
        authority_active: Arc<AtomicBool>,
        admission_open: Arc<AtomicBool>,
        activation_refresh: Arc<dyn FastActivationRefresh>,
        directory: impl AsRef<Path>,
    ) -> Result<Self, FastExecutionError> {
        let directory = directory.as_ref();
        fs::create_dir_all(directory)?;
        let outcomes_directory = directory.join("outcomes");
        fs::create_dir_all(&outcomes_directory)?;
        let outcomes = load_outcomes(&outcomes_directory)?;
        let transaction_results = load_transaction_results(&outcomes_directory)?;
        Ok(Self {
            runtime,
            engine,
            provider,
            proof_collector,
            journal: Arc::new(DurableJournal::open(directory.join("journal"))?),
            deposit_queue,
            l1_block_tracker,
            outcomes_directory,
            outcomes: Mutex::new(outcomes),
            transaction_results: Mutex::new(transaction_results),
            expected_epoch,
            authority_anchor,
            authority_admission,
            authority_active,
            admission_open,
            activation_refresh,
            batch: OnceLock::new(),
        })
    }

    /// Install the canonical scheduler after the OpenRaft committed handle exists. Production and
    /// peer request serving start only after this succeeds.
    pub fn install_fast_batch_scheduler(
        &self,
        scheduler: FastBatchScheduler,
    ) -> Result<(), FastExecutionError> {
        self.batch
            .set(scheduler)
            .map_err(|_| FastExecutionError::BatchAlreadyInstalled)
    }

    fn imported_anchor(
        input: &ReplicatedBlockInput,
    ) -> Result<Option<NumHash>, FastExecutionError> {
        let attributes: ZonePayloadAttributes = bincode::deserialize(&input.l1_inputs)?;
        Ok(match &attributes.tempo_import {
            TempoImport::Full(prepared) => Some(prepared.header.num_hash()),
            TempoImport::CheckpointOnly(headers) => headers.last().map(|header| header.num_hash()),
            TempoImport::SameAnchor(_) => None,
        })
    }

    /// Shared fsynced protocol journal used by committed execution, delivery recovery, and
    /// replenishment. Opening a second writer for the same path would violate append ordering.
    pub fn protocol_journal(&self) -> Arc<DurableJournal> {
        self.journal.clone()
    }

    async fn refresh_authority(&self, anchor: NumHash) {
        self.authority_active.store(false, Ordering::Release);
        self.admission_open.store(false, Ordering::Release);
        match self.activation_refresh.refresh(anchor).await {
            Ok(refresh) if refresh.opening().protocol_epoch == self.expected_epoch => {
                match self
                    .authority_admission
                    .observe_finalized(anchor.number, anchor.hash)
                {
                    Ok(()) => {
                        *self
                            .authority_anchor
                            .write()
                            .expect("fast anchor lock poisoned") = refresh.opening();
                        self.admission_open
                            .store(refresh.admission_open(), Ordering::Release);
                        self.authority_active.store(true, Ordering::Release);
                    }
                    Err(error) => {
                        error!(target: "zone::fast", %error, "revoked fast production after finalized progress validation failed");
                    }
                }
            }
            Ok(_) => {
                error!(target: "zone::fast", "revoked fast production after protocol epoch changed");
            }
            Err(error) => {
                error!(target: "zone::fast", %error, "revoked fast production after finalized capability revalidation failed");
            }
        }
    }

    /// Fsync a completed two-signature certificate assembled over this exact local record.
    pub(crate) fn persist_certificate(
        &self,
        certificate: OutcomeCertificate,
    ) -> Result<(), FastExecutionError> {
        let mut outcomes = self
            .outcomes
            .lock()
            .map_err(|_| FastExecutionError::Poisoned)?;
        let record = outcomes
            .get_mut(&certificate.body.transfer_id)
            .ok_or(FastExecutionError::UnknownTransfer)?;
        if record.body != certificate.body {
            return Err(FastExecutionError::OutcomeConflict);
        }
        if record
            .certificate
            .as_ref()
            .is_some_and(|existing| existing != &certificate)
        {
            return Err(FastExecutionError::OutcomeConflict);
        }
        record.certificate = Some(certificate);
        persist_outcomes(&self.outcomes_directory, &outcomes)
    }

    /// Return only outcomes reconstructed for this exact committed log entry.
    pub(crate) fn outcomes_for_commit(
        &self,
        term: u64,
        index: u64,
    ) -> Result<Vec<CommittedTransferRecord>, FastExecutionError> {
        Ok(self
            .outcomes
            .lock()
            .map_err(|_| FastExecutionError::Poisoned)?
            .values()
            .filter(|record| record.body.log_term == term && record.body.log_index == index)
            .cloned()
            .collect())
    }

    /// Snapshot every executor record. The committed state-machine handle must fence these
    /// coordinates against its fsynced applied prefix before exposing them outside the runtime.
    pub fn committed_transfers_snapshot(
        &self,
    ) -> Result<Vec<CommittedTransferRecord>, FastExecutionError> {
        let mut records = self
            .outcomes
            .lock()
            .map_err(|_| FastExecutionError::Poisoned)?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by_key(|record| {
            (
                record.body.log_term,
                record.body.log_index,
                record.body.transfer_id,
            )
        });
        Ok(records)
    }

    /// Read the canonical status retained for any transaction in a committed Raft block,
    /// including successful `recordOutcome` and reverted disposition attempts that emit no token
    /// outcome event. The state-machine handle performs the applied-prefix fence.
    pub fn committed_transaction_result(
        &self,
        transaction_hash: B256,
    ) -> Result<Option<(u64, u64, bool)>, FastExecutionError> {
        Ok(self
            .transaction_results
            .lock()
            .map_err(|_| FastExecutionError::Poisoned)?
            .get(&transaction_hash)
            .map(|result| (result.log_term, result.log_index, result.succeeded)))
    }

    /// Return original Raft coordinates for committed outcomes whose quorum certificate has not
    /// yet been fsynced. Recovery must recollect against these coordinates, never a new entry.
    pub(crate) fn uncertified_commits(&self) -> Result<Vec<RaftCommit>, FastExecutionError> {
        let outcomes = self
            .outcomes
            .lock()
            .map_err(|_| FastExecutionError::Poisoned)?;
        let mut commits = BTreeMap::new();
        for record in outcomes
            .values()
            .filter(|record| record.certificate.is_none())
        {
            let body = &record.body;
            let commit = RaftCommit {
                term: body.log_term,
                index: body.log_index,
                block: CommittedBlock {
                    input_digest: B256::ZERO,
                    block_height: body.block_height,
                    block_hash: body.block_hash,
                    state_root: body.state_root,
                    receipts_root: B256::ZERO,
                },
            };
            if commits
                .insert((commit.term, commit.index), commit.clone())
                .is_some_and(|existing| existing != commit)
            {
                return Err(FastExecutionError::OutcomeConflict);
            }
        }
        Ok(commits.into_values().collect())
    }

    /// Reconstruct and durably journal a signature from local committed receipts. No wire body is
    /// accepted by this boundary.
    pub(crate) fn sign_committed_transfer(
        &self,
        activation: &FastActivation,
        signer: &PrivateKeySigner,
        transfer_id: B256,
    ) -> Result<(CertificateBody, SignatureBytes), FastExecutionError> {
        let record = self
            .outcomes
            .lock()
            .map_err(|_| FastExecutionError::Poisoned)?
            .get(&transfer_id)
            .cloned()
            .ok_or(FastExecutionError::UnknownTransfer)?;
        let commit = RaftCommit {
            term: record.body.log_term,
            index: record.body.log_index,
            block: CommittedBlock {
                input_digest: B256::ZERO,
                block_height: record.body.block_height,
                block_hash: record.body.block_hash,
                state_root: record.body.state_root,
                receipts_root: B256::ZERO,
            },
        };
        let body = record.body;
        let signature = sign_committed_outcome(
            self.journal.as_ref(),
            activation,
            &commit,
            signer.address(),
            |_| body.clone(),
            |digest| {
                SignatureBytes(
                    signer
                        .sign_hash_sync(&digest)
                        .expect("validated local secp256k1 signer")
                        .as_bytes(),
                )
            },
        )
        .map_err(|error| FastExecutionError::Signing(error.to_string()))?;
        Ok((body, signature))
    }

    fn apply_block(
        &self,
        log_id: LogId<u64>,
        input: &ReplicatedBlockInput,
    ) -> Result<CommittedBlock, FastExecutionError> {
        let attributes: ZonePayloadAttributes = bincode::deserialize(&input.l1_inputs)?;
        if let TempoImport::SameAnchor(opening) = &attributes.tempo_import
            && opening.protocol_epoch != self.expected_epoch
        {
            return Err(FastExecutionError::ProtocolEpoch {
                expected: self.expected_epoch,
                actual: opening.protocol_epoch,
            });
        }
        let block = decode_block(&input.block_input)?;
        if block.header.parent_hash() != input.parent_hash {
            return Err(FastExecutionError::ParentMismatch);
        }
        let encoded_transactions: Vec<Bytes> = block
            .body
            .transactions
            .iter()
            .map(|transaction| alloy_eips::eip2718::Encodable2718::encoded_2718(transaction).into())
            .collect::<Vec<_>>();
        if encoded_transactions != input.transactions {
            return Err(FastExecutionError::TransactionMismatch);
        }
        let sealed = SealedBlock::seal_slow(block);
        let header = sealed.sealed_header().clone();
        let header_hash = header.hash();
        let payload = ZonePayloadTypes::block_to_payload(sealed, None);
        let engine = self.engine.clone();
        let collector = self.proof_collector.clone();
        run_on_runtime(&self.runtime, async move {
            let status = engine.new_payload(payload).await?;
            if !status.is_valid() {
                return Err(eyre::eyre!("Reth rejected committed block: {status:?}"));
            }
            if let Some(collector) = collector {
                let mut deserializer = minicbor_serde::Deserializer::from(minicbor::Decoder::new(
                    input.replay_witness.as_ref(),
                ));
                let proof = serde::Deserialize::deserialize(&mut deserializer)?;
                collector.persist_received(proof).await?;
            }
            let forkchoice = alloy_rpc_types_engine::ForkchoiceState::same_hash(header_hash);
            let status = engine.fork_choice_updated(forkchoice, None).await?;
            if !status.is_valid() {
                return Err(eyre::eyre!(
                    "Reth rejected committed forkchoice: {status:?}"
                ));
            }
            Ok::<_, eyre::Report>(())
        })?;
        let canonical = self
            .provider
            .sealed_header(header.number())?
            .ok_or(FastExecutionError::CanonicalHeaderMissing)?;
        if canonical.hash() != header_hash {
            return Err(FastExecutionError::CanonicalHashMismatch);
        }
        let receipts = self
            .provider
            .receipts_by_block(BlockHashOrNumber::Number(header.number()))?
            .ok_or(FastExecutionError::ReceiptsMissing)?;
        let canonical_block = self
            .provider
            .block_by_number(header.number())?
            .ok_or(FastExecutionError::CanonicalBlockMissing)?;
        if canonical_block.body.transactions.len() != receipts.len() {
            return Err(FastExecutionError::ReceiptCountMismatch);
        }
        let mut outcomes = self
            .outcomes
            .lock()
            .map_err(|_| FastExecutionError::Poisoned)?;
        reconstruct_outcomes(log_id, &header, &canonical_block, &receipts, &mut outcomes)?;
        persist_outcomes(&self.outcomes_directory, &outcomes)?;
        let mut transaction_results = self
            .transaction_results
            .lock()
            .map_err(|_| FastExecutionError::Poisoned)?;
        reconstruct_transaction_results(
            log_id,
            &canonical_block,
            &receipts,
            &mut transaction_results,
        )?;
        persist_transaction_results(&self.outcomes_directory, &transaction_results)?;
        let output = CommittedBlock {
            input_digest: input.digest(),
            block_height: header.number(),
            block_hash: header.hash(),
            state_root: header.state_root(),
            receipts_root: header.receipts_root(),
        };
        self.persist_replay_material(log_id, input, &output, false)?;
        Ok(output)
    }

    fn persist_replay_material(
        &self,
        log_id: LogId<u64>,
        input: &ReplicatedBlockInput,
        output: &CommittedBlock,
        restore_witness: bool,
    ) -> Result<(), FastExecutionError> {
        if restore_witness && let Some(collector) = self.proof_collector.clone() {
            let mut deserializer = minicbor_serde::Deserializer::from(minicbor::Decoder::new(
                input.replay_witness.as_ref(),
            ));
            let proof = serde::Deserialize::deserialize(&mut deserializer)
                .map_err(|error| FastExecutionError::Witness(error.to_string()))?;
            run_on_runtime(&self.runtime, async move {
                collector.persist_received(proof).await?;
                Ok::<_, eyre::Report>(())
            })?;
        }
        let attributes: ZonePayloadAttributes = bincode::deserialize(&input.l1_inputs)?;
        match attributes.tempo_import {
            TempoImport::Full(prepared) => {
                let anchor = prepared.header.num_hash();
                self.deposit_queue
                    .confirm_operational_through(anchor)
                    .map_err(FastExecutionError::Engine)?;
                self.l1_block_tracker.prune_through(anchor.number);
            }
            TempoImport::CheckpointOnly(headers) => {
                let anchor = headers
                    .last()
                    .ok_or(FastExecutionError::InvalidL1Input)?
                    .num_hash();
                self.deposit_queue
                    .defer_through(anchor)
                    .map_err(FastExecutionError::Engine)?;
                self.l1_block_tracker.prune_through(anchor.number);
            }
            TempoImport::SameAnchor(_) => {}
        }
        self.journal.persist_replicated_block(JournaledBlockInput {
            log_term: log_id.leader_id.term,
            log_index: log_id.index,
            block_height: output.block_height,
            block_hash: output.block_hash,
            state_root: output.state_root,
            block_input: input.block_input.to_vec(),
            transactions: input
                .transactions
                .iter()
                .map(|value| value.to_vec())
                .collect(),
            l1_execution_input: input.l1_inputs.to_vec(),
            witness: input.replay_witness.to_vec(),
        })?;
        Ok(())
    }
}

impl<P> DurableStateMachineExecution for CanonicalFastExecution<P>
where
    P: BlockNumReader
        + BlockReader<Block = Block>
        + HeaderProvider<Header = TempoHeader>
        + ReceiptProvider<Receipt = tempo_primitives::TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    type Error = FastExecutionError;

    fn apply_committed<'a>(
        &'a self,
        log_id: LogId<u64>,
        input: &'a ReplicatedBlockInput,
    ) -> Pin<Box<dyn Future<Output = Result<CommittedBlock, Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            self.batch
                .get()
                .ok_or(FastExecutionError::BatchNotInstalled)?
                .validate_ready(input)
                .map_err(|error| FastExecutionError::Batch(error.to_string()))?;
            let attributes: ZonePayloadAttributes = bincode::deserialize(&input.l1_inputs)?;
            if let TempoImport::SameAnchor(opening) = &attributes.tempo_import
                && opening.protocol_epoch != self.expected_epoch
            {
                return Err(FastExecutionError::ProtocolEpoch {
                    expected: self.expected_epoch,
                    actual: opening.protocol_epoch,
                });
            }
            let imported_anchor = Self::imported_anchor(input)?;
            let output = self.apply_block(log_id, input)?;
            if let Some(anchor) = imported_anchor {
                self.refresh_authority(anchor).await;
            }
            Ok(output)
        })
    }

    fn restore_committed<'a>(
        &'a self,
        blocks: &'a [AppliedBlock],
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async move {
            // First reconcile execution. A matching canonical block still needs outcome replay below:
            // the outcome image can be missing after operator recovery or snapshot installation.
            for applied in blocks {
                let canonical = self.provider.sealed_header(applied.output.block_height)?;
                match canonical {
                    Some(header) if header.hash() == applied.output.block_hash => {
                        self.persist_replay_material(
                            applied.log_id,
                            &applied.input,
                            &applied.output,
                            true,
                        )?;
                    }
                    Some(_) => return Err(FastExecutionError::CommittedForkConflict),
                    None => {
                        let restored = self.apply_block(applied.log_id, &applied.input)?;
                        if restored != applied.output {
                            return Err(FastExecutionError::RestoreMismatch);
                        }
                    }
                }
            }

            // Rebuild the index from the committed prefix in order, rather than trusting a possibly
            // incomplete sidecar. Preserve a certificate only when its exact reconstructed body still
            // matches; stale or speculative records are dropped.
            let retained_certificates = self
                .outcomes
                .lock()
                .map_err(|_| FastExecutionError::Poisoned)?
                .iter()
                .filter_map(|(transfer_id, record)| {
                    record
                        .certificate
                        .clone()
                        .map(|certificate| (*transfer_id, (record.body.clone(), certificate)))
                })
                .collect::<BTreeMap<_, _>>();
            let mut rebuilt = BTreeMap::new();
            let mut rebuilt_transaction_results = BTreeMap::new();
            for applied in blocks {
                let header = self
                    .provider
                    .sealed_header(applied.output.block_height)?
                    .ok_or(FastExecutionError::CanonicalHeaderMissing)?;
                if header.hash() != applied.output.block_hash {
                    return Err(FastExecutionError::CommittedForkConflict);
                }
                let block = self
                    .provider
                    .block_by_number(applied.output.block_height)?
                    .ok_or(FastExecutionError::CanonicalBlockMissing)?;
                let receipts = self
                    .provider
                    .receipts_by_block(BlockHashOrNumber::Number(applied.output.block_height))?
                    .ok_or(FastExecutionError::ReceiptsMissing)?;
                if block.body.transactions.len() != receipts.len() {
                    return Err(FastExecutionError::ReceiptCountMismatch);
                }
                reconstruct_outcomes(applied.log_id, &header, &block, &receipts, &mut rebuilt)?;
                reconstruct_transaction_results(
                    applied.log_id,
                    &block,
                    &receipts,
                    &mut rebuilt_transaction_results,
                )?;
            }
            for (transfer_id, record) in &mut rebuilt {
                if let Some((body, certificate)) = retained_certificates.get(transfer_id)
                    && body == &record.body
                {
                    record.certificate = Some(certificate.clone());
                }
            }
            persist_outcomes(&self.outcomes_directory, &rebuilt)?;
            persist_transaction_results(&self.outcomes_directory, &rebuilt_transaction_results)?;
            *self
                .outcomes
                .lock()
                .map_err(|_| FastExecutionError::Poisoned)? = rebuilt;
            *self
                .transaction_results
                .lock()
                .map_err(|_| FastExecutionError::Poisoned)? = rebuilt_transaction_results;
            for applied in blocks.iter().rev() {
                if let Some(anchor) = Self::imported_anchor(&applied.input)? {
                    self.refresh_authority(anchor).await;
                    break;
                }
            }
            Ok(())
        })
    }

    fn committed_transfer(
        &self,
        transfer_id: B256,
    ) -> Result<Option<CommittedTransferRecord>, Self::Error> {
        Ok(self
            .outcomes
            .lock()
            .map_err(|_| FastExecutionError::Poisoned)?
            .get(&transfer_id)
            .cloned())
    }
}

fn decode_block(bytes: &[u8]) -> Result<Block, FastExecutionError> {
    let mut input = bytes;
    let block = Block::decode(&mut input)?;
    if !input.is_empty() {
        return Err(FastExecutionError::TrailingBlockBytes);
    }
    Ok(block)
}

fn reconstruct_outcomes(
    log_id: LogId<u64>,
    header: &reth_primitives_traits::SealedHeader<TempoHeader>,
    block: &Block,
    receipts: &[tempo_primitives::TempoReceipt],
    outcomes: &mut BTreeMap<B256, CommittedTransferRecord>,
) -> Result<(), FastExecutionError> {
    for (transaction, receipt) in block.body.transactions.iter().zip(receipts) {
        if !receipt.status() || transaction.to() != Some(FAST_TRANSFER_ADDRESS) {
            continue;
        }
        let intent = decode_intent(transaction.input())?;
        for log in receipt
            .logs()
            .iter()
            .filter(|log| log.address == FAST_TRANSFER_ADDRESS)
        {
            let Some(topic) = log.topics().first() else {
                continue;
            };
            let event = if *topic == keccak256("Locked(bytes32,bytes32,address,address,uint128)") {
                let intent = intent.as_ref().ok_or(FastExecutionError::MissingIntent)?;
                validate_intent_topics(log, intent)?;
                if indexed_address(log, 3)? != intent.sender
                    || data_address(log.data.data.as_ref(), 0)? != intent.asset.source_token
                {
                    return Err(FastExecutionError::InvalidEvent);
                }
                Some((
                    intent.clone(),
                    TransferOutcome::Locked {
                        escrow: FAST_TRANSFER_ADDRESS,
                        amount: word(log.data.data.as_ref(), 1)?,
                    },
                ))
            } else if *topic == keccak256("Paid(bytes32,bytes32,address,address,uint128)") {
                let intent = intent.as_ref().ok_or(FastExecutionError::MissingIntent)?;
                validate_intent_topics(log, intent)?;
                if indexed_address(log, 3)? != intent.recipient
                    || data_address(log.data.data.as_ref(), 0)? != intent.asset.destination_token
                {
                    return Err(FastExecutionError::InvalidEvent);
                }
                Some((
                    intent.clone(),
                    TransferOutcome::Paid {
                        pool: intent.destination_pool,
                        recipient: intent.recipient,
                        principal: word(log.data.data.as_ref(), 1)?,
                    },
                ))
            } else if *topic == keccak256("Rejected(bytes32,bytes32,uint8)") {
                let intent = intent.as_ref().ok_or(FastExecutionError::MissingIntent)?;
                validate_intent_topics(log, intent)?;
                Some((
                    intent.clone(),
                    TransferOutcome::Rejected {
                        reason: u8::try_from(word(log.data.data.as_ref(), 0)?)
                            .ok()
                            .and_then(|value| RejectionReason::try_from(value).ok())
                            .ok_or(FastExecutionError::InvalidEvent)?,
                    },
                ))
            } else if *topic == keccak256("EscrowDisposed(bytes32,uint8,address,uint128)") {
                let transfer_id = *log
                    .topics()
                    .get(1)
                    .ok_or(FastExecutionError::InvalidEvent)?;
                let existing = outcomes
                    .get(&transfer_id)
                    .ok_or(FastExecutionError::MissingIntent)?;
                let outcome = u8::try_from(word(log.data.data.as_ref(), 0)?)
                    .map_err(|_| FastExecutionError::InvalidEvent)?;
                let beneficiary = Address::from_word(
                    *log.topics()
                        .get(2)
                        .ok_or(FastExecutionError::InvalidEvent)?,
                );
                let amount = word(log.data.data.as_ref(), 1)?;
                Some((
                    existing.intent.clone(),
                    match outcome {
                        1 => TransferOutcome::Released {
                            beneficiary,
                            amount,
                        },
                        2 => TransferOutcome::Refunded {
                            beneficiary,
                            amount,
                        },
                        _ => return Err(FastExecutionError::InvalidEvent),
                    },
                ))
            } else {
                None
            };
            let Some((intent, outcome)) = event else {
                continue;
            };
            let transfer_id = intent.transfer_id();
            let zone = match outcome {
                TransferOutcome::Paid { .. } | TransferOutcome::Rejected { .. } => {
                    intent.destination
                }
                _ => intent.source,
            };
            let mut record = CommittedTransferRecord {
                intent: intent.clone(),
                body: CertificateBody {
                    transfer_id,
                    intent_hash: intent.intent_hash(),
                    zone,
                    log_term: log_id.leader_id.term,
                    log_index: log_id.index,
                    block_height: header.number(),
                    block_hash: header.hash(),
                    state_root: header.state_root(),
                    transaction_hash: *transaction.tx_hash(),
                    outcome,
                },
                certificate: None,
            };
            if let Some(old) = outcomes.get(&transfer_id) {
                if old.body == record.body && old.intent == record.intent {
                    record.certificate.clone_from(&old.certificate);
                }
                let source_disposition = matches!(old.body.outcome, TransferOutcome::Locked { .. })
                    && matches!(
                        record.body.outcome,
                        TransferOutcome::Released { .. } | TransferOutcome::Refunded { .. }
                    )
                    && old.intent == record.intent;
                if old.body != record.body && !source_disposition {
                    return Err(FastExecutionError::OutcomeConflict);
                }
            }
            outcomes.insert(transfer_id, record);
        }
    }
    Ok(())
}

fn reconstruct_transaction_results(
    log_id: LogId<u64>,
    block: &Block,
    receipts: &[tempo_primitives::TempoReceipt],
    transaction_results: &mut BTreeMap<B256, CommittedTransactionResult>,
) -> Result<(), FastExecutionError> {
    for (transaction, receipt) in block.body.transactions.iter().zip(receipts) {
        let transaction_hash = *transaction.tx_hash();
        let result = CommittedTransactionResult {
            log_term: log_id.leader_id.term,
            log_index: log_id.index,
            succeeded: receipt.status(),
        };
        if transaction_results
            .insert(transaction_hash, result)
            .is_some_and(|existing| existing != result)
        {
            return Err(FastExecutionError::TransactionResultConflict);
        }
    }
    Ok(())
}

fn decode_intent(input: &[u8]) -> Result<Option<TransferIntent>, FastExecutionError> {
    if input.starts_with(&IFastTransfer::lockCall::SELECTOR) {
        return Ok(Some(TransferIntent::decode(
            &IFastTransfer::lockCall::abi_decode(input)?.canonicalIntent,
        )?));
    }
    if input.starts_with(&IFastTransfer::resolveCall::SELECTOR) {
        return Ok(Some(TransferIntent::decode(
            &IFastTransfer::resolveCall::abi_decode(input)?.canonicalIntent,
        )?));
    }
    if input.starts_with(&IFastTransfer::recordOutcomeCall::SELECTOR) {
        return Ok(Some(TransferIntent::decode(
            &IFastTransfer::recordOutcomeCall::abi_decode(input)?.canonicalIntent,
        )?));
    }
    Ok(None)
}

fn word(data: &[u8], index: usize) -> Result<U256, FastExecutionError> {
    let start = index
        .checked_mul(32)
        .ok_or(FastExecutionError::InvalidEvent)?;
    let end = start
        .checked_add(32)
        .ok_or(FastExecutionError::InvalidEvent)?;
    Ok(U256::from_be_slice(
        data.get(start..end)
            .ok_or(FastExecutionError::InvalidEvent)?,
    ))
}

fn validate_intent_topics(
    log: &alloy_primitives::Log,
    intent: &TransferIntent,
) -> Result<(), FastExecutionError> {
    if log.topics().get(1) != Some(&intent.transfer_id())
        || log.topics().get(2) != Some(&intent.intent_hash())
    {
        return Err(FastExecutionError::InvalidEvent);
    }
    Ok(())
}

fn indexed_address(
    log: &alloy_primitives::Log,
    index: usize,
) -> Result<Address, FastExecutionError> {
    Ok(Address::from_word(
        *log.topics()
            .get(index)
            .ok_or(FastExecutionError::InvalidEvent)?,
    ))
}

fn data_address(data: &[u8], index: usize) -> Result<Address, FastExecutionError> {
    let start = index
        .checked_mul(32)
        .ok_or(FastExecutionError::InvalidEvent)?;
    let encoded = data
        .get(start..start + 32)
        .ok_or(FastExecutionError::InvalidEvent)?;
    if encoded[..12].iter().any(|byte| *byte != 0) {
        return Err(FastExecutionError::InvalidEvent);
    }
    Ok(Address::from_slice(&encoded[12..]))
}

fn run_on_runtime<T>(
    runtime: &tokio::runtime::Handle,
    future: impl Future<Output = Result<T, eyre::Report>>,
) -> Result<T, FastExecutionError> {
    tokio::task::block_in_place(|| runtime.block_on(future)).map_err(FastExecutionError::Engine)
}

#[derive(Serialize, Deserialize)]
struct DiskRecord {
    intent: Vec<u8>,
    body: Vec<u8>,
    certificate: Option<Vec<u8>>,
}

fn persist_outcomes(
    directory: &Path,
    outcomes: &BTreeMap<B256, CommittedTransferRecord>,
) -> Result<(), FastExecutionError> {
    let records = outcomes
        .values()
        .map(|record| DiskRecord {
            intent: record.intent.canonical_bytes(),
            body: record.body.canonical_bytes(),
            certificate: record
                .certificate
                .as_ref()
                .map(|certificate| certificate.canonical_bytes()),
        })
        .collect::<Vec<_>>();
    let payload = bincode::serialize(&records)?;
    if payload.len() > MAX_OUTCOMES_BYTES {
        return Err(FastExecutionError::OutcomesTooLarge);
    }
    let mut encoded = Vec::with_capacity(OUTCOMES_MAGIC.len() + 32 + payload.len());
    encoded.extend_from_slice(OUTCOMES_MAGIC);
    encoded.extend_from_slice(keccak256(&payload).as_slice());
    encoded.extend_from_slice(&payload);
    let temporary = directory.join(OUTCOMES_TEMP);
    let final_path = directory.join(OUTCOMES_FILE);
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&encoded)?;
    file.flush()?;
    file.sync_all()?;
    fs::rename(temporary, final_path)?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

fn load_outcomes(
    directory: &Path,
) -> Result<BTreeMap<B256, CommittedTransferRecord>, FastExecutionError> {
    let path = directory.join(OUTCOMES_FILE);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let encoded = fs::read(path)?;
    if encoded.len() < OUTCOMES_MAGIC.len() + 32 || &encoded[..8] != OUTCOMES_MAGIC {
        return Err(FastExecutionError::CorruptOutcomes);
    }
    let payload = &encoded[40..];
    if keccak256(payload).as_slice() != &encoded[8..40] {
        return Err(FastExecutionError::CorruptOutcomes);
    }
    let records: Vec<DiskRecord> = bincode::deserialize(payload)?;
    let mut outcomes = BTreeMap::new();
    for record in records {
        let intent = TransferIntent::decode(&record.intent)?;
        let body = decode_exact::<CertificateBody>(&record.body, MAX_CERTIFICATE_BYTES)?;
        let certificate = record
            .certificate
            .map(|encoded| OutcomeCertificate::decode(&encoded))
            .transpose()?;
        let transfer_id = intent.transfer_id();
        if body.transfer_id != transfer_id || body.intent_hash != intent.intent_hash() {
            return Err(FastExecutionError::CorruptOutcomes);
        }
        outcomes.insert(
            transfer_id,
            CommittedTransferRecord {
                intent,
                body,
                certificate,
            },
        );
    }
    Ok(outcomes)
}

fn persist_transaction_results(
    directory: &Path,
    results: &BTreeMap<B256, CommittedTransactionResult>,
) -> Result<(), FastExecutionError> {
    let payload = bincode::serialize(results)?;
    if payload.len() > MAX_OUTCOMES_BYTES {
        return Err(FastExecutionError::OutcomesTooLarge);
    }
    let mut encoded = Vec::with_capacity(TRANSACTION_RESULTS_MAGIC.len() + 32 + payload.len());
    encoded.extend_from_slice(TRANSACTION_RESULTS_MAGIC);
    encoded.extend_from_slice(keccak256(&payload).as_slice());
    encoded.extend_from_slice(&payload);
    let temporary = directory.join(TRANSACTION_RESULTS_TEMP);
    let final_path = directory.join(TRANSACTION_RESULTS_FILE);
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&encoded)?;
    file.flush()?;
    file.sync_all()?;
    fs::rename(temporary, final_path)?;
    File::open(directory)?.sync_all()?;
    Ok(())
}

fn load_transaction_results(
    directory: &Path,
) -> Result<BTreeMap<B256, CommittedTransactionResult>, FastExecutionError> {
    let path = directory.join(TRANSACTION_RESULTS_FILE);
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let encoded = fs::read(path)?;
    if encoded.len() < TRANSACTION_RESULTS_MAGIC.len() + 32
        || &encoded[..TRANSACTION_RESULTS_MAGIC.len()] != TRANSACTION_RESULTS_MAGIC
    {
        return Err(FastExecutionError::CorruptTransactionResults);
    }
    let checksum_start = TRANSACTION_RESULTS_MAGIC.len();
    let payload_start = checksum_start + 32;
    let payload = &encoded[payload_start..];
    if keccak256(payload).as_slice() != &encoded[checksum_start..payload_start] {
        return Err(FastExecutionError::CorruptTransactionResults);
    }
    Ok(bincode::deserialize(payload)?)
}

#[derive(Debug, thiserror::Error)]
pub enum FastExecutionError {
    #[error("committed block has trailing bytes")]
    TrailingBlockBytes,
    #[error("committed block parent does not match replicated parent")]
    ParentMismatch,
    #[error("committed block transactions do not match replicated transaction bytes")]
    TransactionMismatch,
    #[error("canonical header is missing after committed execution")]
    CanonicalHeaderMissing,
    #[error("canonical block is missing after committed execution")]
    CanonicalBlockMissing,
    #[error("canonical hash differs from committed execution")]
    CanonicalHashMismatch,
    #[error("canonical receipts are missing after committed execution")]
    ReceiptsMissing,
    #[error("canonical transaction and receipt counts differ")]
    ReceiptCountMismatch,
    #[error("existing canonical chain conflicts with the committed prefix")]
    CommittedForkConflict,
    #[error("restored execution differs from the durable committed output")]
    RestoreMismatch,
    #[error("fast-transfer receipt is missing its original canonical intent")]
    MissingIntent,
    #[error("invalid fast-transfer receipt event")]
    InvalidEvent,
    #[error("retained committed outcome conflicts with an existing transfer record")]
    OutcomeConflict,
    #[error("retained transaction result conflicts with its original committed position")]
    TransactionResultConflict,
    #[error("unknown committed transfer")]
    UnknownTransfer,
    #[error("retained committed outcome store is corrupt")]
    CorruptOutcomes,
    #[error("retained committed transaction-result store is corrupt")]
    CorruptTransactionResults,
    #[error("retained committed outcomes exceed the configured storage bound")]
    OutcomesTooLarge,
    #[error("committed L1 input is empty or invalid")]
    InvalidL1Input,
    #[error("same-anchor protocol epoch mismatch: expected {expected}, got {actual}")]
    ProtocolEpoch { expected: u64, actual: u64 },
    #[error("canonical fast batch scheduler is not installed")]
    BatchNotInstalled,
    #[error("canonical fast batch scheduler was installed more than once")]
    BatchAlreadyInstalled,
    #[error("invalid canonical fast settlement boundary: {0}")]
    Batch(String),
    #[error("committed outcome lock poisoned")]
    Poisoned,
    #[error("committed outcome signing failed: {0}")]
    Signing(String),
    #[error("committed replay witness is invalid: {0}")]
    Witness(String),
    #[error("Reth Engine API execution failed: {0}")]
    Engine(eyre::Report),
    #[error(transparent)]
    Provider(#[from] reth_storage_api::errors::provider::ProviderError),
    #[error(transparent)]
    Journal(#[from] zone_fast_transfer::JournalError),
    #[error(transparent)]
    Rlp(#[from] alloy_rlp::Error),
    #[error(transparent)]
    Abi(#[from] alloy_sol_types::Error),
    #[error(transparent)]
    Codec(#[from] zone_primitives::fast_transfer::CodecError),
    #[error(transparent)]
    Bincode(#[from] bincode::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
}
