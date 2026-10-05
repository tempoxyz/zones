//! Crash-recoverable C5 distributed epoch drain and checkpoint production.
//!
//! The driver deliberately separates deterministic protocol construction from I/O adapters.  Its
//! adapters are production boundaries, not authorities: every signature body is rebuilt from the
//! fsynced committed prefix, every provider action is journaled with a nonce before submission,
//! and registry/transport observations are re-read after ambiguous failures.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::Arc,
};

use alloy_consensus::crypto::secp256k1::recover_signer;
use alloy_primitives::{Address, B256, Signature, U256, keccak256};
use zone_fast_transfer::{
    DurableJournal, EpochRoster, QuorumVerifier, SigningRecord,
    drain::{
        BarrierInventory, BarrierResolutionInventory, CheckpointImage, CommittedDisposition,
        CommittedSourceLock, CommittedTerminal, DRAIN_PEER_COUNT, DrainCertificate, DrainError,
        barriers_hash,
    },
};
use zone_primitives::fast_transfer::{
    FastBarrierResolution, FastBarrierStatement, FastCheckpointStatement, SignatureBytes,
};

const FINAL_SETTLEMENT_DOMAIN: &[u8] = b"TEMPO_ZONE_FAST_FINAL_SETTLEMENT_T14_V1";
const BARRIERS_DOMAIN: &[u8] = b"TEMPO_ZONE_FAST_BARRIERS_T14_V1";

pub type DrainFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One finalized peer in exact Tempo registry order.
#[derive(Clone, Debug)]
pub struct DrainPeer {
    pub zone_id: u32,
    pub roster: EpochRoster,
    /// Finalized closure hash of this destination epoch.
    pub closure_hash: B256,
}

/// Immutable old/next epoch configuration loaded from finalized Tempo state.
#[derive(Clone, Debug)]
pub struct FastDrainConfig {
    pub local_roster: EpochRoster,
    pub local_member: Address,
    pub peers: [DrainPeer; DRAIN_PEER_COUNT],
    pub next_roster: EpochRoster,
}

impl FastDrainConfig {
    pub fn validate(&self) -> Result<(), FastDrainError> {
        if !self.local_roster.members.contains(&self.local_member)
            || self.local_roster.domain.authority_epoch == 0
            || self.next_roster.domain.portal != self.local_roster.domain.portal
            || self.next_roster.domain.authority_epoch <= self.local_roster.domain.authority_epoch
            || self.next_roster.domain.l1_chain_id != self.local_roster.domain.l1_chain_id
            || !self.next_roster.members.contains(&self.local_member)
        {
            return Err(FastDrainError::InvalidConfiguration);
        }
        let mut portals = BTreeSet::new();
        let mut zones = BTreeSet::new();
        for peer in &self.peers {
            if peer.zone_id != peer.roster.domain.zone_id
                || peer.roster.domain.portal == self.local_roster.domain.portal
                || peer.roster.domain.l1_chain_id != self.local_roster.domain.l1_chain_id
                || peer.closure_hash.is_zero()
                || !portals.insert(peer.roster.domain.portal)
                || !zones.insert(peer.zone_id)
            {
                return Err(FastDrainError::InvalidConfiguration);
            }
        }
        Ok(())
    }
}

/// Exact committed coordinates of the no-new-lock barrier or checkpoint entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommittedDrainPoint {
    pub log_term: u64,
    pub log_index: u64,
    pub block_height: u64,
    pub block_hash: B256,
    pub state_root: B256,
    pub imported_anchor_number: u64,
    pub imported_anchor_hash: B256,
}

impl CommittedDrainPoint {
    fn validate(self) -> Result<Self, FastDrainError> {
        if self.log_index == 0
            || self.block_hash.is_zero()
            || self.state_root.is_zero()
            || self.imported_anchor_hash.is_zero()
        {
            return Err(FastDrainError::InvalidCommittedPoint);
        }
        Ok(self)
    }
}

/// Exact source journal projection through one durable applied prefix.
#[derive(Clone, Debug, Default)]
pub struct SourceDrainSnapshot {
    pub locks: Vec<CommittedSourceLock>,
    pub lock_evidence: BTreeMap<B256, SourceLockEvidence>,
    pub terminals: BTreeMap<B256, CommittedTerminal>,
    pub dispositions: BTreeMap<B256, CommittedDisposition>,
    pub policy_blocked: BTreeSet<B256>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceLockEvidence {
    pub native_calldata: Vec<u8>,
    pub replay_witness: Vec<u8>,
    pub canonical_receipt: Vec<u8>,
}

/// Old accepted prefix read from finalized imported L1 plus committed local execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FinalAcceptedPrefix {
    pub zone_height: u64,
    pub block_hash: B256,
    pub state_root: B256,
    pub withdrawal_batch_index: u64,
    pub imported_anchor_number: u64,
    pub imported_anchor_hash: B256,
}

/// Purpose prevents a signature recovered after restart from being replayed for another phase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DrainSigningPurpose {
    Barrier,
    Resolution,
    FinalSettlement,
    Checkpoint,
}

/// Durable workflow keys. Implementations use these keys as idempotency identities.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum DrainObjectKey {
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedRegistryAction {
    pub action_id: B256,
    pub signer: Address,
    pub nonce: u64,
    pub target: Address,
    pub calldata: Vec<u8>,
    pub submission_hashes: Vec<B256>,
}

