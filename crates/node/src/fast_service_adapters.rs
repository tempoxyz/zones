//! Concrete production adapters for the T14 direct-operator service.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_consensus::{Transaction as _, transaction::TxHashRef as _};
use alloy_eips::{
    BlockId,
    eip2718::{Decodable2718 as _, Encodable2718 as _},
};
use alloy_network::{ReceiptResponse as _, TransactionBuilder as _};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, keccak256};
use alloy_provider::{DynProvider, Provider as _, ProviderBuilder};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolCall as _, SolEvent as _};
use reth_storage_api::{BlockNumReader, BlockReader, HeaderProvider, ReceiptProvider};
use reth_transaction_pool::{
    PoolTransaction as _, TransactionOrigin, TransactionPool, error::PoolErrorKind,
};
use tempo_alloy::{
    TempoNetwork, provider::ext::TempoProviderBuilderExt as _, rpc::TempoTransactionRequest,
};
use tempo_primitives::{Block, TempoHeader, TempoReceipt};
use tempo_transaction_pool::transaction::TempoPooledTransaction;
use tempo_zone_contracts::{FAST_TRANSFER_ADDRESS, IFastTransfer, ImportedBarrierCall};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use zone_fast_transfer::{
    AuthenticatedPeerSession, DurableJournal, EconomicActionKind, EconomicActionRecord,
    EpochRoster, JournalIncomingRecord, drain::ProvenBarrierLock,
};
use zone_p2p::{
    AuthenticatedInterZoneRequest, InterZoneAuthoritySet, InterZonePeerAuthority,
    InterZoneServicePorts, InterZoneServiceRequest, NextRosterHandoffAuthoritySet,
};
use zone_primitives::fast_transfer::{
    CancellationRequest, CanonicalEncode, ExposureRetirementEvidence, FastBarrierStatement,
    HeaderAncestryProof, MAX_CERTIFICATE_BYTES, MAX_INTENT_BYTES, OutcomeCertificate,
    QuoteCertificate, TransferIntent, TransferOutcome, ZoneDomain,
};
use zone_rpc::auth::AuthContext;

use crate::{
    fast_drain::{DrainClosureObservation, DrainFuture, FastDrainError, FastDrainNative},
    fast_drain_adapters::{
        DrainCommonwareRoute, FastDrainIncoming, is_checkpoint_handoff_payload,
        is_fast_drain_payload,
    },
    fast_execution::CanonicalFastExecution,
    fast_raft_state_machine::{
        CommittedProtocolKind, CommittedProtocolRecord, CommittedStateHandle,
        CommittedTransferRecord,
    },
    fast_runtime::ProductionOutcomeCertification,
    fast_service::{
        CancellationStore, CertifiedPaymentReceipt, CommittedReceiptSink, CommittedTransferSource,
        FastServiceConfig, FastServiceError, FastServiceRoute, FastServiceTransport,
        FastServiceWire, FastTransferService, IncomingRecoverySource, PeerEndpoint,
        PersistedIncomingDelivery, PrivateTransferStatus, ResolveTrigger, ServiceAcknowledgment,
        ServiceDelivery, ServiceFuture, SubmitResult,
    },
};

/// Authenticated C5 consumer installed after C4 owns the sole Commonware incoming receiver.
/// Implementations must return only after durable service ingestion; an error withholds the ACK.
pub trait FastDrainIncomingHandler: Send + Sync + 'static {
    fn receive_authenticated<'a>(
        &'a self,
        session: &'a AuthenticatedPeerSession,
        stream: u64,
        sequence: u64,
        payload: &'a [u8],
    ) -> ServiceFuture<'a, Result<(), FastServiceError>>;
}

type FastDrainIncomingRegistry = RwLock<Option<Arc<dyn FastDrainIncomingHandler>>>;

pub trait FastNextRosterCheckpointHandler: Send + Sync + 'static {
    fn receive_authenticated<'a>(
        &'a self,
        session: &'a AuthenticatedPeerSession,
        stream: u64,
        sequence: u64,
        payload: &'a [u8],
    ) -> ServiceFuture<'a, Result<Vec<u8>, FastServiceError>>;
}

/// Bind a standalone next-member carrier only after that process has installed the exact old
/// accepted-prefix OpenRaft image and constructed its committed-state-backed signer handler.
pub async fn spawn_next_roster_checkpoint_signer(
    ports: InterZoneServicePorts,
    handler: Arc<dyn FastNextRosterCheckpointHandler>,
    stop: CancellationToken,
) -> Result<tokio::task::JoinHandle<()>, FastServiceError> {
    let mut incoming = ports.incoming;
    let keep_authority = ports.authority;
    let keep_handoff_authority = ports.next_roster_handoff_authority;
    let keep_requests = ports.requests;
    Ok(tokio::spawn(async move {
        let _keepers = (keep_authority, keep_handoff_authority, keep_requests);
        loop {
            tokio::select! {
                () = stop.cancelled() => return,
                request = incoming.recv() => {
                    let Some(request) = request else { return };
                    let result = if is_checkpoint_handoff_payload(&request.payload) {
                        handler.receive_authenticated(
                            &request.session,
                            request.stream,
                            request.sequence,
                            &request.payload,
                        ).await.map_err(|error| error.to_string())
                    } else {
                        Err("standalone next-roster endpoint accepts checkpoint handoff only".to_owned())
                    };
                    let _ = request.response.send(result);
                }
            }
        }
    }))
}

type FastNextRosterCheckpointRegistry = RwLock<Option<Arc<dyn FastNextRosterCheckpointHandler>>>;

impl FastDrainIncomingHandler for FastDrainIncoming {
    fn receive_authenticated<'a>(
        &'a self,
        session: &'a AuthenticatedPeerSession,
        stream: u64,
        sequence: u64,
        payload: &'a [u8],
    ) -> ServiceFuture<'a, Result<(), FastServiceError>> {
        Box::pin(async move {
            FastDrainIncoming::receive_authenticated(self, session, stream, sequence, payload)
                .await
                .map_err(|error| FastServiceError::Transport(error.to_string()))
        })
    }
}

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
const INCOMING_DISPATCH_LIMIT: usize = 64;
const INCOMING_ADMISSION_DISPATCH_LIMIT: usize = 48;

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
    handoff_authority: mpsc::Sender<NextRosterHandoffAuthoritySet>,
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
            handoff_authority: ports.next_roster_handoff_authority,
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
                            && ack.sequence == sequence
                            && ack.response_payload.is_empty() =>
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

