//! Concrete production adapters for the T14 direct-operator service.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_consensus::Transaction as _;
use alloy_network::TransactionBuilder as _;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, keccak256};
use alloy_provider::{DynProvider, Provider as _, ProviderBuilder};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall as _;
use reth_storage_api::{BlockNumReader, BlockReader, HeaderProvider, ReceiptProvider};
use reth_transaction_pool::{
    PoolTransaction as _, TransactionOrigin, TransactionPool, error::PoolErrorKind,
};
use tempo_alloy::{
    TempoNetwork, provider::ext::TempoProviderBuilderExt as _, rpc::TempoTransactionRequest,
};
use tempo_primitives::{Block, TempoHeader, TempoReceipt};
use tempo_transaction_pool::transaction::TempoPooledTransaction;
use tempo_zone_contracts::{FAST_TRANSFER_ADDRESS, IFastTransfer};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use zone_fast_transfer::{
    DurableJournal, EconomicActionKind, EconomicActionRecord, JournalIncomingRecord,
};
use zone_p2p::{
    AuthenticatedInterZoneRequest, InterZoneAuthoritySet, InterZonePeerAuthority,
    InterZoneServicePorts, InterZoneServiceRequest,
};
use zone_primitives::fast_transfer::{
    CancellationRequest, CanonicalEncode, MAX_CERTIFICATE_BYTES, MAX_INTENT_BYTES,
    OutcomeCertificate, QuoteCertificate, TransferIntent, TransferOutcome,
};
use zone_rpc::auth::AuthContext;

use crate::{
    fast_execution::CanonicalFastExecution,
    fast_raft_state_machine::{CommittedStateHandle, CommittedTransferRecord},
    fast_runtime::ProductionOutcomeCertification,
    fast_service::{
        CancellationStore, CertifiedPaymentReceipt, CommittedReceiptSink, CommittedTransferSource,
        FastServiceConfig, FastServiceError, FastServiceRoute, FastServiceTransport,
        FastServiceWire, FastTransferService, IncomingRecoverySource, PersistedIncomingDelivery,
        PrivateTransferStatus, ResolveTrigger, ServiceAcknowledgment, ServiceDelivery,
        ServiceFuture, SubmitResult,
    },
};

const SERVICE_ENVELOPE_VERSION: u8 = 1;
const SERVICE_ENVELOPE_HEADER_BYTES: usize = 2;
const SERVICE_ENVELOPE_MAX_BYTES: usize = SERVICE_ENVELOPE_HEADER_BYTES
    + 4
    + MAX_INTENT_BYTES
    + 4
    + MAX_CERTIFICATE_BYTES
    + 4
    + MAX_CERTIFICATE_BYTES;
const RECEIPT_CHANNEL_CAPACITY: usize = 128;

/// Canonical, bounded service codec. Every variable field is independently length-delimited and
/// bounded; no serde format participates in the peer protocol.
#[derive(Clone, Copy, Debug, Default)]
pub struct CanonicalFastServiceWire;

impl FastServiceWire for CanonicalFastServiceWire {
    fn encode(&self, delivery: &ServiceDelivery) -> Result<Vec<u8>, FastServiceError> {
        let mut encoded = Vec::new();
        encoded.push(SERVICE_ENVELOPE_VERSION);
        match delivery {
            ServiceDelivery::Quote(quote) => {
                encoded.push(0);
                put_bounded(
                    &mut encoded,
                    &quote.canonical_bytes(),
                    MAX_CERTIFICATE_BYTES,
                )?;
            }
            ServiceDelivery::Locked {
                intent,
                certificate,
                cancellation,
            } => {
                encoded.push(1);
                put_bounded(&mut encoded, &intent.canonical_bytes(), MAX_INTENT_BYTES)?;
                put_bounded(
                    &mut encoded,
                    &certificate.canonical_bytes(),
                    MAX_CERTIFICATE_BYTES,
                )?;
                put_bounded(
                    &mut encoded,
                    &cancellation
                        .as_ref()
                        .map(CanonicalEncode::canonical_bytes)
                        .unwrap_or_default(),
                    MAX_CERTIFICATE_BYTES,
                )?;
            }
            ServiceDelivery::Terminal {
                intent,
                certificate,
            } => {
                encoded.push(2);
                put_bounded(&mut encoded, &intent.canonical_bytes(), MAX_INTENT_BYTES)?;
                put_bounded(
                    &mut encoded,
                    &certificate.canonical_bytes(),
                    MAX_CERTIFICATE_BYTES,
                )?;
            }
            ServiceDelivery::Disposition {
                intent,
                certificate,
            } => {
                encoded.push(3);
                put_bounded(&mut encoded, &intent.canonical_bytes(), MAX_INTENT_BYTES)?;
                put_bounded(
                    &mut encoded,
                    &certificate.canonical_bytes(),
                    MAX_CERTIFICATE_BYTES,
                )?;
            }
        }
        if encoded.len() > SERVICE_ENVELOPE_MAX_BYTES {
            return Err(FastServiceError::Transport(
                "service envelope exceeds its canonical bound".to_owned(),
            ));
        }
        Ok(encoded)
    }

