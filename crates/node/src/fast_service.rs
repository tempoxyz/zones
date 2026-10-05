//! Production direct-operator delivery service for T14 instant transfers.
//!
//! This module owns scheduling and recovery, not consensus or token state. Every economic change
//! is submitted as a signed transaction to the native FastTransfer ABI through
//! [`NativeTransactionSubmitter`]. The service never writes the execution database and never
//! treats a transport acknowledgment as a payment outcome.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use alloy_consensus::crypto::secp256k1::recover_signer;
use alloy_primitives::{Address, B256, Signature, keccak256};
use futures::{StreamExt as _, stream::FuturesUnordered};
use tokio::sync::{Notify, RwLock, Semaphore};
use tokio_util::sync::CancellationToken;
use zone_fast_transfer::{
    AdmissionController, AdmissionError, AuthenticatedPeerSession, DeliveryRecord, DeliveryStore,
    DeliveryTransition, DurableJournal, EconomicActionKind, EpochRoster, ProtocolLimits,
    QuorumVerifier,
    admission::{RouteKey, ValueCaps},
};
use zone_primitives::fast_transfer::{
    CancellationRequest, CanonicalEncode, MAX_CERTIFICATE_BYTES, MAX_INTENT_BYTES,
    OutcomeCertificate, QuoteCertificate, TransferIntent, TransferOutcome,
};

use crate::{
    fast_drain::DrainClosureObservation, fast_raft_state_machine::CommittedTransferRecord,
};

const RETRY_MIN: Duration = Duration::from_millis(50);
const RETRY_MAX: Duration = Duration::from_secs(2);
const MAX_DELIVERY_RESERVATION: usize = MAX_INTENT_BYTES + 2 * MAX_CERTIFICATE_BYTES + 128;
/// Native receipt waits are deliberately concurrent, but remain bounded independently per peer at
/// the protocol's 32-verification limit. Admission can occupy at most 24 slots, leaving eight for
/// terminal recovery even while new-payment processing is saturated.
const SEMANTIC_CONCURRENCY_PER_PEER: usize = 32;
const SEMANTIC_ADMISSION_CONCURRENCY_PER_PEER: usize = 24;
/// Outbound response timeouts for one unavailable Zone cannot consume another Zone's permits.
const SEND_CONCURRENCY_PER_PEER: usize = 4;
const SEND_ADMISSION_CONCURRENCY_PER_PEER: usize = 2;

pub type ServiceFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One finalized remote replica endpoint. The opaque endpoint is routing data only; the
/// authenticated roster member returned by the encrypted transport is the identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerEndpoint {
    pub member: Address,
    /// Actual Commonware authenticated identity configured for this roster member. This is
    /// routing data until the member signs it into the transport-session transcript.
    pub ed25519: zone_p2p::P2pPeerId,
    pub endpoint: String,
}

/// Exact finalized route configuration used for both quote and transport validation.
#[derive(Clone, Debug)]
pub struct FastServiceRoute {
    pub roster: EpochRoster,
    pub endpoints: [PeerEndpoint; 3],
    /// Remote-destination quote used for new local source locks.
    pub remote_quote: QuoteCertificate,
    /// Local-destination quote published privately to this remote source.
    pub local_quote: QuoteCertificate,
}

impl FastServiceRoute {
    fn validate(&self, local: &EpochRoster) -> Result<(), FastServiceError> {
        if self.roster.domain.zone_id == local.domain.zone_id
            || self.roster.domain.l1_chain_id != local.domain.l1_chain_id
            || self.roster.domain.protocol_version != local.domain.protocol_version
            || self
                .endpoints
                .iter()
                .any(|endpoint| endpoint.endpoint.is_empty())
        {
            return Err(FastServiceError::InvalidRoute("domain or endpoint"));
        }
        validate_endpoint_roster(&self.endpoints, &self.roster)?;
        let remote_quote = &self.remote_quote.quote;
        if remote_quote.source != local.domain || remote_quote.destination != self.roster.domain {
            return Err(FastServiceError::InvalidRoute("quote domains"));
        }
        QuorumVerifier::new(self.roster.clone())
            .verify_quote(&self.remote_quote)
            .map_err(|error| FastServiceError::Verification(error.to_string()))?;
        let local_quote = &self.local_quote.quote;
        if local_quote.source != self.roster.domain || local_quote.destination != local.domain {
            return Err(FastServiceError::InvalidRoute("published quote domains"));
        }
        QuorumVerifier::new(local.clone())
            .verify_quote(&self.local_quote)
            .map_err(|error| FastServiceError::Verification(error.to_string()))?;
        Ok(())
    }
}

/// Validated service resources. `stream` is a fresh, nonzero process-incarnation identifier;
/// callers must generate it from cryptographic randomness and must not reuse it after restart.
#[derive(Clone, Debug)]
pub struct FastServiceConfig {
    pub local_roster: EpochRoster,
    pub local_member: Address,
    pub local_endpoints: [PeerEndpoint; 3],
    pub stream: u64,
    pub routes: BTreeMap<u32, FastServiceRoute>,
    pub limits: ProtocolLimits,
    /// Explicit base-unit caps for every configured outgoing and incoming route.
    pub value_caps: HashMap<RouteKey, ValueCaps>,
    /// Queue space unavailable to new locks and reserved for terminal/recovery work.
    pub reserved_terminal_bytes: usize,
    pub health_max_age: Duration,
}

impl FastServiceConfig {
    pub fn validate(&self) -> Result<(), FastServiceError> {
        if !self.local_roster.members.contains(&self.local_member)
            || self.stream == 0
            || self.routes.is_empty()
            || self.routes.len() > 9
            || self.limits.unresolved_zone == 0
            || self.limits.unresolved_zone > 10_000
            || self.limits.unresolved_route == 0
            || self.limits.unresolved_route > 1_000
            || self.limits.unresolved_sender == 0
            || self.limits.unresolved_sender > 100
            || self.limits.queued_zone_bytes == 0
            || self.limits.queued_zone_bytes > 64 * 1024 * 1024
            || self.limits.queued_peer_bytes == 0
            || self.limits.queued_peer_bytes > 8 * 1024 * 1024
            || self.limits.queued_peer_bytes > self.limits.queued_zone_bytes
            || self.limits.journal_bytes == 0
            || self.limits.journal_bytes > 1024 * 1024 * 1024
            || self.reserved_terminal_bytes < MAX_DELIVERY_RESERVATION
            || self.reserved_terminal_bytes >= self.limits.queued_zone_bytes
            || self.health_max_age.is_zero()
            || self.limits.verification_concurrency_per_peer != 32
        {
            return Err(FastServiceError::InvalidConfiguration);
        }
        validate_endpoint_roster(&self.local_endpoints, &self.local_roster)?;
        for (zone, route) in &self.routes {
            if *zone != route.roster.domain.zone_id {
                return Err(FastServiceError::InvalidRoute("route key"));
            }
            route.validate(&self.local_roster)?;
            for key in [
                RouteKey {
                    source_zone: route.remote_quote.quote.source.zone_id,
                    destination_zone: route.remote_quote.quote.destination.zone_id,
                    l1_token: route.remote_quote.quote.asset.l1_token,
                },
                RouteKey {
                    source_zone: route.local_quote.quote.source.zone_id,
                    destination_zone: route.local_quote.quote.destination.zone_id,
                    l1_token: route.local_quote.quote.asset.l1_token,
                },
            ] {
                if !self.value_caps.contains_key(&key) {
                    return Err(FastServiceError::InvalidRoute("missing base-unit caps"));
                }
            }
        }
        Ok(())
    }
}

fn validate_endpoint_roster(
    endpoints: &[PeerEndpoint; 3],
    roster: &EpochRoster,
) -> Result<(), FastServiceError> {
    if endpoints
        .iter()
        .any(|endpoint| endpoint.endpoint.is_empty())
    {
        return Err(FastServiceError::InvalidRoute("empty endpoint"));
    }
    let endpoint_members = endpoints
        .iter()
        .map(|endpoint| endpoint.member)
        .collect::<std::collections::BTreeSet<_>>();
    let endpoint_identities = endpoints
        .iter()
        .map(|endpoint| endpoint.ed25519.clone())
        .collect::<std::collections::BTreeSet<_>>();
    let roster_members = roster
        .members
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    if endpoint_members != roster_members || endpoint_identities.len() != 3 {
        return Err(FastServiceError::InvalidRoute("endpoint identity roster"));
    }
    Ok(())
}

/// Semantic delivery encoded by the canonical core wire codec. The intent is deliberately
/// included: its hash in a certificate is insufficient input for the native resolve call.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::large_enum_variant)] // Canonical bounded certificate bodies remain inline on wire.
pub enum ServiceDelivery {
    Quote(QuoteCertificate),
    Locked {
        intent: TransferIntent,
        certificate: OutcomeCertificate,
        cancellation: Option<CancellationRequest>,
    },
    Terminal {
        intent: TransferIntent,
        certificate: OutcomeCertificate,
    },
    Disposition {
        intent: TransferIntent,
        certificate: OutcomeCertificate,
    },
}

impl ServiceDelivery {
    fn transfer_id(&self) -> Option<B256> {
        match self {
            Self::Quote(_) => None,
            Self::Locked { intent, .. }
            | Self::Terminal { intent, .. }
            | Self::Disposition { intent, .. } => Some(intent.transfer_id()),
        }
    }

    fn class(&self) -> DeliveryClass {
        match self {
            Self::Quote(_) => DeliveryClass::Admission,
            Self::Locked { .. } | Self::Terminal { .. } | Self::Disposition { .. } => {
                DeliveryClass::Terminal
            }
        }
    }
}

/// Canonical, bounded codec supplied by core primitives. Implementations must not use JSON or
/// bincode and must enforce the protocol's independent intent/certificate/cancellation limits.
pub trait FastServiceWire: Send + Sync + 'static {
    fn encode(&self, delivery: &ServiceDelivery) -> Result<Vec<u8>, FastServiceError>;
    fn decode(&self, encoded: &[u8]) -> Result<ServiceDelivery, FastServiceError>;
}