/// Fsync-backed drain state. All mutating calls must complete `fsync` before returning.
pub trait FastDrainJournal: Send + Sync + 'static {
    fn persist_closure(
        &self,
        closure_hash: B256,
        point: CommittedDrainPoint,
    ) -> Result<(), FastDrainError>;
    fn closure(&self, epoch: u64) -> Result<Option<(B256, CommittedDrainPoint)>, FastDrainError>;
    fn persist_barrier(
        &self,
        key: &DrainObjectKey,
        inventory: &BarrierInventory,
    ) -> Result<(), FastDrainError>;
    fn barrier(&self, key: &DrainObjectKey) -> Result<Option<BarrierInventory>, FastDrainError>;
    fn persist_resolution(
        &self,
        key: &DrainObjectKey,
        resolution: &BarrierResolutionInventory,
    ) -> Result<(), FastDrainError>;
    fn resolution(
        &self,
        key: &DrainObjectKey,
    ) -> Result<Option<BarrierResolutionInventory>, FastDrainError>;
    fn persist_certificate(
        &self,
        purpose: DrainSigningPurpose,
        key: &DrainObjectKey,
        certificate: DrainCertificate,
    ) -> Result<(), FastDrainError>;
    fn certificate(
        &self,
        purpose: DrainSigningPurpose,
        key: &DrainObjectKey,
    ) -> Result<Option<DrainCertificate>, FastDrainError>;
    fn mark_lock_resolved(
        &self,
        key: &DrainObjectKey,
        transfer_id: B256,
    ) -> Result<(), FastDrainError>;
    fn lock_resolved(
        &self,
        key: &DrainObjectKey,
        transfer_id: B256,
    ) -> Result<bool, FastDrainError>;
    fn prepare_registry_action(
        &self,
        key: &DrainObjectKey,
        signer: Address,
        target: Address,
        calldata: Vec<u8>,
    ) -> Result<PreparedRegistryAction, FastDrainError>;
    fn record_registry_submission(
        &self,
        action_id: B256,
        transaction_hash: B256,
    ) -> Result<(), FastDrainError>;
    fn complete_registry_action(
        &self,
        action_id: B256,
        transaction_hash: B256,
    ) -> Result<(), FastDrainError>;
    fn persist_checkpoint_image(
        &self,
        key: &DrainObjectKey,
        image: &CheckpointImage,
    ) -> Result<(), FastDrainError>;
    fn checkpoint_image(
        &self,
        key: &DrainObjectKey,
    ) -> Result<Option<CheckpointImage>, FastDrainError>;
}

/// Same-Raft-log committed view. Implementations must enumerate from the applied image, never the
/// executor's speculative sidecar.
pub trait FastDrainCommittedState: Send + Sync + 'static {
    fn commit_no_new_locks<'a>(
        &'a self,
        epoch: u64,
        closure_hash: B256,
    ) -> DrainFuture<'a, Result<CommittedDrainPoint, FastDrainError>>;
    fn source_snapshot(
        &self,
        point: CommittedDrainPoint,
        destination: &EpochRoster,
    ) -> Result<SourceDrainSnapshot, FastDrainError>;
    /// Prove the destination closure was imported by the same committed entry/prefix used for
    /// the source inventory; an out-of-band finalized-provider read is insufficient.
    fn assert_imported_destination_closure(
        &self,
        point: CommittedDrainPoint,
        destination: &EpochRoster,
        closure_hash: B256,
    ) -> Result<(), FastDrainError>;
    fn current_source_snapshot(
        &self,
        point: CommittedDrainPoint,
        destination: &EpochRoster,
    ) -> Result<(SourceDrainSnapshot, CommittedDrainPoint), FastDrainError>;
    fn commit_imported_barrier<'a>(
        &'a self,
        inventory: &'a BarrierInventory,
    ) -> DrainFuture<'a, Result<CommittedDrainPoint, FastDrainError>>;
    fn assert_signing_body(
        &self,
        purpose: DrainSigningPurpose,
        key: &DrainObjectKey,
        digest: B256,
        point: CommittedDrainPoint,
    ) -> Result<(), FastDrainError>;
    fn final_accepted_prefix(
        &self,
    ) -> Result<(FinalAcceptedPrefix, CommittedDrainPoint), FastDrainError>;
    fn checkpoint_image(
        &self,
        settlement_hash: B256,
        next: &EpochRoster,
    ) -> Result<CheckpointImage, FastDrainError>;
    fn install_checkpoint<'a>(
        &'a self,
        image: &'a CheckpointImage,
    ) -> DrainFuture<'a, Result<CommittedDrainPoint, FastDrainError>>;
    fn installed_checkpoint_hash(&self) -> Result<B256, FastDrainError>;
}

/// Local member key and private authenticated calls to the other two old/next roster members.
pub trait FastDrainCommittee: Send + Sync + 'static {
    fn sign_local<'a>(
        &'a self,
        purpose: DrainSigningPurpose,
        key: &'a DrainObjectKey,
        digest: B256,
        point: CommittedDrainPoint,
    ) -> DrainFuture<'a, Result<SignatureBytes, FastDrainError>>;
    fn request_signature<'a>(
        &'a self,
        member: Address,
        purpose: DrainSigningPurpose,
        key: &'a DrainObjectKey,
        digest: B256,
        point: CommittedDrainPoint,
    ) -> DrainFuture<'a, Result<SignatureBytes, FastDrainError>>;
}

/// Direct private authenticated Zone-to-Zone extension. There is no broadcast operation.
pub trait FastDrainTransport: Send + Sync + 'static {
    fn send_barrier<'a>(
        &'a self,
        peer: &'a DrainPeer,
        inventory: &'a BarrierInventory,
        certificate: DrainCertificate,
    ) -> DrainFuture<'a, Result<(), FastDrainError>>;
    fn send_resolution<'a>(
        &'a self,
        peer: &'a DrainPeer,
        inventory: &'a BarrierResolutionInventory,
        certificate: DrainCertificate,
    ) -> DrainFuture<'a, Result<(), FastDrainError>>;
}

