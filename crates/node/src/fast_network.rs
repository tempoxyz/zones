//! OpenRaft transport over the Zone's existing authenticated Commonware membership.

use crate::{
    fast_drain::{CommittedDrainPoint, DrainObjectKey, DrainSigningPurpose},
    fast_drain_adapters::DrainSignatureNetworkRequest,
    fast_quorum::{
        AuthenticatedRaftPeer, FastRaftConfig, RaftTransport, TransportError, TransportFuture,
    },
};
use alloy_primitives::{Address, B256};
use bincode::Options as _;
use openraft::{
    BasicNode,
    network::RPCOption,
    raft::{
        AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest,
        InstallSnapshotResponse, VoteRequest, VoteResponse,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};
use zone_p2p::{MAX_RAFT_MESSAGE_SIZE, P2pCommand, P2pPeerId, RaftRequestFrame, RaftResponseFrame};
use zone_primitives::fast_transfer::{CertificateBody, SignatureBytes};

pub(crate) type HandlerFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;

/// Authenticated selector for one immutable locally reconstructed committed outcome. The body
/// hash is an identity check only; peers never accept a caller-supplied certificate body.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct OutcomeSigningRequest {
    /// Stable transfer identity used to locate local history.
    pub transfer_id: B256,
    /// Original committed Raft term.
    pub log_term: u64,
    /// Original committed Raft index.
    pub log_index: u64,
    /// Hash of the locally reconstructed complete certificate body.
    pub body_hash: B256,
}

impl From<&CertificateBody> for OutcomeSigningRequest {
    fn from(body: &CertificateBody) -> Self {
        Self {
            transfer_id: body.transfer_id,
            log_term: body.log_term,
            log_index: body.log_index,
            body_hash: body.body_hash(),
        }
    }
}

pub trait DrainPhaseSigner: Send + Sync + 'static {
    fn sign(
        &self,
        authenticated_requester: Address,
        purpose: DrainSigningPurpose,
        key: &DrainObjectKey,
        digest: B256,
        point: CommittedDrainPoint,
    ) -> Result<SignatureBytes, String>;
}