    fn decode(&self, encoded: &[u8]) -> Result<ServiceDelivery, FastServiceError> {
        if encoded.len() > SERVICE_ENVELOPE_MAX_BYTES {
            return Err(codec_error("service envelope exceeds its canonical bound"));
        }
        let mut reader = EnvelopeReader::new(encoded);
        if reader.byte()? != SERVICE_ENVELOPE_VERSION {
            return Err(codec_error("unsupported service envelope version"));
        }
        let delivery = match reader.byte()? {
            0 => ServiceDelivery::Quote(
                QuoteCertificate::decode(reader.bounded(MAX_CERTIFICATE_BYTES)?)
                    .map_err(|error| codec_error(error.to_string()))?,
            ),
            1 => {
                let intent = TransferIntent::decode(reader.bounded(MAX_INTENT_BYTES)?)
                    .map_err(|error| codec_error(error.to_string()))?;
                let certificate =
                    OutcomeCertificate::decode(reader.bounded(MAX_CERTIFICATE_BYTES)?)
                        .map_err(|error| codec_error(error.to_string()))?;
                let cancellation = reader.bounded(MAX_CERTIFICATE_BYTES)?;
                let cancellation = if cancellation.is_empty() {
                    None
                } else {
                    Some(
                        CancellationRequest::decode(cancellation)
                            .map_err(|error| codec_error(error.to_string()))?,
                    )
                };
                ServiceDelivery::Locked {
                    intent,
                    certificate,
                    cancellation,
                }
            }
            2 => ServiceDelivery::Terminal {
                intent: TransferIntent::decode(reader.bounded(MAX_INTENT_BYTES)?)
                    .map_err(|error| codec_error(error.to_string()))?,
                certificate: OutcomeCertificate::decode(reader.bounded(MAX_CERTIFICATE_BYTES)?)
                    .map_err(|error| codec_error(error.to_string()))?,
            },
            3 => ServiceDelivery::Disposition {
                intent: TransferIntent::decode(reader.bounded(MAX_INTENT_BYTES)?)
                    .map_err(|error| codec_error(error.to_string()))?,
                certificate: OutcomeCertificate::decode(reader.bounded(MAX_CERTIFICATE_BYTES)?)
                    .map_err(|error| codec_error(error.to_string()))?,
            },
            _ => return Err(codec_error("unknown service envelope kind")),
        };
        if !reader.is_empty() {
            return Err(codec_error("trailing service envelope bytes"));
        }
        validate_delivery_binding(&delivery)?;
        Ok(delivery)
    }
}

fn validate_delivery_binding(delivery: &ServiceDelivery) -> Result<(), FastServiceError> {
    let (intent, certificate) = match delivery {
        ServiceDelivery::Quote(_) => return Ok(()),
        ServiceDelivery::Locked {
            intent,
            certificate,
            cancellation,
        } => {
            if let Some(cancellation) = cancellation
                && (cancellation.transfer_id != intent.transfer_id()
                    || cancellation.intent_hash != intent.intent_hash()
                    || cancellation.sender != intent.sender
                    || cancellation.source != intent.source)
            {
                return Err(FastServiceError::InvalidCancellation);
            }
            (intent, certificate)
        }
        ServiceDelivery::Terminal {
            intent,
            certificate,
        }
        | ServiceDelivery::Disposition {
            intent,
            certificate,
        } => (intent, certificate),
    };
    if certificate.body.transfer_id != intent.transfer_id()
        || certificate.body.intent_hash != intent.intent_hash()
    {
        return Err(FastServiceError::InvalidIntent(
            "certificate does not bind the complete intent",
        ));
    }
    Ok(())
}

fn put_bounded(output: &mut Vec<u8>, value: &[u8], maximum: usize) -> Result<(), FastServiceError> {
    if value.len() > maximum {
        return Err(codec_error("service envelope field exceeds its bound"));
    }
    let length = u32::try_from(value.len())
        .map_err(|_| codec_error("service envelope field length overflows"))?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(value);
    Ok(())
}

struct EnvelopeReader<'a> {
    encoded: &'a [u8],
    offset: usize,
}

impl<'a> EnvelopeReader<'a> {
    const fn new(encoded: &'a [u8]) -> Self {
        Self { encoded, offset: 0 }
    }

    fn byte(&mut self) -> Result<u8, FastServiceError> {
        Ok(self.take(1)?[0])
    }

    fn bounded(&mut self, maximum: usize) -> Result<&'a [u8], FastServiceError> {
        let length = u32::from_be_bytes(
            self.take(4)?
                .try_into()
                .expect("four-byte service field length"),
        ) as usize;
        if length > maximum {
            return Err(codec_error("service envelope field exceeds its bound"));
        }
        self.take(length)
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], FastServiceError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| codec_error("service envelope offset overflows"))?;
        let value = self
            .encoded
            .get(self.offset..end)
            .ok_or_else(|| codec_error("truncated service envelope"))?;
        self.offset = end;
        Ok(value)
    }

    fn is_empty(&self) -> bool {
        self.offset == self.encoded.len()
    }
}

fn codec_error(error: impl Into<String>) -> FastServiceError {
    FastServiceError::Transport(error.into())
}

/// Concrete bounded port owned by the dedicated Commonware runtime.
#[derive(Clone)]
pub struct FastServiceCommonwarePort {
    authority: mpsc::Sender<InterZoneAuthoritySet>,
    requests: mpsc::Sender<InterZoneServiceRequest>,
    incoming: Arc<Mutex<Option<mpsc::Receiver<AuthenticatedInterZoneRequest>>>>,
    response_timeout: Duration,
}

impl std::fmt::Debug for FastServiceCommonwarePort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FastServiceCommonwarePort")
            .field("response_timeout", &self.response_timeout)
            .finish_non_exhaustive()
    }
}

impl FastServiceCommonwarePort {
    pub fn new(
        ports: InterZoneServicePorts,
        response_timeout: Duration,
    ) -> Result<Self, FastServiceError> {
        if ports.requests.max_capacity() == 0
            || ports.authority.max_capacity() == 0
            || response_timeout.is_zero()
        {
            return Err(FastServiceError::InvalidConfiguration);
        }
        Ok(Self {
            authority: ports.authority,
            requests: ports.requests,
            incoming: Arc::new(Mutex::new(Some(ports.incoming))),
            response_timeout,
        })
    }

