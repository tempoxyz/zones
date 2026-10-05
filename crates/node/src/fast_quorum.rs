//! Maintained-Raft adapter and finalized T14 activation fence for instant Zone execution.
//!
//! This module deliberately contains no CLI switch. Fast execution is allowed only from exact
//! finalized Portal registry evidence carrying the T14 native compatibility pin.

use std::{
    collections::BTreeSet,
    fmt,
    future::Future,
    io::Cursor,
    path::Path,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use alloy_consensus::{BlockHeader as _, Transaction as _};
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_rlp::Decodable as _;
use alloy_sol_types::{SolCall as _, SolValue};
use openraft::{
    BasicNode, Config, Raft,
    error::{InstallSnapshotError, RPCError, RaftError, Unreachable},
    network::{RPCOption, RaftNetwork, RaftNetworkFactory},
    raft::{
        AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest,
        InstallSnapshotResponse, VoteRequest, VoteResponse,
    },
};
use serde::{Deserialize, Serialize};
use zone_fast_transfer::{
    CertificateError, DurableJournal, EpochRoster, JournalError, QuorumVerifier,
    ReplicatedBlockInput as JournaledBlockInput, SigningRecord,
};
use zone_payload::{FastSettlementBoundary, ZonePayloadAttributes};
use zone_primitives::fast_transfer::{
    CertificateBody, OutcomeCertificate, SignatureBytes, TransferIntent, ZoneDomain,
};

use crate::{
    fast_raft_state_machine::{
        CommittedBlockRef, CommittedReadError, CommittedStateHandle, CommittedTransferRecord,
        DurableRaftStateMachine, DurableStateMachineExecution,
    },
    fast_raft_store::DurableRaftLogStore,
};

/// Protocol version implemented at the selected T14 boundary.
pub const FAST_PROTOCOL_VERSION: u32 = 1;
pub const MAX_ANCHOR_STALENESS: Duration = Duration::from_secs(2);
pub const MAX_LIVE_CLOCK_SKEW: Duration = Duration::from_millis(100);
pub const MAX_RAFT_APPEND_ENTRIES: usize = 256;
pub const MAX_RAFT_RPC_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_RAFT_SNAPSHOT_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Literal hashed by `ZonePortal.FAST_PROTOCOL_NATIVE_PIN()` at T14.
pub const T14_FAST_PROTOCOL_PIN_LABEL: &[u8] = b"TEMPO_ZONE_FAST_PROTOCOL_T14_V1";

pub fn t14_fast_protocol_native_pin() -> B256 {
    keccak256(T14_FAST_PROTOCOL_PIN_LABEL)
}

/// Complete deterministic input replicated for one Zone block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicatedBlockInput {
    /// Authority epoch imported from finalized L1 state.
    pub epoch: u64,
    /// Parent Zone block hash.
    pub parent_hash: B256,
    /// Canonical encoded block/payload attributes.
    pub block_input: Bytes,
    /// Complete ordered transactions, including the opening system transaction.
    pub transactions: Vec<Bytes>,
    /// Finalized L1 execution inputs used by the block.
    pub l1_inputs: Bytes,
    /// Replay witness needed by followers and proof generation.
    pub replay_witness: Bytes,
}

/// Identity of the preceding canonical `BatchFinalized` boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CanonicalFastSettlementBoundary {
    pub block_height: u64,
    pub block_hash: B256,
    pub timestamp_millis: u64,
}

impl ReplicatedBlockInput {
    /// Stable digest used to reject a state-machine response for different input bytes.
    pub fn digest(&self) -> B256 {
        let encoded = bincode::serialize(self)
            .expect("ReplicatedBlockInput contains only infallibly serializable values");
        keccak256(encoded)
    }

    /// Exact encoded bytes retained for this complete replay/proof input in the OpenRaft log.
    pub fn retained_encoded_len(&self) -> Result<u64, FastBoundaryValidationError> {
        bincode::serialized_size(self).map_err(FastBoundaryValidationError::Attributes)
    }

    /// Decode the optional canonical boundary from the replicated payload attributes.
    pub fn fast_settlement_boundary(
        &self,
    ) -> Result<Option<FastSettlementBoundary>, FastBoundaryValidationError> {
        let attributes: ZonePayloadAttributes = bincode::deserialize(&self.l1_inputs)
            .map_err(FastBoundaryValidationError::Attributes)?;
        Ok(attributes.fast_settlement_boundary)
    }