pub trait FastRaftPeerHandler: Send + Sync + 'static {
    fn append_entries(
        &self,
        peer: AuthenticatedRaftPeer,
        request: AppendEntriesRequest<FastRaftConfig>,
    ) -> HandlerFuture<'_, AppendEntriesResponse<u64>>;
    fn vote(
        &self,
        peer: AuthenticatedRaftPeer,
        request: VoteRequest<u64>,
    ) -> HandlerFuture<'_, VoteResponse<u64>>;
    fn install_snapshot(
        &self,
        peer: AuthenticatedRaftPeer,
        request: InstallSnapshotRequest<FastRaftConfig>,
    ) -> HandlerFuture<'_, InstallSnapshotResponse<u64>>;
    /// The request carries no peer-supplied execution summary: the receiver must reconstruct the
    /// exact body from its own fsynced committed prefix before signing it.
    fn sign_outcome(
        &self,
        _peer: AuthenticatedRaftPeer,
        _request: OutcomeSigningRequest,
    ) -> HandlerFuture<'_, SignedOutcome> {
        Box::pin(async { Err("committed-outcome signing is not installed".to_owned()) })
    }
    /// The authenticated requester identity is supplied by the transport. Implementations must
    /// rebuild the purpose-separated body from their own fsynced committed prefix.
    fn sign_drain_phase(
        &self,
        _peer: AuthenticatedRaftPeer,
        _purpose: DrainSigningPurpose,
        _key: DrainObjectKey,
        _digest: B256,
        _point: CommittedDrainPoint,
    ) -> HandlerFuture<'_, SignatureBytes> {
        Box::pin(async { Err("committed drain signing is not installed".to_owned()) })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignedOutcome {
    pub body: Vec<u8>,
    pub signature: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct FinalizedPeerIdentity {
    pub member: Address,
    pub transport: P2pPeerId,
}

#[derive(Clone, Debug)]
pub struct FastNetworkConfig {
    pub epoch: u64,
    pub local_node_id: u64,
    pub local_member: Address,
    pub local_transport: P2pPeerId,
    pub members: BTreeMap<u64, FinalizedPeerIdentity>,
    pub rpc_timeout: Duration,
}

impl FastNetworkConfig {
    pub fn validate(&self) -> Result<(), FastNetworkError> {
        if self.epoch == 0 || self.members.len() != 3 || self.rpc_timeout.is_zero() {
            return Err(FastNetworkError::InvalidTopology);
        }
        let local = self
            .members
            .get(&self.local_node_id)
            .ok_or(FastNetworkError::InvalidTopology)?;
        let members = self
            .members
            .values()
            .map(|peer| peer.member)
            .collect::<std::collections::BTreeSet<_>>();
        let transports = self
            .members
            .values()
            .map(|peer| peer.transport.clone())
            .collect::<std::collections::BTreeSet<_>>();
        if local.member != self.local_member
            || local.transport != self.local_transport
            || members.len() != 3
            || transports.len() != 3
            || members.contains(&Address::ZERO)
        {
            return Err(FastNetworkError::InvalidTopology);
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct AuthenticatedRaftTransport {
    config: Arc<FastNetworkConfig>,
    commands: mpsc::Sender<P2pCommand>,
    pending: Arc<Mutex<HashMap<u64, PendingResponse>>>,
    next_request: Arc<AtomicU64>,
}

struct PendingResponse {
    peer: P2pPeerId,
    sender: oneshot::Sender<RaftResponseFrame>,
}

impl AuthenticatedRaftTransport {
    pub fn new(
        config: FastNetworkConfig,
        commands: mpsc::Sender<P2pCommand>,
        mut responses: mpsc::Receiver<RaftResponseFrame>,
    ) -> Result<Self, FastNetworkError> {
        config.validate()?;
        let pending = Arc::new(Mutex::new(HashMap::<u64, PendingResponse>::new()));
        let response_pending = pending.clone();
        tokio::spawn(async move {
            while let Some(frame) = responses.recv().await {
                let sender = {
                    let mut pending = response_pending.lock().expect("Raft response map poisoned");
                    match pending.get(&frame.request_id) {
                        Some(expected) if expected.peer == frame.peer => pending
                            .remove(&frame.request_id)
                            .map(|response| response.sender),
                        _ => None,
                    }
                };
                if let Some(sender) = sender {
                    let _ = sender.send(frame);
                }
            }
        });
        Ok(Self {
            config: Arc::new(config),
            commands,
            pending,
            next_request: Arc::new(AtomicU64::new(0)),
        })
    }

    pub const fn config(&self) -> &Arc<FastNetworkConfig> {
        &self.config
    }

    async fn call(
        &self,
        target: u64,
        request: RaftRequest,
        timeout: Duration,
    ) -> Result<RaftResponse, TransportError> {
        let peer = self
            .config
            .members
            .get(&target)
            .ok_or_else(|| TransportError {
                message: format!("unknown finalized Raft member {target}"),
            })?;
        let payload = encode_frame(&request).map_err(transport_error)?;
        if payload.len() > MAX_RAFT_MESSAGE_SIZE {
            return Err(TransportError {
                message: "Raft request is oversized".to_owned(),
            });
        }
        let (request_id, receiver) = loop {
            let request_id = self
                .next_request
                .fetch_add(1, Ordering::Relaxed)
                .wrapping_add(1);
            if request_id == 0 {
                continue;
            }
            let mut pending = self.pending.lock().map_err(|_| TransportError {
                message: "Raft response map poisoned".to_owned(),
            })?;
            if pending.contains_key(&request_id) {
                continue;
            }
            let (sender, receiver) = oneshot::channel();
            pending.insert(
                request_id,
                PendingResponse {
                    peer: peer.transport.clone(),
                    sender,
                },
            );
            break (request_id, receiver);
        };
        if self
            .commands
            .send(P2pCommand::SendRaftRequest {
                target: peer.transport.clone(),
                request_id,
                payload,
            })
            .await
            .is_err()
        {
            if let Ok(mut pending) = self.pending.lock() {
                pending.remove(&request_id);
            }
            return Err(TransportError {
                message: "authenticated P2P command channel closed".to_owned(),
            });
        }
        let frame = match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(frame)) => frame,
            Ok(Err(error)) => {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.remove(&request_id);
                }
                return Err(transport_error(error));
            }
            Err(_) => {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.remove(&request_id);
                }
                return Err(TransportError {
                    message: "authenticated Raft response timed out".to_owned(),
                });
            }
        };
        if frame.peer != peer.transport {
            return Err(TransportError {
                message: "Raft response came from the wrong authenticated peer".to_owned(),
            });
        }
        decode_frame(&frame.payload).map_err(transport_error)
    }

    pub async fn request_outcome_signature(
        &self,
        target: u64,
        request: OutcomeSigningRequest,
    ) -> Result<SignedOutcome, TransportError> {
        match self
            .call(
                target,
                RaftRequest::SignOutcome(request),
                self.config.rpc_timeout,
            )
            .await?
        {
            RaftResponse::OutcomeSignature(value) => Ok(value),
            RaftResponse::Error(message) => Err(TransportError { message }),
            _ => Err(wrong_response()),
        }
    }

    pub async fn request_drain_signature(
        &self,
        target_member: Address,
        purpose: DrainSigningPurpose,
        key: DrainObjectKey,
        digest: B256,
        point: CommittedDrainPoint,
    ) -> Result<SignatureBytes, TransportError> {
        let target = self
            .config
            .members
            .iter()
            .find_map(|(node_id, peer)| (peer.member == target_member).then_some(*node_id))
            .ok_or_else(|| TransportError {
                message: "unknown finalized drain-signing member".to_owned(),
            })?;
        match self
            .call(
                target,
                RaftRequest::SignDrain(DrainSignRequestWire {
                    purpose: purpose.into(),
                    key: key.into(),
                    digest,
                    point: point.into(),
                }),
                self.config.rpc_timeout,
            )
            .await?
        {
            RaftResponse::DrainSignature(bytes) => {
                let bytes: [u8; 65] = bytes.try_into().map_err(|_| TransportError {
                    message: "invalid drain signature length".to_owned(),
                })?;
                Ok(SignatureBytes(bytes))
            }
            RaftResponse::Error(message) => Err(TransportError { message }),
            _ => Err(wrong_response()),
        }
    }
}