/// Native resolver for delayed old-epoch locks. It must be idempotent by transfer ID and wait for
/// the resolving transaction to enter the fsynced committed prefix.
pub trait FastDrainNative: Send + Sync + 'static {
    fn resolve_old_lock<'a>(
        &'a self,
        lock: &'a CommittedSourceLock,
    ) -> DrainFuture<'a, Result<(), FastDrainError>>;
}

/// Tempo provider/signer adapter. `calldata` is exact typed ABI calldata produced by this adapter;
/// `submit_prepared` must use the already-persisted nonce and must not allocate another nonce.
pub trait FastDrainRegistry: Send + Sync + 'static {
    fn portal(&self) -> Address;
    fn target(&self) -> Address;
    fn encode_record_barrier(
        &self,
        statement: &FastBarrierStatement,
        certificate: DrainCertificate,
    ) -> Result<Vec<u8>, FastDrainError>;
    fn encode_finalize_barrier(
        &self,
        epoch: u64,
        peer: Address,
        resolution: &FastBarrierResolution,
        certificate: DrainCertificate,
    ) -> Result<Vec<u8>, FastDrainError>;
    fn encode_final_settlement(
        &self,
        epoch: u64,
        prefix: FinalAcceptedPrefix,
        certificate: DrainCertificate,
    ) -> Result<Vec<u8>, FastDrainError>;
    fn encode_install_checkpoint(
        &self,
        statement: &FastCheckpointStatement,
        next_members: [Address; 3],
        certificate: DrainCertificate,
    ) -> Result<Vec<u8>, FastDrainError>;
    fn submit_prepared<'a>(
        &'a self,
        action: &'a PreparedRegistryAction,
    ) -> DrainFuture<'a, Result<B256, FastDrainError>>;
    fn transaction_committed<'a>(
        &'a self,
        transaction_hash: B256,
    ) -> DrainFuture<'a, Result<bool, FastDrainError>>;
    fn barrier_recorded<'a>(
        &'a self,
        epoch: u64,
        peer: Address,
        barrier_hash: B256,
    ) -> DrainFuture<'a, Result<bool, FastDrainError>>;
    fn barrier_finalized<'a>(
        &'a self,
        epoch: u64,
        peer: Address,
        resolution_hash: B256,
    ) -> DrainFuture<'a, Result<bool, FastDrainError>>;
    fn final_settlement_hash<'a>(
        &'a self,
        epoch: u64,
    ) -> DrainFuture<'a, Result<Option<B256>, FastDrainError>>;
    fn checkpoint_hash<'a>(
        &'a self,
        epoch: u64,
    ) -> DrainFuture<'a, Result<Option<B256>, FastDrainError>>;
}

pub struct FastDrainService {
    config: FastDrainConfig,
    journal: Arc<dyn FastDrainJournal>,
    signing_journal: Arc<DurableJournal>,
    committed: Arc<dyn FastDrainCommittedState>,
    committee: Arc<dyn FastDrainCommittee>,
    transport: Arc<dyn FastDrainTransport>,
    native: Arc<dyn FastDrainNative>,
    registry: Arc<dyn FastDrainRegistry>,
}