    async fn install_authority(
        &self,
        authority: InterZoneAuthoritySet,
    ) -> Result<(), FastServiceError> {
        self.authority
            .send(authority)
            .await
            .map_err(|_| FastServiceError::Transport("Commonware authority port is closed".into()))
    }

    fn take_incoming(
        &self,
    ) -> Result<mpsc::Receiver<AuthenticatedInterZoneRequest>, FastServiceError> {
        self.incoming
            .lock()
            .map_err(|_| FastServiceError::Poisoned)?
            .take()
            .ok_or(FastServiceError::InvalidConfiguration)
    }
}

/// Private unicast transport over the runtime's actual Commonware authenticated/encrypted peer
/// connections. The runtime tries one exact finalized replica per request; this adapter rotates
/// through all three endpoints on failure and validates the authenticated acknowledgment.
#[derive(Clone, Debug)]
pub struct CommonwareFastServiceTransport {
    port: FastServiceCommonwarePort,
}

impl CommonwareFastServiceTransport {
    pub const fn new(port: FastServiceCommonwarePort) -> Self {
        Self { port }
    }
}

impl FastServiceTransport for CommonwareFastServiceTransport {
    fn send_authenticated_encrypted<'a>(
        &'a self,
        route: &'a FastServiceRoute,
        stream: u64,
        sequence: u64,
        encoded: &'a [u8],
    ) -> ServiceFuture<'a, Result<ServiceAcknowledgment, FastServiceError>> {
        Box::pin(async move {
            if encoded.len() > SERVICE_ENVELOPE_MAX_BYTES {
                return Err(FastServiceError::Transport(
                    "outbound service envelope is oversized".to_owned(),
                ));
            }
            let first = usize::try_from(sequence % 3).expect("modulo three fits usize");
            let mut last_error = "all finalized peer endpoints failed".to_owned();
            for offset in 0..3 {
                let endpoint_index = (first + offset) % 3;
                let (response, receiver) = oneshot::channel();
                let endpoint = &route.endpoints[endpoint_index];
                let request = InterZoneServiceRequest {
                    target: endpoint.ed25519.clone(),
                    remote_zone_id: route.roster.domain.zone_id,
                    stream,
                    sequence,
                    payload: encoded.to_vec(),
                    response,
                };
                if self.port.requests.send(request).await.is_err() {
                    return Err(FastServiceError::Transport(
                        "Commonware service port is closed".to_owned(),
                    ));
                }
                match tokio::time::timeout(self.port.response_timeout, receiver).await {
                    Ok(Ok(Ok(ack)))
                        if ack.remote_domain == route.roster.domain
                            && route.roster.members.contains(&ack.remote_member)
                            && ack.remote_member == endpoint.member
                            && ack.authenticated_peer == endpoint.ed25519
                            && ack.stream == stream
                            && ack.sequence == sequence =>
                    {
                        return Ok(ServiceAcknowledgment {
                            remote_member: ack.remote_member,
                            stream,
                            sequence,
                        });
                    }
                    Ok(Ok(Ok(_))) => {
                        last_error = "Commonware acknowledgment identity mismatch".to_owned();
                    }
                    Ok(Ok(Err(error))) => last_error = error,
                    Ok(Err(_)) => last_error = "Commonware response channel dropped".to_owned(),
                    Err(_) => last_error = "Commonware service request timed out".to_owned(),
                }
            }
            Err(FastServiceError::Transport(last_error))
        })
    }
}

/// Durable journal adapter for sender cancellations and ordered incoming recovery.
#[derive(Clone, Debug)]
pub struct DurableServiceJournalAdapter {
    journal: Arc<DurableJournal>,
}

impl DurableServiceJournalAdapter {
    pub const fn new(journal: Arc<DurableJournal>) -> Self {
        Self { journal }
    }
}

impl CancellationStore for DurableServiceJournalAdapter {
    fn persist(&self, request: &CancellationRequest) -> Result<(), FastServiceError> {
        self.journal
            .persist_cancellation(request.clone())
            .map(|_| ())
            .map_err(storage_error)
    }

    fn load_pending(&self) -> Result<Vec<CancellationRequest>, FastServiceError> {
        self.journal.pending_cancellations().map_err(storage_error)
    }

    fn complete(&self, transfer_id: B256) -> Result<(), FastServiceError> {
        self.journal
            .complete_cancellation(transfer_id)
            .map(|_| ())
            .map_err(storage_error)
    }
}

impl IncomingRecoverySource for DurableServiceJournalAdapter {
    fn load_unprocessed(&self) -> Result<Vec<PersistedIncomingDelivery>, FastServiceError> {
        self.journal
            .unprocessed_incoming()
            .map_err(storage_error)
            .map(|records| records.into_iter().map(incoming_record).collect())
    }
}

fn incoming_record(record: JournalIncomingRecord) -> PersistedIncomingDelivery {
    PersistedIncomingDelivery {
        peer_zone: record.peer_zone,
        stream: record.stream,
        sequence: record.sequence,
        expected_sequence: record.expected_sequence,
        payload: record.payload,
    }
}

fn storage_error(error: impl ToString) -> FastServiceError {
    FastServiceError::Storage(error.to_string())
}

/// One authenticated subscriber. The RPC layer must derive `caller` from its existing
/// `AuthContext`; this type has no unauthenticated constructor or wildcard subscription.
pub struct PrivateReceiptSubscription {
    pub caller: Address,
    pub expires_at: u64,
    receiver: mpsc::Receiver<CommittedTransferRecord>,
}

impl PrivateReceiptSubscription {
    pub async fn recv(&mut self) -> Option<CommittedTransferRecord> {
        self.receiver.recv().await
    }
}

#[derive(Debug)]
struct ReceiptSubscriber {
    caller: Address,
    expires_at: u64,
    sender: mpsc::Sender<CommittedTransferRecord>,
}