    /// Validate a ready boundary against the committed predecessor and the actual finalization
    /// transaction in this same replicated entry.
    pub fn validate_fast_settlement_boundary(
        &self,
        previous: CanonicalFastSettlementBoundary,
        expected_cumulative_retained_bytes: u64,
    ) -> Result<FastSettlementBoundary, FastBoundaryValidationError> {
        let boundary = self
            .fast_settlement_boundary()?
            .ok_or(FastBoundaryValidationError::MissingMarker)?;
        if (
            boundary.previous_boundary_height,
            boundary.previous_boundary_hash,
            boundary.previous_boundary_timestamp_millis,
        ) != (
            previous.block_height,
            previous.block_hash,
            previous.timestamp_millis,
        ) {
            return Err(FastBoundaryValidationError::PreviousBoundary);
        }
        if boundary.cumulative_retained_bytes != expected_cumulative_retained_bytes {
            return Err(FastBoundaryValidationError::RetainedBytes {
                expected: expected_cumulative_retained_bytes,
                actual: boundary.cumulative_retained_bytes,
            });
        }

        let mut encoded_block = self.block_input.as_ref();
        let block = tempo_primitives::Block::decode(&mut encoded_block)
            .map_err(|error| FastBoundaryValidationError::Block(error.to_string()))?;
        if !encoded_block.is_empty() {
            return Err(FastBoundaryValidationError::TrailingBlockBytes);
        }
        let timestamp_millis = alloy_consensus::BlockHeader::timestamp(&block.header)
            .checked_mul(1_000)
            .and_then(|value| value.checked_add(block.header.timestamp_millis_part))
            .ok_or(FastBoundaryValidationError::TimestampOverflow)?;
        boundary
            .validate_payload_binding(
                block.header.number(),
                block.header.parent_hash(),
                timestamp_millis,
            )
            .map_err(FastBoundaryValidationError::Marker)?;

        let finalization_selector =
            zone_payload::abi::IZoneOutbox::finalizeWithdrawalBatchCall::SELECTOR;
        let mut matching = block.body.transactions.iter().filter(|transaction| {
            transaction.to() == Some(zone_payload::abi::ZONE_OUTBOX_ADDRESS)
                && transaction.input().starts_with(&finalization_selector)
        });
        let finalization = matching
            .next()
            .ok_or(FastBoundaryValidationError::MissingFinalization)?;
        if matching.next().is_some()
            || block.body.transactions.last() != Some(finalization)
            || finalization.input().as_ref() != boundary.finalization_calldata.as_ref()
        {
            return Err(FastBoundaryValidationError::FinalizationMismatch);
        }
        let tempo_primitives::TempoTxEnvelope::Legacy(signed) = finalization else {
            return Err(FastBoundaryValidationError::FinalizationMismatch);
        };
        let transaction = signed.tx();
        if signed.signature() != &tempo_primitives::transaction::envelope::TEMPO_SYSTEM_TX_SIGNATURE
            || transaction.chain_id.is_none()
            || transaction.nonce != 0
            || transaction.gas_price != 0
            || transaction.gas_limit != 0
            || transaction.value != U256::ZERO
        {
            return Err(FastBoundaryValidationError::FinalizationMismatch);
        }
        Ok(boundary)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FastBoundaryValidationError {
    #[error("failed to decode fast-settlement payload attributes: {0}")]
    Attributes(bincode::Error),
    #[error("fast-settlement boundary marker is missing")]
    MissingMarker,
    #[error("fast-settlement boundary predecessor does not match the canonical prefix")]
    PreviousBoundary,
    #[error("fast-settlement retained byte count mismatch: expected {expected}, got {actual}")]
    RetainedBytes { expected: u64, actual: u64 },
    #[error("failed to decode replicated block: {0}")]
    Block(String),
    #[error("replicated block has trailing bytes")]
    TrailingBlockBytes,
    #[error("replicated block timestamp overflows milliseconds")]
    TimestampOverflow,
    #[error("invalid fast-settlement marker: {0}")]
    Marker(&'static str),
    #[error("replicated boundary has no finalizeWithdrawalBatch transaction")]
    MissingFinalization,
    #[error("replicated boundary finalization transaction/calldata mismatch")]
    FinalizationMismatch,
}

impl fmt::Display for ReplicatedBlockInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "block(epoch={},digest={})", self.epoch, self.digest())
    }
}

/// State-machine response produced only after committed execution and durable replay storage.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedBlock {
    pub input_digest: B256,
    pub block_height: u64,
    pub block_hash: B256,
    pub state_root: B256,
    pub receipts_root: B256,
}

openraft::declare_raft_types!(
    /// OpenRaft configuration for Zone block ordering.
    pub FastRaftConfig:
        D = ReplicatedBlockInput,
        R = CommittedBlock,
        Node = BasicNode,
);

/// Handle around OpenRaft's maintained election, replication, and current-term commit rules.
///
/// Storage and network implementations are supplied by the node assembly. In particular, the
/// storage passed to `openraft::Raft::new` must fsync term/vote/log/snapshot data before invoking
/// OpenRaft's completion callbacks; this adapter never treats append acknowledgement as commit.
#[derive(Clone)]
pub struct FastRaft {
    raft: Raft<FastRaftConfig>,
}

pub(crate) type TransportFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, TransportError>> + Send + 'a>>;

/// Authenticated transport used by OpenRaft. Implementations must verify that the connection's
/// peer identity is the requested epoch member rather than trusting a reused socket address.
pub trait RaftTransport: Send + Sync + 'static {
    fn append_entries(
        &self,
        target: u64,
        node: &BasicNode,
        request: AppendEntriesRequest<FastRaftConfig>,
        deadline: RPCOption,
    ) -> TransportFuture<'_, AppendEntriesResponse<u64>>;

    fn vote(
        &self,
        target: u64,
        node: &BasicNode,
        request: VoteRequest<u64>,
        deadline: RPCOption,
    ) -> TransportFuture<'_, VoteResponse<u64>>;

    fn install_snapshot(
        &self,
        target: u64,
        node: &BasicNode,
        request: InstallSnapshotRequest<FastRaftConfig>,
        deadline: RPCOption,
    ) -> TransportFuture<'_, InstallSnapshotResponse<u64>>;
}

#[derive(Debug, thiserror::Error)]
#[error("authenticated Raft transport failed: {message}")]
pub struct TransportError {
    pub message: String,
}

/// OpenRaft network factory over the Zone's authenticated member transport.
#[derive(Clone)]
pub struct FastRaftNetworkFactory<T> {
    transport: Arc<T>,
}

impl<T> FastRaftNetworkFactory<T> {
    pub const fn new(transport: Arc<T>) -> Self {
        Self { transport }
    }
}

pub struct FastRaftNetwork<T> {
    target: u64,
    node: BasicNode,
    transport: Arc<T>,
}

impl<T: RaftTransport> RaftNetworkFactory<FastRaftConfig> for FastRaftNetworkFactory<T> {
    type Network = FastRaftNetwork<T>;