impl FastDrainService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: FastDrainConfig,
        journal: Arc<dyn FastDrainJournal>,
        signing_journal: Arc<DurableJournal>,
        committed: Arc<dyn FastDrainCommittedState>,
        committee: Arc<dyn FastDrainCommittee>,
        transport: Arc<dyn FastDrainTransport>,
        native: Arc<dyn FastDrainNative>,
        registry: Arc<dyn FastDrainRegistry>,
    ) -> Result<Self, FastDrainError> {
        config.validate()?;
        if registry.portal() != config.local_roster.domain.portal {
            return Err(FastDrainError::InvalidConfiguration);
        }
        Ok(Self {
            config,
            journal,
            signing_journal,
            committed,
            committee,
            transport,
            native,
            registry,
        })
    }

    /// Commit admission closure through Raft, then create, certify, register and directly deliver
    /// all nine source barriers. Restart reuses every durable object and recollects only a missing
    /// second signature over the identical digest.
    pub async fn close_and_publish(&self, closure_hash: B256) -> Result<(), FastDrainError> {
        if closure_hash.is_zero() {
            return Err(FastDrainError::InvalidClosure);
        }
        let epoch = self.config.local_roster.domain.authority_epoch;
        let point = match self.journal.closure(epoch)? {
            Some((existing, point)) if existing == closure_hash => point.validate()?,
            Some(_) => return Err(FastDrainError::ConflictingClosure),
            None => {
                let point = self
                    .committed
                    .commit_no_new_locks(epoch, closure_hash)
                    .await?
                    .validate()?;
                self.journal.persist_closure(closure_hash, point)?;
                point
            }
        };
        for peer in &self.config.peers {
            let key = DrainObjectKey::OutboundBarrier {
                epoch,
                destination: peer.roster.domain.portal,
            };
            let inventory = match self.journal.barrier(&key)? {
                Some(inventory) => inventory,
                None => {
                    self.committed.assert_imported_destination_closure(
                        point,
                        &peer.roster,
                        peer.closure_hash,
                    )?;
                    let snapshot = self.committed.source_snapshot(point, &peer.roster)?;
                    validate_source_evidence(&snapshot)?;
                    let inventory = BarrierInventory::build(
                        self.config.local_roster.domain.l1_chain_id,
                        peer.roster.domain.portal,
                        peer.roster.domain.authority_epoch,
                        peer.closure_hash,
                        self.config.local_roster.domain.portal,
                        epoch,
                        point.imported_anchor_number,
                        point.imported_anchor_hash,
                        point.log_term,
                        point.log_index,
                        point.block_height,
                        point.block_hash,
                        point.state_root,
                        snapshot.locks,
                        &snapshot.terminals,
                        &snapshot.dispositions,
                        &snapshot.policy_blocked,
                    )?;
                    inventory.verify_complete()?;
                    self.journal.persist_barrier(&key, &inventory)?;
                    inventory
                }
            };
            let digest = inventory.statement.registry_digest(inventory.l1_chain_id);
            let certificate = self
                .collect_old_certificate(DrainSigningPurpose::Barrier, &key, digest, point)
                .await?;
            QuorumVerifier::new(self.config.local_roster.clone())
                .verify_source_barrier(&inventory.statement, &certificate.signatures)
                .map_err(|error| FastDrainError::Certificate(error.to_string()))?;
            self.transport
                .send_barrier(peer, &inventory, certificate)
                .await?;
        }
        Ok(())
    }

    /// Durable destination ingestion. The full signed list is committed through Raft before any
    /// old lock is resolved; every included lock is retried idempotently, including delayed ones.
    pub async fn receive_barrier(
        &self,
        source: &DrainPeer,
        inventory: BarrierInventory,
        certificate: DrainCertificate,
        expected_closure_hash: B256,
    ) -> Result<(), FastDrainError> {
        let epoch = self.config.local_roster.domain.authority_epoch;
        if inventory.statement.destination_portal != self.config.local_roster.domain.portal
            || inventory.statement.destination_epoch != epoch
            || inventory.statement.source_portal != source.roster.domain.portal
            || inventory.statement.closure_hash != expected_closure_hash
            || certificate.digest != inventory.statement.registry_digest(inventory.l1_chain_id)
        {
            return Err(FastDrainError::WrongBarrier);
        }
        inventory.verify_complete()?;
        QuorumVerifier::new(source.roster.clone())
            .verify_source_barrier(&inventory.statement, &certificate.signatures)
            .map_err(|error| FastDrainError::Certificate(error.to_string()))?;
        let key = DrainObjectKey::InboundBarrier {
            epoch,
            source: source.roster.domain.portal,
        };
        match self.journal.barrier(&key)? {
            Some(existing) if existing == inventory => {}
            Some(_) => return Err(FastDrainError::ConflictingBarrier),
            None => {
                self.committed
                    .commit_imported_barrier(&inventory)
                    .await?
                    .validate()?;
                self.journal.persist_barrier(&key, &inventory)?;
            }
        };
        self.journal
            .persist_certificate(DrainSigningPurpose::Barrier, &key, certificate)?;
        self.submit_barrier(&key, &inventory, certificate).await?;
        for entry in &inventory.locks {
            let transfer_id = entry.lock.transfer_id();
            if !self.journal.lock_resolved(&key, transfer_id)? {
                self.native.resolve_old_lock(&entry.lock).await?;
                self.journal.mark_lock_resolved(&key, transfer_id)?;
            }
        }
        Ok(())
    }

    /// Produce a resolution only from current committed terminal and source-disposition records.
    /// A policy block or unavailable evidence leaves the epoch unfinished.
    pub async fn finalize_outbound_barrier(&self, peer: &DrainPeer) -> Result<(), FastDrainError> {
        let epoch = self.config.local_roster.domain.authority_epoch;
        let barrier_key = DrainObjectKey::OutboundBarrier {
            epoch,
            destination: peer.roster.domain.portal,
        };
        let barrier = self
            .journal
            .barrier(&barrier_key)?
            .ok_or(FastDrainError::MissingBarrier)?;
        let point = self
            .journal
            .closure(epoch)?
            .ok_or(FastDrainError::MissingClosure)?
            .1;
        let (snapshot, resolution_point) = self
            .committed
            .current_source_snapshot(point, &peer.roster)?;
        validate_source_evidence(&snapshot)?;
        let key = DrainObjectKey::Resolution {
            destination_epoch: peer.roster.domain.authority_epoch,
            destination: peer.roster.domain.portal,
            source: self.config.local_roster.domain.portal,
        };
        let resolution = match self.journal.resolution(&key)? {
            Some(resolution) => resolution,
            None => {
                let resolution = BarrierResolutionInventory::build(
                    &barrier,
                    &snapshot.terminals,
                    &snapshot.dispositions,
                    &snapshot.policy_blocked,
                )?;
                resolution.verify(&barrier)?;
                self.journal.persist_resolution(&key, &resolution)?;
                resolution
            }
        };
        let digest = resolution.resolution.registry_digest(
            self.config.local_roster.domain.l1_chain_id,
            peer.roster.domain.portal,
            peer.roster.domain.authority_epoch,
            self.config.local_roster.domain.portal,
        );
        let certificate = self
            .collect_old_certificate(
                DrainSigningPurpose::Resolution,
                &key,
                digest,
                resolution_point,
            )
            .await?;
        QuorumVerifier::new(self.config.local_roster.clone())
            .verify_barrier_resolution(
                peer.roster.domain.portal,
                peer.roster.domain.authority_epoch,
                &resolution.resolution,
                &certificate.signatures,
            )
            .map_err(|error| FastDrainError::Certificate(error.to_string()))?;
        self.transport
            .send_resolution(peer, &resolution, certificate)
            .await?;
        Ok(())
    }

    /// Destination-side resolution ingestion and Tempo finalization. The resolution is accepted
    /// only for the exact previously committed source barrier.
    pub async fn receive_resolution(
        &self,
        source: &DrainPeer,
        resolution: BarrierResolutionInventory,
        certificate: DrainCertificate,
    ) -> Result<(), FastDrainError> {
        let epoch = self.config.local_roster.domain.authority_epoch;
        let barrier_key = DrainObjectKey::InboundBarrier {
            epoch,
            source: source.roster.domain.portal,
        };
        let barrier = self
            .journal
            .barrier(&barrier_key)?
            .ok_or(FastDrainError::MissingBarrier)?;
        resolution.verify(&barrier)?;
        let digest = resolution.resolution.registry_digest(
            self.config.local_roster.domain.l1_chain_id,
            self.config.local_roster.domain.portal,
            epoch,
            source.roster.domain.portal,
        );
        if certificate.digest != digest {
            return Err(FastDrainError::ConflictingCertificate);
        }
        QuorumVerifier::new(source.roster.clone())
            .verify_barrier_resolution(
                self.config.local_roster.domain.portal,
                epoch,
                &resolution.resolution,
                &certificate.signatures,
            )
            .map_err(|error| FastDrainError::Certificate(error.to_string()))?;
        let key = DrainObjectKey::Resolution {
            destination_epoch: epoch,
            destination: self.config.local_roster.domain.portal,
            source: source.roster.domain.portal,
        };
        match self.journal.resolution(&key)? {
            Some(existing) if existing != resolution => {
                return Err(FastDrainError::ConflictingBarrier);
            }
            Some(_) => {}
            None => self.journal.persist_resolution(&key, &resolution)?,
        }
        self.journal
            .persist_certificate(DrainSigningPurpose::Resolution, &key, certificate)?;
        self.submit_resolution(&key, source, &resolution, certificate)
            .await
    }

    /// Bind all nine exact barrier/resolution hashes and the actual accepted old prefix.
    pub async fn record_final_settlement(&self) -> Result<B256, FastDrainError> {
        let epoch = self.config.local_roster.domain.authority_epoch;
        let mut registry_order = Vec::with_capacity(DRAIN_PEER_COUNT);
        for peer in &self.config.peers {
            let barrier_key = DrainObjectKey::InboundBarrier {
                epoch,
                source: peer.roster.domain.portal,
            };
            let barrier = self
                .journal
                .barrier(&barrier_key)?
                .ok_or(FastDrainError::MissingBarrier)?;
            let resolution_key = DrainObjectKey::Resolution {
                destination_epoch: epoch,
                destination: self.config.local_roster.domain.portal,
                source: peer.roster.domain.portal,
            };
            let resolution = self
                .journal
                .resolution(&resolution_key)?
                .ok_or(FastDrainError::MissingResolution)?;
            resolution.verify(&barrier)?;
            let barrier_hash = barrier.statement.registry_digest(barrier.l1_chain_id);
            let resolution_hash = resolution.resolution.registry_digest(
                self.config.local_roster.domain.l1_chain_id,
                self.config.local_roster.domain.portal,
                epoch,
                peer.roster.domain.portal,
            );
            if !self
                .registry
                .barrier_finalized(epoch, peer.roster.domain.portal, resolution_hash)
                .await?
            {
                return Err(FastDrainError::MissingResolution);
            }
            registry_order.push((peer.roster.domain.portal, barrier_hash, resolution_hash));
        }
        let barriers = barriers_hash(&registry_order)?;
        let (prefix, settlement_point) = self.committed.final_accepted_prefix()?;
        if prefix.block_hash.is_zero()
            || prefix.state_root != settlement_point.state_root
            || prefix.block_hash != settlement_point.block_hash
            || prefix.zone_height != settlement_point.block_height
            || prefix.imported_anchor_number != settlement_point.imported_anchor_number
            || prefix.imported_anchor_hash != settlement_point.imported_anchor_hash
        {
            return Err(FastDrainError::InvalidCommittedPoint);
        }
        let settlement_hash = final_settlement_digest(
            self.config.local_roster.domain.l1_chain_id,
            self.config.local_roster.domain.portal,
            epoch,
            self.config.local_roster.domain.roster_hash,
            self.journal
                .closure(epoch)?
                .ok_or(FastDrainError::MissingClosure)?
                .0,
            prefix,
            barriers,
        );
        let key = DrainObjectKey::FinalSettlement { epoch };
        let certificate = self
            .collect_old_certificate(
                DrainSigningPurpose::FinalSettlement,
                &key,
                settlement_hash,
                settlement_point,
            )
            .await?;
        let calldata = self
            .registry
            .encode_final_settlement(epoch, prefix, certificate)?;
        self.submit_registry_action(&key, calldata).await?;
        let observed = self.registry.final_settlement_hash(epoch).await?;
        if observed != Some(settlement_hash) {
            return Err(FastDrainError::RegistryObservationMismatch);
        }
        Ok(settlement_hash)
    }

    /// Install the exact nonempty checkpoint image before collecting next-roster acknowledgments.
    /// Runtime adapters must compare and replace/reconcile real state; initialization from empty is
    /// not an implementation of `install_checkpoint`.
    pub async fn install_and_ack_checkpoint(
        &self,
        settlement_hash: B256,
    ) -> Result<DrainCertificate, FastDrainError> {
        let old_epoch = self.config.local_roster.domain.authority_epoch;
        let next_epoch = self.config.next_roster.domain.authority_epoch;
        let key = DrainObjectKey::Checkpoint {
            old_epoch,
            next_epoch,
        };
        let image = match self.journal.checkpoint_image(&key)? {
            Some(image) => image,
            None => {
                let image = self
                    .committed
                    .checkpoint_image(settlement_hash, &self.config.next_roster)?;
                image.validate()?;
                self.journal.persist_checkpoint_image(&key, &image)?;
                image
            }
        };
        let point = self
            .committed
            .install_checkpoint(&image)
            .await?
            .validate()?;
        if self.committed.installed_checkpoint_hash()? != image.image_hash()? {
            return Err(FastDrainError::CheckpointInstallationMismatch);
        }
        if point.log_term != image.statement.checkpoint_log_term
            || point.log_index != image.statement.checkpoint_log_index
            || U256::from(point.block_height) != image.statement.checkpoint_height
            || point.block_hash != image.statement.checkpoint_block_hash
            || point.state_root != image.statement.checkpoint_state_root
        {
            return Err(FastDrainError::CheckpointInstallationMismatch);
        }
        let digest = image
            .statement
            .registry_digest(self.config.local_roster.domain.l1_chain_id);
        let certificate = self
            .collect_next_certificate(DrainSigningPurpose::Checkpoint, &key, digest, point)
            .await?;
        QuorumVerifier::verify_next_roster_checkpoint(
            self.config.local_roster.domain.l1_chain_id,
            &image.statement,
            self.config.next_roster.members,
            &certificate.signatures,
        )
        .map_err(|error| FastDrainError::Certificate(error.to_string()))?;
        let calldata = self.registry.encode_install_checkpoint(
            &image.statement,
            self.config.next_roster.members,
            certificate,
        )?;
        self.submit_registry_action(&key, calldata).await?;
        if self.registry.checkpoint_hash(old_epoch).await? != Some(digest) {
            return Err(FastDrainError::RegistryObservationMismatch);
        }
        Ok(certificate)
    }

    async fn collect_old_certificate(
        &self,
        purpose: DrainSigningPurpose,
        key: &DrainObjectKey,
        digest: B256,
        point: CommittedDrainPoint,
    ) -> Result<DrainCertificate, FastDrainError> {
        self.collect_certificate(&self.config.local_roster, purpose, key, digest, point)
            .await
    }

    async fn collect_next_certificate(
        &self,
        purpose: DrainSigningPurpose,
        key: &DrainObjectKey,
        digest: B256,
        point: CommittedDrainPoint,
    ) -> Result<DrainCertificate, FastDrainError> {
        self.collect_certificate(&self.config.next_roster, purpose, key, digest, point)
            .await
    }

    async fn collect_certificate(
        &self,
        roster: &EpochRoster,
        purpose: DrainSigningPurpose,
        key: &DrainObjectKey,
        digest: B256,
        point: CommittedDrainPoint,
    ) -> Result<DrainCertificate, FastDrainError> {
        if let Some(certificate) = self.journal.certificate(purpose, key)? {
            if certificate.digest != digest {
                return Err(FastDrainError::ConflictingCertificate);
            }
            verify_drain_certificate(roster, certificate)?;
            return Ok(certificate);
        }
        self.committed
            .assert_signing_body(purpose, key, digest, point)?;
        let local = match self
            .signing_journal
            .signing_record(digest, self.config.local_member)
            .map_err(|error| FastDrainError::Storage(error.to_string()))?
        {
            Some(record)
                if record.log_term == point.log_term && record.log_index == point.log_index =>
            {
                record.signature
            }
            Some(_) => return Err(FastDrainError::ConflictingCertificate),
            None => {
                let signature = self
                    .committee
                    .sign_local(purpose, key, digest, point)
                    .await?;
                verify_member_signature(digest, signature, self.config.local_member)?;
                self.signing_journal
                    .persist_signing_record(SigningRecord {
                        digest,
                        signer: self.config.local_member,
                        signature,
                        log_term: point.log_term,
                        log_index: point.log_index,
                    })
                    .map_err(|error| FastDrainError::Storage(error.to_string()))?;
                signature
            }
        };
        let mut second = None;
        for member in roster.members {
            if member == self.config.local_member {
                continue;
            }
            if let Some(record) = self
                .signing_journal
                .signing_record(digest, member)
                .map_err(|error| FastDrainError::Storage(error.to_string()))?
            {
                if record.log_term != point.log_term || record.log_index != point.log_index {
                    return Err(FastDrainError::ConflictingCertificate);
                }
                second = Some(record.signature);
                break;
            }
            if let Ok(signature) = self
                .committee
                .request_signature(member, purpose, key, digest, point)
                .await
            {
                if verify_member_signature(digest, signature, member).is_err() {
                    continue;
                }
                self.signing_journal
                    .persist_signing_record(SigningRecord {
                        digest,
                        signer: member,
                        signature,
                        log_term: point.log_term,
                        log_index: point.log_index,
                    })
                    .map_err(|error| FastDrainError::Storage(error.to_string()))?;
                second = Some(signature);
                break;
            }
        }
        let certificate = DrainCertificate {
            digest,
            signatures: [local, second.ok_or(FastDrainError::UnavailableQuorum)?],
        };
        verify_drain_certificate(roster, certificate)?;
        self.journal
            .persist_certificate(purpose, key, certificate)?;
        Ok(certificate)
    }

    async fn submit_barrier(
        &self,
        key: &DrainObjectKey,
        inventory: &BarrierInventory,
        certificate: DrainCertificate,
    ) -> Result<(), FastDrainError> {
        let hash = inventory.statement.registry_digest(inventory.l1_chain_id);
        if self
            .registry
            .barrier_recorded(
                inventory.statement.destination_epoch,
                inventory.statement.source_portal,
                hash,
            )
            .await?
        {
            return Ok(());
        }
        let calldata = self
            .registry
            .encode_record_barrier(&inventory.statement, certificate)?;
        self.submit_registry_action(key, calldata).await?;
        if !self
            .registry
            .barrier_recorded(
                inventory.statement.destination_epoch,
                inventory.statement.source_portal,
                hash,
            )
            .await?
        {
            return Err(FastDrainError::RegistryObservationMismatch);
        }
        Ok(())
    }

    async fn submit_resolution(
        &self,
        key: &DrainObjectKey,
        source: &DrainPeer,
        resolution: &BarrierResolutionInventory,
        certificate: DrainCertificate,
    ) -> Result<(), FastDrainError> {
        let digest = resolution.resolution.registry_digest(
            self.config.local_roster.domain.l1_chain_id,
            self.config.local_roster.domain.portal,
            self.config.local_roster.domain.authority_epoch,
            source.roster.domain.portal,
        );
        if self
            .registry
            .barrier_finalized(
                self.config.local_roster.domain.authority_epoch,
                source.roster.domain.portal,
                digest,
            )
            .await?
        {
            return Ok(());
        }
        let calldata = self.registry.encode_finalize_barrier(
            self.config.local_roster.domain.authority_epoch,
            source.roster.domain.portal,
            &resolution.resolution,
            certificate,
        )?;
        self.submit_registry_action(key, calldata).await?;
        if !self
            .registry
            .barrier_finalized(
                self.config.local_roster.domain.authority_epoch,
                source.roster.domain.portal,
                digest,
            )
            .await?
        {
            return Err(FastDrainError::RegistryObservationMismatch);
        }
        Ok(())
    }

    async fn submit_registry_action(
        &self,
        key: &DrainObjectKey,
        calldata: Vec<u8>,
    ) -> Result<(), FastDrainError> {
        let action = self.journal.prepare_registry_action(
            key,
            self.config.local_member,
            self.registry.target(),
            calldata,
        )?;
        for transaction_hash in action.submission_hashes.iter().rev().copied() {
            if self
                .registry
                .transaction_committed(transaction_hash)
                .await?
            {
                self.journal
                    .complete_registry_action(action.action_id, transaction_hash)?;
                return Ok(());
            }
        }
        let transaction_hash = self.registry.submit_prepared(&action).await?;
        self.journal
            .record_registry_submission(action.action_id, transaction_hash)?;
        if !self
            .registry
            .transaction_committed(transaction_hash)
            .await?
        {
            return Err(FastDrainError::AmbiguousRegistrySubmission);
        }
        self.journal
            .complete_registry_action(action.action_id, transaction_hash)?;
        Ok(())
    }
}