/// In-process private receipt channel. Publication fanout is filtered by the complete retained
/// intent and never places raw intents, certificates, or principals in metrics labels.
#[derive(Debug, Default)]
pub struct PrivateCommittedReceiptHub {
    next_id: Mutex<u64>,
    subscribers: Mutex<HashMap<u64, ReceiptSubscriber>>,
}

impl PrivateCommittedReceiptHub {
    pub fn subscribe(
        &self,
        auth: &AuthContext,
    ) -> Result<PrivateReceiptSubscription, FastServiceError> {
        if auth.caller == Address::ZERO || auth.expires_at <= now_unix_seconds() {
            return Err(FastServiceError::UnauthorizedPrincipal);
        }
        let (sender, receiver) = mpsc::channel(RECEIPT_CHANNEL_CAPACITY);
        let mut next_id = self
            .next_id
            .lock()
            .map_err(|_| FastServiceError::Poisoned)?;
        *next_id = next_id
            .checked_add(1)
            .ok_or(FastServiceError::InvalidConfiguration)?;
        self.subscribers
            .lock()
            .map_err(|_| FastServiceError::Poisoned)?
            .insert(
                *next_id,
                ReceiptSubscriber {
                    caller: auth.caller,
                    expires_at: auth.expires_at,
                    sender,
                },
            );
        Ok(PrivateReceiptSubscription {
            caller: auth.caller,
            expires_at: auth.expires_at,
            receiver,
        })
    }
}

impl CommittedReceiptSink for PrivateCommittedReceiptHub {
    fn publish(&self, record: &CommittedTransferRecord) -> Result<(), FastServiceError> {
        if record.certificate.is_none() {
            return Err(FastServiceError::CommittedState(
                "refusing to publish an uncertified committed outcome".to_owned(),
            ));
        }
        let mut subscribers = self
            .subscribers
            .lock()
            .map_err(|_| FastServiceError::Poisoned)?;
        subscribers.retain(|_, subscriber| {
            if subscriber.expires_at <= now_unix_seconds() {
                return false;
            }
            if !receipt_principal(subscriber.caller, &record.intent) {
                return true;
            }
            matches!(subscriber.sender.try_send(record.clone()), Ok(()))
        });
        Ok(())
    }
}

fn receipt_principal(caller: Address, intent: &TransferIntent) -> bool {
    caller == intent.sender
        || caller == intent.recipient
        || caller == intent.destination_pool
        || caller == intent.reimbursement_account
}

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Fsynced committed-prefix reader plus the production two-member certification hook.
pub struct CanonicalCommittedTransferSource<P> {
    committed: CommittedStateHandle<CanonicalFastExecution<P>>,
    certification: Arc<ProductionOutcomeCertification<P>>,
}

impl<P> CanonicalCommittedTransferSource<P> {
    pub const fn new(
        committed: CommittedStateHandle<CanonicalFastExecution<P>>,
        certification: Arc<ProductionOutcomeCertification<P>>,
    ) -> Self {
        Self {
            committed,
            certification,
        }
    }
}

impl<P> CommittedTransferSource for CanonicalCommittedTransferSource<P>
where
    P: BlockNumReader
        + BlockReader<Block = Block>
        + HeaderProvider<Header = TempoHeader>
        + ReceiptProvider<Receipt = TempoReceipt>
        + Clone
        + Send
        + Sync
        + 'static,
{
    fn committed_transfers(&self) -> Result<Vec<CommittedTransferRecord>, FastServiceError> {
        self.committed
            .committed_transfers()
            .map_err(committed_error)
    }

    fn committed_transfer(
        &self,
        transfer_id: B256,
    ) -> Result<Option<CommittedTransferRecord>, FastServiceError> {
        self.committed
            .committed_transfer(transfer_id)
            .map_err(committed_error)
    }

    fn committed_height(&self) -> Result<u64, FastServiceError> {
        self.committed
            .committed_head()
            .map_err(committed_error)?
            .map(|head| head.block.block_height)
            .ok_or_else(|| {
                FastServiceError::CommittedState("committed fast head is unavailable".to_owned())
            })
    }

    fn committed_transaction_result(
        &self,
        transaction_hash: B256,
    ) -> Result<Option<bool>, FastServiceError> {
        self.committed
            .committed_transaction_result(transaction_hash)
            .map_err(committed_error)
    }

    fn ensure_certificate<'a>(
        &'a self,
        record: &'a CommittedTransferRecord,
    ) -> ServiceFuture<'a, Result<OutcomeCertificate, FastServiceError>> {
        Box::pin(async move {
            self.certification
                .ensure_transfer_certificate(record.body.transfer_id)
                .await
                .map_err(FastServiceError::CommittedState)
        })
    }
}

fn committed_error(error: impl ToString) -> FastServiceError {
    FastServiceError::CommittedState(error.to_string())
}

/// Validated resources for native fast-transfer submissions.
pub struct ZoneNativeTransactionConfig {
    provider: DynProvider<TempoNetwork>,
    operator_signer: PrivateKeySigner,
    chain_id: u64,
    fee_token: Address,
    commit_timeout: Duration,
}

impl ZoneNativeTransactionConfig {
    /// Connect an owned wallet-backed Zone provider. Production assembly supplies the endpoint,
    /// signer and fee token explicitly; none are inferred or defaulted by this adapter.
    pub async fn connect(
        rpc_endpoint: &str,
        operator_signer: PrivateKeySigner,
        chain_id: u64,
        fee_token: Address,
        commit_timeout: Duration,
    ) -> Result<Self, FastServiceError> {
        if rpc_endpoint.is_empty() {
            return Err(FastServiceError::InvalidConfiguration);
        }
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .with_nonce_key_filler()
            .wallet(operator_signer.clone())
            .connect(rpc_endpoint)
            .await
            .map_err(native_error)?
            .erased();
        let config = Self {
            provider,
            operator_signer,
            chain_id,
            fee_token,
            commit_timeout,
        };
        config.validate().await?;
        Ok(config)
    }