    async fn new_client(&mut self, target: u64, node: &BasicNode) -> Self::Network {
        FastRaftNetwork {
            target,
            node: node.clone(),
            transport: self.transport.clone(),
        }
    }
}

impl<T: RaftTransport> RaftNetwork<FastRaftConfig> for FastRaftNetwork<T> {
    async fn append_entries(
        &mut self,
        request: AppendEntriesRequest<FastRaftConfig>,
        option: RPCOption,
    ) -> Result<AppendEntriesResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        self.transport
            .append_entries(self.target, &self.node, request, option)
            .await
            .map_err(|error| RPCError::Unreachable(Unreachable::new(&error)))
    }

    async fn vote(
        &mut self,
        request: VoteRequest<u64>,
        option: RPCOption,
    ) -> Result<VoteResponse<u64>, RPCError<u64, BasicNode, RaftError<u64>>> {
        self.transport
            .vote(self.target, &self.node, request, option)
            .await
            .map_err(|error| RPCError::Unreachable(Unreachable::new(&error)))
    }

    async fn install_snapshot(
        &mut self,
        request: InstallSnapshotRequest<FastRaftConfig>,
        option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<u64>,
        RPCError<u64, BasicNode, RaftError<u64, InstallSnapshotError>>,
    > {
        self.transport
            .install_snapshot(self.target, &self.node, request, option)
            .await
            .map_err(|error| RPCError::Unreachable(Unreachable::new(&error)))
    }
}

impl FastRaft {
    pub const fn new(raft: Raft<FastRaftConfig>) -> Self {
        Self { raft }
    }

    /// Replicate and wait for committed state-machine application.
    pub async fn commit(&self, input: ReplicatedBlockInput) -> Result<RaftCommit, FastRaftError> {
        let expected = input.digest();
        let response = self
            .raft
            .client_write(input)
            .await
            .map_err(|error| FastRaftError::Write(error.to_string()))?;
        if response.data.input_digest != expected {
            return Err(FastRaftError::MismatchedExecution {
                expected,
                actual: response.data.input_digest,
            });
        }
        Ok(RaftCommit {
            term: response.log_id.leader_id.term,
            index: response.log_id.index,
            block: response.data,
        })
    }

    /// OpenRaft handle used by authenticated peer RPC handlers.
    pub const fn inner(&self) -> &Raft<FastRaftConfig> {
        &self.raft
    }
}

/// Assemble a real OpenRaft node after finalized activation evidence identifies the local member.
///
/// This opens and restores both durable stores before spawning OpenRaft. Cluster initialization
/// and peer RPC exposure remain explicit caller operations on [`FastRaft::inner`].
pub async fn assemble_fast_raft<T, E>(
    activation: &FastActivation,
    local_signer: Address,
    config: Arc<Config>,
    transport: Arc<T>,
    directory: impl AsRef<Path>,
    executor: Arc<E>,
) -> Result<FastRaftRuntime<E>, AssembleFastRaftError>
where
    T: RaftTransport,
    E: DurableStateMachineExecution,
{
    let member = activation
        .epoch()
        .members
        .iter()
        .position(|member| *member == local_signer)
        .ok_or(AssembleFastRaftError::LocalSignerNotInRoster)?;
    // Registry order is the stable Raft node-id mapping for the life of this fenced epoch.
    let node_id = u64::try_from(member + 1).expect("three-member index fits u64");
    let directory = directory.as_ref();
    let log_store =
        DurableRaftLogStore::open(directory.join("log")).map_err(AssembleFastRaftError::Storage)?;
    let state_machine = DurableRaftStateMachine::open(directory.join("state-machine"), executor)
        .await
        .map_err(AssembleFastRaftError::Storage)?;
    let committed = state_machine.committed_handle();
    let network = FastRaftNetworkFactory::new(transport);
    let raft = Raft::new(node_id, config, network, log_store, state_machine)
        .await
        .map_err(|error| AssembleFastRaftError::OpenRaft(error.to_string()))?;
    Ok(FastRaftRuntime {
        raft: FastRaft::new(raft),
        committed,
        activation: activation.clone(),
    })
}

/// Running consensus plus the only transfer/head view suitable for authenticated RPC.
pub struct FastRaftRuntime<E> {
    pub raft: FastRaft,
    committed: CommittedStateHandle<E>,
    activation: FastActivation,
}

impl<E: DurableStateMachineExecution> FastRaftRuntime<E> {
    pub fn committed_head(
        &self,
    ) -> Result<Option<CommittedBlockRef>, CommittedReadError<E::Error>> {
        self.committed.committed_head()
    }

    pub fn committed_transfer(
        &self,
        transfer_id: B256,
    ) -> Result<Option<CommittedTransferRecord>, RuntimeReadError<E::Error>> {
        let record = self
            .committed
            .committed_transfer(transfer_id)
            .map_err(RuntimeReadError::Committed)?;
        if record
            .as_ref()
            .is_some_and(|record| !self.matches_activation(&record.body.zone))
        {
            return Err(RuntimeReadError::WrongEpoch);
        }
        Ok(record)
    }

    pub const fn committed_handle(&self) -> &CommittedStateHandle<E> {
        &self.committed
    }

    /// Handle an AppendEntries request only after the transport maps its authenticated peer key
    /// to this finalized epoch identity.
    pub async fn handle_append_entries(
        &self,
        peer: AuthenticatedRaftPeer,
        request: AppendEntriesRequest<FastRaftConfig>,
    ) -> Result<AppendEntriesResponse<u64>, PeerRaftRpcError> {
        self.authorize_peer(peer, request.vote.leader_id.node_id)?;
        if request.entries.len() > MAX_RAFT_APPEND_ENTRIES
            || append_payload_bytes(&request) > MAX_RAFT_RPC_BYTES
        {
            return Err(PeerRaftRpcError::Oversized);
        }
        self.raft
            .inner()
            .append_entries(request)
            .await
            .map_err(|error| PeerRaftRpcError::OpenRaft(error.to_string()))
    }