/// Successful durable transport acknowledgment from an authenticated encrypted session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceAcknowledgment {
    pub remote_member: Address,
    pub stream: u64,
    pub sequence: u64,
}

/// Runtime-owned encrypted inter-Zone transport. Implementations try all three endpoints and must
/// authenticate the returned member against `route.roster`; plaintext and broadcast transports
/// do not satisfy this interface.
pub trait FastServiceTransport: Send + Sync + 'static {
    fn send_authenticated_encrypted<'a>(
        &'a self,
        route: &'a FastServiceRoute,
        stream: u64,
        sequence: u64,
        encoded: &'a [u8],
    ) -> ServiceFuture<'a, Result<ServiceAcknowledgment, FastServiceError>>;
}

/// Fsynced committed-state and certificate-assembly boundary.
pub trait CommittedTransferSource: Send + Sync + 'static {
    /// Return every non-compacted committed transfer needed to reconstruct delivery work.
    fn committed_transfers(&self) -> Result<Vec<CommittedTransferRecord>, FastServiceError>;
    fn committed_transfer(
        &self,
        transfer_id: B256,
    ) -> Result<Option<CommittedTransferRecord>, FastServiceError>;
    fn committed_height(&self) -> Result<u64, FastServiceError>;
    /// Actual canonical receipt status fenced by the fsynced applied Raft prefix.
    fn committed_transaction_result(
        &self,
        transaction_hash: B256,
    ) -> Result<Option<bool>, FastServiceError>;
    /// Recover/recollect two signatures over the identical committed body, persisting the local
    /// signing record and assembled certificate before returning it.
    fn ensure_certificate<'a>(
        &'a self,
        record: &'a CommittedTransferRecord,
    ) -> ServiceFuture<'a, Result<OutcomeCertificate, FastServiceError>>;
}

/// Signed native-call adapter. Each method submits through a validated provider/signer and waits
/// until the resulting transaction is in the committed Raft prefix before returning.
pub trait NativeTransactionSubmitter: Send + Sync + 'static {
    /// Pure pre-admission validation of the signed transaction and exact lock calldata.
    fn validate_signed_lock(
        &self,
        caller: Address,
        intent: &TransferIntent,
        quote: &QuoteCertificate,
        signed_transaction: &[u8],
    ) -> Result<(), FastServiceError>;
    /// Validate sender, chain, nonce and exact `IFastTransfer.lock(intent, quote)` calldata, then
    /// submit the already-signed Zone transaction through the ordinary transaction pool.
    fn submit_signed_lock<'a>(
        &'a self,
        caller: Address,
        intent: &'a TransferIntent,
        quote: &'a QuoteCertificate,
        signed_transaction: &'a [u8],
    ) -> ServiceFuture<'a, Result<B256, FastServiceError>>;
    fn resolve<'a>(
        &'a self,
        intent: &'a TransferIntent,
        lock: &'a OutcomeCertificate,
        cancellation: Option<&'a CancellationRequest>,
        trigger: ResolveTrigger,
    ) -> ServiceFuture<'a, Result<B256, FastServiceError>>;
    fn record_outcome<'a>(
        &'a self,
        intent: &'a TransferIntent,
        outcome: &'a OutcomeCertificate,
    ) -> ServiceFuture<'a, Result<B256, FastServiceError>>;
    fn dispose_escrow<'a>(
        &'a self,
        transfer_id: B256,
    ) -> ServiceFuture<'a, Result<B256, FastServiceError>>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolveTrigger {
    Delivery,
    SenderCancellation,
    /// Destination committed height, never wall time, reached the quote expiry.
    ExpiryRecovery,
}

/// Private committed receipt publisher. Implementations scope subscribers to authenticated
/// sender, recipient, or involved operator principals; raw certificates are never broadcast.
pub trait CommittedReceiptSink: Send + Sync + 'static {
    fn publish(&self, record: &CommittedTransferRecord) -> Result<(), FastServiceError>;
}

/// Durable sender-cancellation store. Persistence must complete before an authenticated cancel
/// RPC reports acceptance; restart must retain the request until the source disposition commits.
pub trait CancellationStore: Send + Sync + 'static {
    fn persist(&self, request: &CancellationRequest) -> Result<(), FastServiceError>;
    fn load_pending(&self) -> Result<Vec<CancellationRequest>, FastServiceError>;
    fn complete(&self, transfer_id: B256) -> Result<(), FastServiceError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistedIncomingDelivery {
    pub peer_zone: u32,
    pub stream: u64,
    pub sequence: u64,
    /// First sequence strictly after the journal's durable contiguous cursor for this stream.
    pub expected_sequence: u64,
    pub payload: Vec<u8>,
}