    pub async fn validate(&self) -> Result<(), FastServiceError> {
        if self.operator_signer.address() == Address::ZERO
            || self.chain_id == 0
            || self.fee_token == Address::ZERO
            || self.commit_timeout.is_zero()
        {
            return Err(FastServiceError::InvalidConfiguration);
        }
        let provider_chain = self.provider.get_chain_id().await.map_err(native_error)?;
        if provider_chain != self.chain_id {
            return Err(FastServiceError::InvalidConfiguration);
        }
        Ok(())
    }
}

/// Real Zone transaction adapter. User locks are decoded and inserted into the ordinary local
/// transaction pool. Operator calls use a wallet-backed Alloy Zone provider, with exact calldata
/// and nonce persisted before submission, and complete only after canonical committed execution
/// reconstructs the expected outcome.
pub struct ZoneNativeTransactionSubmitter<T> {
    config: ZoneNativeTransactionConfig,
    pool: T,
    journal: Arc<DurableJournal>,
    committed: Arc<dyn CommittedTransferSource>,
    nonce_lock: tokio::sync::Mutex<()>,
}

impl<T> ZoneNativeTransactionSubmitter<T>
where
    T: TransactionPool<Transaction = TempoPooledTransaction> + Clone + Send + Sync + 'static,
{
    pub async fn new(
        config: ZoneNativeTransactionConfig,
        pool: T,
        journal: Arc<DurableJournal>,
        committed: Arc<dyn CommittedTransferSource>,
    ) -> Result<Self, FastServiceError> {
        config.validate().await?;
        Ok(Self {
            config,
            pool,
            journal,
            committed,
            nonce_lock: tokio::sync::Mutex::new(()),
        })
    }

    fn checked_signed_lock(
        &self,
        caller: Address,
        intent: &TransferIntent,
        quote: &QuoteCertificate,
        signed_transaction: &[u8],
    ) -> Result<TempoPooledTransaction, FastServiceError> {
        let transaction = TempoPooledTransaction::recover_raw_transaction(signed_transaction)
            .map_err(native_error)?;
        let expected = IFastTransfer::lockCall {
            canonicalIntent: intent.canonical_bytes().into(),
            quoteCertificate: quote.canonical_bytes().into(),
        }
        .abi_encode();
        let calls = transaction.inner().calls().collect::<Vec<_>>();
        if transaction.sender() != caller
            || caller != intent.sender
            || transaction.chain_id() != Some(self.config.chain_id)
            || transaction.value() != U256::ZERO
            || calls.len() != 1
            || calls[0].0 != TxKind::Call(FAST_TRANSFER_ADDRESS)
            || calls[0].1.as_ref() != expected
        {
            return Err(FastServiceError::NativeSubmission(
                "signed lock transaction does not exactly match caller, chain, target and calldata"
                    .to_owned(),
            ));
        }
        Ok(transaction)
    }

    async fn submit_operator_call(
        &self,
        transfer_id: B256,
        kind: EconomicActionKind,
        calldata: Vec<u8>,
        expected: ExpectedCommittedOutcome,
    ) -> Result<B256, FastServiceError> {
        let _nonce_guard = self.nonce_lock.lock().await;
        let action_id = economic_action_id(transfer_id, kind, &calldata);
        let mut action = if let Some(existing) = self
            .journal
            .economic_action(action_id)
            .map_err(storage_error)?
        {
            if existing.transfer_id != transfer_id
                || existing.kind != kind
                || existing.signer != self.config.operator_signer.address()
                || existing.chain_id != self.config.chain_id
                || existing.target != FAST_TRANSFER_ADDRESS
                || existing.calldata != calldata
            {
                return Err(FastServiceError::Storage(
                    "persisted economic action conflicts with requested native call".to_owned(),
                ));
            }
            existing
        } else {
            let nonce = self
                .config
                .provider
                .get_transaction_count(self.config.operator_signer.address())
                .pending()
                .await
                .map_err(native_error)?;
            let action = EconomicActionRecord {
                action_id,
                transfer_id,
                kind,
                signer: self.config.operator_signer.address(),
                nonce,
                chain_id: self.config.chain_id,
                target: FAST_TRANSFER_ADDRESS,
                calldata: calldata.clone(),
                submission_hashes: Vec::new(),
                completed_transaction_hash: None,
            };
            self.journal
                .persist_economic_action(action.clone())
                .map_err(storage_error)?;
            action
        };

        let mut reverted_hash = None;
        for transaction_hash in action.submission_hashes.iter().rev().copied() {
            match self
                .committed
                .committed_transaction_result(transaction_hash)?
            {
                Some(true) => {
                    self.wait_for_committed(transfer_id, transaction_hash, expected)
                        .await?;
                    return Ok(transaction_hash);
                }
                Some(false) => reverted_hash = Some(transaction_hash),
                None => {}
            }
        }
        if reverted_hash.is_some() {
            let next_nonce = self
                .config
                .provider
                .get_transaction_count(self.config.operator_signer.address())
                .pending()
                .await
                .map_err(native_error)?;
            action = self
                .journal
                .next_economic_attempt(action_id, next_nonce)
                .map_err(storage_error)?;
        }

        let request = TempoTransactionRequest {
            inner: TransactionRequest::default()
                .with_from(action.signer)
                .with_to(action.target)
                .with_nonce(action.nonce)
                .with_input(Bytes::from(action.calldata.clone())),
            fee_token: Some(self.config.fee_token),
            ..Default::default()
        };
        let pending = self
            .config
            .provider
            .send_transaction(request)
            .await
            .map_err(native_error)?;
        let transaction_hash = *pending.tx_hash();
        self.journal
            .record_economic_submission(action_id, transaction_hash)
            .map_err(storage_error)?;
        self.wait_for_committed(transfer_id, transaction_hash, expected)
            .await?;
        self.journal
            .complete_economic_action(action_id, transaction_hash)
            .map_err(storage_error)?;
        Ok(transaction_hash)
    }

    async fn wait_for_committed(
        &self,
        transfer_id: B256,
        transaction_hash: B256,
        expected: ExpectedCommittedOutcome,
    ) -> Result<(), FastServiceError> {
        tokio::time::timeout(self.config.commit_timeout, async {
            loop {
                match self
                    .committed
                    .committed_transaction_result(transaction_hash)?
                {
                    Some(false) => {
                        return Err(FastServiceError::NativeSubmission(
                            "native call canonically reverted; the liability remains queued"
                                .to_owned(),
                        ));
                    }
                    Some(true) if expected == ExpectedCommittedOutcome::SuccessfulCall => {
                        return Ok(());
                    }
                    Some(true) => {
                        if let Some(record) = self.committed.committed_transfer(transfer_id)?
                            && record.body.transaction_hash == transaction_hash
                            && expected.matches(&record.body.outcome)
                        {
                            return Ok(());
                        }
                    }
                    None => {}
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .map_err(|_| {
            FastServiceError::NativeSubmission(
                "transaction was not observed in the canonical committed prefix".to_owned(),
            )
        })?
    }
}

impl<T> crate::fast_service::NativeTransactionSubmitter for ZoneNativeTransactionSubmitter<T>
where
    T: TransactionPool<Transaction = TempoPooledTransaction> + Clone + Send + Sync + 'static,
{
    fn validate_signed_lock(
        &self,
        caller: Address,
        intent: &TransferIntent,
        quote: &QuoteCertificate,
        signed_transaction: &[u8],
    ) -> Result<(), FastServiceError> {
        self.checked_signed_lock(caller, intent, quote, signed_transaction)
            .map(|_| ())
    }

    fn submit_signed_lock<'a>(
        &'a self,
        caller: Address,
        intent: &'a TransferIntent,
        quote: &'a QuoteCertificate,
        signed_transaction: &'a [u8],
    ) -> ServiceFuture<'a, Result<B256, FastServiceError>> {
        Box::pin(async move {
            let transaction =
                self.checked_signed_lock(caller, intent, quote, signed_transaction)?;
            let transaction_hash = *transaction.hash();
            if let Err(error) = self
                .pool
                .add_transaction(TransactionOrigin::Local, transaction)
                .await
                && !matches!(error.kind, PoolErrorKind::AlreadyImported)
            {
                return Err(native_error(error));
            }
            self.wait_for_committed(
                intent.transfer_id(),
                transaction_hash,
                ExpectedCommittedOutcome::Locked,
            )
            .await?;
            Ok(transaction_hash)
        })
    }

    fn resolve<'a>(
        &'a self,
        intent: &'a TransferIntent,
        lock: &'a OutcomeCertificate,
        cancellation: Option<&'a CancellationRequest>,
        _trigger: ResolveTrigger,
    ) -> ServiceFuture<'a, Result<B256, FastServiceError>> {
        Box::pin(async move {
            let calldata = IFastTransfer::resolveCall {
                canonicalIntent: intent.canonical_bytes().into(),
                lockCertificate: lock.canonical_bytes().into(),
                cancellation: cancellation
                    .map(CanonicalEncode::canonical_bytes)
                    .unwrap_or_default()
                    .into(),
            }
            .abi_encode();
            self.submit_operator_call(
                intent.transfer_id(),
                EconomicActionKind::Resolve,
                calldata,
                ExpectedCommittedOutcome::DestinationTerminal,
            )
            .await
        })
    }

    fn record_outcome<'a>(
        &'a self,
        intent: &'a TransferIntent,
        outcome: &'a OutcomeCertificate,
    ) -> ServiceFuture<'a, Result<B256, FastServiceError>> {
        Box::pin(async move {
            let calldata = IFastTransfer::recordOutcomeCall {
                canonicalIntent: intent.canonical_bytes().into(),
                outcomeCertificate: outcome.canonical_bytes().into(),
            }
            .abi_encode();
            self.submit_operator_call(
                intent.transfer_id(),
                EconomicActionKind::RecordOutcome,
                calldata,
                ExpectedCommittedOutcome::SuccessfulCall,
            )
            .await
        })
    }

    fn dispose_escrow<'a>(
        &'a self,
        transfer_id: B256,
    ) -> ServiceFuture<'a, Result<B256, FastServiceError>> {
        Box::pin(async move {
            let calldata = IFastTransfer::disposeEscrowCall {
                transferId: transfer_id,
            }
            .abi_encode();
            self.submit_operator_call(
                transfer_id,
                EconomicActionKind::DisposeEscrow,
                calldata,
                ExpectedCommittedOutcome::SourceDisposed,
            )
            .await
        })
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ExpectedCommittedOutcome {
    Locked,
    DestinationTerminal,
    SuccessfulCall,
    SourceDisposed,
}