    pub async fn handle_vote(
        &self,
        peer: AuthenticatedRaftPeer,
        request: VoteRequest<u64>,
    ) -> Result<VoteResponse<u64>, PeerRaftRpcError> {
        self.authorize_peer(peer, request.vote.leader_id.node_id)?;
        self.raft
            .inner()
            .vote(request)
            .await
            .map_err(|error| PeerRaftRpcError::OpenRaft(error.to_string()))
    }

    pub async fn handle_install_snapshot(
        &self,
        peer: AuthenticatedRaftPeer,
        request: InstallSnapshotRequest<FastRaftConfig>,
    ) -> Result<InstallSnapshotResponse<u64>, PeerRaftRpcError> {
        self.authorize_peer(peer, request.vote.leader_id.node_id)?;
        if request.data.len() > MAX_RAFT_SNAPSHOT_CHUNK_BYTES {
            return Err(PeerRaftRpcError::Oversized);
        }
        self.raft
            .inner()
            .install_snapshot(request)
            .await
            .map_err(|error| PeerRaftRpcError::OpenRaft(error.to_string()))
    }

    fn authorize_peer(
        &self,
        peer: AuthenticatedRaftPeer,
        claimed_node_id: u64,
    ) -> Result<(), PeerRaftRpcError> {
        let epoch = self.activation.epoch();
        let expected = usize::try_from(peer.node_id)
            .ok()
            .and_then(|id| id.checked_sub(1))
            .and_then(|index| epoch.members.get(index));
        if peer.epoch != epoch.epoch
            || peer.node_id != claimed_node_id
            || expected != Some(&peer.member)
        {
            return Err(PeerRaftRpcError::Unauthorized);
        }
        Ok(())
    }

    fn matches_activation(&self, domain: &ZoneDomain) -> bool {
        let epoch = self.activation.epoch();
        domain.l1_chain_id == epoch.l1_chain_id
            && domain.zone_id == epoch.zone_id
            && domain.chain_id == epoch.zone_chain_id
            && domain.portal == epoch.portal
            && domain.authority_epoch == epoch.epoch
            && domain.roster_hash == epoch.roster_hash
            && u32::from(domain.protocol_version) == epoch.protocol_version
    }
}

/// Identity already authenticated by the private Commonware session and manifest mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthenticatedRaftPeer {
    pub epoch: u64,
    pub node_id: u64,
    pub member: Address,
}

#[derive(Debug, thiserror::Error)]
pub enum PeerRaftRpcError {
    #[error("Raft RPC peer is not the claimed member of the finalized epoch")]
    Unauthorized,
    #[error("Raft RPC exceeds its bounded frame or entry limit")]
    Oversized,
    #[error("OpenRaft rejected peer RPC: {0}")]
    OpenRaft(String),
}

fn append_payload_bytes(request: &AppendEntriesRequest<FastRaftConfig>) -> usize {
    request
        .entries
        .iter()
        .filter_map(|entry| match &entry.payload {
            openraft::EntryPayload::Normal(input) => Some(input),
            _ => None,
        })
        .fold(0usize, |total, input| {
            total
                .saturating_add(input.block_input.len())
                .saturating_add(input.l1_inputs.len())
                .saturating_add(input.replay_witness.len())
                .saturating_add(
                    input
                        .transactions
                        .iter()
                        .fold(0usize, |size, tx| size.saturating_add(tx.len())),
                )
        })
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeReadError<E: std::error::Error + 'static> {
    #[error(transparent)]
    Committed(CommittedReadError<E>),
    #[error("committed transfer belongs to a different finalized authority epoch")]
    WrongEpoch,
}

#[derive(Debug, thiserror::Error)]
pub enum AssembleFastRaftError {
    #[error("local outcome signer is not in the finalized epoch roster")]
    LocalSignerNotInRoster,
    #[error("failed to open durable Raft storage: {0}")]
    Storage(std::io::Error),
    #[error("failed to start OpenRaft: {0}")]
    OpenRaft(String),
}

/// Original Raft coordinates are part of every outcome certificate body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RaftCommit {
    pub term: u64,
    pub index: u64,
    pub block: CommittedBlock,
}

/// Verify that a received certificate names the exact locally committed entry and execution
/// result before accepting its token outcome.
pub fn verify_committed_certificate(
    verifier: &QuorumVerifier,
    certificate: &OutcomeCertificate,
    intent: &TransferIntent,
    commit: &RaftCommit,
) -> Result<[Address; 2], CommittedCertificateError> {
    let body = &certificate.body;
    if body.log_term != commit.term
        || body.log_index != commit.index
        || body.block_height != commit.block.block_height
        || body.block_hash != commit.block.block_hash
        || body.state_root != commit.block.state_root
    {
        return Err(CommittedCertificateError::CommitMismatch);
    }
    verifier
        .verify_outcome(certificate, intent)
        .map_err(CommittedCertificateError::Certificate)
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CommittedCertificateError {
    #[error("certificate does not identify the locally committed Raft execution")]
    CommitMismatch,
    #[error(transparent)]
    Certificate(#[from] CertificateError),
}

/// Durable committed-prefix metadata used to fence canonical reads after a crash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommittedPrefix {
    pub term: u64,
    pub index: u64,
    pub block_height: u64,
    pub block_hash: B256,
    pub state_root: B256,
}

/// Persistence boundary shared by Raft application and canonical-Reth promotion.
pub trait CommittedPrefixStore: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Fsync replay data and the committed pointer before returning.
    fn persist_committed(
        &self,
        input: &ReplicatedBlockInput,
        committed: &CommittedPrefix,
    ) -> Result<(), Self::Error>;

    fn committed_prefix(&self) -> Result<Option<CommittedPrefix>, Self::Error>;
}

