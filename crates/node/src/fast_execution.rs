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
    BlockHeader as _, Sealable as _, Transaction as _, TxReceipt as _, transaction::TxHashRef as _,
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
use zone_fast_transfer::{
    DurableJournal, EpochRoster, QuorumVerifier, ReplicatedBlockInput as JournaledBlockInput,
};
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
    fast_network::OutcomeSigningRequest,
    fast_quorum::{
        CommittedBlock, FastActivation, FastAdmissionClock, RaftCommit, ReplicatedBlockInput,
        sign_committed_outcome,
    },
    fast_raft_state_machine::{
        AppliedBlock, CertifiedExecutionRecord, CommittedProtocolKind, CommittedProtocolRecord,
        CommittedTransferRecord, DurableStateMachineExecution,
    },
};

const OUTCOMES_FILE: &str = "committed-outcomes.bin";
const OUTCOMES_TEMP: &str = "committed-outcomes.tmp";
const OUTCOMES_MAGIC: &[u8; 8] = b"ZFOUT002";
const LEGACY_OUTCOMES_MAGIC: &[u8; 8] = b"ZFOUT001";
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

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct HistoricalOutcomeKey {
    transfer_id: B256,
    log_term: u64,
    log_index: u64,
    body_hash: B256,
}

impl From<&CertificateBody> for HistoricalOutcomeKey {
    fn from(body: &CertificateBody) -> Self {
        Self {
            transfer_id: body.transfer_id,
            log_term: body.log_term,
            log_index: body.log_index,
            body_hash: body.body_hash(),
        }
    }
}

impl From<OutcomeSigningRequest> for HistoricalOutcomeKey {
    fn from(request: OutcomeSigningRequest) -> Self {
        Self {
            transfer_id: request.transfer_id,
            log_term: request.log_term,
            log_index: request.log_index,
            body_hash: request.body_hash,
        }
    }
}