/// Bridge the C5 committee request channel onto the authenticated finalized-member transport.
pub async fn serve_drain_signature_requests(
    transport: AuthenticatedRaftTransport,
    mut requests: mpsc::Receiver<DrainSignatureNetworkRequest>,
) -> Result<(), FastNetworkError> {
    while let Some(request) = requests.recv().await {
        let result = transport
            .request_drain_signature(
                request.target_member,
                request.purpose,
                request.key,
                request.digest,
                request.point,
            )
            .await
            .map_err(|error| error.to_string());
        let _ = request.response.send(result);
    }
    Err(FastNetworkError::ChannelClosed)
}

impl RaftTransport for AuthenticatedRaftTransport {
    fn append_entries(
        &self,
        target: u64,
        _: &BasicNode,
        request: AppendEntriesRequest<FastRaftConfig>,
        option: RPCOption,
    ) -> TransportFuture<'_, AppendEntriesResponse<u64>> {
        Box::pin(async move {
            let timeout = self.config.rpc_timeout.min(option.hard_ttl());
            match self
                .call(target, RaftRequest::Append(request), timeout)
                .await?
            {
                RaftResponse::Append(value) => Ok(value),
                RaftResponse::Error(message) => Err(TransportError { message }),
                _ => Err(wrong_response()),
            }
        })
    }
    fn vote(
        &self,
        target: u64,
        _: &BasicNode,
        request: VoteRequest<u64>,
        option: RPCOption,
    ) -> TransportFuture<'_, VoteResponse<u64>> {
        Box::pin(async move {
            let timeout = self.config.rpc_timeout.min(option.hard_ttl());
            match self
                .call(target, RaftRequest::Vote(request), timeout)
                .await?
            {
                RaftResponse::Vote(value) => Ok(value),
                RaftResponse::Error(message) => Err(TransportError { message }),
                _ => Err(wrong_response()),
            }
        })
    }
    fn install_snapshot(
        &self,
        target: u64,
        _: &BasicNode,
        request: InstallSnapshotRequest<FastRaftConfig>,
        option: RPCOption,
    ) -> TransportFuture<'_, InstallSnapshotResponse<u64>> {
        Box::pin(async move {
            let timeout = self.config.rpc_timeout.min(option.hard_ttl());
            match self
                .call(target, RaftRequest::Snapshot(request), timeout)
                .await?
            {
                RaftResponse::Snapshot(value) => Ok(value),
                RaftResponse::Error(message) => Err(TransportError { message }),
                _ => Err(wrong_response()),
            }
        })
    }
}