impl CommittedPrefixStore for DurableJournal {
    type Error = JournalError;

    fn persist_committed(
        &self,
        input: &ReplicatedBlockInput,
        committed: &CommittedPrefix,
    ) -> Result<(), Self::Error> {
        self.persist_replicated_block(JournaledBlockInput {
            log_term: committed.term,
            log_index: committed.index,
            block_height: committed.block_height,
            block_hash: committed.block_hash,
            state_root: committed.state_root,
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

    fn committed_prefix(&self) -> Result<Option<CommittedPrefix>, Self::Error> {
        Ok(self
            .replicated_blocks_from(0)?
            .last()
            .map(|record| CommittedPrefix {
                term: record.log_term,
                index: record.log_index,
                block_height: record.block_height,
                block_hash: record.block_hash,
                state_root: record.state_root,
            }))
    }
}

/// Deterministic execution hook used by the OpenRaft state machine after commitment.
pub trait DeterministicBlockExecution {
    type Error: std::error::Error + Send + Sync + 'static;

    fn execute_replicated(
        &self,
        input: &ReplicatedBlockInput,
    ) -> Result<CommittedBlock, Self::Error>;
}

/// Execute an applied Raft entry and persist all replay data before returning the application
/// response. OpenRaft only applies committed entries, so this is the earliest signing-eligible
/// boundary; signature journaling is a separate subsequent fsync.
pub fn apply_committed_entry<S, E>(
    store: &S,
    executor: &E,
    term: u64,
    index: u64,
    input: &ReplicatedBlockInput,
) -> Result<CommittedBlock, ApplyCommittedError<S::Error, E::Error>>
where
    S: CommittedPrefixStore,
    E: DeterministicBlockExecution,
{
    let block = executor
        .execute_replicated(input)
        .map_err(ApplyCommittedError::Execution)?;
    if block.input_digest != input.digest() {
        return Err(ApplyCommittedError::InputDigest {
            expected: input.digest(),
            actual: block.input_digest,
        });
    }
    let prefix = CommittedPrefix {
        term,
        index,
        block_height: block.block_height,
        block_hash: block.block_hash,
        state_root: block.state_root,
    };
    store
        .persist_committed(input, &prefix)
        .map_err(ApplyCommittedError::Persistence)?;
    Ok(block)
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyCommittedError<S, E>
where
    S: std::error::Error + 'static,
    E: std::error::Error + 'static,
{
    #[error("committed block execution failed: {0}")]
    Execution(E),
    #[error("committed execution input digest mismatch: expected {expected}, got {actual}")]
    InputDigest { expected: B256, actual: B256 },
    #[error("committed replay data persistence failed: {0}")]
    Persistence(S),
}

/// Adapter for comparing and replaying Reth's canonical head.
pub trait CanonicalReplay: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    fn canonical_height(&self) -> Result<u64, Self::Error>;

    /// Replay the durable committed sequence through `target`, checking all hashes and roots.
    fn replay_committed_through(&self, target: &CommittedPrefix) -> Result<(), Self::Error>;
}

/// Prevent canonical balances/receipts from observing a head behind the durable Raft prefix.
pub fn reconcile_before_canonical_read<S, R>(
    store: &S,
    replay: &R,
) -> Result<Option<CommittedPrefix>, CanonicalReadError<S::Error, R::Error>>
where
    S: CommittedPrefixStore,
    R: CanonicalReplay,
{
    let Some(committed) = store
        .committed_prefix()
        .map_err(CanonicalReadError::Store)?
    else {
        return Ok(None);
    };
    let canonical = replay
        .canonical_height()
        .map_err(CanonicalReadError::Replay)?;
    if canonical < committed.block_height {
        replay
            .replay_committed_through(&committed)
            .map_err(CanonicalReadError::Replay)?;
    }
    let recovered = replay
        .canonical_height()
        .map_err(CanonicalReadError::Replay)?;
    if recovered < committed.block_height {
        return Err(CanonicalReadError::StillBehind {
            canonical: recovered,
            committed: committed.block_height,
        });
    }
    Ok(Some(committed))
}

#[derive(Debug, thiserror::Error)]
pub enum CanonicalReadError<S, R>
where
    S: std::error::Error + 'static,
    R: std::error::Error + 'static,
{
    #[error("failed to read durable committed prefix: {0}")]
    Store(S),
    #[error("failed to reconcile canonical state: {0}")]
    Replay(R),
    #[error("canonical head {canonical} remains behind committed head {committed}")]
    StillBehind { canonical: u64, committed: u64 },
}

#[derive(Debug, thiserror::Error)]
pub enum FastRaftError {
    #[error("Raft client write failed: {0}")]
    Write(String),
    #[error("committed state machine executed different input: expected {expected}, got {actual}")]
    MismatchedExecution { expected: B256, actual: B256 },
}

/// Finalized registry evidence required before fast execution may be assembled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalizedFastEpoch {
    pub l1_chain_id: u64,
    pub portal: Address,
    pub zone_id: u32,
    pub zone_chain_id: u64,
    pub epoch: u64,
    pub protocol_version: u32,
    pub threshold: u8,
    /// Immutable on-chain proof policy included in the authority commitment.
    pub proof_mode: u8,
    pub expected_verifier_code_hash: B256,
    pub expected_verifier_config_hash: B256,
    pub members: [Address; 3],
    pub peer_portals: [Address; 9],
    pub roster_hash: B256,
    pub finalized_l1_block: u64,
}

/// Exact Portal fields read from the imported finalized T14 L1 state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalizedT14Capability {
    pub epoch: FinalizedFastEpoch,
    pub native_pin: B256,
    pub t14_active_at_anchor: bool,
    pub current_epoch: u64,
    pub activated_at_l1_block: u64,
    pub closed: bool,
    pub retired: bool,
}

/// Capability token. It cannot be constructed from configuration or a local manifest.
#[derive(Clone, Debug)]
pub struct FastActivation(Arc<FinalizedFastEpoch>);

impl FastActivation {
    /// Validate finalized authority evidence and dependency compatibility.
    pub fn from_finalized_epoch(evidence: FinalizedT14Capability) -> Result<Self, ActivationError> {
        let epoch = evidence.epoch;
        if evidence.native_pin != t14_fast_protocol_native_pin() {
            return Err(ActivationError::NativePin {
                expected: t14_fast_protocol_native_pin(),
                actual: evidence.native_pin,
            });
        }
        if !evidence.t14_active_at_anchor {
            return Err(ActivationError::T14Inactive);
        }
        if epoch.finalized_l1_block == 0
            || evidence.activated_at_l1_block == 0
            || evidence.activated_at_l1_block > epoch.finalized_l1_block
        {
            return Err(ActivationError::NotFinalized);
        }
        if evidence.current_epoch != epoch.epoch {
            return Err(ActivationError::NotCurrent {
                expected: evidence.current_epoch,
                actual: epoch.epoch,
            });
        }
        // Closure revokes only new lock/quote admission. The exact historical roster remains
        // authoritative for delayed resolves, outcomes, dispositions, proofs and retirement until
        // the finalized registry marks it retired, including after a process restart.
        if evidence.retired {
            return Err(ActivationError::Retired);
        }
        if epoch.protocol_version != FAST_PROTOCOL_VERSION {
            return Err(ActivationError::ProtocolVersion {
                expected: FAST_PROTOCOL_VERSION,
                actual: epoch.protocol_version,
            });
        }
        if epoch.threshold != 2 {
            return Err(ActivationError::Threshold(epoch.threshold));
        }
        if !matches!(epoch.proof_mode, 1 | 2)
            || epoch.expected_verifier_code_hash.is_zero()
            || epoch.expected_verifier_config_hash.is_zero()
        {
            return Err(ActivationError::ProofPolicy);
        }
        let distinct = epoch.members.into_iter().collect::<BTreeSet<_>>();
        if distinct.len() != 3 || distinct.contains(&Address::ZERO) {
            return Err(ActivationError::Roster);
        }
        let peer_portals = epoch.peer_portals.into_iter().collect::<BTreeSet<_>>();
        if peer_portals.len() != 9
            || peer_portals.contains(&Address::ZERO)
            || peer_portals.contains(&epoch.portal)
        {
            return Err(ActivationError::PeerRoster);
        }
        let expected_roster_hash = keccak256(
            (
                keccak256("TEMPO_ZONE_FAST_ROSTER_T14_V1"),
                epoch.portal,
                epoch.epoch,
                epoch.protocol_version,
                U256::from(epoch.threshold),
                U256::from(epoch.proof_mode),
                epoch.expected_verifier_code_hash,
                epoch.expected_verifier_config_hash,
                epoch.members.to_vec(),
                epoch.peer_portals.to_vec(),
            )
                .abi_encode(),
        );
        if epoch.roster_hash != expected_roster_hash {
            return Err(ActivationError::RosterHash);
        }
        let protocol_version = u16::try_from(epoch.protocol_version).map_err(|_| {
            ActivationError::ProtocolVersion {
                expected: FAST_PROTOCOL_VERSION,
                actual: epoch.protocol_version,
            }
        })?;
        EpochRoster::from_finalized_registry(
            ZoneDomain {
                l1_chain_id: epoch.l1_chain_id,
                zone_id: epoch.zone_id,
                chain_id: epoch.zone_chain_id,
                portal: epoch.portal,
                authority_epoch: epoch.epoch,
                roster_hash: epoch.roster_hash,
                protocol_version,
            },
            epoch.members,
        )
        .map_err(|_| ActivationError::RosterHash)?;
        Ok(Self(Arc::new(epoch)))
    }