#[derive(Clone, Debug)]
struct CommittedImportedBarrierSubmission {
    transaction_hash: B256,
    canonical_receipt: Vec<u8>,
    receipt_block_hash: B256,
    receipt_block_number: u64,
}

trait ImportedBarrierNativeSubmitter: Send + Sync + 'static {
    fn submit_imported_barrier<'a>(
        &'a self,
        call: ImportedBarrierCall,
        certificate_digest: B256,
    ) -> ServiceFuture<'a, Result<CommittedImportedBarrierSubmission, FastServiceError>>;
}

#[derive(Clone, Debug)]
struct CommittedExposureSubmission {
    action_id: B256,
    transaction_hash: B256,
    calldata: Vec<u8>,
    canonical_receipt: Vec<u8>,
    receipt_block_hash: B256,
    receipt_block_number: u64,
    receipt_root: B256,
}

pub(crate) struct VerifiedExposureSubmission {
    action_id: B256,
    transaction_hash: B256,
}

trait ExposureNativeSubmitter: Send + Sync + 'static {
    fn record_ancestry_checkpoint<'a>(
        &'a self,
        source_portal: Address,
        header_chain: HeaderAncestryProof,
    ) -> ServiceFuture<'a, Result<B256, FastServiceError>>;

    fn retire_exposure<'a>(
        &'a self,
        evidence: ExposureRetirementEvidence,
        source_zone: B256,
    ) -> ServiceFuture<'a, Result<CommittedExposureSubmission, FastServiceError>>;

    fn complete_retirement(
        &self,
        action_id: B256,
        transaction_hash: B256,
    ) -> Result<(), FastServiceError>;
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
        expected_intent_hash: Option<B256>,
        complete_action: bool,
    ) -> Result<B256, FastServiceError> {
        let action_id = economic_action_id(transfer_id, kind, &calldata);
        // Serialize only nonce allocation, durable preparation, submission and durable hash
        // recording. Waiting for Raft application must not serialize unrelated recovery work.
        let transaction_hash = {
            let _nonce_guard = self.nonce_lock.lock().await;
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

            let mut committed_success = None;
            // A single reverted replacement does not resolve another hash whose canonical result
            // is still unknown. Only an entirely canonical-failed attempt may consume a new nonce.
            let mut all_submissions_reverted = !action.submission_hashes.is_empty();
            for transaction_hash in action.submission_hashes.iter().rev().copied() {
                match self
                    .committed
                    .committed_transaction_result(transaction_hash)?
                {
                    Some(true) => {
                        committed_success = Some(transaction_hash);
                        break;
                    }
                    Some(false) => {}
                    None => all_submissions_reverted = false,
                }
            }
            if let Some(transaction_hash) = committed_success {
                transaction_hash
            } else {
                if all_submissions_reverted {
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
                transaction_hash
            }
        };
        self.wait_for_committed(
            transfer_id,
            transaction_hash,
            expected,
            expected_intent_hash,
        )
        .await?;
        if complete_action {
            self.journal
                .complete_economic_action(action_id, transaction_hash)
                .map_err(storage_error)?;
        }
        Ok(transaction_hash)
    }

    async fn wait_for_committed(
        &self,
        transfer_id: B256,
        transaction_hash: B256,
        expected: ExpectedCommittedOutcome,
        expected_intent_hash: Option<B256>,
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
                            && expected_intent_hash
                                .is_none_or(|hash| record.intent.intent_hash() == hash)
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
                ExpectedCommittedOutcome::Locked {
                    escrow: FAST_TRANSFER_ADDRESS,
                    amount: intent.principal.checked_add(intent.fee).ok_or_else(|| {
                        FastServiceError::NativeSubmission(
                            "lock principal plus fee overflows".to_owned(),
                        )
                    })?,
                },
                Some(intent.intent_hash()),
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
                // Ordinary open-epoch delivery has no drain inclusion proof. After closure
                // native execution rejects this path; the committed C5 barrier resolver must
                // provide the exact historical proof rather than treating a retry as authority.
                barrierProof: Bytes::new(),
            }
            .abi_encode();
            self.submit_operator_call(
                intent.transfer_id(),
                EconomicActionKind::Resolve,
                calldata,
                ExpectedCommittedOutcome::DestinationTerminal {
                    pool: intent.destination_pool,
                    recipient: intent.recipient,
                    principal: intent.principal,
                },
                Some(intent.intent_hash()),
                true,
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
                None,
                true,
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
                None,
                true,
            )
            .await
        })
    }
}

impl<T> ImportedBarrierNativeSubmitter for ZoneNativeTransactionSubmitter<T>
where
    T: TransactionPool<Transaction = TempoPooledTransaction> + Clone + Send + Sync + 'static,
{
    fn submit_imported_barrier<'a>(
        &'a self,
        call: ImportedBarrierCall,
        certificate_digest: B256,
    ) -> ServiceFuture<'a, Result<CommittedImportedBarrierSubmission, FastServiceError>> {
        Box::pin(async move {
            if call.target != FAST_TRANSFER_ADDRESS || call.calldata.is_empty() {
                return Err(FastServiceError::NativeSubmission(
                    "invalid imported-barrier target or calldata".to_owned(),
                ));
            }
            let transaction_hash = self
                .submit_operator_call(
                    call.barrier_digest,
                    EconomicActionKind::RecordOutcome,
                    call.calldata.to_vec(),
                    ExpectedCommittedOutcome::SuccessfulCall,
                    None,
                    true,
                )
                .await?;
            let receipt = self
                .config
                .provider
                .get_transaction_receipt(transaction_hash)
                .await
                .map_err(native_error)?
                .ok_or_else(|| {
                    FastServiceError::NativeSubmission(
                        "committed imported-barrier receipt is unavailable".to_owned(),
                    )
                })?;
            if !receipt.status() {
                return Err(FastServiceError::NativeSubmission(
                    "imported-barrier transaction canonically reverted".to_owned(),
                ));
            }
            let matching_events = receipt
                .logs()
                .iter()
                .filter_map(|log| {
                    IFastTransfer::ImportedBarrierRecorded::decode_log(&log.inner).ok()
                })
                .filter(|event| {
                    event.data.destinationEpoch == call.destination_epoch
                        && event.data.sourcePortal == call.source_portal
                        && event.data.sourceEpoch == call.source_epoch
                        && event.data.barrierDigest == certificate_digest
                        && event.data.completeLockRoot == call.complete_lock_root
                })
                .count();
            if matching_events != 1 {
                return Err(FastServiceError::NativeSubmission(
                    "committed receipt does not contain the exact imported-barrier event"
                        .to_owned(),
                ));
            }
            let receipt_block_hash = receipt.block_hash().ok_or_else(|| {
                FastServiceError::NativeSubmission(
                    "committed imported-barrier receipt has no block hash".to_owned(),
                )
            })?;
            let receipt_block_number = receipt.block_number().ok_or_else(|| {
                FastServiceError::NativeSubmission(
                    "committed imported-barrier receipt has no block number".to_owned(),
                )
            })?;
            Ok(CommittedImportedBarrierSubmission {
                transaction_hash,
                canonical_receipt: serde_json::to_vec(&receipt).map_err(storage_error)?,
                receipt_block_hash,
                receipt_block_number,
            })
        })
    }
}