impl ExpectedCommittedOutcome {
    fn matches(self, outcome: &TransferOutcome) -> bool {
        match self {
            Self::Locked => matches!(outcome, TransferOutcome::Locked { .. }),
            Self::DestinationTerminal => {
                matches!(
                    outcome,
                    TransferOutcome::Paid { .. } | TransferOutcome::Rejected { .. }
                )
            }
            Self::SuccessfulCall => false,
            Self::SourceDisposed => matches!(
                outcome,
                TransferOutcome::Released { .. } | TransferOutcome::Refunded { .. }
            ),
        }
    }
}

fn economic_action_id(transfer_id: B256, kind: EconomicActionKind, calldata: &[u8]) -> B256 {
    let kind_tag = match kind {
        EconomicActionKind::Resolve => 0,
        EconomicActionKind::RecordOutcome => 1,
        EconomicActionKind::DisposeEscrow => 2,
    };
    let mut encoded = Vec::with_capacity(32 + 1 + 32 + 39);
    encoded.extend_from_slice(b"tempo.zone.fast-service.economic-action.v1");
    encoded.extend_from_slice(transfer_id.as_slice());
    encoded.push(kind_tag);
    encoded.extend_from_slice(keccak256(calldata).as_slice());
    keccak256(encoded)
}

fn native_error(error: impl ToString) -> FastServiceError {
    FastServiceError::NativeSubmission(error.to_string())
}