    pub fn epoch(&self) -> &FinalizedFastEpoch {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ActivationError {
    #[error("T14 fast native compatibility pin mismatch: expected {expected}, got {actual}")]
    NativePin { expected: B256, actual: B256 },
    #[error("T14 is not active at the finalized L1 anchor")]
    T14Inactive,
    #[error("fast epoch activation is not included in finalized L1 state")]
    NotFinalized,
    #[error(
        "fast epoch is not current: finalized current epoch {expected}, evidence epoch {actual}"
    )]
    NotCurrent { expected: u64, actual: u64 },
    #[error("fast epoch is retired and has no remaining ordering authority")]
    Retired,
    #[error("fast protocol version mismatch: expected {expected}, got {actual}")]
    ProtocolVersion { expected: u32, actual: u32 },
    #[error("fast epoch threshold must be two, got {0}")]
    Threshold(u8),
    #[error("fast epoch proof mode and immutable verifier hashes must be enrolled")]
    ProofPolicy,
    #[error("fast epoch must contain exactly three distinct nonzero members")]
    Roster,
    #[error("fast epoch must contain exactly nine distinct nonzero peer Portals")]
    PeerRoster,
    #[error("fast epoch roster hash does not match its members")]
    RosterHash,
}

#[derive(Clone, Copy, Debug)]
struct FinalizedProgress {
    number: u64,
    hash: B256,
    observed_at: Instant,
}

/// Live-only admission clock. Historical replay never calls this gate.
#[derive(Clone, Debug, Default)]
pub struct FastAdmissionClock(Arc<Mutex<Option<FinalizedProgress>>>);

impl FastAdmissionClock {
    /// Record genuine finalized L1 progress. Re-observing the same header does not refresh age.
    pub fn observe_finalized(&self, number: u64, hash: B256) -> Result<(), FreshnessError> {
        let mut progress = self.0.lock().expect("fast admission clock poisoned");
        match *progress {
            Some(current) if number < current.number => Err(FreshnessError::Regressed {
                current: current.number,
                observed: number,
            }),
            Some(current) if number == current.number && hash != current.hash => {
                Err(FreshnessError::ConflictingFinalizedHeader { number })
            }
            Some(current) if number == current.number => Ok(()),
            _ => {
                *progress = Some(FinalizedProgress {
                    number,
                    hash,
                    observed_at: Instant::now(),
                });
                Ok(())
            }
        }
    }