fn validate_source_evidence(snapshot: &SourceDrainSnapshot) -> Result<(), FastDrainError> {
    if snapshot.lock_evidence.len() != snapshot.locks.len() {
        return Err(FastDrainError::IncompleteCommittedHistory);
    }
    for lock in &snapshot.locks {
        let Some(evidence) = snapshot.lock_evidence.get(&lock.transfer_id()) else {
            return Err(FastDrainError::IncompleteCommittedHistory);
        };
        if evidence.native_calldata.is_empty()
            || evidence.replay_witness.is_empty()
            || evidence.canonical_receipt.is_empty()
        {
            return Err(FastDrainError::IncompleteCommittedHistory);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub fn final_settlement_digest(
    l1_chain_id: u64,
    portal: Address,
    epoch: u64,
    roster_hash: B256,
    closure_hash: B256,
    prefix: FinalAcceptedPrefix,
    barriers_hash: B256,
) -> B256 {
    let mut encoded = Vec::with_capacity(10 * 32);
    put_b256(&mut encoded, keccak256(FINAL_SETTLEMENT_DOMAIN));
    put_u256(&mut encoded, U256::from(l1_chain_id));
    put_address(&mut encoded, portal);
    put_u256(&mut encoded, U256::from(epoch));
    put_b256(&mut encoded, roster_hash);
    put_b256(&mut encoded, closure_hash);
    put_u256(&mut encoded, U256::from(prefix.zone_height));
    put_b256(&mut encoded, prefix.block_hash);
    put_u256(&mut encoded, U256::from(prefix.withdrawal_batch_index));
    put_b256(&mut encoded, barriers_hash);
    keccak256(encoded)
}

/// Solidity-compatible registry-order fold used for cross-language tests.
pub fn solidity_barriers_hash(entries: &[(Address, B256, B256); DRAIN_PEER_COUNT]) -> B256 {
    let mut hash = keccak256(BARRIERS_DOMAIN);
    for (portal, barrier, resolution) in entries {
        let mut encoded = Vec::with_capacity(4 * 32);
        put_b256(&mut encoded, hash);
        put_address(&mut encoded, *portal);
        put_b256(&mut encoded, *barrier);
        put_b256(&mut encoded, *resolution);
        hash = keccak256(encoded);
    }
    hash
}

fn put_b256(out: &mut Vec<u8>, value: B256) {
    out.extend_from_slice(value.as_slice());
}
fn put_u256(out: &mut Vec<u8>, value: U256) {
    out.extend_from_slice(&value.to_be_bytes::<32>());
}
fn put_address(out: &mut Vec<u8>, value: Address) {
    out.extend_from_slice(&[0; 12]);
    out.extend_from_slice(value.as_slice());
}

fn verify_member_signature(
    digest: B256,
    signature: SignatureBytes,
    expected: Address,
) -> Result<(), FastDrainError> {
    let signature = Signature::try_from(signature.0.as_slice())
        .map_err(|_| FastDrainError::Certificate("malformed signature".to_owned()))?;
    let recovered = recover_signer(&signature, digest)
        .map_err(|_| FastDrainError::Certificate("unrecoverable signature".to_owned()))?;
    if recovered != expected {
        return Err(FastDrainError::Certificate(
            "signature does not match requested finalized member".to_owned(),
        ));
    }
    Ok(())
}

fn verify_drain_certificate(
    roster: &EpochRoster,
    certificate: DrainCertificate,
) -> Result<(), FastDrainError> {
    let mut recovered = BTreeSet::new();
    for signature in certificate.signatures {
        let signature = Signature::try_from(signature.0.as_slice())
            .map_err(|_| FastDrainError::Certificate("malformed signature".to_owned()))?;
        let signer = recover_signer(&signature, certificate.digest)
            .map_err(|_| FastDrainError::Certificate("unrecoverable signature".to_owned()))?;
        if !roster.members.contains(&signer) || !recovered.insert(signer) {
            return Err(FastDrainError::Certificate(
                "signature is duplicate or outside finalized roster".to_owned(),
            ));
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum FastDrainError {
    #[error("invalid distributed drain configuration")]
    InvalidConfiguration,
    #[error("invalid or zero closure hash")]
    InvalidClosure,
    #[error("a different closure is already committed")]
    ConflictingClosure,
    #[error("invalid committed drain point")]
    InvalidCommittedPoint,
    #[error("committed transfer history is incomplete or compacted")]
    IncompleteCommittedHistory,
    #[error("barrier does not bind this source/destination closure")]
    WrongBarrier,
    #[error("a different barrier is already durable")]
    ConflictingBarrier,
    #[error("a durable drain object conflicts with its idempotency key")]
    ConflictingJournalObject,
    #[error("a prepared registry action conflicts with its durable identity")]
    ConflictingRegistryAction,
    #[error("missing durable source barrier")]
    MissingBarrier,
    #[error("missing durable finalized peer resolution")]
    MissingResolution,
    #[error("missing durable closure")]
    MissingClosure,
    #[error("registry peer order differs from the finalized order")]
    WrongBarrierOrder,
    #[error("durable signature/certificate conflicts with the reconstructed body")]
    ConflictingCertificate,
    #[error("a second honest committee member is unavailable")]
    UnavailableQuorum,
    #[error("checkpoint installation did not reproduce the signed image")]
    CheckpointInstallationMismatch,
    #[error("provider submission is not yet canonically resolved")]
    AmbiguousRegistrySubmission,
    #[error("Tempo registry observation does not match the submitted body")]
    RegistryObservationMismatch,
    #[error("certificate verification failed: {0}")]
    Certificate(String),
    #[error("drain protocol validation failed: {0}")]
    Protocol(#[from] DrainError),
    #[error("drain storage failed: {0}")]
    Storage(String),
    #[error("drain consensus failed: {0}")]
    Consensus(String),
    #[error("drain transport failed: {0}")]
    Transport(String),
    #[error("native drain resolution failed: {0}")]
    Native(String),
    #[error("Tempo registry call failed: {0}")]
    Registry(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use zone_primitives::fast_transfer::ZoneDomain;

    fn roster(zone: u32, portal: u8, epoch: u64, members: [u8; 3]) -> EpochRoster {
        EpochRoster {
            domain: ZoneDomain {
                l1_chain_id: 42,
                zone_id: zone,
                chain_id: 1_000 + u64::from(zone),
                portal: Address::repeat_byte(portal),
                authority_epoch: epoch,
                roster_hash: B256::repeat_byte(portal),
                protocol_version: 1,
            },
            members: members.map(Address::repeat_byte),
        }
    }

    #[test]
    fn solidity_and_core_barrier_folds_match_for_all_nine_records() {
        let entries = std::array::from_fn(|index| {
            (
                Address::repeat_byte((index + 1) as u8),
                B256::repeat_byte((index + 20) as u8),
                B256::repeat_byte((index + 40) as u8),
            )
        });
        assert_eq!(
            solidity_barriers_hash(&entries),
            barriers_hash(&entries).expect("exact nine unique peers")
        );
        let mut mutated = entries;
        mutated[8].2 = B256::repeat_byte(99);
        assert_ne!(
            solidity_barriers_hash(&entries),
            solidity_barriers_hash(&mutated)
        );
    }

    #[test]
    fn final_settlement_digest_binds_every_old_prefix_coordinate() {
        let prefix = FinalAcceptedPrefix {
            zone_height: 55,
            block_hash: B256::repeat_byte(7),
            state_root: B256::repeat_byte(6),
            withdrawal_batch_index: 8,
            imported_anchor_number: 100,
            imported_anchor_hash: B256::repeat_byte(5),
        };
        let digest = final_settlement_digest(
            42,
            Address::repeat_byte(1),
            9,
            B256::repeat_byte(2),
            B256::repeat_byte(3),
            prefix,
            B256::repeat_byte(4),
        );
        for changed in [
            FinalAcceptedPrefix {
                zone_height: 56,
                ..prefix
            },
            FinalAcceptedPrefix {
                block_hash: B256::repeat_byte(8),
                ..prefix
            },
            FinalAcceptedPrefix {
                withdrawal_batch_index: 9,
                ..prefix
            },
        ] {
            assert_ne!(
                digest,
                final_settlement_digest(
                    42,
                    Address::repeat_byte(1),
                    9,
                    B256::repeat_byte(2),
                    B256::repeat_byte(3),
                    changed,
                    B256::repeat_byte(4),
                )
            );
        }
    }

    #[test]
    fn configuration_requires_nine_unique_finalized_destinations() {
        let local = roster(10, 10, 7, [60, 61, 62]);
        let next = roster(10, 10, 8, [60, 63, 64]);
        let peers = std::array::from_fn(|index| DrainPeer {
            zone_id: index as u32 + 20,
            roster: roster(index as u32 + 20, index as u8 + 20, 9, [70, 71, 72]),
            closure_hash: B256::repeat_byte(index as u8 + 1),
        });
        let valid = FastDrainConfig {
            local_roster: local.clone(),
            local_member: Address::repeat_byte(60),
            peers: peers.clone(),
            next_roster: next.clone(),
        };
        valid.validate().expect("nine unique peers");

        let mut duplicate = peers;
        duplicate[8].roster.domain.portal = duplicate[0].roster.domain.portal;
        assert!(matches!(
            FastDrainConfig {
                local_roster: local,
                local_member: Address::repeat_byte(60),
                peers: duplicate,
                next_roster: next,
            }
            .validate(),
            Err(FastDrainError::InvalidConfiguration)
        ));
    }
}