/// Object-safe service facade for authenticated RPC registration and runtime supervision.
#[derive(Clone)]
pub struct FastServiceHandle {
    service: Arc<FastTransferService>,
    receipts: Arc<PrivateCommittedReceiptHub>,
    runtime: Arc<FastServiceRuntime>,
}

struct FastServiceRuntime {
    stop: CancellationToken,
}

impl Drop for FastServiceRuntime {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl FastServiceHandle {
    const fn new(
        service: Arc<FastTransferService>,
        receipts: Arc<PrivateCommittedReceiptHub>,
        runtime: Arc<FastServiceRuntime>,
    ) -> Self {
        Self {
            service,
            receipts,
            runtime,
        }
    }

    pub async fn submit(
        &self,
        auth: &AuthContext,
        intent: TransferIntent,
        quote: QuoteCertificate,
        signed_transaction: &[u8],
    ) -> Result<SubmitResult, FastServiceError> {
        let caller = authenticated_caller(auth)?;
        self.service
            .submit(caller, intent, quote, signed_transaction)
            .await
    }

    pub fn status(
        &self,
        auth: &AuthContext,
        transfer_id: B256,
    ) -> Result<Option<PrivateTransferStatus>, FastServiceError> {
        self.service
            .status(authenticated_caller(auth)?, transfer_id)
    }

    pub fn payment_receipt(
        &self,
        auth: &AuthContext,
        transfer_id: B256,
    ) -> Result<Option<CertifiedPaymentReceipt>, FastServiceError> {
        self.service
            .payment_receipt(authenticated_caller(auth)?, transfer_id)
    }

    pub async fn cancel(
        &self,
        auth: &AuthContext,
        cancellation: CancellationRequest,
    ) -> Result<(), FastServiceError> {
        let caller = authenticated_caller(auth)?;
        self.service
            .request_cancellation(caller, cancellation)
            .await
    }

    pub fn subscribe(
        &self,
        auth: &AuthContext,
    ) -> Result<PrivateReceiptSubscription, FastServiceError> {
        self.receipts.subscribe(auth)
    }

    pub const fn service(&self) -> &Arc<FastTransferService> {
        &self.service
    }