impl<T> ExposureNativeSubmitter for ZoneNativeTransactionSubmitter<T>
where
    T: TransactionPool<Transaction = TempoPooledTransaction> + Clone + Send + Sync + 'static,
{
    fn record_ancestry_checkpoint<'a>(
        &'a self,
        source_portal: Address,
        header_chain: HeaderAncestryProof,
    ) -> ServiceFuture<'a, Result<B256, FastServiceError>> {
        Box::pin(async move {
            if source_portal.is_zero() || header_chain.headers.is_empty() {
                return Err(FastServiceError::InvalidConfiguration);
            }
            let encoded = header_chain.canonical_bytes();
            let calldata = IFastTransfer::recordAncestryCheckpointCall {
                sourcePortal: source_portal,
                headerChain: encoded.clone().into(),
            }
            .abi_encode();
            self.submit_operator_call(
                keccak256([source_portal.as_slice(), keccak256(&encoded).as_slice()].concat()),
                EconomicActionKind::RecordAncestryCheckpoint,
                calldata,
                ExpectedCommittedOutcome::SuccessfulCall,
                None,
                true,
            )
            .await
        })
    }

    fn retire_exposure<'a>(
        &'a self,
        evidence: ExposureRetirementEvidence,
        source_zone: B256,
    ) -> ServiceFuture<'a, Result<CommittedExposureSubmission, FastServiceError>> {
        Box::pin(async move {
            let calldata = IFastTransfer::retireExposureCall {
                canonicalEvidence: evidence.canonical_bytes().into(),
            }
            .abi_encode();
            let action_id = economic_action_id(
                evidence.transfer_id,
                EconomicActionKind::RetireExposure,
                &calldata,
            );
            let transaction_hash = self
                .submit_operator_call(
                    evidence.transfer_id,
                    EconomicActionKind::RetireExposure,
                    calldata,
                    ExpectedCommittedOutcome::SuccessfulCall,
                    Some(evidence.intent_hash),
                    false,
                )
                .await?;
            let receipt = self
                .config
                .provider
                .get_transaction_receipt(transaction_hash)
                .await
                .map_err(native_error)?
                .ok_or_else(|| {
                    FastServiceError::NativeSubmission(
                        "committed exposure-retirement receipt is unavailable".into(),
                    )
                })?;
            if !receipt.status() {
                return Err(FastServiceError::NativeSubmission(
                    "exposure-retirement receipt reverted".into(),
                ));
            }
            let matching = receipt
                .inner
                .inner
                .receipt
                .logs
                .iter()
                .filter_map(|log| IFastTransfer::ExposureRetired::decode_log(&log.inner).ok())
                .filter(|event| {
                    event.data.transferId == evidence.transfer_id
                        && event.data.sourceZone == source_zone
                        && event.data.token == evidence.destination_token
                        && U256::from(event.data.principal) == evidence.principal
                })
                .count();
            if matching != 1 {
                return Err(FastServiceError::NativeSubmission(
                    "committed receipt lacks the exact ExposureRetired effect".into(),
                ));
            }
            let receipt_block_hash = receipt.block_hash().ok_or_else(|| {
                FastServiceError::NativeSubmission(
                    "exposure-retirement receipt has no block hash".into(),
                )
            })?;
            let receipt_block_number = receipt.block_number().ok_or_else(|| {
                FastServiceError::NativeSubmission(
                    "exposure-retirement receipt has no block number".into(),
                )
            })?;
            let receipt_index = usize::try_from(receipt.transaction_index().ok_or_else(|| {
                FastServiceError::NativeSubmission(
                    "exposure-retirement receipt has no transaction index".into(),
                )
            })?)
            .map_err(native_error)?;
            let canonical_target_receipt = receipt
                .inner
                .inner
                .clone()
                .map_receipt(|receipt| receipt.map_logs(|log| log.into_inner()))
                .encoded_2718();
            let mut block_receipts = self
                .config
                .provider
                .get_block_receipts(BlockId::hash_canonical(receipt_block_hash))
                .await
                .map_err(native_error)?
                .ok_or_else(|| {
                    FastServiceError::NativeSubmission(
                        "committed exposure-retirement block receipts are unavailable".into(),
                    )
                })?;
            block_receipts.sort_by_key(|receipt| receipt.transaction_index());
            let mut encoded_receipts = Vec::with_capacity(block_receipts.len());
            for (index, block_receipt) in block_receipts.into_iter().enumerate() {
                if block_receipt.transaction_index() != Some(index as u64) {
                    return Err(FastServiceError::NativeSubmission(
                        "committed exposure-retirement block receipts are non-contiguous".into(),
                    ));
                }
                encoded_receipts.push(
                    block_receipt
                        .inner
                        .inner
                        .map_receipt(|receipt| receipt.map_logs(|log| log.into_inner()))
                        .encoded_2718(),
                );
            }
            if encoded_receipts.get(receipt_index) != Some(&canonical_target_receipt) {
                return Err(FastServiceError::NativeSubmission(
                    "exposure-retirement receipt is absent from its complete block receipts".into(),
                ));
            }
            Ok(CommittedExposureSubmission {
                action_id,
                transaction_hash,
                calldata: IFastTransfer::retireExposureCall {
                    canonicalEvidence: evidence.canonical_bytes().into(),
                }
                .abi_encode(),
                canonical_receipt: serde_json::to_vec(&receipt).map_err(storage_error)?,
                receipt_block_hash,
                receipt_block_number,
                receipt_root: canonical_receipts_root(&encoded_receipts),
            })
        })
    }

    fn complete_retirement(
        &self,
        action_id: B256,
        transaction_hash: B256,
    ) -> Result<(), FastServiceError> {
        self.journal
            .complete_economic_action(action_id, transaction_hash)
            .map(|_| ())
            .map_err(storage_error)
    }
}