/// Ordered view over durable incoming journal records strictly after each contiguous cursor.
pub trait IncomingRecoverySource: Send + Sync + 'static {
    fn load_unprocessed(&self) -> Result<Vec<PersistedIncomingDelivery>, FastServiceError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrivateTransferState {
    Locked,
    Paid,
    Rejected,
    Released,
    Refunded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PrivateTransferStatus {
    pub transfer_id: B256,
    pub intent_hash: B256,
    pub state: PrivateTransferState,
    pub block_height: u64,
    pub block_hash: B256,
    pub state_root: B256,
    pub transaction_hash: B256,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubmitResult {
    pub transfer_id: B256,
    pub transaction_hash: B256,
    /// `None` means pool admission succeeded but no committed lock is known yet. It must never be
    /// interpreted as failure or used to create another transfer nonce.
    pub committed: Option<PrivateTransferStatus>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CertifiedPaymentReceipt {
    pub status: PrivateTransferStatus,
    pub certificate: OutcomeCertificate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeliveryClass {
    Admission,
    Terminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DeliveryKind {
    Quote,
    Locked,
    Terminal,
    Disposition,
}

impl ServiceDelivery {
    fn kind(&self) -> DeliveryKind {
        match self {
            Self::Quote(_) => DeliveryKind::Quote,
            Self::Locked { .. } => DeliveryKind::Locked,
            Self::Terminal { .. } => DeliveryKind::Terminal,
            Self::Disposition { .. } => DeliveryKind::Disposition,
        }
    }
}

#[derive(Clone, Debug)]
struct PendingDelivery {
    peer_zone: u32,
    record: DeliveryRecord,
    class: DeliveryClass,
    kind: DeliveryKind,
    next_attempt: Instant,
    in_flight: bool,
}

#[derive(Clone, Debug, Default)]
struct RouteHealth {
    last_success: Option<Instant>,
    consecutive_failures: u32,
}

#[derive(Clone, Copy, Debug)]
struct IncomingRetry {
    attempts: u32,
    next_attempt: Instant,
}

#[derive(Default)]
struct ServiceState {
    pending: BTreeMap<(u32, u64, u64), PendingDelivery>,
    next_sequence: HashMap<u32, u64>,
    incoming_next: HashMap<(u32, u64), u64>,
    incoming_in_flight: BTreeSet<(u32, u64, u64)>,
    incoming_completed: BTreeSet<(u32, u64, u64)>,
    incoming_retries: HashMap<(u32, u64, u64), IncomingRetry>,
    queue_zone_bytes: usize,
    queue_peer_bytes: HashMap<u32, usize>,
    health: HashMap<u32, RouteHealth>,
    cancellations: HashMap<B256, CancellationRequest>,
}

/// Admission authority derived only from closure state at committed imported L1 anchors.
/// Endpoint configuration is deliberately absent: it is routing metadata, never authority.
struct ImportedAnchorAdmission {
    local_portal: Address,
    local_closure: Option<B256>,
    route_portals: BTreeMap<u32, Address>,
    route_closures: BTreeMap<u32, B256>,
}

impl ImportedAnchorAdmission {
    fn new(config: &FastServiceConfig) -> Result<Self, FastServiceError> {
        let route_portals = config
            .routes
            .iter()
            .map(|(zone, route)| (*zone, route.roster.domain.portal))
            .collect::<BTreeMap<_, _>>();
        let distinct = route_portals
            .values()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        if distinct.len() != route_portals.len()
            || distinct.contains(&config.local_roster.domain.portal)
        {
            return Err(FastServiceError::InvalidConfiguration);
        }
        Ok(Self {
            local_portal: config.local_roster.domain.portal,
            local_closure: None,
            route_portals,
            route_closures: BTreeMap::new(),
        })
    }

    fn apply(&mut self, observation: &DrainClosureObservation) -> Result<(), FastServiceError> {
        if observation.local.is_some_and(|hash| hash.is_zero())
            || observation.destinations.values().any(|hash| hash.is_zero())
        {
            return Err(FastServiceError::InvalidClosureObservation);
        }
        if let (Some(current), Some(observed)) = (self.local_closure, observation.local)
            && current != observed
        {
            return Err(FastServiceError::InvalidClosureObservation);
        }

        let mut observed_routes = BTreeMap::new();
        for (portal, closure_hash) in &observation.destinations {
            if *portal == self.local_portal {
                return Err(FastServiceError::InvalidClosureObservation);
            }
            let zone = self
                .route_portals
                .iter()
                .find_map(|(zone, configured)| (*configured == *portal).then_some(*zone))
                .ok_or(FastServiceError::InvalidClosureObservation)?;
            if self
                .route_closures
                .get(&zone)
                .is_some_and(|current| current != closure_hash)
            {
                return Err(FastServiceError::InvalidClosureObservation);
            }
            observed_routes.insert(zone, *closure_hash);
        }

        if let Some(closure_hash) = observation.local {
            self.local_closure.get_or_insert(closure_hash);
        }
        self.route_closures.extend(observed_routes);
        Ok(())
    }

    fn allows_new_lock(&self, destination_zone: u32) -> bool {
        self.local_closure.is_none()
            && self.route_portals.contains_key(&destination_zone)
            && !self.route_closures.contains_key(&destination_zone)
    }

    fn allows_quote_publication(&self) -> bool {
        self.local_closure.is_none()
    }

    fn allows_delivery(&self, kind: DeliveryKind) -> bool {
        kind != DeliveryKind::Quote || self.allows_quote_publication()
    }
}

/// Long-lived C4 delivery worker.
pub struct FastTransferService {
    config: FastServiceConfig,
    journal: Arc<DurableJournal>,
    wire: Arc<dyn FastServiceWire>,
    transport: Arc<dyn FastServiceTransport>,
    committed: Arc<dyn CommittedTransferSource>,
    native: Arc<dyn NativeTransactionSubmitter>,
    receipts: Arc<dyn CommittedReceiptSink>,
    cancellations: Arc<dyn CancellationStore>,
    incoming: Arc<dyn IncomingRecoverySource>,
    verification: BTreeMap<u32, Arc<Semaphore>>,
    imported_anchor_admission: RwLock<ImportedAnchorAdmission>,
    admission: Mutex<AdmissionController>,
    state: Mutex<ServiceState>,
    incoming_available: Notify,
    outgoing_available: Notify,
    exposure_recovery_backlog: AtomicBool,
}

impl FastTransferService {
    pub fn new(
        config: FastServiceConfig,
        journal: Arc<DurableJournal>,
        wire: Arc<dyn FastServiceWire>,
        transport: Arc<dyn FastServiceTransport>,
        committed: Arc<dyn CommittedTransferSource>,
        native: Arc<dyn NativeTransactionSubmitter>,
        receipts: Arc<dyn CommittedReceiptSink>,
        cancellations: Arc<dyn CancellationStore>,
        incoming: Arc<dyn IncomingRecoverySource>,
    ) -> Result<Self, FastServiceError> {
        config.validate()?;
        let verification = config
            .routes
            .keys()
            .map(|zone| {
                (
                    *zone,
                    Arc::new(Semaphore::new(
                        config.limits.verification_concurrency_per_peer,
                    )),
                )
            })
            .collect();
        let mut admission = AdmissionController::new(config.limits.clone());
        for (route, caps) in &config.value_caps {
            admission.set_value_caps(*route, *caps);
        }
        let imported_anchor_admission = ImportedAnchorAdmission::new(&config)?;
        let service = Self {
            config,
            journal,
            wire,
            transport,
            committed,
            native,
            receipts,
            cancellations,
            incoming,
            verification,
            imported_anchor_admission: RwLock::new(imported_anchor_admission),
            admission: Mutex::new(admission),
            state: Mutex::new(ServiceState::default()),
            incoming_available: Notify::new(),
            outgoing_available: Notify::new(),
            exposure_recovery_backlog: AtomicBool::new(false),
        };
        service.recover_delivery_queue()?;
        service.recover_cancellations()?;
        service.recover_admission()?;
        Ok(service)
    }

    pub(crate) fn route_domains(
        &self,
    ) -> BTreeMap<u32, zone_primitives::fast_transfer::ZoneDomain> {
        self.config
            .routes
            .iter()
            .map(|(zone, route)| (*zone, route.roster.domain))
            .collect()
    }

    /// Gate a new source lock on exact route/quote identity, bounded queue headroom, and recent
    /// authenticated peer health. Native execution remains the authoritative liquidity decision.
    pub fn quote_for_new_lock(
        &self,
        intent: &TransferIntent,
    ) -> Result<QuoteCertificate, FastServiceError> {
        let gate = self
            .imported_anchor_admission
            .try_read()
            .map_err(|_| FastServiceError::AdmissionClosed)?;
        self.quote_for_new_lock_with_gate(intent, &gate)
    }

    fn quote_for_new_lock_with_gate(
        &self,
        intent: &TransferIntent,
        gate: &ImportedAnchorAdmission,
    ) -> Result<QuoteCertificate, FastServiceError> {
        if self.exposure_recovery_backlog.load(Ordering::Acquire) {
            return Err(FastServiceError::AdmissionClosed);
        }
        if intent.source != self.config.local_roster.domain {
            return Err(FastServiceError::InvalidIntent("wrong source domain"));
        }
        let route = self.route(intent.destination.zone_id)?;
        if !gate.allows_new_lock(intent.destination.zone_id) {
            return Err(FastServiceError::AdmissionClosed);
        }
        let quote = &route.remote_quote.quote;
        if quote.source != intent.source
            || quote.destination != intent.destination
            || quote.asset != intent.asset
            || quote.quote_id != intent.quote_id
            || quote.fee != intent.fee
            || quote.maximum_principal < intent.principal
            || quote.expiry_height != intent.destination_expiry_height
            || quote.destination_pool != intent.destination_pool
            || quote.reimbursement_account != intent.reimbursement_account
        {
            return Err(FastServiceError::InvalidIntent("standing quote mismatch"));
        }
        let state = self.state.lock().map_err(|_| FastServiceError::Poisoned)?;
        let healthy = state
            .health
            .get(&intent.destination.zone_id)
            .and_then(|health| health.last_success)
            .is_some_and(|last| last.elapsed() <= self.config.health_max_age);
        if !healthy {
            return Err(FastServiceError::RouteUnhealthy);
        }
        let admission_zone_limit = self
            .config
            .limits
            .queued_zone_bytes
            .saturating_sub(self.config.reserved_terminal_bytes);
        let projected_zone = state
            .queue_zone_bytes
            .checked_add(MAX_DELIVERY_RESERVATION)
            .ok_or(FastServiceError::QueueFull)?;
        let projected_peer = state
            .queue_peer_bytes
            .get(&intent.destination.zone_id)
            .copied()
            .unwrap_or(0)
            .checked_add(MAX_DELIVERY_RESERVATION)
            .ok_or(FastServiceError::QueueFull)?;
        if projected_zone > admission_zone_limit
            || projected_peer > self.config.limits.queued_peer_bytes
        {
            return Err(FastServiceError::QueueFull);
        }
        Ok(route.remote_quote.clone())
    }

    /// Authenticated user submission. The adapter must reject any signed transaction whose
    /// recovered sender or native lock calldata differs from these already validated values.
    pub async fn submit(
        &self,
        caller: Address,
        intent: TransferIntent,
        quote: QuoteCertificate,
        signed_transaction: &[u8],
    ) -> Result<SubmitResult, FastServiceError> {
        if caller != intent.sender || intent.refund_account != intent.sender {
            return Err(FastServiceError::UnauthorizedPrincipal);
        }
        // Linearize the service admission decision under the imported-anchor read fence. Do not
        // retain it while waiting for Raft commitment: an L1 import that closes admission must be
        // able to finish while a transaction admitted at the preceding anchor is still pending.
        let gate = self.imported_anchor_admission.read().await;
        let configured = self.quote_for_new_lock_with_gate(&intent, &gate)?;
        if quote != configured {
            return Err(FastServiceError::InvalidIntent("unconfigured quote"));
        }
        self.native
            .validate_signed_lock(caller, &intent, &quote, signed_transaction)?;
        self.reserve_obligation(&intent, signed_transaction.len())?;
        drop(gate);
        let transaction_hash = self
            .native
            .submit_signed_lock(caller, &intent, &quote, signed_transaction)
            .await?;
        let record = self.committed.committed_transfer(intent.transfer_id())?;
        let committed = record.as_ref().map(private_status).transpose()?;
        if let Some(record) = record
            && let Err(error) = self.reconcile_record(record).await
            && !matches!(&error, FastServiceError::QueueFull)
        {
            return Err(error);
        }
        Ok(SubmitResult {
            transfer_id: intent.transfer_id(),
            transaction_hash,
            committed,
        })
    }

    /// Atomically apply closure state observed at the latest committed imported L1 anchor.
    /// Closures are monotonic. Unknown Portals and changed closure hashes fail closed; configured
    /// transport endpoints are never consulted as authority.
    pub async fn apply_drain_closure_observation(
        &self,
        observation: DrainClosureObservation,
    ) -> Result<(), FastServiceError> {
        let mut gate = self.imported_anchor_admission.write().await;
        gate.apply(&observation)?;
        let mut state = self.state.lock().map_err(|_| FastServiceError::Poisoned)?;
        if !gate.allows_quote_publication() {
            state.health.clear();
        } else {
            state.health.retain(|zone, _| gate.allows_new_lock(*zone));
        }
        Ok(())
    }

    /// Authenticated private status. Unrelated principals receive `None`, indistinguishable from
    /// an unknown transfer ID.
    pub fn status(
        &self,
        caller: Address,
        transfer_id: B256,
    ) -> Result<Option<PrivateTransferStatus>, FastServiceError> {
        let Some(record) = self.committed.committed_transfer(transfer_id)? else {
            return Ok(None);
        };
        if !authorized_principal(caller, &record.intent) {
            return Ok(None);
        }
        Ok(Some(private_status(&record)?))
    }

    /// Return a recipient-spendable receipt only for a committed `Paid` transition with its
    /// durable two-member certificate. Unknown/locked/uncertified state remains `None`.
    pub fn payment_receipt(
        &self,
        caller: Address,
        transfer_id: B256,
    ) -> Result<Option<CertifiedPaymentReceipt>, FastServiceError> {
        let Some(record) = self.committed.committed_transfer(transfer_id)? else {
            return Ok(None);
        };
        if !authorized_principal(caller, &record.intent)
            || !matches!(&record.body.outcome, TransferOutcome::Paid { .. })
        {
            return Ok(None);
        }
        let Some(certificate) = record.certificate.clone() else {
            return Ok(None);
        };
        QuorumVerifier::new(self.config.local_roster.clone())
            .verify_outcome(&certificate, &record.intent)
            .map_err(|error| FastServiceError::Verification(error.to_string()))?;
        Ok(Some(CertifiedPaymentReceipt {
            status: private_status(&record)?,
            certificate,
        }))
    }

    /// Persist a valid sender cancellation for attachment to lock replay. It is never delivered
    /// without the matching committed lock and its source quorum certificate.
    pub async fn request_cancellation(
        &self,
        caller: Address,
        cancellation: CancellationRequest,
    ) -> Result<(), FastServiceError> {
        if caller != cancellation.sender {
            return Err(FastServiceError::UnauthorizedPrincipal);
        }
        let Some(record) = self
            .committed
            .committed_transfer(cancellation.transfer_id)?
        else {
            return Err(FastServiceError::LockEvidenceMissing);
        };
        if record.intent.sender != cancellation.sender
            || record.intent.intent_hash() != cancellation.intent_hash
            || cancellation.source != record.intent.source
            || !matches!(&record.body.outcome, TransferOutcome::Locked { .. })
        {
            return Err(FastServiceError::InvalidCancellation);
        }
        verify_cancellation_signature(&cancellation)?;
        let certificate = self.committed.ensure_certificate(&record).await?;
        QuorumVerifier::new(self.config.local_roster.clone())
            .verify_outcome(&certificate, &record.intent)
            .map_err(|error| FastServiceError::Verification(error.to_string()))?;
        self.cancellations.persist(&cancellation)?;
        self.state
            .lock()
            .map_err(|_| FastServiceError::Poisoned)?
            .cancellations
            .insert(cancellation.transfer_id, cancellation);
        match self.reconcile_record(record).await {
            Ok(()) | Err(FastServiceError::QueueFull) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Persist an authenticated frame before returning its transport acknowledgment, then process
    /// it asynchronously/idempotently. Runtime must send the returned acknowledgment on the same
    /// encrypted authenticated session.
    pub async fn receive_authenticated(
        &self,
        session: &AuthenticatedPeerSession,
        stream: u64,
        sequence: u64,
        encoded: &[u8],
    ) -> Result<ServiceAcknowledgment, FastServiceError> {
        let peer_zone = session.remote_roster().domain.zone_id;
        let route = self.route(peer_zone)?;
        if session.local_roster() != &self.config.local_roster
            || session.remote_roster() != &route.roster
            || !route.roster.members.contains(&session.remote_member())
            || !session.authenticates_delivery(stream, sequence)
        {
            return Err(FastServiceError::UnauthenticatedPeer);
        }
        // Decode before persistence only to reject oversized/malformed traffic. Semantic work is
        // deliberately not awaited here: the fsynced frame is the transport-ACK boundary, while
        // the bounded recovery workers retry its idempotent native work until it commits.
        self.wire.decode(encoded)?;
        let acknowledgment = persist_transport_ack(
            &self.journal,
            peer_zone,
            session.remote_member(),
            stream,
            sequence,
            encoded,
        )?;
        {
            let mut state = self.state.lock().map_err(|_| FastServiceError::Poisoned)?;
            let health = state.health.entry(peer_zone).or_default();
            health.last_success = Some(Instant::now());
            health.consecutive_failures = 0;
        }
        self.incoming_available.notify_one();
        Ok(acknowledgment)
    }

    /// Reconstruct semantic work from committed records and continuously retry unfinished frames.
    pub async fn run(self: Arc<Self>, stop: CancellationToken) -> Result<(), FastServiceError> {
        let incoming = self.clone().run_incoming_worker(stop.clone());
        let committed = self.clone().run_committed_worker(stop.clone());
        let outgoing = self.run_outgoing_worker(stop);
        tokio::try_join!(incoming, committed, outgoing)?;
        Ok(())
    }

    pub async fn reconcile_once(&self) -> Result<(), FastServiceError> {
        self.recover_incoming().await?;
        self.reconcile_committed().await?;
        self.send_ready().await;
        Ok(())
    }

    async fn run_incoming_worker(
        self: Arc<Self>,
        stop: CancellationToken,
    ) -> Result<(), FastServiceError> {
        let mut interval = tokio::time::interval(RETRY_MIN);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = stop.cancelled() => return Ok(()),
                _ = interval.tick() => self.recover_incoming().await?,
                () = self.incoming_available.notified() => self.recover_incoming().await?,
            }
        }
    }

    async fn run_committed_worker(
        self: Arc<Self>,
        stop: CancellationToken,
    ) -> Result<(), FastServiceError> {
        let mut interval = tokio::time::interval(RETRY_MIN);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = stop.cancelled() => return Ok(()),
                _ = interval.tick() => self.reconcile_committed().await?,
            }
        }
    }

    async fn run_outgoing_worker(
        self: Arc<Self>,
        stop: CancellationToken,
    ) -> Result<(), FastServiceError> {
        let mut interval = tokio::time::interval(RETRY_MIN);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = stop.cancelled() => return Ok(()),
                _ = interval.tick() => self.send_ready().await,
                () = self.outgoing_available.notified() => self.send_ready().await,
            }
        }
    }

    fn recover_delivery_queue(&self) -> Result<(), FastServiceError> {
        let mut state = self.state.lock().map_err(|_| FastServiceError::Poisoned)?;
        for peer_zone in self.config.routes.keys().copied() {
            for record in self
                .journal
                .load_unfinished(peer_zone)
                .map_err(|error| FastServiceError::Storage(error.to_string()))?
            {
                let delivery = self.wire.decode(&record.payload)?;
                let class = delivery.class();
                let kind = delivery.kind();
                reserve_queue(
                    &self.config,
                    &mut state,
                    peer_zone,
                    record.payload.len(),
                    class,
                )?;
                state
                    .next_sequence
                    .entry(peer_zone)
                    .and_modify(|next| *next = (*next).max(record.sequence.saturating_add(1)))
                    .or_insert(record.sequence.saturating_add(1));
                state.pending.insert(
                    (peer_zone, record.stream, record.sequence),
                    PendingDelivery {
                        peer_zone,
                        record,
                        class,
                        kind,
                        next_attempt: Instant::now(),
                        in_flight: false,
                    },
                );
            }
            state.next_sequence.entry(peer_zone).or_insert(1);
        }
        Ok(())
    }

    fn recover_cancellations(&self) -> Result<(), FastServiceError> {
        let cancellations = self.cancellations.load_pending()?;
        let mut state = self.state.lock().map_err(|_| FastServiceError::Poisoned)?;
        for cancellation in cancellations {
            verify_cancellation_signature(&cancellation)?;
            state
                .cancellations
                .insert(cancellation.transfer_id, cancellation);
        }
        Ok(())
    }

    async fn recover_incoming(&self) -> Result<(), FastServiceError> {
        let mut records = self.incoming.load_unprocessed()?;
        records.sort_by_key(|record| {
            let class = self
                .wire
                .decode(&record.payload)
                .map(|delivery| delivery.class())
                .unwrap_or(DeliveryClass::Terminal);
            (
                matches!(class, DeliveryClass::Admission),
                record.peer_zone,
                record.stream,
                record.sequence,
            )
        });
        let per_peer = self
            .config
            .routes
            .keys()
            .map(|peer| {
                (
                    *peer,
                    (
                        Arc::new(Semaphore::new(SEMANTIC_CONCURRENCY_PER_PEER)),
                        Arc::new(Semaphore::new(SEMANTIC_ADMISSION_CONCURRENCY_PER_PEER)),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut work = FuturesUnordered::new();
        for record in records {
            let key = (record.peer_zone, record.stream, record.sequence);
            let delivery = self.wire.decode(&record.payload)?;
            let class = delivery.class();
            let retry_id = delivery
                .transfer_id()
                .unwrap_or_else(|| keccak256(&record.payload));
            let should_process = {
                let mut state = self.state.lock().map_err(|_| FastServiceError::Poisoned)?;
                state
                    .incoming_next
                    .entry((record.peer_zone, record.stream))
                    .or_insert(record.expected_sequence);
                if state.incoming_completed.contains(&key)
                    || state
                        .incoming_retries
                        .get(&key)
                        .is_some_and(|retry| retry.next_attempt > Instant::now())
                    || !state.incoming_in_flight.insert(key)
                {
                    false
                } else {
                    true
                }
            };
            if !should_process {
                continue;
            }
            let Some((all, admission)) = per_peer.get(&record.peer_zone).cloned() else {
                self.clear_incoming_in_flight(key)?;
                return Err(FastServiceError::UnknownRoute(record.peer_zone));
            };
            work.push(async move {
                let _all = all
                    .acquire_owned()
                    .await
                    .map_err(|_| FastServiceError::InvalidConfiguration)?;
                let _admission = if class == DeliveryClass::Admission {
                    Some(
                        admission
                            .acquire_owned()
                            .await
                            .map_err(|_| FastServiceError::InvalidConfiguration)?,
                    )
                } else {
                    None
                };
                let result = self.process_durable(record.peer_zone, delivery).await;
                Ok::<_, FastServiceError>((key, retry_id, result))
            });
        }

        while let Some(result) = work.next().await {
            let (key, retry_id, result) = result?;
            match result {
                Ok(()) => self.complete_incoming(key)?,
                Err(error) => {
                    self.record_incoming_failure(key, retry_id)?;
                    tracing::warn!(
                        target: "zone::fast",
                        peer_zone = key.0,
                        stream = key.1,
                        sequence = key.2,
                        %error,
                        "durable incoming semantic work remains queued"
                    );
                }
            }
        }
        Ok(())
    }

    fn clear_incoming_in_flight(&self, key: (u32, u64, u64)) -> Result<(), FastServiceError> {
        self.state
            .lock()
            .map_err(|_| FastServiceError::Poisoned)?
            .incoming_in_flight
            .remove(&key);
        Ok(())
    }

    fn record_incoming_failure(
        &self,
        key: (u32, u64, u64),
        retry_id: B256,
    ) -> Result<(), FastServiceError> {
        let mut state = self.state.lock().map_err(|_| FastServiceError::Poisoned)?;
        state.incoming_in_flight.remove(&key);
        let retry = state.incoming_retries.entry(key).or_insert(IncomingRetry {
            attempts: 0,
            next_attempt: Instant::now(),
        });
        retry.attempts = retry.attempts.saturating_add(1);
        retry.next_attempt = Instant::now() + retry_delay(retry.attempts, retry_id);
        Ok(())
    }

    /// Persist only the actual contiguous done prefix. Out-of-order completions remain in memory;
    /// after a crash they are safely replayed from their original fsynced frame.
    fn complete_incoming(&self, key: (u32, u64, u64)) -> Result<(), FastServiceError> {
        let (peer_zone, stream, sequence) = key;
        let mut state = self.state.lock().map_err(|_| FastServiceError::Poisoned)?;
        state.incoming_in_flight.remove(&key);
        state.incoming_retries.remove(&key);
        state.incoming_completed.insert(key);
        let mut expected = state
            .incoming_next
            .get(&(peer_zone, stream))
            .copied()
            .unwrap_or(1);
        expected = advance_completed_prefix(
            &self.journal,
            &mut state.incoming_completed,
            peer_zone,
            stream,
            expected,
        )?;
        state.incoming_next.insert((peer_zone, stream), expected);
        debug_assert!(sequence < expected || !state.incoming_completed.is_empty());
        Ok(())
    }

    fn recover_admission(&self) -> Result<(), FastServiceError> {
        for record in self.committed.committed_transfers()? {
            let unresolved = match &record.body.outcome {
                TransferOutcome::Locked { .. } | TransferOutcome::Rejected { .. } => true,
                TransferOutcome::Paid { .. } => !self
                    .journal
                    .economic_action_completed(
                        record.body.transfer_id,
                        EconomicActionKind::RetireExposure,
                    )
                    .map_err(|error| FastServiceError::Storage(error.to_string()))?,
                TransferOutcome::Released { .. } | TransferOutcome::Refunded { .. } => false,
            };
            if unresolved {
                self.reserve_obligation(&record.intent, record.intent.canonical_bytes().len())?;
            }
        }
        Ok(())
    }

    fn reserve_obligation(
        &self,
        intent: &TransferIntent,
        retained_bytes: usize,
    ) -> Result<(), FastServiceError> {
        self.admission
            .lock()
            .map_err(|_| FastServiceError::Poisoned)?
            .reserve(intent, retained_bytes, retained_bytes)
            .map(|_| ())
            .map_err(|error| FastServiceError::Admission(error.to_string()))
    }

    fn release_obligation(&self, transfer_id: B256) -> Result<(), FastServiceError> {
        match self
            .admission
            .lock()
            .map_err(|_| FastServiceError::Poisoned)?
            .release(transfer_id)
        {
            Ok(()) | Err(AdmissionError::UnknownReservation) => Ok(()),
            Err(error) => Err(FastServiceError::Admission(error.to_string())),
        }
    }

    /// Recover paid destination claims that have an authenticated, durable source release but
    /// still require canonical accepted-prefix evidence. Transport processing and the source
    /// disposition certificate alone deliberately do not release destination capacity.
    pub(crate) fn exposure_retirement_candidates(
        &self,
    ) -> Result<Vec<(TransferIntent, OutcomeCertificate)>, FastServiceError> {
        let mut committed = HashMap::new();
        for record in self.committed.committed_transfers()? {
            if matches!(record.body.outcome, TransferOutcome::Paid { .. })
                && !self
                    .journal
                    .economic_action_completed(
                        record.body.transfer_id,
                        EconomicActionKind::RetireExposure,
                    )
                    .map_err(|error| FastServiceError::Storage(error.to_string()))?
            {
                committed.insert(record.body.transfer_id, record.intent);
            }
        }
        let mut candidates = BTreeMap::new();
        for record in self
            .journal
            .all_incoming()
            .map_err(|error| FastServiceError::Storage(error.to_string()))?
        {
            let ServiceDelivery::Disposition {
                intent,
                certificate,
            } = self.wire.decode(&record.payload)?
            else {
                continue;
            };
            if !matches!(certificate.body.outcome, TransferOutcome::Released { .. }) {
                continue;
            }
            let Some(paid_intent) = committed.get(&intent.transfer_id()) else {
                continue;
            };
            if paid_intent != &intent || intent.source.zone_id != record.peer_zone {
                return Err(FastServiceError::CommittedState(
                    "source release does not bind the committed paid destination record".into(),
                ));
            }
            let route = self.route(record.peer_zone)?;
            QuorumVerifier::new(route.roster.clone())
                .verify_outcome(&certificate, &intent)
                .map_err(|error| FastServiceError::Verification(error.to_string()))?;
            match candidates.get(&intent.transfer_id()) {
                Some((old_intent, old_certificate))
                    if old_intent == &intent && old_certificate == &certificate => {}
                Some(_) => {
                    return Err(FastServiceError::CommittedState(
                        "conflicting durable source release certificates".into(),
                    ));
                }
                None => {
                    candidates.insert(intent.transfer_id(), (intent, certificate));
                }
            }
        }
        Ok(candidates.into_values().collect())
    }

    /// Release the C4 resource reservation only after the native retirement transaction and its
    /// exact `ExposureRetired` effect are both present in the applied Raft prefix.
    pub(crate) fn complete_exposure_retirement(
        &self,
        transfer_id: B256,
    ) -> Result<(), FastServiceError> {
        self.release_obligation(transfer_id)
    }

    pub(crate) fn set_exposure_recovery_backlog(&self, paused: bool) {
        self.exposure_recovery_backlog
            .store(paused, Ordering::Release);
    }

    async fn reconcile_committed(&self) -> Result<(), FastServiceError> {
        {
            let gate = self.imported_anchor_admission.read().await;
            if gate.allows_quote_publication() {
                for (peer_zone, route) in &self.config.routes {
                    if let Err(error) = self.queue(
                        *peer_zone,
                        ServiceDelivery::Quote(route.local_quote.clone()),
                    ) && !matches!(&error, FastServiceError::QueueFull)
                    {
                        return Err(error);
                    }
                }
            }
        }
        let local = self.config.local_roster.domain;
        let mut records = self.committed.committed_transfers()?;
        records.sort_by_key(|record| {
            reconcile_target(record, local)
                .map(|(_, class)| matches!(class, DeliveryClass::Admission))
                .unwrap_or(true)
        });
        let per_peer = self
            .config
            .routes
            .keys()
            .map(|peer| {
                (
                    *peer,
                    (
                        Arc::new(Semaphore::new(SEMANTIC_CONCURRENCY_PER_PEER)),
                        Arc::new(Semaphore::new(SEMANTIC_ADMISSION_CONCURRENCY_PER_PEER)),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut work = FuturesUnordered::new();
        for record in records {
            let Some((peer_zone, class)) = reconcile_target(&record, local) else {
                continue;
            };
            let Some((all, admission)) = per_peer.get(&peer_zone).cloned() else {
                return Err(FastServiceError::UnknownRoute(peer_zone));
            };
            work.push(async move {
                let _all = all
                    .acquire_owned()
                    .await
                    .map_err(|_| FastServiceError::InvalidConfiguration)?;
                let _admission = if class == DeliveryClass::Admission {
                    Some(
                        admission
                            .acquire_owned()
                            .await
                            .map_err(|_| FastServiceError::InvalidConfiguration)?,
                    )
                } else {
                    None
                };
                let transfer_id = record.body.transfer_id;
                Ok::<_, FastServiceError>((
                    peer_zone,
                    transfer_id,
                    self.reconcile_record(record).await,
                ))
            });
        }
        while let Some(result) = work.next().await {
            let (peer_zone, transfer_id, result) = result?;
            if let Err(error) = result
                && !matches!(error, FastServiceError::QueueFull)
            {
                tracing::warn!(
                    target: "zone::fast",
                    peer_zone,
                    %transfer_id,
                    %error,
                    "committed transfer certification/delivery remains queued"
                );
            }
        }
        Ok(())
    }

    async fn reconcile_record(
        &self,
        record: CommittedTransferRecord,
    ) -> Result<(), FastServiceError> {
        let local = self.config.local_roster.domain;
        let (peer_zone, delivery) = match &record.body.outcome {
            TransferOutcome::Locked { .. } if record.body.zone == local => {
                let certificate = self.committed.ensure_certificate(&record).await?;
                let cancellation = self
                    .state
                    .lock()
                    .map_err(|_| FastServiceError::Poisoned)?
                    .cancellations
                    .get(&record.body.transfer_id)
                    .cloned();
                (
                    record.intent.destination.zone_id,
                    ServiceDelivery::Locked {
                        intent: record.intent.clone(),
                        certificate,
                        cancellation,
                    },
                )
            }
            TransferOutcome::Paid { .. } | TransferOutcome::Rejected { .. }
                if record.body.zone == local =>
            {
                let certificate = self.committed.ensure_certificate(&record).await?;
                self.receipts.publish(&CommittedTransferRecord {
                    certificate: Some(certificate.clone()),
                    ..record.clone()
                })?;
                (
                    record.intent.source.zone_id,
                    ServiceDelivery::Terminal {
                        intent: record.intent.clone(),
                        certificate,
                    },
                )
            }
            TransferOutcome::Released { .. } | TransferOutcome::Refunded { .. }
                if record.body.zone == local =>
            {
                let certificate = self.committed.ensure_certificate(&record).await?;
                self.receipts.publish(&CommittedTransferRecord {
                    certificate: Some(certificate.clone()),
                    ..record.clone()
                })?;
                self.complete_source_lock(record.body.transfer_id)?;
                self.release_obligation(record.body.transfer_id)?;
                self.cancellations.complete(record.body.transfer_id)?;
                self.state
                    .lock()
                    .map_err(|_| FastServiceError::Poisoned)?
                    .cancellations
                    .remove(&record.body.transfer_id);
                (
                    record.intent.destination.zone_id,
                    ServiceDelivery::Disposition {
                        intent: record.intent.clone(),
                        certificate,
                    },
                )
            }
            _ => return Ok(()),
        };
        self.queue(peer_zone, delivery)
    }

    fn queue(&self, peer_zone: u32, delivery: ServiceDelivery) -> Result<(), FastServiceError> {
        self.route(peer_zone)?;
        let payload = self.wire.encode(&delivery)?;
        let transfer_id = delivery
            .transfer_id()
            .unwrap_or_else(|| keccak256(&payload));
        let class = delivery.class();
        let kind = delivery.kind();
        let mut state = self.state.lock().map_err(|_| FastServiceError::Poisoned)?;
        if state.pending.values().any(|pending| {
            pending.peer_zone == peer_zone
                && pending.record.transfer_id == transfer_id
                && pending.record.payload == payload
        }) {
            return Ok(());
        }
        reserve_queue(&self.config, &mut state, peer_zone, payload.len(), class)?;
        let sequence = *state.next_sequence.entry(peer_zone).or_insert(1);
        state
            .next_sequence
            .insert(peer_zone, sequence.saturating_add(1));
        let record = DeliveryRecord::queued(self.config.stream, sequence, transfer_id, payload);
        if let Err(error) = self.journal.persist_outgoing(peer_zone, &record) {
            release_queue(&mut state, peer_zone, record.payload.len());
            return Err(FastServiceError::Storage(error.to_string()));
        }
        state.pending.insert(
            (peer_zone, record.stream, record.sequence),
            PendingDelivery {
                peer_zone,
                record,
                class,
                kind,
                next_attempt: Instant::now(),
                in_flight: false,
            },
        );
        self.outgoing_available.notify_one();
        Ok(())
    }

    async fn send_ready(&self) {
        let mut sends: FuturesUnordered<
            ServiceFuture<
                '_,
                (
                    (u32, u64, u64),
                    Result<ServiceAcknowledgment, FastServiceError>,
                ),
            >,
        > = FuturesUnordered::new();
        loop {
            let Ok(ready) = self.take_ready_sends() else {
                return;
            };
            for (key, pending) in ready {
                sends.push(self.send_pending(key, pending));
            }
            if sends.is_empty() {
                return;
            }
            tokio::select! {
                Some((key, result)) = sends.next() => self.record_send_result(key, result),
                () = self.outgoing_available.notified() => {}
            }
        }
    }

    fn take_ready_sends(
        &self,
    ) -> Result<Vec<((u32, u64, u64), PendingDelivery)>, FastServiceError> {
        let mut state = self.state.lock().map_err(|_| FastServiceError::Poisoned)?;
        let mut active = HashMap::<u32, (usize, usize)>::new();
        for pending in state.pending.values().filter(|pending| pending.in_flight) {
            let counts = active.entry(pending.peer_zone).or_default();
            counts.0 += 1;
            if pending.class == DeliveryClass::Admission {
                counts.1 += 1;
            }
        }
        let now = Instant::now();
        let mut keys = state
            .pending
            .iter()
            .filter(|(_, pending)| !pending.in_flight && pending.next_attempt <= now)
            .map(|(key, pending)| (*key, pending.class))
            .collect::<Vec<_>>();
        keys.sort_by_key(|(_, class)| matches!(class, DeliveryClass::Admission));
        let mut selected = Vec::new();
        for (key, class) in keys {
            let peer = key.0;
            let counts = active.entry(peer).or_default();
            if counts.0 >= SEND_CONCURRENCY_PER_PEER
                || (class == DeliveryClass::Admission
                    && counts.1 >= SEND_ADMISSION_CONCURRENCY_PER_PEER)
            {
                continue;
            }
            let Some(pending) = state.pending.get_mut(&key) else {
                continue;
            };
            pending.in_flight = true;
            counts.0 += 1;
            if class == DeliveryClass::Admission {
                counts.1 += 1;
            }
            selected.push((key, pending.clone()));
        }
        Ok(selected)
    }

    fn send_pending<'a>(
        &'a self,
        key: (u32, u64, u64),
        pending: PendingDelivery,
    ) -> ServiceFuture<
        'a,
        (
            (u32, u64, u64),
            Result<ServiceAcknowledgment, FastServiceError>,
        ),
    > {
        Box::pin(async move {
            let gate = if pending.kind == DeliveryKind::Quote {
                let gate = self.imported_anchor_admission.read().await;
                if !gate.allows_delivery(pending.kind) {
                    return (key, Err(FastServiceError::AdmissionClosed));
                }
                Some(gate)
            } else {
                None
            };
            let result = match self.route(pending.peer_zone) {
                Ok(route) => {
                    self.transport
                        .send_authenticated_encrypted(
                            route,
                            pending.record.stream,
                            pending.record.sequence,
                            &pending.record.payload,
                        )
                        .await
                }
                Err(error) => Err(error),
            };
            drop(gate);
            (key, result)
        })
    }

    fn record_send_result(
        &self,
        key: (u32, u64, u64),
        result: Result<ServiceAcknowledgment, FastServiceError>,
    ) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let Some(mut pending) = state.pending.remove(&key) else {
            return;
        };
        pending.in_flight = false;
        let health = state.health.entry(pending.peer_zone).or_default();
        match result {
            Ok(ack)
                if ack.stream == pending.record.stream
                    && ack.sequence == pending.record.sequence
                    && self
                        .config
                        .routes
                        .get(&pending.peer_zone)
                        .is_some_and(|route| route.roster.members.contains(&ack.remote_member)) =>
            {
                health.last_success = Some(Instant::now());
                health.consecutive_failures = 0;
                if pending.record.transition == DeliveryTransition::Queued {
                    let _ = self.journal.transition(
                        pending.peer_zone,
                        pending.record.stream,
                        pending.record.sequence,
                        DeliveryTransition::TransportAcknowledged,
                    );
                    pending.record.transition = DeliveryTransition::TransportAcknowledged;
                }
            }
            _ => {
                health.consecutive_failures = health.consecutive_failures.saturating_add(1);
            }
        }
        pending.record.attempts = pending.record.attempts.saturating_add(1);
        let delay = retry_delay(pending.record.attempts, pending.record.transfer_id);
        pending.record.retry_after = delay;
        pending.next_attempt = Instant::now() + delay;
        // Even an authenticated transport acknowledgment is not terminal protocol evidence.
        state.pending.insert(key, pending);
    }

    async fn process_durable(
        &self,
        peer_zone: u32,
        delivery: ServiceDelivery,
    ) -> Result<(), FastServiceError> {
        let permit = self
            .verification
            .get(&peer_zone)
            .ok_or(FastServiceError::UnknownRoute(peer_zone))?
            .clone()
            .try_acquire_owned()
            .map_err(|_| FastServiceError::VerificationBusy)?;
        let route = self.route(peer_zone)?;
        let verifier = QuorumVerifier::new(route.roster.clone());
        match delivery {
            ServiceDelivery::Quote(quote) => {
                verifier
                    .verify_quote(&quote)
                    .map_err(|error| FastServiceError::Verification(error.to_string()))?;
                if quote != route.remote_quote {
                    return Err(FastServiceError::InvalidRoute("unconfigured quote"));
                }
            }
            ServiceDelivery::Locked {
                intent,
                certificate,
                cancellation,
            } => {
                if intent.source.zone_id != peer_zone
                    || intent.destination != self.config.local_roster.domain
                    || !matches!(&certificate.body.outcome, TransferOutcome::Locked { .. })
                {
                    return Err(FastServiceError::InvalidIntent("lock route/outcome"));
                }
                verifier
                    .verify_outcome(&certificate, &intent)
                    .map_err(|error| FastServiceError::Verification(error.to_string()))?;
                self.reserve_obligation(&intent, intent.canonical_bytes().len())?;
                // Expiry is intentionally evaluated against this committed destination height by
                // native execution. Wall-clock age never authorizes a rejection or source refund.
                let destination_height = self.committed.committed_height()?;
                if let Some(cancellation) = cancellation.as_ref() {
                    if cancellation.transfer_id != intent.transfer_id()
                        || cancellation.intent_hash != intent.intent_hash()
                        || cancellation.sender != intent.sender
                        || cancellation.source != intent.source
                    {
                        return Err(FastServiceError::InvalidCancellation);
                    }
                    verify_cancellation_signature(cancellation)?;
                }
                let trigger = if cancellation.is_some() {
                    ResolveTrigger::SenderCancellation
                } else if destination_height >= intent.destination_expiry_height {
                    ResolveTrigger::ExpiryRecovery
                } else {
                    ResolveTrigger::Delivery
                };
                self.native
                    .resolve(&intent, &certificate, cancellation.as_ref(), trigger)
                    .await?;
            }
            ServiceDelivery::Terminal {
                intent,
                certificate,
            } => {
                if intent.destination.zone_id != peer_zone
                    || intent.source != self.config.local_roster.domain
                    || !matches!(
                        &certificate.body.outcome,
                        TransferOutcome::Paid { .. } | TransferOutcome::Rejected { .. }
                    )
                {
                    return Err(FastServiceError::InvalidIntent("terminal route/outcome"));
                }
                verifier
                    .verify_outcome(&certificate, &intent)
                    .map_err(|error| FastServiceError::Verification(error.to_string()))?;
                self.native.record_outcome(&intent, &certificate).await?;
                self.mark_terminal_received(intent.transfer_id())?;
                // A separate committed call is mandatory: policy failure leaves the remembered
                // decision pending and this at-least-once worker retries the same disposition.
                self.native.dispose_escrow(intent.transfer_id()).await?;
            }
            ServiceDelivery::Disposition {
                intent,
                certificate,
            } => {
                if intent.source.zone_id != peer_zone
                    || intent.destination != self.config.local_roster.domain
                    || !matches!(
                        &certificate.body.outcome,
                        TransferOutcome::Released { .. } | TransferOutcome::Refunded { .. }
                    )
                {
                    return Err(FastServiceError::InvalidIntent("disposition route/outcome"));
                }
                verifier
                    .verify_outcome(&certificate, &intent)
                    .map_err(|error| FastServiceError::Verification(error.to_string()))?;
                self.complete_terminal_delivery(intent.transfer_id())?;
                // A paid claim remains reserved until accepted source-receipt proof drives the
                // native retireExposure operation. Rejected/refunded transfers created no native
                // destination exposure and can release their reservation now.
                if disposition_releases_reservation(&certificate.body.outcome) {
                    self.release_obligation(intent.transfer_id())?;
                }
            }
        }
        drop(permit);
        Ok(())
    }

    fn complete_terminal_delivery(&self, transfer_id: B256) -> Result<(), FastServiceError> {
        self.complete_matching(
            transfer_id,
            DeliveryKind::Terminal,
            DeliveryTransition::SourceDisposed,
        )
    }

    fn complete_source_lock(&self, transfer_id: B256) -> Result<(), FastServiceError> {
        self.complete_matching(
            transfer_id,
            DeliveryKind::Locked,
            DeliveryTransition::SourceDisposed,
        )
    }

    fn mark_terminal_received(&self, transfer_id: B256) -> Result<(), FastServiceError> {
        self.complete_matching(
            transfer_id,
            DeliveryKind::Locked,
            DeliveryTransition::TerminalReceived,
        )
    }

    fn complete_matching(
        &self,
        transfer_id: B256,
        kind: DeliveryKind,
        transition: DeliveryTransition,
    ) -> Result<(), FastServiceError> {
        let mut state = self.state.lock().map_err(|_| FastServiceError::Poisoned)?;
        let keys = state
            .pending
            .iter()
            .filter(|(_, pending)| {
                pending.record.transfer_id == transfer_id && pending.kind == kind
            })
            .map(|(key, _)| *key)
            .collect::<Vec<_>>();
        for key in keys {
            let Some(mut pending) = state.pending.remove(&key) else {
                continue;
            };
            self.journal
                .transition(
                    pending.peer_zone,
                    pending.record.stream,
                    pending.record.sequence,
                    transition,
                )
                .map_err(|error| FastServiceError::Storage(error.to_string()))?;
            if transition == DeliveryTransition::SourceDisposed {
                release_queue(&mut state, pending.peer_zone, pending.record.payload.len());
            } else {
                pending.record.transition = transition;
                state.pending.insert(key, pending);
            }
        }
        Ok(())
    }

    fn route(&self, zone: u32) -> Result<&FastServiceRoute, FastServiceError> {
        self.config
            .routes
            .get(&zone)
            .ok_or(FastServiceError::UnknownRoute(zone))
    }
}

fn advance_completed_prefix(
    journal: &DurableJournal,
    completed: &mut BTreeSet<(u32, u64, u64)>,
    peer_zone: u32,
    stream: u64,
    mut expected: u64,
) -> Result<u64, FastServiceError> {
    while completed.contains(&(peer_zone, stream, expected)) {
        journal
            .advance_contiguous_cursor(peer_zone, stream, expected)
            .map_err(|error| FastServiceError::Storage(error.to_string()))?;
        completed.remove(&(peer_zone, stream, expected));
        expected = expected.saturating_add(1);
    }
    Ok(expected)
}

fn persist_transport_ack(
    journal: &DurableJournal,
    peer_zone: u32,
    remote_member: Address,
    stream: u64,
    sequence: u64,
    encoded: &[u8],
) -> Result<ServiceAcknowledgment, FastServiceError> {
    journal
        .persist_incoming(peer_zone, stream, sequence, encoded)
        .map_err(|error| FastServiceError::Storage(error.to_string()))?;
    Ok(ServiceAcknowledgment {
        remote_member,
        stream,
        sequence,
    })
}

fn disposition_releases_reservation(outcome: &TransferOutcome) -> bool {
    matches!(outcome, TransferOutcome::Refunded { .. })
}

fn reconcile_target(
    record: &CommittedTransferRecord,
    local: zone_primitives::fast_transfer::ZoneDomain,
) -> Option<(u32, DeliveryClass)> {
    let peer = match &record.body.outcome {
        TransferOutcome::Locked { .. } if record.body.zone == local => {
            record.intent.destination.zone_id
        }
        TransferOutcome::Paid { .. }
        | TransferOutcome::Rejected { .. }
        | TransferOutcome::Released { .. }
        | TransferOutcome::Refunded { .. }
            if record.body.zone == local =>
        {
            if matches!(
                &record.body.outcome,
                TransferOutcome::Paid { .. } | TransferOutcome::Rejected { .. }
            ) {
                record.intent.source.zone_id
            } else {
                record.intent.destination.zone_id
            }
        }
        _ => return None,
    };
    Some((peer, DeliveryClass::Terminal))
}

fn reserve_queue(
    config: &FastServiceConfig,
    state: &mut ServiceState,
    peer_zone: u32,
    bytes: usize,
    class: DeliveryClass,
) -> Result<(), FastServiceError> {
    let zone_limit = match class {
        DeliveryClass::Admission => config
            .limits
            .queued_zone_bytes
            .saturating_sub(config.reserved_terminal_bytes),
        DeliveryClass::Terminal => config.limits.queued_zone_bytes,
    };
    let zone_next = state
        .queue_zone_bytes
        .checked_add(bytes)
        .ok_or(FastServiceError::QueueFull)?;
    let peer_next = state
        .queue_peer_bytes
        .get(&peer_zone)
        .copied()
        .unwrap_or(0)
        .checked_add(bytes)
        .ok_or(FastServiceError::QueueFull)?;
    if zone_next > zone_limit || peer_next > config.limits.queued_peer_bytes {
        return Err(FastServiceError::QueueFull);
    }
    state.queue_zone_bytes = zone_next;
    state.queue_peer_bytes.insert(peer_zone, peer_next);
    Ok(())
}

fn release_queue(state: &mut ServiceState, peer_zone: u32, bytes: usize) {
    state.queue_zone_bytes = state.queue_zone_bytes.saturating_sub(bytes);
    if let Some(peer) = state.queue_peer_bytes.get_mut(&peer_zone) {
        *peer = peer.saturating_sub(bytes);
    }
}

fn verify_cancellation_signature(
    cancellation: &CancellationRequest,
) -> Result<(), FastServiceError> {
    let signature = Signature::try_from(cancellation.signature.0.as_slice())
        .map_err(|_| FastServiceError::InvalidCancellation)?;
    let signer = recover_signer(&signature, cancellation.request_hash())
        .map_err(|_| FastServiceError::InvalidCancellation)?;
    if signer != cancellation.sender {
        return Err(FastServiceError::InvalidCancellation);
    }
    Ok(())
}

fn authorized_principal(caller: Address, intent: &TransferIntent) -> bool {
    [
        intent.sender,
        intent.recipient,
        intent.destination_pool,
        intent.reimbursement_account,
    ]
    .contains(&caller)
}

fn private_status(
    record: &CommittedTransferRecord,
) -> Result<PrivateTransferStatus, FastServiceError> {
    if record.body.transfer_id != record.intent.transfer_id()
        || record.body.intent_hash != record.intent.intent_hash()
    {
        return Err(FastServiceError::CommittedState(
            "committed transfer body does not bind its intent".to_owned(),
        ));
    }
    let state = match &record.body.outcome {
        TransferOutcome::Locked { .. } => PrivateTransferState::Locked,
        TransferOutcome::Paid { .. } => PrivateTransferState::Paid,
        TransferOutcome::Rejected { .. } => PrivateTransferState::Rejected,
        TransferOutcome::Released { .. } => PrivateTransferState::Released,
        TransferOutcome::Refunded { .. } => PrivateTransferState::Refunded,
    };
    Ok(PrivateTransferStatus {
        transfer_id: record.body.transfer_id,
        intent_hash: record.body.intent_hash,
        state,
        block_height: record.body.block_height,
        block_hash: record.body.block_hash,
        state_root: record.body.state_root,
        transaction_hash: record.body.transaction_hash,
    })
}

fn retry_delay(attempts: u32, transfer_id: B256) -> Duration {
    let shift = attempts.saturating_sub(1).min(6);
    let base = 50u64.checked_shl(shift).unwrap_or(2_000).min(2_000);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut seed = Vec::with_capacity(32 + 4 + 16);
    seed.extend_from_slice(transfer_id.as_slice());
    seed.extend_from_slice(&attempts.to_be_bytes());
    seed.extend_from_slice(&time.to_be_bytes());
    let hash = keccak256(seed);
    let jitter = u64::from_be_bytes(hash[..8].try_into().expect("eight bytes")) % (base / 4 + 1);
    Duration::from_millis((base + jitter).min(RETRY_MAX.as_millis() as u64).max(50))
}

#[derive(Debug, thiserror::Error)]
pub enum FastServiceError {
    #[error("invalid T14 service configuration")]
    InvalidConfiguration,
    #[error("invalid configured route: {0}")]
    InvalidRoute(&'static str),
    #[error("unknown configured peer Zone {0}")]
    UnknownRoute(u32),
    #[error("invalid transfer intent: {0}")]
    InvalidIntent(&'static str),
    #[error("route has no recent authenticated healthy session")]
    RouteUnhealthy,
    #[error("bounded delivery queue is full")]
    QueueFull,
    #[error("fast-transfer admission failed: {0}")]
    Admission(String),
    #[error("fast-transfer admission is closed at the imported L1 anchor")]
    AdmissionClosed,
    #[error("invalid imported-anchor drain closure observation")]
    InvalidClosureObservation,
    #[error("incoming delivery cursor gap: expected {expected}, received {actual}")]
    CursorGap { expected: u64, actual: u64 },
    #[error("peer certificate verification capacity is exhausted")]
    VerificationBusy,
    #[error("peer session is not authenticated to the finalized route roster")]
    UnauthenticatedPeer,
    #[error("sender cancellation is invalid")]
    InvalidCancellation,
    #[error("sender cancellation has no matching committed lock certificate")]
    LockEvidenceMissing,
    #[error("authenticated principal is not authorized for this transfer")]
    UnauthorizedPrincipal,
    #[error("certificate verification failed: {0}")]
    Verification(String),
    #[error("durable service storage failed: {0}")]
    Storage(String),
    #[error("native signed transaction submission failed: {0}")]
    NativeSubmission(String),
    #[error("authenticated encrypted transport failed: {0}")]
    Transport(String),
    #[error("committed state read failed: {0}")]
    CommittedState(String),
    #[error("service state lock poisoned")]
    Poisoned,
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use tempfile::tempdir;
    use tokio::sync::oneshot;
    use zone_primitives::fast_transfer::{AssetId, ZoneDomain};

    use super::*;

    fn gate() -> ImportedAnchorAdmission {
        ImportedAnchorAdmission {
            local_portal: Address::repeat_byte(1),
            local_closure: None,
            route_portals: BTreeMap::from([
                (2, Address::repeat_byte(2)),
                (3, Address::repeat_byte(3)),
            ]),
            route_closures: BTreeMap::new(),
        }
    }

    #[test]
    fn remote_closure_disables_only_that_new_lock_route() {
        let mut gate = gate();
        gate.apply(&DrainClosureObservation {
            local: None,
            destinations: BTreeMap::from([(Address::repeat_byte(2), B256::repeat_byte(22))]),
        })
        .unwrap();

        assert!(!gate.allows_new_lock(2));
        assert!(gate.allows_new_lock(3));
        assert!(gate.allows_quote_publication());
        assert!(gate.allows_delivery(DeliveryKind::Locked));
        assert!(gate.allows_delivery(DeliveryKind::Terminal));
    }

    #[test]
    fn local_closure_stops_quotes_and_all_new_locks_but_not_recovery() {
        let mut gate = gate();
        gate.apply(&DrainClosureObservation {
            local: Some(B256::repeat_byte(11)),
            destinations: BTreeMap::new(),
        })
        .unwrap();

        assert!(!gate.allows_new_lock(2));
        assert!(!gate.allows_new_lock(3));
        assert!(!gate.allows_quote_publication());
        assert!(!gate.allows_delivery(DeliveryKind::Quote));
        assert!(gate.allows_delivery(DeliveryKind::Locked));
        assert!(gate.allows_delivery(DeliveryKind::Terminal));
    }

    #[test]
    fn closure_authority_is_portal_bound_monotonic_and_atomic() {
        let mut gate = gate();
        let closed = DrainClosureObservation {
            local: None,
            destinations: BTreeMap::from([(Address::repeat_byte(2), B256::repeat_byte(22))]),
        };
        gate.apply(&closed).unwrap();

        // An observation that omits an already finalized closure cannot reopen admission.
        gate.apply(&DrainClosureObservation::default()).unwrap();
        assert!(!gate.allows_new_lock(2));

        let before = gate.route_closures.clone();
        assert!(matches!(
            gate.apply(&DrainClosureObservation {
                local: None,
                destinations: BTreeMap::from([
                    (Address::repeat_byte(2), B256::repeat_byte(23)),
                    (Address::repeat_byte(9), B256::repeat_byte(99)),
                ]),
            }),
            Err(FastServiceError::InvalidClosureObservation)
        ));
        assert_eq!(gate.route_closures, before);
        assert!(gate.allows_new_lock(3));
    }

    #[test]
    fn source_release_certificate_alone_does_not_free_paid_reservation() {
        let source = ZoneDomain {
            l1_chain_id: 1,
            zone_id: 1,
            chain_id: 101,
            portal: Address::repeat_byte(1),
            authority_epoch: 1,
            roster_hash: B256::repeat_byte(1),
            protocol_version: 1,
        };
        let destination = ZoneDomain {
            zone_id: 2,
            chain_id: 102,
            portal: Address::repeat_byte(2),
            roster_hash: B256::repeat_byte(2),
            ..source
        };
        let intent = TransferIntent {
            source,
            destination,
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
            principal: alloy_primitives::U256::from(10),
            fee: alloy_primitives::U256::from(1),
            quote_id: B256::repeat_byte(10),
            destination_expiry_height: 100,
            transfer_nonce: 1,
        };
        let route = RouteKey::from_intent(&intent);
        let mut admission = AdmissionController::new(ProtocolLimits::default());
        admission.set_value_caps(
            route,
            ValueCaps {
                route: intent.principal,
                sender: intent.principal,
            },
        );
        admission.reserve(&intent, 1, 1).unwrap();
        let released = TransferOutcome::Released {
            beneficiary: intent.reimbursement_account,
            amount: intent.principal + intent.fee,
        };
        if disposition_releases_reservation(&released) {
            admission.release(intent.transfer_id()).unwrap();
        }
        assert_eq!(admission.unresolved(), 1);

        let mut second = intent.clone();
        second.transfer_nonce += 1;
        assert!(matches!(
            admission.reserve(&second, 1, 1),
            Err(AdmissionError::Limit("route base-unit exposure"))
        ));
        assert!(disposition_releases_reservation(
            &TransferOutcome::Refunded {
                beneficiary: Address::repeat_byte(2),
                amount: alloy_primitives::U256::from(11),
            }
        ));
    }

    #[tokio::test]
    async fn stalled_peer_does_not_block_a_healthy_peer_completion() {
        let peers = BTreeMap::from([
            (2, Arc::new(Semaphore::new(SEND_CONCURRENCY_PER_PEER))),
            (3, Arc::new(Semaphore::new(SEND_CONCURRENCY_PER_PEER))),
        ]);
        let (release, stalled) = oneshot::channel::<()>();
        let mut sends: FuturesUnordered<ServiceFuture<'_, u32>> = FuturesUnordered::new();
        let failed = peers.get(&2).unwrap().clone();
        sends.push(Box::pin(async move {
            let _permit = failed.acquire_owned().await.unwrap();
            let _ = stalled.await;
            2
        }));
        let healthy = peers.get(&3).unwrap().clone();
        sends.push(Box::pin(async move {
            let _permit = healthy.acquire_owned().await.unwrap();
            3
        }));

        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), sends.next())
                .await
                .unwrap(),
            Some(3)
        );
        let _ = release.send(());
        assert_eq!(sends.next().await, Some(2));
    }

    #[tokio::test]
    async fn admission_saturation_leaves_real_terminal_permits() {
        let all = Arc::new(Semaphore::new(SEMANTIC_CONCURRENCY_PER_PEER));
        let admission = Arc::new(Semaphore::new(SEMANTIC_ADMISSION_CONCURRENCY_PER_PEER));
        let mut admission_permits = Vec::new();
        for _ in 0..SEMANTIC_ADMISSION_CONCURRENCY_PER_PEER {
            admission_permits.push((
                all.clone().try_acquire_owned().unwrap(),
                admission.clone().try_acquire_owned().unwrap(),
            ));
        }
        assert!(admission.clone().try_acquire_owned().is_err());
        let terminal_one = all.clone().try_acquire_owned().unwrap();
        let terminal_two = all.clone().try_acquire_owned().unwrap();
        assert!(all.clone().try_acquire_owned().is_err());
        drop(terminal_one);
        assert!(all.try_acquire_owned().is_ok());
        drop((terminal_two, admission_permits));
    }

    #[tokio::test]
    async fn semantic_inflight_is_concurrent_and_bounded() {
        let permits = Arc::new(Semaphore::new(SEMANTIC_CONCURRENCY_PER_PEER));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let mut work = FuturesUnordered::new();
        for _ in 0..64 {
            let permits = permits.clone();
            let active = active.clone();
            let maximum = maximum.clone();
            work.push(async move {
                let _permit = permits.acquire_owned().await.unwrap();
                let now = active.fetch_add(1, AtomicOrdering::SeqCst) + 1;
                maximum.fetch_max(now, AtomicOrdering::SeqCst);
                tokio::time::sleep(Duration::from_millis(2)).await;
                active.fetch_sub(1, AtomicOrdering::SeqCst);
            });
        }
        while work.next().await.is_some() {}
        assert_eq!(
            maximum.load(AtomicOrdering::SeqCst),
            SEMANTIC_CONCURRENCY_PER_PEER
        );
        assert_eq!(active.load(AtomicOrdering::SeqCst), 0);
    }

    #[test]
    fn reordered_completion_keeps_gap_across_restart_then_advances_prefix() {
        let directory = tempdir().unwrap();
        let journal = DurableJournal::open(directory.path()).unwrap();
        for sequence in [2, 1, 3] {
            journal
                .persist_incoming(2, 9, sequence, &[sequence as u8])
                .unwrap();
        }
        let mut completed = BTreeSet::from([(2, 9, 2), (2, 9, 3)]);
        assert_eq!(
            advance_completed_prefix(&journal, &mut completed, 2, 9, 1).unwrap(),
            1
        );
        drop(journal);

        let journal = DurableJournal::open(directory.path()).unwrap();
        assert_eq!(journal.unprocessed_incoming().unwrap().len(), 3);
        completed.insert((2, 9, 1));
        assert_eq!(
            advance_completed_prefix(&journal, &mut completed, 2, 9, 1).unwrap(),
            4
        );
        drop(journal);
        let journal = DurableJournal::open(directory.path()).unwrap();
        assert!(journal.unprocessed_incoming().unwrap().is_empty());
    }

    #[test]
    fn transport_ack_is_exactly_durable_receipt_and_never_paid() {
        let directory = tempdir().unwrap();
        let journal = DurableJournal::open(directory.path()).unwrap();
        let transfer_id = B256::repeat_byte(41);
        let ack = persist_transport_ack(
            &journal,
            2,
            Address::repeat_byte(7),
            11,
            5,
            transfer_id.as_slice(),
        )
        .unwrap();
        assert_eq!(ack.stream, 11);
        assert_eq!(ack.sequence, 5);
        assert_eq!(journal.unprocessed_incoming().unwrap().len(), 1);
        assert_eq!(journal.tombstone(transfer_id).unwrap(), None);
        assert!(
            !journal
                .economic_action_completed(transfer_id, EconomicActionKind::Resolve)
                .unwrap()
        );
        drop(journal);
        assert_eq!(
            DurableJournal::open(directory.path())
                .unwrap()
                .unprocessed_incoming()
                .unwrap()
                .len(),
            1
        );
    }
}