#[derive(Default)]
struct OutcomeStore {
    latest: BTreeMap<B256, CommittedTransferRecord>,
    history: BTreeMap<HistoricalOutcomeKey, CommittedTransferRecord>,
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
    outcomes: Mutex<OutcomeStore>,
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
        retain_certificate(&mut outcomes, certificate)?;
        persist_outcomes(&self.outcomes_directory, &outcomes)
    }

    /// Return only outcomes reconstructed for this exact committed log entry.
    pub(crate) fn outcomes_for_commit(
        &self,
        term: u64,
        index: u64,
    ) -> Result<Vec<CommittedTransferRecord>, FastExecutionError> {
        Ok(outcome_records_for_commit(
            &*self
                .outcomes
                .lock()
                .map_err(|_| FastExecutionError::Poisoned)?,
            term,
            index,
        ))
    }

    /// Reconstruct the complete immutable history record for a certificate from the exact
    /// canonical transaction/receipt and the fsynced applied Raft entry. The caller persists this
    /// in the committed state image before exposing the certificate through the service facade.
    pub(crate) fn certified_execution_record(
        &self,
        applied: &AppliedBlock,
        intent: &TransferIntent,
        certificate: &OutcomeCertificate,
    ) -> Result<CertifiedExecutionRecord, FastExecutionError> {
        let body = &certificate.body;
        if body.transfer_id != intent.transfer_id()
            || body.intent_hash != intent.intent_hash()
            || body.log_term != applied.log_id.leader_id.term
            || body.log_index != applied.log_id.index
            || body.block_height != applied.output.block_height
            || body.block_hash != applied.output.block_hash
            || body.state_root != applied.output.state_root
        {
            return Err(FastExecutionError::OutcomeConflict);
        }
        let block = self
            .provider
            .block_by_number(body.block_height)?
            .ok_or(FastExecutionError::CanonicalBlockMissing)?;
        let receipts = self
            .provider
            .receipts_by_block(BlockHashOrNumber::Number(body.block_height))?
            .ok_or(FastExecutionError::ReceiptsMissing)?;
        if block.body.transactions.len() != receipts.len() {
            return Err(FastExecutionError::ReceiptCountMismatch);
        }
        let (transaction, receipt) = block
            .body
            .transactions
            .iter()
            .zip(&receipts)
            .find(|(transaction, _)| *transaction.tx_hash() == body.transaction_hash)
            .ok_or(FastExecutionError::UnknownTransfer)?;
        if !receipt.status()
            || transaction.to() != Some(FAST_TRANSFER_ADDRESS)
            || !transaction_matches_intent_and_outcome(transaction.input(), intent, &body.outcome)?
        {
            return Err(FastExecutionError::OutcomeConflict);
        }
        Ok(CertifiedExecutionRecord {
            transfer_id: body.transfer_id,
            log_id: applied.log_id,
            block_height: body.block_height,
            block_hash: body.block_hash,
            state_root: body.state_root,
            transaction_hash: body.transaction_hash,
            canonical_intent: intent.canonical_bytes(),
            canonical_certificate: certificate.canonical_bytes(),
            native_calldata: transaction.input().to_vec(),
            canonical_receipt: bincode::serialize(receipt)?,
            replay_witness: applied.input.replay_witness.to_vec(),
        })
    }

    /// Recover destination terminal certificates that the native `recordOutcome` path already
    /// authenticated at this local committed coordinate. These are retained separately from the
    /// later source disposition certificate so drain recovery never depends on the latest-state
    /// projection.
    pub(crate) fn authenticated_terminal_records(
        &self,
        applied: &AppliedBlock,
    ) -> Result<Vec<CertifiedExecutionRecord>, FastExecutionError> {
        let block = self
            .provider
            .block_by_number(applied.output.block_height)?
            .ok_or(FastExecutionError::CanonicalBlockMissing)?;
        if block.header.hash_slow() != applied.output.block_hash {
            return Err(FastExecutionError::CanonicalHashMismatch);
        }
        let receipts = self
            .provider
            .receipts_by_block(BlockHashOrNumber::Number(applied.output.block_height))?
            .ok_or(FastExecutionError::ReceiptsMissing)?;
        if block.body.transactions.len() != receipts.len() {
            return Err(FastExecutionError::ReceiptCountMismatch);
        }
        let mut records = Vec::new();
        for (transaction, receipt) in block.body.transactions.iter().zip(&receipts) {
            if !receipt.status()
                || transaction.to() != Some(FAST_TRANSFER_ADDRESS)
                || !transaction
                    .input()
                    .starts_with(&IFastTransfer::recordOutcomeCall::SELECTOR)
            {
                continue;
            }
            let call = IFastTransfer::recordOutcomeCall::abi_decode(transaction.input())?;
            let intent = TransferIntent::decode(&call.canonicalIntent)?;
            let certificate = OutcomeCertificate::decode(&call.outcomeCertificate)?;
            if certificate.body.transfer_id != intent.transfer_id()
                || certificate.body.intent_hash != intent.intent_hash()
                || certificate.body.zone != intent.destination
                || !matches!(
                    certificate.body.outcome,
                    TransferOutcome::Paid { .. } | TransferOutcome::Rejected { .. }
                )
            {
                return Err(FastExecutionError::OutcomeConflict);
            }
            records.push(CertifiedExecutionRecord {
                transfer_id: certificate.body.transfer_id,
                log_id: applied.log_id,
                block_height: applied.output.block_height,
                block_hash: applied.output.block_hash,
                state_root: applied.output.state_root,
                transaction_hash: *transaction.tx_hash(),
                canonical_intent: intent.canonical_bytes(),
                canonical_certificate: certificate.canonical_bytes(),
                native_calldata: transaction.input().to_vec(),
                canonical_receipt: bincode::serialize(receipt)?,
                replay_witness: applied.input.replay_witness.to_vec(),
            });
        }
        Ok(records)
    }

    /// Bind a protocol observation to the successful canonical opening transaction of an exact
    /// applied Raft block. Callers must separately prove the payload from that block's imported
    /// anchor before persisting the record.
    pub(crate) fn imported_protocol_record(
        &self,
        applied: &AppliedBlock,
        kind: CommittedProtocolKind,
        canonical_payload: Vec<u8>,
    ) -> Result<CommittedProtocolRecord, FastExecutionError> {
        let attributes: ZonePayloadAttributes = bincode::deserialize(&applied.input.l1_inputs)?;
        if !matches!(attributes.tempo_import, TempoImport::Full(_)) || canonical_payload.is_empty()
        {
            return Err(FastExecutionError::OutcomeConflict);
        }
        let block = self
            .provider
            .block_by_number(applied.output.block_height)?
            .ok_or(FastExecutionError::CanonicalBlockMissing)?;
        if block.header.hash_slow() != applied.output.block_hash {
            return Err(FastExecutionError::CanonicalHashMismatch);
        }
        let receipts = self
            .provider
            .receipts_by_block(BlockHashOrNumber::Number(applied.output.block_height))?
            .ok_or(FastExecutionError::ReceiptsMissing)?;
        let (transaction, receipt) = block
            .body
            .transactions
            .first()
            .zip(receipts.first())
            .ok_or(FastExecutionError::ReceiptCountMismatch)?;
        if block.body.transactions.len() != receipts.len()
            || !receipt.status()
            || transaction.input().is_empty()
        {
            return Err(FastExecutionError::ReceiptCountMismatch);
        }
        Ok(CommittedProtocolRecord {
            log_id: applied.log_id,
            block_height: applied.output.block_height,
            block_hash: applied.output.block_hash,
            state_root: applied.output.state_root,
            transaction_hash: *transaction.tx_hash(),
            kind,
            canonical_payload,
            native_calldata: transaction.input().to_vec(),
            canonical_receipt: bincode::serialize(receipt)?,
        })
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
            .latest
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

    /// Resolve an authenticated signing selector against locally reconstructed immutable history.
    /// The request identifies a record but cannot provide or alter any signed body fields.
    pub(crate) fn outcome_for_signing(
        &self,
        request: OutcomeSigningRequest,
    ) -> Result<CommittedTransferRecord, FastExecutionError> {
        historical_outcome(
            &*self
                .outcomes
                .lock()
                .map_err(|_| FastExecutionError::Poisoned)?,
            request,
        )
    }

    /// Reconstruct and durably journal a signature from local committed receipts. No wire body is
    /// accepted by this boundary.
    pub(crate) fn sign_committed_transfer(
        &self,
        activation: &FastActivation,
        signer: &PrivateKeySigner,
        request: OutcomeSigningRequest,
    ) -> Result<(CertificateBody, SignatureBytes), FastExecutionError> {
        let record = historical_outcome(
            &*self
                .outcomes
                .lock()
                .map_err(|_| FastExecutionError::Poisoned)?,
            request,
        )?;
        sign_historical_outcome(self.journal.as_ref(), activation, signer, record)
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
                .history
                .iter()
                .filter_map(|(key, record)| {
                    record
                        .certificate
                        .clone()
                        .map(|certificate| (*key, (record.body.clone(), certificate)))
                })
                .collect::<BTreeMap<_, _>>();
            let mut rebuilt = OutcomeStore::default();
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
            for (key, record) in &mut rebuilt.history {
                if let Some((body, certificate)) = retained_certificates.get(key)
                    && body == &record.body
                {
                    record.certificate = Some(certificate.clone());
                }
            }
            for record in rebuilt.history.values() {
                if rebuilt
                    .latest
                    .get(&record.body.transfer_id)
                    .is_some_and(|latest| latest.body == record.body)
                {
                    rebuilt
                        .latest
                        .insert(record.body.transfer_id, record.clone());
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
            .latest
            .get(&transfer_id)
            .cloned())
    }
}

fn sign_historical_outcome(
    journal: &DurableJournal,
    activation: &FastActivation,
    signer: &PrivateKeySigner,
    record: CommittedTransferRecord,
) -> Result<(CertificateBody, SignatureBytes), FastExecutionError> {
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
    let epoch = activation.epoch();
    let protocol_version = u16::try_from(epoch.protocol_version)
        .map_err(|_| FastExecutionError::Signing("invalid finalized protocol version".into()))?;
    let domain = ZoneDomain {
        l1_chain_id: epoch.l1_chain_id,
        zone_id: epoch.zone_id,
        chain_id: epoch.zone_chain_id,
        portal: epoch.portal,
        authority_epoch: epoch.epoch,
        roster_hash: epoch.roster_hash,
        protocol_version,
    };
    if body.zone != domain || !epoch.members.contains(&signer.address()) {
        return Err(FastExecutionError::Signing(
            "historical outcome does not match the finalized signing authority".to_owned(),
        ));
    }
    let verifier = QuorumVerifier::new(
        EpochRoster::from_finalized_registry(domain, epoch.members)
            .map_err(|error| FastExecutionError::Signing(error.to_string()))?,
    );
    let unsigned = OutcomeCertificate {
        body: body.clone(),
        signatures: [SignatureBytes([0; 65]); 2],
    };
    let digest = verifier.outcome_digest(&unsigned);
    if let Some(existing) = journal.signing_record(digest, signer.address())? {
        if existing.log_term != body.log_term || existing.log_index != body.log_index {
            return Err(FastExecutionError::Signing(
                "durable outcome signing record has conflicting coordinates".to_owned(),
            ));
        }
        return Ok((body, existing.signature));
    }
    let signature = sign_committed_outcome(
        journal,
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

fn outcome_matches_applied(record: &CommittedTransferRecord, applied: &AppliedBlock) -> bool {
    applied.log_id.leader_id.term == record.body.log_term
        && applied.log_id.index == record.body.log_index
        && applied.output.block_height == record.body.block_height
        && applied.output.block_hash == record.body.block_hash
        && applied.output.state_root == record.body.state_root
}

pub(crate) fn validate_outcome_in_applied_prefix(
    record: &CommittedTransferRecord,
    applied: &[AppliedBlock],
) -> Result<(), FastExecutionError> {
    applied
        .iter()
        .any(|applied| outcome_matches_applied(record, applied))
        .then_some(())
        .ok_or(FastExecutionError::OutcomeOutsideCommittedPrefix)
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
    outcomes: &mut OutcomeStore,
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
                    .latest
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
            retain_reconstructed_outcome(outcomes, record)?;
        }
    }
    Ok(())
}

fn retain_reconstructed_outcome(
    outcomes: &mut OutcomeStore,
    mut record: CommittedTransferRecord,
) -> Result<(), FastExecutionError> {
    let transfer_id = record.body.transfer_id;
    if let Some(old) = outcomes.latest.get(&transfer_id) {
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
    let key = HistoricalOutcomeKey::from(&record.body);
    if outcomes
        .history
        .insert(key, record.clone())
        .is_some_and(|existing| existing != record)
    {
        return Err(FastExecutionError::OutcomeConflict);
    }
    outcomes.latest.insert(transfer_id, record);
    Ok(())
}

fn outcome_records_for_commit(
    outcomes: &OutcomeStore,
    term: u64,
    index: u64,
) -> Vec<CommittedTransferRecord> {
    outcomes
        .history
        .values()
        .filter(|record| record.body.log_term == term && record.body.log_index == index)
        .cloned()
        .collect()
}

fn historical_outcome(
    outcomes: &OutcomeStore,
    request: OutcomeSigningRequest,
) -> Result<CommittedTransferRecord, FastExecutionError> {
    outcomes
        .history
        .get(&request.into())
        .cloned()
        .ok_or(FastExecutionError::UnknownTransfer)
}

fn retain_certificate(
    outcomes: &mut OutcomeStore,
    certificate: OutcomeCertificate,
) -> Result<(), FastExecutionError> {
    let key = HistoricalOutcomeKey::from(&certificate.body);
    let retained = {
        let record = outcomes
            .history
            .get_mut(&key)
            .ok_or(FastExecutionError::UnknownTransfer)?;
        if record.body != certificate.body
            || record
                .certificate
                .as_ref()
                .is_some_and(|existing| existing != &certificate)
        {
            return Err(FastExecutionError::OutcomeConflict);
        }
        record.certificate = Some(certificate);
        record.clone()
    };
    if outcomes
        .latest
        .get(&key.transfer_id)
        .is_some_and(|latest| latest.body == retained.body)
    {
        outcomes.latest.insert(key.transfer_id, retained);
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

fn transaction_matches_intent_and_outcome(
    input: &[u8],
    intent: &TransferIntent,
    outcome: &TransferOutcome,
) -> Result<bool, FastExecutionError> {
    if matches!(
        outcome,
        TransferOutcome::Released { .. } | TransferOutcome::Refunded { .. }
    ) {
        return Ok(
            input.starts_with(&IFastTransfer::disposeEscrowCall::SELECTOR)
                && IFastTransfer::disposeEscrowCall::abi_decode(input)?.transferId
                    == intent.transfer_id(),
        );
    }
    Ok(decode_intent(input)?.as_ref() == Some(intent))
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

fn persist_outcomes(directory: &Path, outcomes: &OutcomeStore) -> Result<(), FastExecutionError> {
    let records = outcomes
        .history
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

fn load_outcomes(directory: &Path) -> Result<OutcomeStore, FastExecutionError> {
    let path = directory.join(OUTCOMES_FILE);
    if !path.exists() {
        return Ok(OutcomeStore::default());
    }
    let encoded = fs::read(path)?;
    if encoded.len() < OUTCOMES_MAGIC.len() + 32
        || (&encoded[..8] != OUTCOMES_MAGIC && &encoded[..8] != LEGACY_OUTCOMES_MAGIC)
    {
        return Err(FastExecutionError::CorruptOutcomes);
    }
    let payload = &encoded[40..];
    if keccak256(payload).as_slice() != &encoded[8..40] {
        return Err(FastExecutionError::CorruptOutcomes);
    }
    let records: Vec<DiskRecord> = bincode::deserialize(payload)?;
    let mut outcomes = OutcomeStore::default();
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
        let record = CommittedTransferRecord {
            intent,
            body,
            certificate,
        };
        let key = HistoricalOutcomeKey::from(&record.body);
        if outcomes
            .history
            .insert(key, record.clone())
            .is_some_and(|existing| existing != record)
        {
            return Err(FastExecutionError::CorruptOutcomes);
        }
        let replace_latest = outcomes.latest.get(&transfer_id).is_none_or(|latest| {
            (record.body.log_index, record.body.log_term)
                > (latest.body.log_index, latest.body.log_term)
        });
        if replace_latest {
            outcomes.latest.insert(transfer_id, record);
        }
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
    #[error("requested outcome is outside the fsynced applied prefix")]
    OutcomeOutsideCommittedPrefix,
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

#[cfg(test)]
mod tests {
    use alloy_sol_types::SolValue as _;
    use openraft::CommittedLeaderId;
    use tempfile::TempDir;
    use zone_primitives::fast_transfer::AssetId;

    use super::*;
    use crate::fast_quorum::{
        FAST_PROTOCOL_VERSION, FinalizedFastEpoch, FinalizedT14Capability, ReplicatedBlockInput,
        t14_fast_protocol_native_pin,
    };

    fn activation(signers: &[PrivateKeySigner; 3]) -> FastActivation {
        let members = signers.each_ref().map(|signer| signer.address());
        let portal = Address::repeat_byte(0x20);
        let peer_portals = std::array::from_fn(|index| Address::with_last_byte(0x40 + index as u8));
        let verifier_code_hash = B256::repeat_byte(0x21);
        let verifier_config_hash = B256::repeat_byte(0x22);
        let roster_hash = keccak256(
            (
                keccak256("TEMPO_ZONE_FAST_ROSTER_T14_V1"),
                portal,
                7_u64,
                FAST_PROTOCOL_VERSION,
                U256::from(2),
                U256::from(1),
                verifier_code_hash,
                verifier_config_hash,
                members.to_vec(),
                peer_portals.to_vec(),
            )
                .abi_encode(),
        );
        FastActivation::from_finalized_epoch(FinalizedT14Capability {
            epoch: FinalizedFastEpoch {
                l1_chain_id: 42,
                portal,
                zone_id: 1,
                zone_chain_id: 4_001,
                epoch: 7,
                protocol_version: FAST_PROTOCOL_VERSION,
                threshold: 2,
                proof_mode: 1,
                expected_verifier_code_hash: verifier_code_hash,
                expected_verifier_config_hash: verifier_config_hash,
                members,
                peer_portals,
                roster_hash,
                finalized_l1_block: 100,
            },
            native_pin: t14_fast_protocol_native_pin(),
            t14_active_at_anchor: true,
            current_epoch: 7,
            activated_at_l1_block: 99,
            closed: false,
            retired: false,
        })
        .unwrap()
    }

    fn domain(activation: &FastActivation) -> ZoneDomain {
        let epoch = activation.epoch();
        ZoneDomain {
            l1_chain_id: epoch.l1_chain_id,
            zone_id: epoch.zone_id,
            chain_id: epoch.zone_chain_id,
            portal: epoch.portal,
            authority_epoch: epoch.epoch,
            roster_hash: epoch.roster_hash,
            protocol_version: u16::try_from(epoch.protocol_version).unwrap(),
        }
    }

    fn intent(source: ZoneDomain) -> TransferIntent {
        TransferIntent {
            source,
            destination: ZoneDomain {
                zone_id: 2,
                chain_id: 4_002,
                portal: Address::repeat_byte(0x30),
                roster_hash: B256::repeat_byte(0x31),
                ..source
            },
            asset: AssetId {
                l1_token: Address::repeat_byte(0x41),
                source_token: Address::repeat_byte(0x42),
                destination_token: Address::repeat_byte(0x43),
                decimals: 6,
            },
            sender: Address::repeat_byte(0x51),
            recipient: Address::repeat_byte(0x52),
            refund_account: Address::repeat_byte(0x51),
            destination_pool: Address::repeat_byte(0x53),
            reimbursement_account: Address::repeat_byte(0x54),
            principal: U256::from(10),
            fee: U256::ZERO,
            quote_id: B256::repeat_byte(0x55),
            destination_expiry_height: 900,
            transfer_nonce: 19,
        }
    }

    fn record(
        intent: &TransferIntent,
        term: u64,
        index: u64,
        outcome: TransferOutcome,
    ) -> CommittedTransferRecord {
        CommittedTransferRecord {
            intent: intent.clone(),
            body: CertificateBody {
                transfer_id: intent.transfer_id(),
                intent_hash: intent.intent_hash(),
                zone: intent.source,
                log_term: term,
                log_index: index,
                block_height: index + 100,
                block_hash: B256::with_last_byte(index as u8),
                state_root: B256::with_last_byte(index as u8 + 1),
                transaction_hash: B256::with_last_byte(index as u8 + 2),
                outcome,
            },
            certificate: None,
        }
    }

    fn applied(record: &CommittedTransferRecord) -> AppliedBlock {
        AppliedBlock {
            log_id: LogId::new(
                CommittedLeaderId::new(record.body.log_term, 1),
                record.body.log_index,
            ),
            input: ReplicatedBlockInput {
                epoch: record.body.zone.authority_epoch,
                parent_hash: B256::repeat_byte(1),
                block_input: Bytes::from_static(&[1]),
                transactions: vec![Bytes::from_static(&[2])],
                l1_inputs: Bytes::from_static(&[3]),
                replay_witness: Bytes::from_static(&[4]),
            },
            output: CommittedBlock {
                input_digest: B256::repeat_byte(5),
                block_height: record.body.block_height,
                block_hash: record.body.block_hash,
                state_root: record.body.state_root,
                receipts_root: B256::repeat_byte(6),
            },
        }
    }

    #[test]
    fn released_latest_does_not_erase_locked_certificate_recovery() {
        let signers = [0x61, 0x62, 0x63]
            .map(|byte| PrivateKeySigner::from_bytes(&B256::with_last_byte(byte)).unwrap());
        let activation = activation(&signers);
        let intent = intent(domain(&activation));
        let locked = record(
            &intent,
            4,
            10,
            TransferOutcome::Locked {
                escrow: FAST_TRANSFER_ADDRESS,
                amount: intent.principal,
            },
        );
        let released = record(
            &intent,
            6,
            14,
            TransferOutcome::Released {
                beneficiary: intent.reimbursement_account,
                amount: intent.principal,
            },
        );
        let request = OutcomeSigningRequest::from(&locked.body);
        let directories = std::array::from_fn::<TempDir, 3, _>(|_| tempfile::tempdir().unwrap());

        // Each replica durably signed Locked, but no full certificate was gossiped before the
        // leader was lost. PaidAwaitingRelease changes native state without emitting a replacement
        // certificate body; Released later becomes only the latest native projection.
        for (index, directory) in directories.iter().enumerate() {
            let mut outcomes = OutcomeStore::default();
            retain_reconstructed_outcome(&mut outcomes, locked.clone()).unwrap();
            let journal = DurableJournal::open(directory.path().join("journal")).unwrap();
            sign_historical_outcome(&journal, &activation, &signers[index], locked.clone())
                .unwrap();
            retain_reconstructed_outcome(&mut outcomes, released.clone()).unwrap();
            persist_outcomes(directory.path(), &outcomes).unwrap();
        }

        // Replica zero is the lost leader. The two durable survivors restart with Released as the
        // latest outcome but recollect signatures over the original Locked term/index/body.
        let mut survivor_stores = Vec::new();
        let mut signatures = Vec::new();
        for index in 1..3 {
            let store = load_outcomes(directories[index].path()).unwrap();
            assert_eq!(store.latest[&intent.transfer_id()].body, released.body);
            let original = historical_outcome(&store, request).unwrap();
            assert_eq!(original.body, locked.body);
            validate_outcome_in_applied_prefix(&original, &[applied(&locked)]).unwrap();
            let journal = DurableJournal::open(directories[index].path().join("journal")).unwrap();
            let (_, signature) =
                sign_historical_outcome(&journal, &activation, &signers[index], original).unwrap();
            signatures.push(signature);
            survivor_stores.push(store);
        }
        let certificate = OutcomeCertificate {
            body: locked.body.clone(),
            signatures: [signatures[0], signatures[1]],
        };
        let epoch = activation.epoch();
        let verifier = QuorumVerifier::new(
            EpochRoster::from_finalized_registry(domain(&activation), epoch.members).unwrap(),
        );
        verifier.verify_outcome(&certificate, &intent).unwrap();

        // Both survivors retain the recovered full historical proof across another restart while
        // native latest remains Released. The original commit is still selected for certified
        // execution history, which is what source-barrier projection consumes.
        for (index, mut store) in survivor_stores.into_iter().enumerate() {
            retain_certificate(&mut store, certificate.clone()).unwrap();
            retain_certificate(&mut store, certificate.clone()).unwrap();
            persist_outcomes(directories[index + 1].path(), &store).unwrap();
            let restarted = load_outcomes(directories[index + 1].path()).unwrap();
            assert_eq!(restarted.latest[&intent.transfer_id()].body, released.body);
            let original = historical_outcome(&restarted, request).unwrap();
            assert_eq!(original.certificate.as_ref(), Some(&certificate));
            assert_eq!(
                outcome_records_for_commit(&restarted, 4, 10),
                vec![original]
            );
        }
    }

    #[test]
    fn historical_signing_identity_and_committed_prefix_fail_closed() {
        let signers = [0x71, 0x72, 0x73]
            .map(|byte| PrivateKeySigner::from_bytes(&B256::with_last_byte(byte)).unwrap());
        let activation = activation(&signers);
        let intent = intent(domain(&activation));
        let locked = record(
            &intent,
            8,
            21,
            TransferOutcome::Locked {
                escrow: FAST_TRANSFER_ADDRESS,
                amount: intent.principal,
            },
        );
        let mut outcomes = OutcomeStore::default();
        retain_reconstructed_outcome(&mut outcomes, locked.clone()).unwrap();
        let valid = OutcomeSigningRequest::from(&locked.body);
        assert_eq!(historical_outcome(&outcomes, valid).unwrap(), locked);

        for invalid in [
            OutcomeSigningRequest {
                transfer_id: B256::repeat_byte(0xdd),
                ..valid
            },
            OutcomeSigningRequest {
                body_hash: B256::repeat_byte(0xff),
                ..valid
            },
            OutcomeSigningRequest {
                log_term: valid.log_term + 1,
                ..valid
            },
            OutcomeSigningRequest {
                log_index: valid.log_index + 1,
                ..valid
            },
        ] {
            assert!(matches!(
                historical_outcome(&outcomes, invalid),
                Err(FastExecutionError::UnknownTransfer)
            ));
        }
        assert!(matches!(
            validate_outcome_in_applied_prefix(&locked, &[]),
            Err(FastExecutionError::OutcomeOutsideCommittedPrefix)
        ));
        let disposition = TransferOutcome::Released {
            beneficiary: intent.reimbursement_account,
            amount: intent.principal,
        };
        let calldata = IFastTransfer::disposeEscrowCall {
            transferId: intent.transfer_id(),
        }
        .abi_encode();
        assert!(transaction_matches_intent_and_outcome(&calldata, &intent, &disposition).unwrap());
        let wrong_calldata = IFastTransfer::disposeEscrowCall {
            transferId: B256::repeat_byte(0xee),
        }
        .abi_encode();
        assert!(
            !transaction_matches_intent_and_outcome(&wrong_calldata, &intent, &disposition)
                .unwrap()
        );
    }
}