    /// Check proposal admission against local monotonic freshness and wall-clock skew.
    pub fn admit_live(&self, proposed_timestamp_millis: u64) -> Result<(), FreshnessError> {
        let progress = self
            .0
            .lock()
            .expect("fast admission clock poisoned")
            .ok_or(FreshnessError::NoFinalizedAnchor)?;
        let age = progress.observed_at.elapsed();
        if age > MAX_ANCHOR_STALENESS {
            return Err(FreshnessError::StaleAnchor(age));
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| FreshnessError::ClockBeforeEpoch)?;
        let proposed = Duration::from_millis(proposed_timestamp_millis);
        let skew = now.abs_diff(proposed);
        if skew > MAX_LIVE_CLOCK_SKEW {
            return Err(FreshnessError::ClockSkew(skew));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FreshnessError {
    #[error("no finalized L1 anchor has been observed")]
    NoFinalizedAnchor,
    #[error("finalized L1 progress regressed from {current} to {observed}")]
    Regressed { current: u64, observed: u64 },
    #[error("conflicting finalized L1 hashes at height {number}")]
    ConflictingFinalizedHeader { number: u64 },
    #[error("finalized L1 anchor is stale by {0:?}")]
    StaleAnchor(Duration),
    #[error("proposal wall-clock skew is {0:?}")]
    ClockSkew(Duration),
    #[error("local clock is before the Unix epoch")]
    ClockBeforeEpoch,
}

/// Durable signing journal used after committed execution.
pub trait SigningJournal: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    /// Persist and fsync the exact certificate body digest before a signature may escape.
    fn persist_signing_record(
        &self,
        epoch: u64,
        term: u64,
        index: u64,
        body_digest: B256,
        signer: Address,
        signature: SignatureBytes,
    ) -> Result<(), Self::Error>;
}

impl SigningJournal for DurableJournal {
    type Error = JournalError;

    fn persist_signing_record(
        &self,
        _epoch: u64,
        term: u64,
        index: u64,
        body_digest: B256,
        signer: Address,
        signature: SignatureBytes,
    ) -> Result<(), Self::Error> {
        Self::persist_signing_record(
            self,
            SigningRecord {
                digest: body_digest,
                signer,
                signature,
                log_term: term,
                log_index: index,
            },
        )?;
        Ok(())
    }
}

/// Reconstruct and sign an outcome locally behind roster, epoch, commit, and durability fences.
pub fn sign_committed_outcome<J, R, F>(
    journal: &J,
    activation: &FastActivation,
    commit: &RaftCommit,
    signer: Address,
    reconstruct: R,
    sign: F,
) -> Result<SignatureBytes, SigningError<J::Error>>
where
    J: SigningJournal,
    R: FnOnce(&RaftCommit) -> CertificateBody,
    F: FnOnce(B256) -> SignatureBytes,
{
    let epoch = activation.epoch();
    if !epoch.members.contains(&signer) {
        return Err(SigningError::SignerNotInRoster(signer));
    }
    let body = reconstruct(commit);
    let protocol_version =
        u16::try_from(epoch.protocol_version).map_err(|_| SigningError::InvalidActivation)?;
    let domain = ZoneDomain {
        l1_chain_id: epoch.l1_chain_id,
        zone_id: epoch.zone_id,
        chain_id: epoch.zone_chain_id,
        portal: epoch.portal,
        authority_epoch: epoch.epoch,
        roster_hash: epoch.roster_hash,
        protocol_version,
    };
    if body.zone != domain
        || body.log_term != commit.term
        || body.log_index != commit.index
        || body.block_height != commit.block.block_height
        || body.block_hash != commit.block.block_hash
        || body.state_root != commit.block.state_root
    {
        return Err(SigningError::OutcomeDoesNotMatchCommit);
    }
    let roster = EpochRoster::from_finalized_registry(domain, epoch.members)
        .map_err(|_| SigningError::InvalidActivation)?;
    let verifier = QuorumVerifier::new(roster);
    let unsigned = OutcomeCertificate {
        body,
        signatures: [SignatureBytes([0; 65]); 2],
    };
    let body_digest = verifier.outcome_digest(&unsigned);
    // Persist the exact locally derived signature before allowing it to escape this call.
    let signature = sign(body_digest);
    journal
        .persist_signing_record(
            activation.epoch().epoch,
            commit.term,
            commit.index,
            body_digest,
            signer,
            signature,
        )
        .map_err(SigningError::Persistence)?;
    Ok(signature)
}

#[derive(Debug, thiserror::Error)]
pub enum SigningError<E: std::error::Error + 'static> {
    #[error("local signer {0} is not in the finalized epoch roster")]
    SignerNotInRoster(Address),
    #[error("finalized activation evidence is internally inconsistent")]
    InvalidActivation,
    #[error("locally reconstructed outcome does not match the committed term/index/result")]
    OutcomeDoesNotMatchCommit,
    #[error("failed to durably record outcome signature: {0}")]
    Persistence(E),
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    fn valid_activation_evidence(closed: bool, retired: bool) -> FinalizedT14Capability {
        let members = [
            Address::repeat_byte(1),
            Address::repeat_byte(2),
            Address::repeat_byte(3),
        ];
        let peer_portals = std::array::from_fn(|index| Address::with_last_byte(4 + index as u8));
        let portal = Address::repeat_byte(20);
        let epoch_number = 7;
        let roster_hash = keccak256(
            (
                keccak256("TEMPO_ZONE_FAST_ROSTER_T14_V1"),
                portal,
                epoch_number,
                FAST_PROTOCOL_VERSION,
                U256::from(2),
                U256::from(1),
                B256::repeat_byte(21),
                B256::repeat_byte(22),
                members.to_vec(),
                peer_portals.to_vec(),
            )
                .abi_encode(),
        );
        FinalizedT14Capability {
            epoch: FinalizedFastEpoch {
                l1_chain_id: 1,
                portal,
                zone_id: 1,
                zone_chain_id: 101,
                epoch: epoch_number,
                protocol_version: FAST_PROTOCOL_VERSION,
                threshold: 2,
                proof_mode: 1,
                expected_verifier_code_hash: B256::repeat_byte(21),
                expected_verifier_config_hash: B256::repeat_byte(22),
                members,
                peer_portals,
                roster_hash,
                finalized_l1_block: 10,
            },
            native_pin: t14_fast_protocol_native_pin(),
            t14_active_at_anchor: true,
            current_epoch: epoch_number,
            activated_at_l1_block: 9,
            closed,
            retired,
        }
    }

    #[test]
    fn activation_rejects_wrong_t14_native_pin() {
        let epoch = FinalizedFastEpoch {
            l1_chain_id: 1,
            portal: Address::repeat_byte(1),
            zone_id: 1,
            zone_chain_id: 101,
            epoch: 1,
            protocol_version: FAST_PROTOCOL_VERSION,
            threshold: 2,
            proof_mode: 1,
            expected_verifier_code_hash: B256::repeat_byte(21),
            expected_verifier_config_hash: B256::repeat_byte(22),
            members: [
                Address::repeat_byte(1),
                Address::repeat_byte(2),
                Address::repeat_byte(3),
            ],
            peer_portals: [
                Address::repeat_byte(4),
                Address::repeat_byte(5),
                Address::repeat_byte(6),
                Address::repeat_byte(7),
                Address::repeat_byte(8),
                Address::repeat_byte(9),
                Address::repeat_byte(10),
                Address::repeat_byte(11),
                Address::repeat_byte(12),
            ],
            roster_hash: B256::ZERO,
            finalized_l1_block: 1,
        };
        let evidence = FinalizedT14Capability {
            current_epoch: epoch.epoch,
            activated_at_l1_block: epoch.finalized_l1_block,
            epoch,
            native_pin: B256::ZERO,
            t14_active_at_anchor: true,
            closed: false,
            retired: false,
        };
        assert!(matches!(
            FastActivation::from_finalized_epoch(evidence),
            Err(ActivationError::NativePin { .. })
        ));
    }

    #[test]
    fn closed_unretired_epoch_retains_drain_authority_but_retired_epoch_does_not() {
        assert!(
            FastActivation::from_finalized_epoch(valid_activation_evidence(true, false)).is_ok()
        );
        assert_eq!(
            FastActivation::from_finalized_epoch(valid_activation_evidence(true, true))
                .unwrap_err(),
            ActivationError::Retired
        );
    }

    #[test]
    fn activation_binds_proof_mode_and_both_verifier_hashes() {
        for field in 0..3 {
            let mut evidence = valid_activation_evidence(false, false);
            match field {
                0 => evidence.epoch.proof_mode = 2,
                1 => evidence.epoch.expected_verifier_code_hash = B256::repeat_byte(23),
                _ => evidence.epoch.expected_verifier_config_hash = B256::repeat_byte(24),
            }
            assert_eq!(
                FastActivation::from_finalized_epoch(evidence).unwrap_err(),
                ActivationError::RosterHash
            );
        }
        let mut evidence = valid_activation_evidence(false, false);
        evidence.epoch.proof_mode = 0;
        assert_eq!(
            FastActivation::from_finalized_epoch(evidence).unwrap_err(),
            ActivationError::ProofPolicy
        );
    }

    #[test]
    fn committed_replay_material_survives_journal_reopen() {
        let directory = tempdir().unwrap();
        let input = ReplicatedBlockInput {
            epoch: 4,
            parent_hash: B256::repeat_byte(1),
            block_input: Bytes::from_static(b"attributes"),
            transactions: vec![Bytes::from_static(b"opening"), Bytes::from_static(b"user")],
            l1_inputs: Bytes::from_static(b"anchor"),
            replay_witness: Bytes::from_static(b"witness"),
        };
        let committed = CommittedPrefix {
            term: 7,
            index: 11,
            block_height: 9,
            block_hash: B256::repeat_byte(2),
            state_root: B256::repeat_byte(3),
        };
        {
            let journal = DurableJournal::open(directory.path()).unwrap();
            journal.persist_committed(&input, &committed).unwrap();
        }
        let reopened = DurableJournal::open(directory.path()).unwrap();
        assert_eq!(reopened.committed_prefix().unwrap(), Some(committed));
        let replay = reopened.replicated_blocks_from(0).unwrap();
        assert_eq!(
            replay[0].transactions,
            vec![b"opening".to_vec(), b"user".to_vec()]
        );
        assert_eq!(replay[0].witness, b"witness");
    }
}