pub async fn serve_fast_raft(
    config: Arc<FastNetworkConfig>,
    commands: mpsc::Sender<P2pCommand>,
    mut requests: mpsc::Receiver<RaftRequestFrame>,
    handler: Arc<dyn FastRaftPeerHandler>,
) -> Result<(), FastNetworkError> {
    while let Some(frame) = requests.recv().await {
        let Some((node_id, identity)) = config
            .members
            .iter()
            .find(|(_, identity)| identity.transport == frame.peer)
        else {
            continue;
        };
        let peer = AuthenticatedRaftPeer {
            epoch: config.epoch,
            node_id: *node_id,
            member: identity.member,
        };
        let response = match decode_frame(&frame.payload) {
            Ok(RaftRequest::Append(request)) => handler
                .append_entries(peer, request)
                .await
                .map(RaftResponse::Append),
            Ok(RaftRequest::Vote(request)) => {
                handler.vote(peer, request).await.map(RaftResponse::Vote)
            }
            Ok(RaftRequest::Snapshot(request)) => handler
                .install_snapshot(peer, request)
                .await
                .map(RaftResponse::Snapshot),
            Ok(RaftRequest::SignOutcome(request)) => handler
                .sign_outcome(peer, request)
                .await
                .map(RaftResponse::OutcomeSignature),
            Ok(RaftRequest::SignDrain(request)) => {
                let request = request.try_into();
                match request {
                    Ok((purpose, key, digest, point)) => handler
                        .sign_drain_phase(peer, purpose, key, digest, point)
                        .await
                        .map(|signature| RaftResponse::DrainSignature(signature.0.to_vec())),
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(format!("invalid bounded Raft request: {error}")),
        }
        .unwrap_or_else(RaftResponse::Error);
        let payload = encode_frame(&response)?;
        commands
            .send(P2pCommand::SendRaftResponse {
                target: frame.peer,
                request_id: frame.request_id,
                payload,
            })
            .await
            .map_err(|_| FastNetworkError::ChannelClosed)?;
    }
    Err(FastNetworkError::ChannelClosed)
}

#[derive(Debug, Serialize, Deserialize)]
enum RaftRequest {
    Append(AppendEntriesRequest<FastRaftConfig>),
    Vote(VoteRequest<u64>),
    Snapshot(InstallSnapshotRequest<FastRaftConfig>),
    SignOutcome(OutcomeSigningRequest),
    SignDrain(DrainSignRequestWire),
}
#[derive(Debug, Serialize, Deserialize)]
enum RaftResponse {
    Append(AppendEntriesResponse<u64>),
    Vote(VoteResponse<u64>),
    Snapshot(InstallSnapshotResponse<u64>),
    OutcomeSignature(SignedOutcome),
    DrainSignature(Vec<u8>),
    Error(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct DrainSignRequestWire {
    purpose: DrainPurposeWire,
    key: DrainKeyWire,
    digest: B256,
    point: DrainPointWire,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
enum DrainPurposeWire {
    Barrier,
    Resolution,
    FinalSettlement,
    Checkpoint,
}

impl From<DrainSigningPurpose> for DrainPurposeWire {
    fn from(value: DrainSigningPurpose) -> Self {
        match value {
            DrainSigningPurpose::Barrier => Self::Barrier,
            DrainSigningPurpose::Resolution => Self::Resolution,
            DrainSigningPurpose::FinalSettlement => Self::FinalSettlement,
            DrainSigningPurpose::Checkpoint => Self::Checkpoint,
        }
    }
}

impl From<DrainPurposeWire> for DrainSigningPurpose {
    fn from(value: DrainPurposeWire) -> Self {
        match value {
            DrainPurposeWire::Barrier => Self::Barrier,
            DrainPurposeWire::Resolution => Self::Resolution,
            DrainPurposeWire::FinalSettlement => Self::FinalSettlement,
            DrainPurposeWire::Checkpoint => Self::Checkpoint,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
enum DrainKeyWire {
    Closure {
        epoch: u64,
    },
    OutboundBarrier {
        epoch: u64,
        destination: Address,
    },
    InboundBarrier {
        epoch: u64,
        source: Address,
    },
    Resolution {
        destination_epoch: u64,
        destination: Address,
        source: Address,
    },
    FinalSettlement {
        epoch: u64,
    },
    Checkpoint {
        old_epoch: u64,
        next_epoch: u64,
    },
    Retirement {
        epoch: u64,
    },
}

impl From<DrainObjectKey> for DrainKeyWire {
    fn from(value: DrainObjectKey) -> Self {
        match value {
            DrainObjectKey::Closure { epoch } => Self::Closure { epoch },
            DrainObjectKey::OutboundBarrier { epoch, destination } => {
                Self::OutboundBarrier { epoch, destination }
            }
            DrainObjectKey::InboundBarrier { epoch, source } => {
                Self::InboundBarrier { epoch, source }
            }
            DrainObjectKey::Resolution {
                destination_epoch,
                destination,
                source,
            } => Self::Resolution {
                destination_epoch,
                destination,
                source,
            },
            DrainObjectKey::FinalSettlement { epoch } => Self::FinalSettlement { epoch },
            DrainObjectKey::Checkpoint {
                old_epoch,
                next_epoch,
            } => Self::Checkpoint {
                old_epoch,
                next_epoch,
            },
            DrainObjectKey::Retirement { epoch } => Self::Retirement { epoch },
        }
    }
}

impl From<DrainKeyWire> for DrainObjectKey {
    fn from(value: DrainKeyWire) -> Self {
        match value {
            DrainKeyWire::Closure { epoch } => Self::Closure { epoch },
            DrainKeyWire::OutboundBarrier { epoch, destination } => {
                Self::OutboundBarrier { epoch, destination }
            }
            DrainKeyWire::InboundBarrier { epoch, source } => {
                Self::InboundBarrier { epoch, source }
            }
            DrainKeyWire::Resolution {
                destination_epoch,
                destination,
                source,
            } => Self::Resolution {
                destination_epoch,
                destination,
                source,
            },
            DrainKeyWire::FinalSettlement { epoch } => Self::FinalSettlement { epoch },
            DrainKeyWire::Checkpoint {
                old_epoch,
                next_epoch,
            } => Self::Checkpoint {
                old_epoch,
                next_epoch,
            },
            DrainKeyWire::Retirement { epoch } => Self::Retirement { epoch },
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct DrainPointWire {
    log_term: u64,
    log_index: u64,
    block_height: u64,
    block_hash: B256,
    state_root: B256,
    imported_anchor_number: u64,
    imported_anchor_hash: B256,
}

impl From<CommittedDrainPoint> for DrainPointWire {
    fn from(value: CommittedDrainPoint) -> Self {
        Self {
            log_term: value.log_term,
            log_index: value.log_index,
            block_height: value.block_height,
            block_hash: value.block_hash,
            state_root: value.state_root,
            imported_anchor_number: value.imported_anchor_number,
            imported_anchor_hash: value.imported_anchor_hash,
        }
    }
}

impl From<DrainPointWire> for CommittedDrainPoint {
    fn from(value: DrainPointWire) -> Self {
        Self {
            log_term: value.log_term,
            log_index: value.log_index,
            block_height: value.block_height,
            block_hash: value.block_hash,
            state_root: value.state_root,
            imported_anchor_number: value.imported_anchor_number,
            imported_anchor_hash: value.imported_anchor_hash,
        }
    }
}

impl TryFrom<DrainSignRequestWire>
    for (
        DrainSigningPurpose,
        DrainObjectKey,
        B256,
        CommittedDrainPoint,
    )
{
    type Error = String;

    fn try_from(value: DrainSignRequestWire) -> Result<Self, Self::Error> {
        let point: CommittedDrainPoint = value.point.into();
        if value.digest.is_zero()
            || point.log_index == 0
            || point.block_hash.is_zero()
            || point.state_root.is_zero()
            || point.imported_anchor_hash.is_zero()
        {
            return Err("invalid bounded drain signing request".to_owned());
        }
        Ok((value.purpose.into(), value.key.into(), value.digest, point))
    }
}
fn wrong_response() -> TransportError {
    TransportError {
        message: "mismatched Raft response kind".to_owned(),
    }
}
fn transport_error(error: impl std::fmt::Display) -> TransportError {
    TransportError {
        message: error.to_string(),
    }
}

fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, bincode::Error> {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(MAX_RAFT_MESSAGE_SIZE as u64)
        .serialize(value)
}

fn decode_frame<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, bincode::Error> {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(MAX_RAFT_MESSAGE_SIZE as u64)
        .reject_trailing_bytes()
        .deserialize(bytes)
}

#[derive(Debug, thiserror::Error)]
pub enum FastNetworkError {
    #[error(
        "fast Raft topology must bind exactly three finalized members to three authenticated manifest identities"
    )]
    InvalidTopology,
    #[error("authenticated P2P Raft channel closed")]
    ChannelClosed,
    #[error(transparent)]
    Codec(#[from] bincode::Error),
}