    pub fn shutdown(&self) {
        self.runtime.stop.cancel();
    }
}

fn authenticated_caller(auth: &AuthContext) -> Result<Address, FastServiceError> {
    if auth.caller == Address::ZERO || auth.expires_at <= now_unix_seconds() {
        return Err(FastServiceError::UnauthorizedPrincipal);
    }
    Ok(auth.caller)
}

/// Assemble every concrete C4 adapter. Route authority remains the validated
/// `FastServiceConfig` imported by runtime from the finalized L1 anchor.
pub async fn assemble_fast_service<T>(
    config: FastServiceConfig,
    journal: Arc<DurableJournal>,
    commonware: FastServiceCommonwarePort,
    committed: Arc<dyn CommittedTransferSource>,
    native_config: ZoneNativeTransactionConfig,
    pool: T,
) -> Result<FastServiceHandle, FastServiceError>
where
    T: TransactionPool<Transaction = TempoPooledTransaction> + Clone + Send + Sync + 'static,
{
    config.validate()?;
    if config.routes.len() != 9 {
        return Err(FastServiceError::InvalidConfiguration);
    }
    native_config.validate().await?;
    let authority = inter_zone_authority(&config)?;
    commonware.install_authority(authority).await?;
    let incoming = commonware.take_incoming()?;
    let receipts = Arc::new(PrivateCommittedReceiptHub::default());
    let durable = Arc::new(DurableServiceJournalAdapter::new(journal.clone()));
    let native = Arc::new(
        ZoneNativeTransactionSubmitter::new(
            native_config,
            pool,
            journal.clone(),
            committed.clone(),
        )
        .await?,
    );
    let service = Arc::new(FastTransferService::new(
        config,
        journal,
        Arc::new(CanonicalFastServiceWire),
        Arc::new(CommonwareFastServiceTransport::new(commonware)),
        committed,
        native,
        receipts.clone(),
        durable.clone(),
        durable,
    )?);
    let runtime = Arc::new(FastServiceRuntime {
        stop: CancellationToken::new(),
    });
    let service_worker = service.clone();
    let service_stop = runtime.stop.clone();
    let service_failure_stop = runtime.stop.clone();
    tokio::spawn(async move {
        if let Err(error) = service_worker.run(service_stop).await {
            tracing::error!(target: "zone::fast", %error, "fast transfer service stopped");
            service_failure_stop.cancel();
        }
    });
    let incoming_service = service.clone();
    let incoming_stop = runtime.stop.clone();
    tokio::spawn(async move {
        run_commonware_incoming(incoming_service, incoming, incoming_stop.clone()).await;
        incoming_stop.cancel();
    });
    Ok(FastServiceHandle::new(service, receipts, runtime))
}

fn inter_zone_authority(
    config: &FastServiceConfig,
) -> Result<InterZoneAuthoritySet, FastServiceError> {
    let mut rosters = std::collections::BTreeMap::new();
    let mut peers = std::collections::BTreeMap::new();
    rosters.insert(
        config.local_roster.domain.zone_id,
        config.local_roster.clone(),
    );
    for endpoint in &config.local_endpoints {
        peers.insert(
            endpoint.ed25519.clone(),
            InterZonePeerAuthority {
                domain: config.local_roster.domain,
                certificate_member: endpoint.member,
            },
        );
    }
    for route in config.routes.values() {
        if rosters
            .insert(route.roster.domain.zone_id, route.roster.clone())
            .is_some()
        {
            return Err(FastServiceError::InvalidConfiguration);
        }
        for endpoint in &route.endpoints {
            if peers
                .insert(
                    endpoint.ed25519.clone(),
                    InterZonePeerAuthority {
                        domain: route.roster.domain,
                        certificate_member: endpoint.member,
                    },
                )
                .is_some()
            {
                return Err(FastServiceError::InvalidConfiguration);
            }
        }
    }
    Ok(InterZoneAuthoritySet { rosters, peers })
}

async fn run_commonware_incoming(
    service: Arc<FastTransferService>,
    mut incoming: mpsc::Receiver<AuthenticatedInterZoneRequest>,
    stop: CancellationToken,
) {
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            request = incoming.recv() => {
                let Some(request) = request else {
                    tracing::error!(target: "zone::fast", "Commonware inter-Zone incoming port closed");
                    return;
                };
                let result = service
                    .receive_authenticated(
                        &request.session,
                        request.stream,
                        request.sequence,
                        &request.payload,
                    )
                    .await
                    .and_then(|ack| {
                        if ack.remote_member == request.session.remote_member()
                            && ack.stream == request.stream
                            && ack.sequence == request.sequence
                        {
                            Ok(())
                        } else {
                            Err(FastServiceError::UnauthenticatedPeer)
                        }
                    })
                    .map_err(|error| error.to_string());
                let _ = request.response.send(result);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zone_primitives::fast_transfer::{AssetId, CertificateBody, SignatureBytes, ZoneDomain};

    fn domain(zone_id: u32, byte: u8) -> ZoneDomain {
        ZoneDomain {
            l1_chain_id: 1,
            zone_id,
            chain_id: 1_000 + u64::from(zone_id),
            portal: Address::repeat_byte(byte),
            authority_epoch: 7,
            roster_hash: B256::repeat_byte(byte),
            protocol_version: 1,
        }
    }

    fn intent() -> TransferIntent {
        TransferIntent {
            source: domain(1, 1),
            destination: domain(2, 2),
            asset: AssetId {
                l1_token: Address::repeat_byte(3),
                source_token: Address::repeat_byte(4),
                destination_token: Address::repeat_byte(5),
                decimals: 6,
            },
            sender: Address::repeat_byte(6),
            recipient: Address::repeat_byte(7),
            refund_account: Address::repeat_byte(6),
            destination_pool: Address::repeat_byte(8),
            reimbursement_account: Address::repeat_byte(9),
            principal: U256::from(100),
            fee: U256::from(2),
            quote_id: B256::repeat_byte(10),
            destination_expiry_height: 100,
            transfer_nonce: 11,
        }
    }

    fn certificate(intent: &TransferIntent) -> OutcomeCertificate {
        OutcomeCertificate {
            body: CertificateBody {
                transfer_id: intent.transfer_id(),
                intent_hash: intent.intent_hash(),
                zone: intent.source,
                log_term: 1,
                log_index: 2,
                block_height: 3,
                block_hash: B256::repeat_byte(12),
                state_root: B256::repeat_byte(13),
                transaction_hash: B256::repeat_byte(14),
                outcome: TransferOutcome::Locked {
                    escrow: FAST_TRANSFER_ADDRESS,
                    amount: U256::from(102),
                },
            },
            signatures: [SignatureBytes([15; 65]), SignatureBytes([16; 65])],
        }
    }

    #[test]
    fn complete_lock_envelope_round_trips_and_rejects_trailing_bytes() {
        let intent = intent();
        let delivery = ServiceDelivery::Locked {
            certificate: certificate(&intent),
            intent,
            cancellation: None,
        };
        let wire = CanonicalFastServiceWire;
        let encoded = wire.encode(&delivery).unwrap();
        assert_eq!(wire.decode(&encoded).unwrap(), delivery);

        let mut trailing = encoded;
        trailing.push(0);
        assert!(wire.decode(&trailing).is_err());
    }

    #[test]
    fn envelope_rejects_certificate_for_different_intent() {
        let first = intent();
        let mut second = first.clone();
        second.transfer_nonce += 1;
        let delivery = ServiceDelivery::Terminal {
            intent: second,
            certificate: certificate(&first),
        };
        let wire = CanonicalFastServiceWire;
        let encoded = wire.encode(&delivery).unwrap();
        assert!(wire.decode(&encoded).is_err());
    }

    #[test]
    fn receipt_channel_only_delivers_to_transfer_principals() {
        let intent = intent();
        let certificate = certificate(&intent);
        let record = CommittedTransferRecord {
            intent: intent.clone(),
            body: certificate.body.clone(),
            certificate: Some(certificate),
        };
        let hub = PrivateCommittedReceiptHub::default();
        let recipient_auth = AuthContext {
            caller: intent.recipient,
            expires_at: u64::MAX,
            keychain_key_id: None,
        };
        let unrelated_auth = AuthContext {
            caller: Address::repeat_byte(0xee),
            expires_at: u64::MAX,
            keychain_key_id: None,
        };
        let mut recipient = hub.subscribe(&recipient_auth).unwrap();
        let mut unrelated = hub.subscribe(&unrelated_auth).unwrap();
        hub.publish(&record).unwrap();

        assert_eq!(recipient.receiver.try_recv().unwrap(), record);
        assert!(matches!(
            unrelated.receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
}
