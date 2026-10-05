//! Maintained-Raft adapter and activation fence for instant Zone execution.
//!
//! This module deliberately contains no CLI switch. Fast execution is allowed only after a
//! finalized L1 fast-epoch registry has been imported and the pinned Tempo/native dependency
//! advertises the matching capability. The dependency pinned by this source revision has no such
//! registry, so [`FastActivation::from_finalized_epoch`] fails closed.

use std::{
    collections::BTreeSet,
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use alloy_primitives::{Address, B256, Bytes, keccak256};
use openraft::{
    BasicNode, Raft,
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
use zone_primitives::fast_transfer::{
    OutcomeCertificate, SignatureBytes, TransferIntent, ZoneDomain,
};

/// Protocol version implemented by the dormant source components.
pub const FAST_PROTOCOL_VERSION: u32 = 1;
pub const MAX_ANCHOR_STALENESS: Duration = Duration::from_secs(2);
pub const MAX_LIVE_CLOCK_SKEW: Duration = Duration::from_millis(100);

/// The pinned Tempo/native revision does not yet expose a finalized fast-epoch registry.
///
/// This constant must only become `true` in the same change that pins and validates the matching
/// L1 factory, portal bytecode, native precompiles, proof format, and RPC clients.
const PINNED_DEPENDENCIES_HAVE_FAST_EPOCH_REGISTRY: bool = false;

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

impl ReplicatedBlockInput {
    /// Stable digest used to reject a state-machine response for different input bytes.
    pub fn digest(&self) -> B256 {
        let encoded = bincode::serialize(self)
            .expect("ReplicatedBlockInput contains only infallibly serializable values");
        keccak256(encoded)
    }
}

impl fmt::Display for ReplicatedBlockInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "block(epoch={},digest={})", self.epoch, self.digest())
    }
}

/// State-machine response produced only after committed execution and durable replay storage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

type TransportFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, TransportError>> + Send + 'a>>;

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
            transactions: input.transactions.iter().map(Bytes::to_vec).collect(),
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
    pub members: [Address; 3],
    pub roster_hash: B256,
    pub finalized_l1_block: u64,
}

/// Capability token. It cannot be constructed from configuration or a local manifest.
#[derive(Clone, Debug)]
pub struct FastActivation(Arc<FinalizedFastEpoch>);

impl FastActivation {
    /// Validate finalized authority evidence and dependency compatibility.
    pub fn from_finalized_epoch(epoch: FinalizedFastEpoch) -> Result<Self, ActivationError> {
        if !PINNED_DEPENDENCIES_HAVE_FAST_EPOCH_REGISTRY {
            return Err(ActivationError::UnsupportedPinnedDependencies);
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
        let distinct = epoch.members.into_iter().collect::<BTreeSet<_>>();
        if distinct.len() != 3 || distinct.contains(&Address::ZERO) {
            return Err(ActivationError::Roster);
        }
        let protocol_version = u16::try_from(epoch.protocol_version).map_err(|_| {
            ActivationError::ProtocolVersion {
                expected: FAST_PROTOCOL_VERSION,
                actual: epoch.protocol_version,
            }
        })?;
        EpochRoster::new(
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
    #[error("pinned Tempo/native dependencies do not provide the finalized fast-epoch registry")]
    UnsupportedPinnedDependencies,
    #[error("fast protocol version mismatch: expected {expected}, got {actual}")]
    ProtocolVersion { expected: u32, actual: u32 },
    #[error("fast epoch threshold must be two, got {0}")]
    Threshold(u8),
    #[error("fast epoch must contain exactly three distinct nonzero members")]
    Roster,
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
        DurableJournal::persist_signing_record(
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

/// Fence a local signing function behind a durable committed-log record.
pub fn sign_committed_outcome<J, F>(
    journal: &J,
    activation: &FastActivation,
    commit: &RaftCommit,
    body_digest: B256,
    signer: Address,
    sign: F,
) -> Result<SignatureBytes, SigningError<J::Error>>
where
    J: SigningJournal,
    F: FnOnce(B256) -> SignatureBytes,
{
    // Construct locally, then persist the exact bytes before allowing them to escape this call.
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
    #[error("failed to durably record outcome signature: {0}")]
    Persistence(E),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_is_fenced_until_dependencies_are_pinned() {
        let epoch = FinalizedFastEpoch {
            l1_chain_id: 1,
            portal: Address::repeat_byte(1),
            zone_id: 1,
            zone_chain_id: 101,
            epoch: 1,
            protocol_version: FAST_PROTOCOL_VERSION,
            threshold: 2,
            members: [
                Address::repeat_byte(1),
                Address::repeat_byte(2),
                Address::repeat_byte(3),
            ],
            roster_hash: B256::ZERO,
            finalized_l1_block: 1,
        };
        assert_eq!(
            FastActivation::from_finalized_epoch(epoch).unwrap_err(),
            ActivationError::UnsupportedPinnedDependencies
        );
    }
}