impl<T> FastDrainNative for ZoneNativeTransactionSubmitter<T>
where
    T: TransactionPool<Transaction = TempoPooledTransaction> + Clone + Send + Sync + 'static,
{
    fn resolve_old_lock<'a>(
        &'a self,
        statement: &'a FastBarrierStatement,
        proven: &'a ProvenBarrierLock,
    ) -> DrainFuture<'a, Result<(), FastDrainError>> {
        Box::pin(async move {
            let (transfer_id, intent_hash, calldata) =
                encode_old_lock_resolution(statement, proven)?;
            self.submit_operator_call(
                transfer_id,
                EconomicActionKind::Resolve,
                calldata,
                ExpectedCommittedOutcome::DestinationTerminal {
                    pool: proven.lock.intent.destination_pool,
                    recipient: proven.lock.intent.recipient,
                    principal: proven.lock.intent.principal,
                },
                Some(intent_hash),
                true,
            )
            .await
            .map_err(|error| FastDrainError::Native(error.to_string()))?;
            Ok(())
        })
    }
}

fn encode_old_lock_resolution(
    statement: &FastBarrierStatement,
    proven: &ProvenBarrierLock,
) -> Result<(B256, B256, Vec<u8>), FastDrainError> {
    let intent = &proven.lock.intent;
    let lock = &proven.lock.lock;
    let body = &lock.body;
    let locked_amount_matches = intent
        .principal
        .checked_add(intent.fee)
        .is_some_and(|expected| {
            matches!(
                &body.outcome,
                TransferOutcome::Locked { escrow, amount }
                    if *escrow == FAST_TRANSFER_ADDRESS && *amount == expected
            )
        });
    if intent.validate().is_err()
        || intent.transfer_id() != body.transfer_id
        || intent.intent_hash() != body.intent_hash
        || intent.source.l1_chain_id == 0
        || intent.destination.l1_chain_id != intent.source.l1_chain_id
        || intent.source.portal != statement.source_portal
        || intent.source.authority_epoch != statement.source_epoch
        || intent.destination.portal != statement.destination_portal
        || intent.destination.authority_epoch != statement.destination_epoch
        || body.zone != intent.source
        || !locked_amount_matches
        || statement.destination_portal.is_zero()
        || statement.source_portal.is_zero()
        || statement.destination_portal == statement.source_portal
        || statement.destination_epoch == 0
        || statement.source_epoch == 0
        || statement.closure_hash.is_zero()
        || statement.imported_anchor_number == 0
        || statement.imported_anchor_hash.is_zero()
        || statement.log_index < statement.lock_log_watermark
        || statement.log_index < body.log_index
        || statement.block_hash.is_zero()
        || statement.state_root.is_zero()
        || statement.complete_lock_root.is_zero()
        || body.log_index == 0
        || body.block_hash.is_zero()
        || body.state_root.is_zero()
    {
        return Err(FastDrainError::Native(
            "old lock does not exactly bind the signed barrier and Locked certificate".to_owned(),
        ));
    }
    let barrier_proof = proven
        .native_barrier_proof(intent.source.l1_chain_id, statement)
        .map_err(FastDrainError::Protocol)?;
    if barrier_proof.is_empty() {
        return Err(FastDrainError::Native(
            "canonical old-lock barrier proof is empty".to_owned(),
        ));
    }
    let calldata = IFastTransfer::resolveCall {
        canonicalIntent: intent.canonical_bytes().into(),
        lockCertificate: lock.canonical_bytes().into(),
        cancellation: Bytes::new(),
        barrierProof: barrier_proof.into(),
    }
    .abi_encode();
    Ok((intent.transfer_id(), intent.intent_hash(), calldata))
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ExpectedCommittedOutcome {
    Locked {
        escrow: Address,
        amount: U256,
    },
    DestinationTerminal {
        pool: Address,
        recipient: Address,
        principal: U256,
    },
    SuccessfulCall,
    SourceDisposed,
}

impl ExpectedCommittedOutcome {
    fn matches(self, outcome: &TransferOutcome) -> bool {
        match self {
            Self::Locked { escrow, amount } => matches!(
                outcome,
                TransferOutcome::Locked {
                    escrow: actual_escrow,
                    amount: actual_amount,
                } if *actual_escrow == escrow && *actual_amount == amount
            ),
            Self::DestinationTerminal {
                pool,
                recipient,
                principal,
            } => {
                matches!(
                    outcome,
                    TransferOutcome::Paid {
                        pool: actual_pool,
                        recipient: actual_recipient,
                        principal: actual_principal,
                    } if *actual_pool == pool
                        && *actual_recipient == recipient
                        && *actual_principal == principal
                ) || matches!(outcome, TransferOutcome::Rejected { .. })
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
        EconomicActionKind::RecordAncestryCheckpoint => 3,
        EconomicActionKind::RetireExposure => 4,
    };
    let mut encoded = Vec::with_capacity(32 + 1 + 32 + 39);
    encoded.extend_from_slice(b"tempo.zone.fast-service.economic-action.v1");
    encoded.extend_from_slice(transfer_id.as_slice());
    encoded.push(kind_tag);
    encoded.extend_from_slice(keccak256(calldata).as_slice());
    keccak256(encoded)
}

fn canonical_receipts_root(receipts: &[Vec<u8>]) -> B256 {
    if receipts.is_empty() {
        return reth_trie_common::EMPTY_ROOT_HASH;
    }
    let mut builder = reth_trie_common::HashBuilder::default();
    for insertion in 0..receipts.len() {
        let index = if insertion > 0x7f {
            insertion
        } else if insertion == 0x7f || insertion + 1 == receipts.len() {
            0
        } else {
            insertion + 1
        };
        let key = alloy_rlp::encode(index);
        builder.add_leaf(reth_trie_common::Nibbles::unpack(&key), &receipts[index]);
    }
    builder.root()
}

fn native_error(error: impl ToString) -> FastServiceError {
    FastServiceError::NativeSubmission(error.to_string())
}

/// Object-safe service facade for authenticated RPC registration and runtime supervision.
#[derive(Clone)]
pub struct FastServiceHandle {
    service: Arc<FastTransferService>,
    receipts: Arc<PrivateCommittedReceiptHub>,
    drain_native: Arc<dyn FastDrainNative>,
    imported_barrier_native: Arc<dyn ImportedBarrierNativeSubmitter>,
    exposure_native: Arc<dyn ExposureNativeSubmitter>,
    drain_incoming: Arc<FastDrainIncomingRegistry>,
    checkpoint_incoming: Arc<FastNextRosterCheckpointRegistry>,
    drain_requests: mpsc::Sender<InterZoneServiceRequest>,
    handoff_authority: mpsc::Sender<NextRosterHandoffAuthoritySet>,
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
    fn new(
        service: Arc<FastTransferService>,
        receipts: Arc<PrivateCommittedReceiptHub>,
        drain_native: Arc<dyn FastDrainNative>,
        imported_barrier_native: Arc<dyn ImportedBarrierNativeSubmitter>,
        exposure_native: Arc<dyn ExposureNativeSubmitter>,
        drain_incoming: Arc<FastDrainIncomingRegistry>,
        checkpoint_incoming: Arc<FastNextRosterCheckpointRegistry>,
        drain_requests: mpsc::Sender<InterZoneServiceRequest>,
        handoff_authority: mpsc::Sender<NextRosterHandoffAuthoritySet>,
        runtime: Arc<FastServiceRuntime>,
    ) -> Self {
        Self {
            service,
            receipts,
            drain_native,
            imported_barrier_native,
            exposure_native,
            drain_incoming,
            checkpoint_incoming,
            drain_requests,
            handoff_authority,
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

    pub(crate) fn exposure_routes(&self) -> BTreeMap<u32, ZoneDomain> {
        self.service.route_domains()
    }

    pub(crate) fn exposure_retirement_candidates(
        &self,
    ) -> Result<Vec<(TransferIntent, OutcomeCertificate)>, FastServiceError> {
        self.service.exposure_retirement_candidates()
    }

    pub(crate) fn complete_exposure_retirement(
        &self,
        transfer_id: B256,
        submission: VerifiedExposureSubmission,
    ) -> Result<(), FastServiceError> {
        // Release the in-memory reservation first. A crash before the following fsynced marker
        // merely reconstructs and retries it; the reverse order could strand a live reservation
        // until restart after the marker made the candidate disappear.
        self.service.complete_exposure_retirement(transfer_id)?;
        self.exposure_native
            .complete_retirement(submission.action_id, submission.transaction_hash)
    }

    pub(crate) fn set_exposure_recovery_backlog(&self, paused: bool) {
        self.service.set_exposure_recovery_backlog(paused);
    }

    /// Run the real accepted-release recovery driver with the explicit nine-source provider map,
    /// global Tempo L1 provider, live imported-anchor watch and cancellation token contained in
    /// `resources`. Runtime must supervise this future; no empty provider facade is accepted.
    pub async fn run_exposure_retirement<P>(
        &self,
        committed: CommittedStateHandle<CanonicalFastExecution<P>>,
        resources: crate::fast_exposure::FastExposureRetirementResources,
    ) -> Result<(), crate::fast_exposure::FastExposureError>
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
        let owner_shutdown = self.runtime.stop.clone();
        tokio::select! {
            result = crate::fast_exposure::run_fast_exposure_retirement(
                self.clone(),
                committed,
                resources,
            ) => result,
            () = owner_shutdown.cancelled() => Ok(()),
        }
    }

    /// Refresh C4 quote/new-lock admission from closure state read at the exact committed
    /// imported L1 anchor. Terminal delivery, cancellation, recovery, and C5 drain remain live.
    pub async fn apply_drain_closure_observation(
        &self,
        observation: DrainClosureObservation,
    ) -> Result<(), FastServiceError> {
        self.service
            .apply_drain_closure_observation(observation)
            .await
    }

    /// The live provider/signer/pool/journal/committed-prefix adapter used by C4. C5 receives this
    /// exact handle, so old-lock recovery cannot be assembled with a placeholder native backend.
    pub fn drain_native(&self) -> Arc<dyn FastDrainNative> {
        self.drain_native.clone()
    }

    /// Submit the prepared canonical `recordImportedBarrier` call through the same operator
    /// wallet/journal path as C4, then bind its successful receipt to the exact applied Raft entry.
    pub async fn submit_imported_barrier<P>(
        &self,
        committed: &CommittedStateHandle<CanonicalFastExecution<P>>,
        call: ImportedBarrierCall,
        certificate_digest: B256,
        canonical_payload: Vec<u8>,
    ) -> Result<CommittedProtocolRecord, FastServiceError>
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
        if canonical_payload.as_slice() != call.calldata.as_ref() {
            return Err(FastServiceError::NativeSubmission(
                "imported-barrier canonical payload must be the exact ABI calldata".to_owned(),
            ));
        }
        let submission = self
            .imported_barrier_native
            .submit_imported_barrier(call.clone(), certificate_digest)
            .await?;
        let image = committed.exact_state_image().map_err(committed_error)?;
        let mut matches = image.blocks.iter().filter(|applied| {
            applied.input.transactions.iter().any(|encoded| {
                let mut bytes = encoded.as_ref();
                tempo_primitives::TempoTxEnvelope::decode_2718(&mut bytes)
                    .ok()
                    .filter(|_| bytes.is_empty())
                    .is_some_and(|transaction| {
                        *transaction.tx_hash() == submission.transaction_hash
                            && transaction.to() == Some(call.target)
                            && transaction.input().as_ref() == call.calldata.as_ref()
                    })
            })
        });
        let applied = matches.next().ok_or_else(|| {
            FastServiceError::CommittedState(
                "imported-barrier transaction is absent from the applied Raft prefix".to_owned(),
            )
        })?;
        if matches.next().is_some()
            || applied.output.block_hash != submission.receipt_block_hash
            || applied.output.block_height != submission.receipt_block_number
        {
            return Err(FastServiceError::CommittedState(
                "imported-barrier receipt does not bind one exact Raft coordinate".to_owned(),
            ));
        }
        Ok(CommittedProtocolRecord {
            log_id: applied.log_id,
            block_height: applied.output.block_height,
            block_hash: applied.output.block_hash,
            state_root: applied.output.state_root,
            transaction_hash: submission.transaction_hash,
            kind: CommittedProtocolKind::ImportedBarrier,
            canonical_payload,
            native_calldata: call.calldata.to_vec(),
            canonical_receipt: submission.canonical_receipt,
        })
    }

    pub(crate) async fn submit_ancestry_checkpoint(
        &self,
        source_portal: Address,
        header_chain: HeaderAncestryProof,
    ) -> Result<B256, FastServiceError> {
        self.exposure_native
            .record_ancestry_checkpoint(source_portal, header_chain)
            .await
    }

    pub(crate) async fn submit_exposure_retirement<P>(
        &self,
        committed: &CommittedStateHandle<CanonicalFastExecution<P>>,
        evidence: ExposureRetirementEvidence,
        source_zone: B256,
    ) -> Result<VerifiedExposureSubmission, FastServiceError>
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
        let transfer_id = evidence.transfer_id;
        let destination_token = evidence.destination_token;
        let principal = evidence.principal;
        let submission = self
            .exposure_native
            .retire_exposure(evidence, source_zone)
            .await?;
        let receipt: tempo_alloy::rpc::TempoTransactionReceipt =
            serde_json::from_slice(&submission.canonical_receipt).map_err(storage_error)?;
        let matching_effects = receipt
            .inner
            .inner
            .receipt
            .logs
            .iter()
            .filter_map(|log| IFastTransfer::ExposureRetired::decode_log(&log.inner).ok())
            .filter(|event| {
                event.data.transferId == transfer_id
                    && event.data.sourceZone == source_zone
                    && event.data.token == destination_token
                    && U256::from(event.data.principal) == principal
            })
            .count();
        if matching_effects != 1 {
            return Err(FastServiceError::CommittedState(
                "retirement receipt lost its exact native effect".into(),
            ));
        }
        let image = committed.exact_state_image().map_err(committed_error)?;
        let mut matches = image.blocks.iter().filter(|applied| {
            applied.input.transactions.iter().any(|encoded| {
                let mut bytes = encoded.as_ref();
                tempo_primitives::TempoTxEnvelope::decode_2718(&mut bytes)
                    .ok()
                    .filter(|_| bytes.is_empty())
                    .is_some_and(|transaction| {
                        *transaction.tx_hash() == submission.transaction_hash
                            && transaction.to() == Some(FAST_TRANSFER_ADDRESS)
                            && transaction.input() == submission.calldata.as_slice()
                    })
            })
        });
        let applied = matches.next().ok_or_else(|| {
            FastServiceError::CommittedState(
                "retireExposure transaction is absent from the applied Raft prefix".into(),
            )
        })?;
        if matches.next().is_some()
            || applied.output.block_hash != submission.receipt_block_hash
            || applied.output.block_height != submission.receipt_block_number
            || applied.output.receipts_root != submission.receipt_root
        {
            return Err(FastServiceError::CommittedState(
                "retireExposure effect does not bind one exact Raft coordinate".into(),
            ));
        }
        Ok(VerifiedExposureSubmission {
            action_id: submission.action_id,
            transaction_hash: submission.transaction_hash,
        })
    }

    /// Reuse the exact authenticated inter-Zone authority and bounded outbound transport owned
    /// by this service. C5 must not create a second port with a different authority/session set.
    pub fn drain_commonware_requests(&self) -> mpsc::Sender<InterZoneServiceRequest> {
        self.drain_requests.clone()
    }

    /// Install the C5 consumer behind the already-running sole incoming receiver. Installation is
    /// one-time for this epoch-scoped service handle; retrying with the same resource is idempotent
    /// and a different resource fails closed instead of replacing an active callback.
    pub fn install_fast_drain_incoming(
        &self,
        handler: Arc<dyn FastDrainIncomingHandler>,
    ) -> Result<(), FastServiceError> {
        install_fast_drain_handler(&self.drain_incoming, handler)
    }

    pub async fn install_next_roster_handoff_authority(
        &self,
        authority: NextRosterHandoffAuthoritySet,
    ) -> Result<(), FastServiceError> {
        self.handoff_authority.send(authority).await.map_err(|_| {
            FastServiceError::Transport("Commonware handoff authority port is closed".into())
        })
    }

    pub fn install_next_roster_checkpoint_handler(
        &self,
        handler: Arc<dyn FastNextRosterCheckpointHandler>,
    ) -> Result<(), FastServiceError> {
        install_checkpoint_handler(&self.checkpoint_incoming, handler)
    }

    pub fn shutdown(&self) {
        self.runtime.stop.cancel();
    }
}

fn install_fast_drain_handler(
    registry: &FastDrainIncomingRegistry,
    handler: Arc<dyn FastDrainIncomingHandler>,
) -> Result<(), FastServiceError> {
    let mut installed = registry.write().map_err(|_| FastServiceError::Poisoned)?;
    match installed.as_ref() {
        Some(existing) if Arc::ptr_eq(existing, &handler) => Ok(()),
        Some(_) => Err(FastServiceError::InvalidConfiguration),
        None => {
            *installed = Some(handler);
            Ok(())
        }
    }
}

fn install_checkpoint_handler(
    registry: &FastNextRosterCheckpointRegistry,
    handler: Arc<dyn FastNextRosterCheckpointHandler>,
) -> Result<(), FastServiceError> {
    let mut installed = registry.write().map_err(|_| FastServiceError::Poisoned)?;
    match installed.as_ref() {
        Some(existing) if Arc::ptr_eq(existing, &handler) => Ok(()),
        Some(_) => Err(FastServiceError::InvalidConfiguration),
        None => {
            *installed = Some(handler);
            Ok(())
        }
    }
}

fn fast_drain_handler_for_payload(
    payload: &[u8],
    registry: &FastDrainIncomingRegistry,
) -> Result<Option<Arc<dyn FastDrainIncomingHandler>>, FastServiceError> {
    if !is_fast_drain_payload(payload) {
        return Ok(None);
    }
    let handler = registry
        .read()
        .map_err(|_| FastServiceError::Poisoned)?
        .clone()
        .ok_or_else(|| {
            FastServiceError::Transport("C5 drain handler is not installed".to_owned())
        })?;
    Ok(Some(handler))
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
    let drain_requests = commonware.requests.clone();
    let handoff_authority = commonware.handoff_authority.clone();
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
    let drain_native: Arc<dyn FastDrainNative> = native.clone();
    let imported_barrier_native: Arc<dyn ImportedBarrierNativeSubmitter> = native.clone();
    let exposure_native: Arc<dyn ExposureNativeSubmitter> = native.clone();
    let drain_incoming = Arc::new(RwLock::new(None));
    let checkpoint_incoming = Arc::new(RwLock::new(None));
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
    let incoming_drain = drain_incoming.clone();
    let incoming_checkpoint = checkpoint_incoming.clone();
    let incoming_stop = runtime.stop.clone();
    tokio::spawn(async move {
        run_commonware_incoming(
            incoming_service,
            incoming_drain,
            incoming_checkpoint,
            incoming,
            incoming_stop.clone(),
        )
        .await;
        incoming_stop.cancel();
    });
    Ok(FastServiceHandle::new(
        service,
        receipts,
        drain_native,
        imported_barrier_native,
        exposure_native,
        drain_incoming,
        checkpoint_incoming,
        drain_requests,
        handoff_authority,
        runtime,
    ))
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

/// Construct the exact six old/next identity bindings used by both the old outbound carrier and
/// each standalone next-member endpoint. Runtime supplies only finalized rosters plus explicit
/// Tempo-config endpoints; this builder rejects partial, duplicate, or cross-Zone substitutions.
pub fn next_roster_handoff_authority(
    old_roster: &EpochRoster,
    old_endpoints: &[PeerEndpoint; 3],
    next_roster: &EpochRoster,
    next_route: &DrainCommonwareRoute,
) -> Result<NextRosterHandoffAuthoritySet, FastServiceError> {
    if next_route.zone_id != next_roster.domain.zone_id
        || next_route.portal != next_roster.domain.portal
        || old_roster.domain.zone_id != next_roster.domain.zone_id
        || old_roster.domain.portal != next_roster.domain.portal
        || old_roster.domain.l1_chain_id != next_roster.domain.l1_chain_id
        || old_roster.domain.chain_id != next_roster.domain.chain_id
        || old_roster.domain.protocol_version != next_roster.domain.protocol_version
        || next_roster.domain.authority_epoch <= old_roster.domain.authority_epoch
    {
        return Err(FastServiceError::InvalidConfiguration);
    }
    let old_members = old_endpoints
        .iter()
        .map(|endpoint| endpoint.member)
        .collect::<BTreeSet<_>>();
    let next_members = next_route
        .endpoints
        .iter()
        .map(|endpoint| endpoint.member)
        .collect::<BTreeSet<_>>();
    if old_members != old_roster.members.into_iter().collect()
        || next_members != next_roster.members.into_iter().collect()
    {
        return Err(FastServiceError::InvalidConfiguration);
    }
    let mut peers = BTreeMap::new();
    for endpoint in old_endpoints {
        if peers
            .insert(
                endpoint.ed25519.clone(),
                InterZonePeerAuthority {
                    domain: old_roster.domain,
                    certificate_member: endpoint.member,
                },
            )
            .is_some()
        {
            return Err(FastServiceError::InvalidConfiguration);
        }
    }
    for endpoint in &next_route.endpoints {
        if peers
            .insert(
                endpoint.identity.clone(),
                InterZonePeerAuthority {
                    domain: next_roster.domain,
                    certificate_member: endpoint.member,
                },
            )
            .is_some()
        {
            return Err(FastServiceError::InvalidConfiguration);
        }
    }
    if peers.len() != 6 {
        return Err(FastServiceError::InvalidConfiguration);
    }
    Ok(NextRosterHandoffAuthoritySet {
        old_roster: old_roster.clone(),
        next_roster: next_roster.clone(),
        peers,
    })
}

async fn run_commonware_incoming(
    service: Arc<FastTransferService>,
    drain: Arc<FastDrainIncomingRegistry>,
    checkpoint: Arc<FastNextRosterCheckpointRegistry>,
    mut incoming: mpsc::Receiver<AuthenticatedInterZoneRequest>,
    stop: CancellationToken,
) {
    let mut tasks = JoinSet::new();
    let mut admission_tasks = 0usize;
    loop {
        if tasks.len() >= INCOMING_DISPATCH_LIMIT {
            tokio::select! {
                () = stop.cancelled() => break,
                completed = tasks.join_next() => {
                    if completed.is_some_and(|result| result.unwrap_or(false)) {
                        admission_tasks = admission_tasks.saturating_sub(1);
                    }
                }
            }
            continue;
        }
        tokio::select! {
            biased;
            () = stop.cancelled() => break,
            completed = tasks.join_next(), if !tasks.is_empty() => {
                if completed.is_some_and(|result| result.unwrap_or(false)) {
                    admission_tasks = admission_tasks.saturating_sub(1);
                }
            }
            request = incoming.recv() => {
                let Some(request) = request else {
                    tracing::error!(target: "zone::fast", "Commonware inter-Zone incoming port closed");
                    break;
                };
                let admission = incoming_request_is_admission(&request.payload);
                if admission && admission_tasks >= INCOMING_ADMISSION_DISPATCH_LIMIT {
                    let _ = request.response.send(Err(
                        "fast admission dispatch capacity is reserved for terminal recovery"
                            .to_owned(),
                    ));
                    continue;
                }
                if admission {
                    admission_tasks += 1;
                }
                let service = service.clone();
                let drain = drain.clone();
                let checkpoint = checkpoint.clone();
                tasks.spawn(async move {
                    let result = dispatch_commonware_request(service, drain, checkpoint, &request)
                        .await
                        .map_err(|error| error.to_string());
                    let _ = request.response.send(result);
                    admission
                });
            }
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
}

fn incoming_request_is_admission(payload: &[u8]) -> bool {
    if is_checkpoint_handoff_payload(payload) || is_fast_drain_payload(payload) {
        return false;
    }
    match CanonicalFastServiceWire.decode(payload) {
        Ok(ServiceDelivery::Quote(_)) | Err(_) => true,
        Ok(_) => false,
    }
}

async fn dispatch_commonware_request(
    service: Arc<FastTransferService>,
    drain: Arc<FastDrainIncomingRegistry>,
    checkpoint: Arc<FastNextRosterCheckpointRegistry>,
    request: &AuthenticatedInterZoneRequest,
) -> Result<Vec<u8>, FastServiceError> {
    if is_checkpoint_handoff_payload(&request.payload) {
        let handler = checkpoint
            .read()
            .map_err(|_| FastServiceError::Poisoned)?
            .clone()
            .ok_or_else(|| {
                FastServiceError::Transport(
                    "next-roster checkpoint signer is not installed".to_owned(),
                )
            })?;
        return handler
            .receive_authenticated(
                &request.session,
                request.stream,
                request.sequence,
                &request.payload,
            )
            .await;
    }
    if let Some(handler) = fast_drain_handler_for_payload(&request.payload, &drain)? {
        return handler
            .receive_authenticated(
                &request.session,
                request.stream,
                request.sequence,
                &request.payload,
            )
            .await
            .map(|()| Vec::new());
    }
    service
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
                Ok(Vec::new())
            } else {
                Err(FastServiceError::UnauthenticatedPeer)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use zone_fast_transfer::drain::{BarrierInventory, CommittedSourceLock};
    use zone_primitives::fast_transfer::{AssetId, CertificateBody, SignatureBytes, ZoneDomain};

    struct DenyDrain;

    impl FastDrainIncomingHandler for DenyDrain {
        fn receive_authenticated<'a>(
            &'a self,
            _session: &'a AuthenticatedPeerSession,
            _stream: u64,
            _sequence: u64,
            _payload: &'a [u8],
        ) -> ServiceFuture<'a, Result<(), FastServiceError>> {
            Box::pin(async { Err(FastServiceError::UnauthenticatedPeer) })
        }
    }

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
    fn locked_work_uses_reserved_terminal_dispatch_and_malformed_work_does_not() {
        let intent = intent();
        let encoded = CanonicalFastServiceWire
            .encode(&ServiceDelivery::Locked {
                certificate: certificate(&intent),
                intent,
                cancellation: None,
            })
            .unwrap();
        assert!(!incoming_request_is_admission(&encoded));
        assert!(incoming_request_is_admission(b"malformed"));
        assert!(!incoming_request_is_admission(b"TZDRN14!recovery"));
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
    fn c5_magic_is_multiplexed_and_missing_handler_fails_closed() {
        let registry = RwLock::new(None);
        assert!(
            fast_drain_handler_for_payload(b"\x01\x01ordinary-c4", &registry)
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            fast_drain_handler_for_payload(b"TZDRN14!", &registry),
            Err(FastServiceError::Transport(_))
        ));
        assert!(!is_fast_drain_payload(b"TZDRN13!"));

        let first: Arc<dyn FastDrainIncomingHandler> = Arc::new(DenyDrain);
        install_fast_drain_handler(&registry, first.clone()).unwrap();
        install_fast_drain_handler(&registry, first).unwrap();
        assert!(
            fast_drain_handler_for_payload(b"TZDRN14!frame", &registry)
                .unwrap()
                .is_some()
        );
        let replacement: Arc<dyn FastDrainIncomingHandler> = Arc::new(DenyDrain);
        assert!(matches!(
            install_fast_drain_handler(&registry, replacement),
            Err(FastServiceError::InvalidConfiguration)
        ));
    }

    #[test]
    fn closed_resolve_uses_exact_four_field_abi_and_rejects_wrong_proof() {
        let intent = intent();
        let lock = CommittedSourceLock {
            lock: certificate(&intent),
            intent,
        };
        let inventory = BarrierInventory::build(
            1,
            lock.intent.destination.portal,
            lock.intent.destination.authority_epoch,
            B256::repeat_byte(21),
            lock.intent.source.portal,
            lock.intent.source.authority_epoch,
            10,
            B256::repeat_byte(22),
            1,
            2,
            3,
            B256::repeat_byte(23),
            B256::repeat_byte(24),
            vec![lock],
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
        )
        .unwrap();
        let (_, _, calldata) =
            encode_old_lock_resolution(&inventory.statement, &inventory.locks[0]).unwrap();
        let call = IFastTransfer::resolveCall::abi_decode(&calldata).unwrap();
        let expected_intent = inventory.locks[0].lock.intent.canonical_bytes();
        let expected_lock = inventory.locks[0].lock.lock.canonical_bytes();
        let expected_proof = inventory.locks[0]
            .native_barrier_proof(1, &inventory.statement)
            .unwrap();
        assert_eq!(call.canonicalIntent.as_ref(), expected_intent.as_slice());
        assert_eq!(call.lockCertificate.as_ref(), expected_lock.as_slice());
        assert!(call.cancellation.is_empty());
        assert_eq!(call.barrierProof.as_ref(), expected_proof.as_slice());

        let mut wrong_statement = inventory.statement.clone();
        wrong_statement.complete_lock_root = B256::repeat_byte(25);
        assert!(matches!(
            encode_old_lock_resolution(&wrong_statement, &inventory.locks[0]),
            Err(FastDrainError::Protocol(_))
        ));

        let mut wrong_locked_amount = inventory.locks[0].clone();
        wrong_locked_amount.lock.lock.body.outcome = TransferOutcome::Locked {
            escrow: FAST_TRANSFER_ADDRESS,
            amount: U256::from(101),
        };
        assert!(matches!(
            encode_old_lock_resolution(&inventory.statement, &wrong_locked_amount),
            Err(FastDrainError::Native(_))
        ));
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
