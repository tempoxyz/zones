//! Deterministic epoch-drain inventories and checkpoint commitments.
//!
//! This module contains no networking and never accepts a caller supplied root.  Roots are
//! reconstructed from bounded, canonically ordered records read from a committed journal.  The
//! node driver owns the durable workflow and transports the resulting bundles.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256, U256, keccak256};
use zone_primitives::fast_transfer::{
    CanonicalEncode, FAST_EMPTY_UNRESOLVED_ROOT, FastBarrierResolution, FastBarrierStatement,
    FastCheckpointStatement, MAX_CERTIFICATE_BYTES, MAX_INTENT_BYTES, OutcomeCertificate,
    SignatureBytes, TransferIntent, TransferOutcome,
};

/// A closed epoch has exactly nine remote Zones.
pub const DRAIN_PEER_COUNT: usize = 9;
/// Matches the protocol-wide unresolved-Zone admission ceiling.
pub const MAX_DRAIN_LOCKS: usize = 10_000;
/// A power-of-two tree containing `MAX_DRAIN_LOCKS` needs at most fourteen siblings.
pub const MAX_DRAIN_PROOF_DEPTH: usize = 14;
/// Upper bound for retained replay commitments in one installed checkpoint.
pub const MAX_CHECKPOINT_REPLAY_BLOCKS: usize = 65_536;
/// Maximum durable encoding accepted for one complete drain object.
pub const MAX_DRAIN_OBJECT_BYTES: usize = 64 * 1024 * 1024;

const LOCK_LEAF_DOMAIN: &[u8] = b"TEMPO_ZONE_FAST_DRAIN_LOCK_LEAF_T14_V1";
const UNRESOLVED_LEAF_DOMAIN: &[u8] = b"TEMPO_ZONE_FAST_DRAIN_UNRESOLVED_LEAF_T14_V1";
const TERMINAL_LEAF_DOMAIN: &[u8] = b"TEMPO_ZONE_FAST_DRAIN_TERMINAL_LEAF_T14_V1";
const DISPOSITION_LEAF_DOMAIN: &[u8] = b"TEMPO_ZONE_FAST_DRAIN_DISPOSITION_LEAF_T14_V1";
const MERKLE_NODE_DOMAIN: &[u8] = b"TEMPO_ZONE_FAST_DRAIN_MERKLE_NODE_T14_V1";
const MERKLE_ROOT_DOMAIN: &[u8] = b"TEMPO_ZONE_FAST_DRAIN_MERKLE_ROOT_T14_V1";
const EMPTY_LOCK_ROOT_DOMAIN: &[u8] = b"TEMPO_ZONE_FAST_DRAIN_EMPTY_LOCKS_T14_V1";
const BARRIERS_DOMAIN: &[u8] = b"TEMPO_ZONE_FAST_BARRIERS_T14_V1";
const CHECKPOINT_IMAGE_DOMAIN: &[u8] = b"TEMPO_ZONE_FAST_CHECKPOINT_IMAGE_T14_V1";
const NATIVE_BARRIER_PROOF_VERSION: u8 = 1;

/// One source lock from the fsynced committed prefix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedSourceLock {
    pub intent: TransferIntent,
    pub lock: OutcomeCertificate,
}

impl CommittedSourceLock {
    fn validate(
        &self,
        source_portal: Address,
        source_epoch: u64,
        destination_portal: Address,
        destination_epoch: u64,
    ) -> Result<(), DrainError> {
        let body = &self.lock.body;
        if self.intent.transfer_id() != body.transfer_id
            || self.intent.intent_hash() != body.intent_hash
            || self.intent.source.portal != source_portal
            || self.intent.source.authority_epoch != source_epoch
            || self.intent.destination.portal != destination_portal
            || self.intent.destination.authority_epoch != destination_epoch
            || body.zone != self.intent.source
            || !matches!(body.outcome, TransferOutcome::Locked { .. })
            || body.log_index == 0
            || body.block_hash.is_zero()
            || body.state_root.is_zero()
        {
            return Err(DrainError::InvalidLock);
        }
        Ok(())
    }

    pub fn transfer_id(&self) -> B256 {
        self.intent.transfer_id()
    }

    fn leaf(&self) -> B256 {
        tagged_hash(
            LOCK_LEAF_DOMAIN,
            &[
                self.transfer_id().as_slice(),
                self.intent.intent_hash().as_slice(),
                &self.lock.body.log_term.to_be_bytes(),
                &self.lock.body.log_index.to_be_bytes(),
                self.lock.body.body_hash().as_slice(),
                &self.lock.canonical_bytes(),
            ],
        )
    }
}

/// Terminal destination outcome for a lock included in the source barrier.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedTerminal {
    pub certificate: OutcomeCertificate,
}

impl CommittedTerminal {
    fn validate(&self, lock: &CommittedSourceLock) -> Result<(), DrainError> {
        let body = &self.certificate.body;
        if body.transfer_id != lock.transfer_id()
            || body.intent_hash != lock.intent.intent_hash()
            || body.zone != lock.intent.destination
            || !matches!(
                body.outcome,
                TransferOutcome::Paid { .. } | TransferOutcome::Rejected { .. }
            )
        {
            return Err(DrainError::InvalidTerminal);
        }
        Ok(())
    }

    fn leaf(&self) -> B256 {
        tagged_hash(
            TERMINAL_LEAF_DOMAIN,
            &[
                self.certificate.body.transfer_id.as_slice(),
                self.certificate.body.body_hash().as_slice(),
                &self.certificate.canonical_bytes(),
            ],
        )
    }
}

/// Terminal source disposition completing the liability created by a lock.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommittedDisposition {
    pub certificate: OutcomeCertificate,
}

impl CommittedDisposition {
    fn validate(
        &self,
        lock: &CommittedSourceLock,
        terminal: &CommittedTerminal,
    ) -> Result<(), DrainError> {
        let body = &self.certificate.body;
        let outcome_matches = matches!(
            (&terminal.certificate.body.outcome, &body.outcome),
            (
                TransferOutcome::Paid { .. },
                TransferOutcome::Released { .. }
            ) | (
                TransferOutcome::Rejected { .. },
                TransferOutcome::Refunded { .. }
            )
        );
        if body.transfer_id != lock.transfer_id()
            || body.intent_hash != lock.intent.intent_hash()
            || body.zone != lock.intent.source
            || !outcome_matches
        {
            return Err(DrainError::InvalidDisposition);
        }
        Ok(())
    }

    fn leaf(&self) -> B256 {
        tagged_hash(
            DISPOSITION_LEAF_DOMAIN,
            &[
                self.certificate.body.transfer_id.as_slice(),
                self.certificate.body.body_hash().as_slice(),
                &self.certificate.canonical_bytes(),
            ],
        )
    }
}

/// Why a barrier lock is still a live liability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Liability {
    AwaitingDestination = 0,
    AwaitingSourceDisposition = 1,
    PolicyBlockedDisposition = 2,
}

/// One unresolved entry.  The state is reconstructed, never supplied independently by callers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnresolvedLock {
    pub transfer_id: B256,
    pub liability: Liability,
}

impl UnresolvedLock {
    fn leaf(&self) -> B256 {
        tagged_hash(
            UNRESOLVED_LEAF_DOMAIN,
            &[self.transfer_id.as_slice(), &[self.liability as u8]],
        )
    }
}

/// Count-bound binary Merkle inclusion proof.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundedInclusionProof {
    pub leaf_index: u32,
    pub leaf_count: u32,
    pub siblings: Vec<B256>,
}

impl BoundedInclusionProof {
    pub fn verify(&self, leaf: B256, expected_root: B256) -> Result<(), DrainError> {
        let count = usize::try_from(self.leaf_count).map_err(|_| DrainError::InvalidProof)?;
        let index = usize::try_from(self.leaf_index).map_err(|_| DrainError::InvalidProof)?;
        if count == 0
            || count > MAX_DRAIN_LOCKS
            || index >= count
            || self.siblings.len() > MAX_DRAIN_PROOF_DEPTH
            || self.siblings.len() != proof_depth(count)
        {
            return Err(DrainError::InvalidProof);
        }
        let mut node = leaf;
        let mut cursor = index;
        for sibling in &self.siblings {
            node = if cursor & 1 == 0 {
                merkle_node(node, *sibling)
            } else {
                merkle_node(*sibling, node)
            };
            cursor >>= 1;
        }
        if bind_root(self.leaf_count, node) != expected_root {
            return Err(DrainError::WrongRoot);
        }
        Ok(())
    }
}

/// A lock plus proofs of membership in the signed complete and, when applicable, unresolved set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProvenBarrierLock {
    pub lock: CommittedSourceLock,
    pub complete_proof: BoundedInclusionProof,
    pub unresolved: Option<(UnresolvedLock, BoundedInclusionProof)>,
}

impl ProvenBarrierLock {
    /// Exact bounded fourth argument for native `resolve`. This format is shared with the T14
    /// precompile decoder and binds the inclusion path to the signed full barrier statement.
    pub fn native_barrier_proof(
        &self,
        l1_chain_id: u64,
        statement: &FastBarrierStatement,
    ) -> Result<Vec<u8>, DrainError> {
        if l1_chain_id == 0
            || self.complete_proof.leaf_count == 0
            || self.complete_proof.siblings.len() > MAX_DRAIN_PROOF_DEPTH
            || self.lock.intent.source.portal != statement.source_portal
            || self.lock.intent.source.authority_epoch != statement.source_epoch
            || self.lock.intent.destination.portal != statement.destination_portal
            || self.lock.intent.destination.authority_epoch != statement.destination_epoch
            || self.lock.lock.body.log_index > statement.lock_log_watermark
        {
            return Err(DrainError::InvalidProof);
        }
        self.complete_proof
            .verify(self.lock.leaf(), statement.complete_lock_root)?;
        let mut out = Vec::with_capacity(250 + self.complete_proof.siblings.len() * 32);
        out.push(NATIVE_BARRIER_PROOF_VERSION);
        out.extend_from_slice(statement.destination_portal.as_slice());
        out.extend_from_slice(&statement.destination_epoch.to_be_bytes());
        out.extend_from_slice(statement.closure_hash.as_slice());
        out.extend_from_slice(statement.source_portal.as_slice());
        out.extend_from_slice(&statement.source_epoch.to_be_bytes());
        out.extend_from_slice(&statement.imported_anchor_number.to_be_bytes());
        out.extend_from_slice(statement.imported_anchor_hash.as_slice());
        out.extend_from_slice(statement.registry_digest(l1_chain_id).as_slice());
        out.extend_from_slice(&statement.lock_log_watermark.to_be_bytes());
        out.extend_from_slice(statement.complete_lock_root.as_slice());
        out.extend_from_slice(&self.complete_proof.leaf_index.to_be_bytes());
        out.extend_from_slice(&self.complete_proof.leaf_count.to_be_bytes());
        out.push(self.complete_proof.siblings.len() as u8);
        for sibling in &self.complete_proof.siblings {
            out.extend_from_slice(sibling.as_slice());
        }
        Ok(out)
    }
}

/// Source-produced full barrier inventory.  Sending the full bounded list lets a destination
/// detect an omitted leaf by reconstructing the signed count-bound root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BarrierInventory {
    pub l1_chain_id: u64,
    pub statement: FastBarrierStatement,
    pub locks: Vec<ProvenBarrierLock>,
}

impl BarrierInventory {
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        l1_chain_id: u64,
        destination_portal: Address,
        destination_epoch: u64,
        closure_hash: B256,
        source_portal: Address,
        source_epoch: u64,
        imported_anchor_number: u64,
        imported_anchor_hash: B256,
        committed_term: u64,
        committed_index: u64,
        block_height: u64,
        block_hash: B256,
        state_root: B256,
        mut locks: Vec<CommittedSourceLock>,
        terminals: &BTreeMap<B256, CommittedTerminal>,
        dispositions: &BTreeMap<B256, CommittedDisposition>,
        policy_blocked: &BTreeSet<B256>,
    ) -> Result<Self, DrainError> {
        if l1_chain_id == 0
            || destination_portal.is_zero()
            || source_portal.is_zero()
            || destination_portal == source_portal
            || destination_epoch == 0
            || source_epoch == 0
            || closure_hash.is_zero()
            || imported_anchor_hash.is_zero()
            || committed_index == 0
            || block_hash.is_zero()
            || state_root.is_zero()
            || locks.len() > MAX_DRAIN_LOCKS
        {
            return Err(DrainError::InvalidBarrier);
        }
        locks.sort_by_key(|lock| (lock.lock.body.log_index, lock.transfer_id()));
        ensure_unique(locks.iter().map(CommittedSourceLock::transfer_id))?;
        for lock in &locks {
            lock.validate(
                source_portal,
                source_epoch,
                destination_portal,
                destination_epoch,
            )?;
            if lock.intent.source.l1_chain_id != l1_chain_id
                || lock.intent.destination.l1_chain_id != l1_chain_id
            {
                return Err(DrainError::InvalidBarrier);
            }
            if lock.lock.body.log_index > committed_index {
                return Err(DrainError::OutsideCommittedPrefix);
            }
        }

        let lock_leaves = locks
            .iter()
            .map(CommittedSourceLock::leaf)
            .collect::<Vec<_>>();
        let lock_tree = MerkleTree::new(&lock_leaves)?;
        let mut unresolved = Vec::new();
        for lock in &locks {
            let transfer_id = lock.transfer_id();
            let liability = match (terminals.get(&transfer_id), dispositions.get(&transfer_id)) {
                (None, None) => Some(Liability::AwaitingDestination),
                (Some(terminal), None) => {
                    terminal.validate(lock)?;
                    Some(if policy_blocked.contains(&transfer_id) {
                        Liability::PolicyBlockedDisposition
                    } else {
                        Liability::AwaitingSourceDisposition
                    })
                }
                (Some(terminal), Some(disposition)) => {
                    terminal.validate(lock)?;
                    disposition.validate(lock, terminal)?;
                    None
                }
                (None, Some(_)) => return Err(DrainError::DispositionBeforeTerminal),
            };
            if let Some(liability) = liability {
                unresolved.push(UnresolvedLock {
                    transfer_id,
                    liability,
                });
            }
        }
        unresolved.sort_by_key(|entry| entry.transfer_id);
        let unresolved_leaves = unresolved
            .iter()
            .map(UnresolvedLock::leaf)
            .collect::<Vec<_>>();
        let unresolved_tree = MerkleTree::new(&unresolved_leaves)?;
        let unresolved_positions = unresolved
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.transfer_id, index))
            .collect::<BTreeMap<_, _>>();
        let watermark = locks.last().map_or(0, |lock| lock.lock.body.log_index);
        let statement = FastBarrierStatement {
            destination_portal,
            destination_epoch,
            closure_hash,
            source_portal,
            source_epoch,
            imported_anchor_number,
            imported_anchor_hash,
            log_term: committed_term,
            log_index: committed_index,
            block_height: U256::from(block_height),
            block_hash,
            state_root,
            lock_log_watermark: watermark,
            complete_lock_root: lock_tree.root(),
            unresolved_root: if unresolved.is_empty() {
                FAST_EMPTY_UNRESOLVED_ROOT
            } else {
                unresolved_tree.root()
            },
            unresolved_count: unresolved.len() as u64,
        };
        let proven = locks
            .into_iter()
            .enumerate()
            .map(|(index, lock)| {
                let transfer_id = lock.transfer_id();
                let unresolved = unresolved_positions.get(&transfer_id).map(|position| {
                    (
                        unresolved[*position].clone(),
                        unresolved_tree.proof(*position),
                    )
                });
                ProvenBarrierLock {
                    lock,
                    complete_proof: lock_tree.proof(index),
                    unresolved,
                }
            })
            .collect();
        Ok(Self {
            l1_chain_id,
            statement,
            locks: proven,
        })
    }

    /// Reconstruct all signed roots and reject a truncated list, wrong watermark, or bad proof.
    pub fn verify_complete(&self) -> Result<(), DrainError> {
        if self.l1_chain_id == 0 || self.locks.len() > MAX_DRAIN_LOCKS {
            return Err(DrainError::TooManyLocks);
        }
        let ids = self
            .locks
            .iter()
            .map(|entry| entry.lock.transfer_id())
            .collect::<Vec<_>>();
        ensure_unique(ids.iter().copied())?;
        let mut previous = None;
        let mut lock_leaves = Vec::with_capacity(self.locks.len());
        let mut unresolved = Vec::new();
        for entry in &self.locks {
            entry.lock.validate(
                self.statement.source_portal,
                self.statement.source_epoch,
                self.statement.destination_portal,
                self.statement.destination_epoch,
            )?;
            if self.l1_chain_id == 0
                || entry.lock.intent.source.l1_chain_id != self.l1_chain_id
                || entry.lock.intent.destination.l1_chain_id != self.l1_chain_id
            {
                return Err(DrainError::InvalidBarrier);
            }
            let key = (entry.lock.lock.body.log_index, entry.lock.transfer_id());
            if previous.is_some_and(|old| old >= key) {
                return Err(DrainError::NonCanonicalOrder);
            }
            previous = Some(key);
            let leaf = entry.lock.leaf();
            entry
                .complete_proof
                .verify(leaf, self.statement.complete_lock_root)?;
            lock_leaves.push(leaf);
            if let Some((unresolved_entry, proof)) = &entry.unresolved {
                if unresolved_entry.transfer_id != entry.lock.transfer_id() {
                    return Err(DrainError::InvalidProof);
                }
                proof.verify(unresolved_entry.leaf(), self.statement.unresolved_root)?;
                unresolved.push(unresolved_entry.clone());
            }
        }
        if merkle_root(&lock_leaves)? != self.statement.complete_lock_root {
            return Err(DrainError::WrongRoot);
        }
        let expected_watermark = self
            .locks
            .last()
            .map_or(0, |entry| entry.lock.lock.body.log_index);
        if expected_watermark != self.statement.lock_log_watermark
            || self
                .locks
                .iter()
                .any(|entry| entry.lock.lock.body.log_index > self.statement.log_index)
        {
            return Err(DrainError::WrongWatermark);
        }
        unresolved.sort_by_key(|entry| entry.transfer_id);
        let unresolved_leaves = unresolved
            .iter()
            .map(UnresolvedLock::leaf)
            .collect::<Vec<_>>();
        let expected_unresolved_root = if unresolved.is_empty() {
            FAST_EMPTY_UNRESOLVED_ROOT
        } else {
            merkle_root(&unresolved_leaves)?
        };
        if unresolved.len() as u64 != self.statement.unresolved_count
            || expected_unresolved_root != self.statement.unresolved_root
        {
            return Err(DrainError::WrongRoot);
        }
        Ok(())
    }

    pub fn unresolved_ids(&self) -> Vec<B256> {
        let mut ids = self
            .locks
            .iter()
            .filter_map(|entry| entry.unresolved.as_ref().map(|value| value.0.transfer_id))
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids
    }

    /// Canonical bounded encoding used by the fsynced journal and chunked private transport.
    pub fn durable_bytes(&self) -> Result<Vec<u8>, DrainError> {
        self.verify_complete()?;
        let mut out = Vec::new();
        out.extend_from_slice(&self.l1_chain_id.to_be_bytes());
        put_barrier_statement(&mut out, &self.statement);
        put_count(&mut out, self.locks.len())?;
        for entry in &self.locks {
            put_bounded(
                &mut out,
                &entry.lock.intent.canonical_bytes(),
                MAX_INTENT_BYTES,
            )?;
            put_bounded(
                &mut out,
                &entry.lock.lock.canonical_bytes(),
                MAX_CERTIFICATE_BYTES,
            )?;
            put_proof(&mut out, &entry.complete_proof)?;
            match &entry.unresolved {
                None => out.push(0),
                Some((unresolved, proof)) => {
                    out.push(1);
                    out.extend_from_slice(unresolved.transfer_id.as_slice());
                    out.push(unresolved.liability as u8);
                    put_proof(&mut out, proof)?;
                }
            }
        }
        ensure_encoded_bound(out)
    }

    /// Decode and fully re-derive roots, ordering, watermark, and every inclusion proof.
    pub fn decode_durable(bytes: &[u8]) -> Result<Self, DrainError> {
        let mut reader = DrainReader::new(bytes)?;
        let l1_chain_id = reader.u64()?;
        let statement = reader.barrier_statement()?;
        let count = reader.count(MAX_DRAIN_LOCKS)?;
        let mut locks = Vec::with_capacity(count);
        for _ in 0..count {
            let intent = TransferIntent::decode(reader.bounded(MAX_INTENT_BYTES)?)
                .map_err(|_| DrainError::InvalidDurableEncoding)?;
            let lock = OutcomeCertificate::decode(reader.bounded(MAX_CERTIFICATE_BYTES)?)
                .map_err(|_| DrainError::InvalidDurableEncoding)?;
            let complete_proof = reader.proof()?;
            let unresolved = match reader.u8()? {
                0 => None,
                1 => {
                    let transfer_id = reader.b256()?;
                    let liability = match reader.u8()? {
                        0 => Liability::AwaitingDestination,
                        1 => Liability::AwaitingSourceDisposition,
                        2 => Liability::PolicyBlockedDisposition,
                        _ => return Err(DrainError::InvalidDurableEncoding),
                    };
                    Some((
                        UnresolvedLock {
                            transfer_id,
                            liability,
                        },
                        reader.proof()?,
                    ))
                }
                _ => return Err(DrainError::InvalidDurableEncoding),
            };
            locks.push(ProvenBarrierLock {
                lock: CommittedSourceLock { intent, lock },
                complete_proof,
                unresolved,
            });
        }
        reader.finish()?;
        let inventory = Self {
            l1_chain_id,
            statement,
            locks,
        };
        inventory.verify_complete()?;
        Ok(inventory)
    }
}

/// Fully derived resolution and the evidence used to derive its roots.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BarrierResolutionInventory {
    pub resolution: FastBarrierResolution,
    pub terminals: Vec<CommittedTerminal>,
    pub dispositions: Vec<CommittedDisposition>,
}

impl BarrierResolutionInventory {
    pub fn build(
        barrier: &BarrierInventory,
        terminals: &BTreeMap<B256, CommittedTerminal>,
        dispositions: &BTreeMap<B256, CommittedDisposition>,
        policy_blocked: &BTreeSet<B256>,
    ) -> Result<Self, DrainError> {
        barrier.verify_complete()?;
        let unresolved_ids = barrier.unresolved_ids();
        let locks = barrier
            .locks
            .iter()
            .map(|entry| (entry.lock.transfer_id(), &entry.lock))
            .collect::<BTreeMap<_, _>>();
        let mut terminal_values = Vec::with_capacity(unresolved_ids.len());
        let mut disposition_values = Vec::with_capacity(unresolved_ids.len());
        for transfer_id in unresolved_ids {
            if policy_blocked.contains(&transfer_id) {
                return Err(DrainError::PolicyBlocked(transfer_id));
            }
            let lock = locks.get(&transfer_id).ok_or(DrainError::MissingLock)?;
            let terminal = terminals
                .get(&transfer_id)
                .ok_or(DrainError::UnresolvedLiability(transfer_id))?;
            let disposition = dispositions
                .get(&transfer_id)
                .ok_or(DrainError::UnresolvedLiability(transfer_id))?;
            terminal.validate(lock)?;
            disposition.validate(lock, terminal)?;
            terminal_values.push(terminal.clone());
            disposition_values.push(disposition.clone());
        }
        let terminal_root = merkle_root(
            &terminal_values
                .iter()
                .map(CommittedTerminal::leaf)
                .collect::<Vec<_>>(),
        )?;
        let disposition_root = merkle_root(
            &disposition_values
                .iter()
                .map(CommittedDisposition::leaf)
                .collect::<Vec<_>>(),
        )?;
        Ok(Self {
            resolution: FastBarrierResolution {
                barrier_hash: barrier.statement.registry_digest(barrier.l1_chain_id),
                terminal_root,
                disposition_root,
                resolved_count: barrier.statement.unresolved_count,
                remaining_unresolved_root: FAST_EMPTY_UNRESOLVED_ROOT,
                remaining_unresolved_count: 0,
            },
            terminals: terminal_values,
            dispositions: disposition_values,
        })
    }

    pub fn verify(&self, barrier: &BarrierInventory) -> Result<(), DrainError> {
        if self.terminals.len() != self.dispositions.len()
            || self.terminals.len() as u64 != barrier.statement.unresolved_count
            || self.resolution.resolved_count != barrier.statement.unresolved_count
            || self.resolution.remaining_unresolved_root != FAST_EMPTY_UNRESOLVED_ROOT
            || self.resolution.remaining_unresolved_count != 0
        {
            return Err(DrainError::UnresolvedRemaining);
        }
        if self.resolution.barrier_hash != barrier.statement.registry_digest(barrier.l1_chain_id)
            || self.resolution.terminal_root
                != merkle_root(
                    &self
                        .terminals
                        .iter()
                        .map(CommittedTerminal::leaf)
                        .collect::<Vec<_>>(),
                )?
            || self.resolution.disposition_root
                != merkle_root(
                    &self
                        .dispositions
                        .iter()
                        .map(CommittedDisposition::leaf)
                        .collect::<Vec<_>>(),
                )?
        {
            return Err(DrainError::WrongRoot);
        }
        Ok(())
    }

    /// Canonical bounded durable encoding. Binding to the original barrier is rechecked by the
    /// service after recovery.
    pub fn durable_bytes(&self) -> Result<Vec<u8>, DrainError> {
        if self.terminals.len() != self.dispositions.len() || self.terminals.len() > MAX_DRAIN_LOCKS
        {
            return Err(DrainError::InvalidDurableEncoding);
        }
        let mut out = Vec::new();
        put_resolution(&mut out, &self.resolution);
        put_count(&mut out, self.terminals.len())?;
        for terminal in &self.terminals {
            put_bounded(
                &mut out,
                &terminal.certificate.canonical_bytes(),
                MAX_CERTIFICATE_BYTES,
            )?;
        }
        for disposition in &self.dispositions {
            put_bounded(
                &mut out,
                &disposition.certificate.canonical_bytes(),
                MAX_CERTIFICATE_BYTES,
            )?;
        }
        ensure_encoded_bound(out)
    }

    pub fn decode_durable(bytes: &[u8]) -> Result<Self, DrainError> {
        let mut reader = DrainReader::new(bytes)?;
        let resolution = reader.resolution()?;
        let count = reader.count(MAX_DRAIN_LOCKS)?;
        let mut terminals = Vec::with_capacity(count);
        for _ in 0..count {
            terminals.push(CommittedTerminal {
                certificate: OutcomeCertificate::decode(reader.bounded(MAX_CERTIFICATE_BYTES)?)
                    .map_err(|_| DrainError::InvalidDurableEncoding)?,
            });
        }
        let mut dispositions = Vec::with_capacity(count);
        for _ in 0..count {
            dispositions.push(CommittedDisposition {
                certificate: OutcomeCertificate::decode(reader.bounded(MAX_CERTIFICATE_BYTES)?)
                    .map_err(|_| DrainError::InvalidDurableEncoding)?,
            });
        }
        reader.finish()?;
        Ok(Self {
            resolution,
            terminals,
            dispositions,
        })
    }
}

/// Exact barrier and resolution hashes in finalized registry order.
pub fn barriers_hash(entries: &[(Address, B256, B256)]) -> Result<B256, DrainError> {
    if entries.len() != DRAIN_PEER_COUNT {
        return Err(DrainError::WrongPeerCount);
    }
    let mut peers = BTreeSet::new();
    if entries.iter().any(|entry| !peers.insert(entry.0)) {
        return Err(DrainError::DuplicateIdentity);
    }
    let mut hash = keccak256(BARRIERS_DOMAIN);
    for (portal, barrier, resolution) in entries {
        if portal.is_zero() || barrier.is_zero() || resolution.is_zero() {
            return Err(DrainError::InvalidBarrier);
        }
        let mut encoded = Vec::with_capacity(4 * 32);
        encoded.extend_from_slice(hash.as_slice());
        encoded.extend_from_slice(&[0; 12]);
        encoded.extend_from_slice(portal.as_slice());
        encoded.extend_from_slice(barrier.as_slice());
        encoded.extend_from_slice(resolution.as_slice());
        hash = keccak256(encoded);
    }
    Ok(hash)
}

/// Durable next-roster image installed before a checkpoint acknowledgment may be signed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CheckpointImage {
    pub l1_chain_id: u64,
    pub statement: FastCheckpointStatement,
    pub canonical_head_hash: B256,
    pub canonical_state_root: B256,
    pub witness_root: B256,
    pub outcomes_root: B256,
    pub replay_barriers_root: B256,
    pub raft_prefix: Vec<B256>,
    /// Canonical retained original locks, terminal outcomes, dispositions and applied receipts.
    pub transfer_history: Vec<u8>,
    /// Complete replay/proof witnesses required to reconstruct the committed prefix.
    pub replay_witnesses: Vec<u8>,
    /// Durable replay-barrier and old-key retention state.
    pub replay_barriers: Vec<u8>,
    /// Fast-service journal, incoming/outgoing cursors and unresolved delivery state.
    pub fast_service_journal: Vec<u8>,
    /// Canonical batch-boundary/finalization state.
    pub batch_boundaries: Vec<u8>,
    /// Replenishment jobs, prepared actions and canonical observations.
    pub replenishment_state: Vec<u8>,
    /// Exact checksummed consensus state-machine snapshot installed by the next roster.
    pub consensus_snapshot: Vec<u8>,
}

impl CheckpointImage {
    pub fn validate(&self) -> Result<(), DrainError> {
        if self.statement.portal.is_zero()
            || self.statement.old_epoch == 0
            || self.statement.next_epoch <= self.statement.old_epoch
            || self.statement.next_roster_hash.is_zero()
            || self.statement.final_block_hash.is_zero()
            || self.statement.final_settlement_hash.is_zero()
            || self.statement.checkpoint_log_index == 0
            || self.statement.checkpoint_block_hash.is_zero()
            || self.statement.checkpoint_state_root.is_zero()
            || self.statement.checkpoint_height != self.statement.final_zone_height
            || self.statement.checkpoint_block_hash != self.statement.final_block_hash
            || self.canonical_head_hash != self.statement.checkpoint_block_hash
            || self.canonical_state_root != self.statement.checkpoint_state_root
            || self.witness_root.is_zero()
            || self.outcomes_root.is_zero()
            || self.replay_barriers_root.is_zero()
            || self.raft_prefix.is_empty()
            || self.raft_prefix.last() != Some(&self.canonical_head_hash)
            || self.raft_prefix.len() > MAX_CHECKPOINT_REPLAY_BLOCKS
            || self.raft_prefix.iter().any(B256::is_zero)
            || self.transfer_history.is_empty()
            || self.replay_witnesses.is_empty()
            || self.replay_barriers.is_empty()
            || self.fast_service_journal.is_empty()
            || self.batch_boundaries.is_empty()
            || self.replenishment_state.is_empty()
            || self.consensus_snapshot.is_empty()
            || self
                .checkpoint_payload_len()
                .is_none_or(|length| length > MAX_DRAIN_OBJECT_BYTES)
        {
            return Err(DrainError::InvalidCheckpoint);
        }
        Ok(())
    }

    pub fn image_hash(&self) -> Result<B256, DrainError> {
        self.validate()?;
        let mut fields = Vec::with_capacity(6 + self.raft_prefix.len());
        fields.push(self.statement.registry_digest(self.l1_chain_id));
        fields.push(self.canonical_head_hash);
        fields.push(self.canonical_state_root);
        fields.push(self.witness_root);
        fields.push(self.outcomes_root);
        fields.push(self.replay_barriers_root);
        fields.extend_from_slice(&self.raft_prefix);
        fields.extend([
            keccak256(&self.transfer_history),
            keccak256(&self.replay_witnesses),
            keccak256(&self.replay_barriers),
            keccak256(&self.fast_service_journal),
            keccak256(&self.batch_boundaries),
            keccak256(&self.replenishment_state),
            keccak256(&self.consensus_snapshot),
        ]);
        Ok(tagged_hash(
            CHECKPOINT_IMAGE_DOMAIN,
            &fields.iter().map(B256::as_slice).collect::<Vec<_>>(),
        ))
    }

    pub fn durable_bytes(&self) -> Result<Vec<u8>, DrainError> {
        self.validate()?;
        let mut out = Vec::new();
        out.extend_from_slice(&self.l1_chain_id.to_be_bytes());
        put_checkpoint_statement(&mut out, &self.statement);
        for value in [
            self.canonical_head_hash,
            self.canonical_state_root,
            self.witness_root,
            self.outcomes_root,
            self.replay_barriers_root,
        ] {
            out.extend_from_slice(value.as_slice());
        }
        put_count(&mut out, self.raft_prefix.len())?;
        for hash in &self.raft_prefix {
            out.extend_from_slice(hash.as_slice());
        }
        for bytes in [
            &self.transfer_history,
            &self.replay_witnesses,
            &self.replay_barriers,
            &self.fast_service_journal,
            &self.batch_boundaries,
            &self.replenishment_state,
            &self.consensus_snapshot,
        ] {
            put_bounded(&mut out, bytes, MAX_DRAIN_OBJECT_BYTES)?;
        }
        ensure_encoded_bound(out)
    }

    pub fn decode_durable(bytes: &[u8]) -> Result<Self, DrainError> {
        let mut reader = DrainReader::new(bytes)?;
        let l1_chain_id = reader.u64()?;
        let statement = reader.checkpoint_statement()?;
        let canonical_head_hash = reader.b256()?;
        let canonical_state_root = reader.b256()?;
        let witness_root = reader.b256()?;
        let outcomes_root = reader.b256()?;
        let replay_barriers_root = reader.b256()?;
        let count = reader.count(MAX_CHECKPOINT_REPLAY_BLOCKS)?;
        let mut raft_prefix = Vec::with_capacity(count);
        for _ in 0..count {
            raft_prefix.push(reader.b256()?);
        }
        let transfer_history = reader.bounded(MAX_DRAIN_OBJECT_BYTES)?.to_vec();
        let replay_witnesses = reader.bounded(MAX_DRAIN_OBJECT_BYTES)?.to_vec();
        let replay_barriers = reader.bounded(MAX_DRAIN_OBJECT_BYTES)?.to_vec();
        let fast_service_journal = reader.bounded(MAX_DRAIN_OBJECT_BYTES)?.to_vec();
        let batch_boundaries = reader.bounded(MAX_DRAIN_OBJECT_BYTES)?.to_vec();
        let replenishment_state = reader.bounded(MAX_DRAIN_OBJECT_BYTES)?.to_vec();
        let consensus_snapshot = reader.bounded(MAX_DRAIN_OBJECT_BYTES)?.to_vec();
        reader.finish()?;
        let image = Self {
            l1_chain_id,
            statement,
            canonical_head_hash,
            canonical_state_root,
            witness_root,
            outcomes_root,
            replay_barriers_root,
            raft_prefix,
            transfer_history,
            replay_witnesses,
            replay_barriers,
            fast_service_journal,
            batch_boundaries,
            replenishment_state,
            consensus_snapshot,
        };
        image.validate()?;
        Ok(image)
    }

    fn checkpoint_payload_len(&self) -> Option<usize> {
        [
            self.transfer_history.len(),
            self.replay_witnesses.len(),
            self.replay_barriers.len(),
            self.fast_service_journal.len(),
            self.batch_boundaries.len(),
            self.replenishment_state.len(),
            self.consensus_snapshot.len(),
        ]
        .into_iter()
        .try_fold(0usize, usize::checked_add)
    }
}

/// A purpose-specific certificate used by the driver for barriers, resolutions and checkpoints.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DrainCertificate {
    pub digest: B256,
    pub signatures: [SignatureBytes; 2],
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DrainError {
    #[error("invalid drain barrier coordinates")]
    InvalidBarrier,
    #[error("invalid committed source lock")]
    InvalidLock,
    #[error("invalid destination terminal outcome")]
    InvalidTerminal,
    #[error("invalid source disposition")]
    InvalidDisposition,
    #[error("source disposition precedes a destination terminal outcome")]
    DispositionBeforeTerminal,
    #[error("record is outside the durable committed prefix")]
    OutsideCommittedPrefix,
    #[error("too many locks in drain inventory")]
    TooManyLocks,
    #[error("duplicate transfer or peer identity")]
    DuplicateIdentity,
    #[error("drain records are not in canonical order")]
    NonCanonicalOrder,
    #[error("invalid bounded inclusion proof")]
    InvalidProof,
    #[error("signed root does not match canonical records")]
    WrongRoot,
    #[error("lock watermark does not match the complete committed prefix")]
    WrongWatermark,
    #[error("barrier references a missing lock")]
    MissingLock,
    #[error("transfer {0} remains an unresolved liability")]
    UnresolvedLiability(B256),
    #[error("transfer {0} has a policy-blocked source disposition")]
    PolicyBlocked(B256),
    #[error("barrier resolution still has unresolved liabilities")]
    UnresolvedRemaining,
    #[error("epoch settlement does not contain exactly nine unique peers")]
    WrongPeerCount,
    #[error("invalid durable checkpoint image")]
    InvalidCheckpoint,
    #[error("invalid or oversized durable drain encoding")]
    InvalidDurableEncoding,
}

#[derive(Clone, Debug)]
struct MerkleTree {
    levels: Vec<Vec<B256>>,
    leaf_count: u32,
}

impl MerkleTree {
    fn new(leaves: &[B256]) -> Result<Self, DrainError> {
        if leaves.len() > MAX_DRAIN_LOCKS {
            return Err(DrainError::TooManyLocks);
        }
        if leaves.is_empty() {
            return Ok(Self {
                levels: vec![vec![keccak256(EMPTY_LOCK_ROOT_DOMAIN)]],
                leaf_count: 0,
            });
        }
        let mut levels = vec![leaves.to_vec()];
        while levels.last().expect("one level").len() > 1 {
            let current = levels.last().expect("one level");
            let mut next = Vec::with_capacity(current.len().div_ceil(2));
            for pair in current.chunks(2) {
                let right = pair.get(1).copied().unwrap_or(pair[0]);
                next.push(merkle_node(pair[0], right));
            }
            levels.push(next);
        }
        Ok(Self {
            levels,
            leaf_count: leaves.len() as u32,
        })
    }

    fn root(&self) -> B256 {
        if self.leaf_count == 0 {
            keccak256(EMPTY_LOCK_ROOT_DOMAIN)
        } else {
            bind_root(self.leaf_count, self.levels.last().expect("one level")[0])
        }
    }

    fn proof(&self, index: usize) -> BoundedInclusionProof {
        debug_assert!(index < self.leaf_count as usize);
        let mut siblings = Vec::with_capacity(self.levels.len().saturating_sub(1));
        let mut cursor = index;
        for level in self.levels.iter().take(self.levels.len().saturating_sub(1)) {
            let sibling = if cursor & 1 == 0 {
                level.get(cursor + 1).copied().unwrap_or(level[cursor])
            } else {
                level[cursor - 1]
            };
            siblings.push(sibling);
            cursor >>= 1;
        }
        BoundedInclusionProof {
            leaf_index: index as u32,
            leaf_count: self.leaf_count,
            siblings,
        }
    }
}

fn merkle_root(leaves: &[B256]) -> Result<B256, DrainError> {
    Ok(MerkleTree::new(leaves)?.root())
}

fn proof_depth(mut count: usize) -> usize {
    let mut depth = 0;
    while count > 1 {
        count = count.div_ceil(2);
        depth += 1;
    }
    depth
}

fn merkle_node(left: B256, right: B256) -> B256 {
    tagged_hash(MERKLE_NODE_DOMAIN, &[left.as_slice(), right.as_slice()])
}

fn bind_root(count: u32, root: B256) -> B256 {
    tagged_hash(MERKLE_ROOT_DOMAIN, &[&count.to_be_bytes(), root.as_slice()])
}

fn tagged_hash(domain: &[u8], fields: &[&[u8]]) -> B256 {
    let size = 4 + domain.len() + fields.iter().map(|field| 4 + field.len()).sum::<usize>();
    let mut encoded = Vec::with_capacity(size);
    encoded.extend_from_slice(&(domain.len() as u32).to_be_bytes());
    encoded.extend_from_slice(domain);
    for field in fields {
        encoded.extend_from_slice(&(field.len() as u32).to_be_bytes());
        encoded.extend_from_slice(field);
    }
    keccak256(encoded)
}

fn ensure_unique(values: impl IntoIterator<Item = B256>) -> Result<(), DrainError> {
    let mut seen = BTreeSet::new();
    for value in values {
        if !seen.insert(value) {
            return Err(DrainError::DuplicateIdentity);
        }
    }
    Ok(())
}

fn ensure_encoded_bound(bytes: Vec<u8>) -> Result<Vec<u8>, DrainError> {
    if bytes.len() > MAX_DRAIN_OBJECT_BYTES {
        Err(DrainError::InvalidDurableEncoding)
    } else {
        Ok(bytes)
    }
}

fn put_count(out: &mut Vec<u8>, count: usize) -> Result<(), DrainError> {
    let count = u32::try_from(count).map_err(|_| DrainError::InvalidDurableEncoding)?;
    out.extend_from_slice(&count.to_be_bytes());
    Ok(())
}

fn put_bounded(out: &mut Vec<u8>, bytes: &[u8], maximum: usize) -> Result<(), DrainError> {
    if bytes.len() > maximum {
        return Err(DrainError::InvalidDurableEncoding);
    }
    put_count(out, bytes.len())?;
    out.extend_from_slice(bytes);
    Ok(())
}

fn put_proof(out: &mut Vec<u8>, proof: &BoundedInclusionProof) -> Result<(), DrainError> {
    if proof.siblings.len() > MAX_DRAIN_PROOF_DEPTH {
        return Err(DrainError::InvalidDurableEncoding);
    }
    out.extend_from_slice(&proof.leaf_index.to_be_bytes());
    out.extend_from_slice(&proof.leaf_count.to_be_bytes());
    put_count(out, proof.siblings.len())?;
    for sibling in &proof.siblings {
        out.extend_from_slice(sibling.as_slice());
    }
    Ok(())
}

fn put_barrier_statement(out: &mut Vec<u8>, value: &FastBarrierStatement) {
    out.extend_from_slice(value.destination_portal.as_slice());
    out.extend_from_slice(&value.destination_epoch.to_be_bytes());
    out.extend_from_slice(value.closure_hash.as_slice());
    out.extend_from_slice(value.source_portal.as_slice());
    out.extend_from_slice(&value.source_epoch.to_be_bytes());
    out.extend_from_slice(&value.imported_anchor_number.to_be_bytes());
    out.extend_from_slice(value.imported_anchor_hash.as_slice());
    out.extend_from_slice(&value.log_term.to_be_bytes());
    out.extend_from_slice(&value.log_index.to_be_bytes());
    out.extend_from_slice(&value.block_height.to_be_bytes::<32>());
    out.extend_from_slice(value.block_hash.as_slice());
    out.extend_from_slice(value.state_root.as_slice());
    out.extend_from_slice(&value.lock_log_watermark.to_be_bytes());
    out.extend_from_slice(value.complete_lock_root.as_slice());
    out.extend_from_slice(value.unresolved_root.as_slice());
    out.extend_from_slice(&value.unresolved_count.to_be_bytes());
}

fn put_resolution(out: &mut Vec<u8>, value: &FastBarrierResolution) {
    out.extend_from_slice(value.barrier_hash.as_slice());
    out.extend_from_slice(value.terminal_root.as_slice());
    out.extend_from_slice(value.disposition_root.as_slice());
    out.extend_from_slice(&value.resolved_count.to_be_bytes());
    out.extend_from_slice(value.remaining_unresolved_root.as_slice());
    out.extend_from_slice(&value.remaining_unresolved_count.to_be_bytes());
}

fn put_checkpoint_statement(out: &mut Vec<u8>, value: &FastCheckpointStatement) {
    out.extend_from_slice(value.portal.as_slice());
    out.extend_from_slice(&value.old_epoch.to_be_bytes());
    out.extend_from_slice(&value.next_epoch.to_be_bytes());
    out.extend_from_slice(value.next_roster_hash.as_slice());
    out.extend_from_slice(&value.final_zone_height.to_be_bytes::<32>());
    out.extend_from_slice(value.final_block_hash.as_slice());
    out.extend_from_slice(&value.final_withdrawal_batch_index.to_be_bytes());
    out.extend_from_slice(value.final_settlement_hash.as_slice());
    out.extend_from_slice(&value.checkpoint_log_term.to_be_bytes());
    out.extend_from_slice(&value.checkpoint_log_index.to_be_bytes());
    out.extend_from_slice(&value.checkpoint_height.to_be_bytes::<32>());
    out.extend_from_slice(value.checkpoint_block_hash.as_slice());
    out.extend_from_slice(value.checkpoint_state_root.as_slice());
}

struct DrainReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> DrainReader<'a> {
    fn new(bytes: &'a [u8]) -> Result<Self, DrainError> {
        if bytes.len() > MAX_DRAIN_OBJECT_BYTES {
            return Err(DrainError::InvalidDurableEncoding);
        }
        Ok(Self { bytes, offset: 0 })
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], DrainError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(DrainError::InvalidDurableEncoding)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(DrainError::InvalidDurableEncoding)?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, DrainError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, DrainError> {
        Ok(u32::from_be_bytes(
            self.take(4)?
                .try_into()
                .map_err(|_| DrainError::InvalidDurableEncoding)?,
        ))
    }

    fn u64(&mut self) -> Result<u64, DrainError> {
        Ok(u64::from_be_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| DrainError::InvalidDurableEncoding)?,
        ))
    }

    fn b256(&mut self) -> Result<B256, DrainError> {
        Ok(B256::from_slice(self.take(32)?))
    }

    fn address(&mut self) -> Result<Address, DrainError> {
        Ok(Address::from_slice(self.take(20)?))
    }

    fn u256(&mut self) -> Result<U256, DrainError> {
        Ok(U256::from_be_slice(self.take(32)?))
    }

    fn count(&mut self, maximum: usize) -> Result<usize, DrainError> {
        let count = self.u32()? as usize;
        if count > maximum {
            return Err(DrainError::InvalidDurableEncoding);
        }
        Ok(count)
    }

    fn bounded(&mut self, maximum: usize) -> Result<&'a [u8], DrainError> {
        let length = self.count(maximum)?;
        self.take(length)
    }

    fn proof(&mut self) -> Result<BoundedInclusionProof, DrainError> {
        let leaf_index = self.u32()?;
        let leaf_count = self.u32()?;
        let count = self.count(MAX_DRAIN_PROOF_DEPTH)?;
        let mut siblings = Vec::with_capacity(count);
        for _ in 0..count {
            siblings.push(self.b256()?);
        }
        Ok(BoundedInclusionProof {
            leaf_index,
            leaf_count,
            siblings,
        })
    }

    fn barrier_statement(&mut self) -> Result<FastBarrierStatement, DrainError> {
        Ok(FastBarrierStatement {
            destination_portal: self.address()?,
            destination_epoch: self.u64()?,
            closure_hash: self.b256()?,
            source_portal: self.address()?,
            source_epoch: self.u64()?,
            imported_anchor_number: self.u64()?,
            imported_anchor_hash: self.b256()?,
            log_term: self.u64()?,
            log_index: self.u64()?,
            block_height: self.u256()?,
            block_hash: self.b256()?,
            state_root: self.b256()?,
            lock_log_watermark: self.u64()?,
            complete_lock_root: self.b256()?,
            unresolved_root: self.b256()?,
            unresolved_count: self.u64()?,
        })
    }

    fn resolution(&mut self) -> Result<FastBarrierResolution, DrainError> {
        Ok(FastBarrierResolution {
            barrier_hash: self.b256()?,
            terminal_root: self.b256()?,
            disposition_root: self.b256()?,
            resolved_count: self.u64()?,
            remaining_unresolved_root: self.b256()?,
            remaining_unresolved_count: self.u64()?,
        })
    }

    fn checkpoint_statement(&mut self) -> Result<FastCheckpointStatement, DrainError> {
        Ok(FastCheckpointStatement {
            portal: self.address()?,
            old_epoch: self.u64()?,
            next_epoch: self.u64()?,
            next_roster_hash: self.b256()?,
            final_zone_height: self.u256()?,
            final_block_hash: self.b256()?,
            final_withdrawal_batch_index: self.u64()?,
            final_settlement_hash: self.b256()?,
            checkpoint_log_term: self.u64()?,
            checkpoint_log_index: self.u64()?,
            checkpoint_height: self.u256()?,
            checkpoint_block_hash: self.b256()?,
            checkpoint_state_root: self.b256()?,
        })
    }

    fn finish(self) -> Result<(), DrainError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(DrainError::InvalidDurableEncoding)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zone_primitives::fast_transfer::{AssetId, CertificateBody, RejectionReason, ZoneDomain};

    const L1_CHAIN_ID: u64 = 4242;

    fn domain(zone: u32, portal: u8, epoch: u64) -> ZoneDomain {
        ZoneDomain {
            l1_chain_id: L1_CHAIN_ID,
            zone_id: zone,
            chain_id: 10_000 + u64::from(zone),
            portal: Address::repeat_byte(portal),
            authority_epoch: epoch,
            roster_hash: B256::repeat_byte(portal),
            protocol_version: 1,
        }
    }

    fn intent(nonce: u64) -> TransferIntent {
        TransferIntent {
            source: domain(1, 1, 7),
            destination: domain(2, 2, 9),
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
            transfer_nonce: nonce,
        }
    }

    fn certificate(
        intent: &TransferIntent,
        zone: ZoneDomain,
        index: u64,
        outcome: TransferOutcome,
    ) -> OutcomeCertificate {
        OutcomeCertificate {
            body: CertificateBody {
                transfer_id: intent.transfer_id(),
                intent_hash: intent.intent_hash(),
                zone,
                log_term: 3,
                log_index: index,
                block_height: index + 10,
                block_hash: B256::from(U256::from(index + 20)),
                state_root: B256::from(U256::from(index + 30)),
                transaction_hash: B256::from(U256::from(index + 40)),
                outcome,
            },
            signatures: [SignatureBytes([11; 65]), SignatureBytes([12; 65])],
        }
    }

    fn lock(nonce: u64, index: u64) -> CommittedSourceLock {
        let intent = intent(nonce);
        let lock = certificate(
            &intent,
            intent.source,
            index,
            TransferOutcome::Locked {
                escrow: Address::repeat_byte(13),
                amount: U256::from(102),
            },
        );
        CommittedSourceLock { intent, lock }
    }

    fn terminal(lock: &CommittedSourceLock, index: u64) -> CommittedTerminal {
        CommittedTerminal {
            certificate: certificate(
                &lock.intent,
                lock.intent.destination,
                index,
                TransferOutcome::Rejected {
                    reason: RejectionReason::QuoteExpired,
                },
            ),
        }
    }

    fn disposition(lock: &CommittedSourceLock, index: u64) -> CommittedDisposition {
        CommittedDisposition {
            certificate: certificate(
                &lock.intent,
                lock.intent.source,
                index,
                TransferOutcome::Refunded {
                    beneficiary: lock.intent.sender,
                    amount: U256::from(102),
                },
            ),
        }
    }

    fn inventory(
        locks: Vec<CommittedSourceLock>,
        terminals: &BTreeMap<B256, CommittedTerminal>,
        dispositions: &BTreeMap<B256, CommittedDisposition>,
        policy: &BTreeSet<B256>,
    ) -> BarrierInventory {
        BarrierInventory::build(
            L1_CHAIN_ID,
            Address::repeat_byte(2),
            9,
            B256::repeat_byte(21),
            Address::repeat_byte(1),
            7,
            55,
            B256::repeat_byte(22),
            4,
            50,
            60,
            B256::repeat_byte(23),
            B256::repeat_byte(24),
            locks,
            terminals,
            dispositions,
            policy,
        )
        .expect("valid barrier")
    }

    #[test]
    fn complete_inventory_rejects_wrong_root_watermark_and_out_of_order() {
        let barrier = inventory(
            vec![lock(2, 12), lock(1, 11)],
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
        );
        barrier.verify_complete().expect("canonical inventory");

        let mut wrong_root = barrier.clone();
        wrong_root.statement.complete_lock_root = B256::repeat_byte(90);
        assert_eq!(wrong_root.verify_complete(), Err(DrainError::WrongRoot));

        let mut wrong_watermark = barrier.clone();
        wrong_watermark.statement.lock_log_watermark += 1;
        assert_eq!(
            wrong_watermark.verify_complete(),
            Err(DrainError::WrongWatermark)
        );

        let mut out_of_order = barrier;
        out_of_order.locks.swap(0, 1);
        assert_eq!(
            out_of_order.verify_complete(),
            Err(DrainError::NonCanonicalOrder)
        );
    }

    #[test]
    fn durable_inventory_and_resolution_round_trip_exactly() {
        let source_lock = lock(1, 11);
        let transfer_id = source_lock.transfer_id();
        let barrier = inventory(
            vec![source_lock.clone()],
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
        );
        let barrier_bytes = barrier.durable_bytes().expect("encode barrier");
        assert_eq!(
            BarrierInventory::decode_durable(&barrier_bytes).expect("decode barrier"),
            barrier
        );
        let native_proof = barrier.locks[0]
            .native_barrier_proof(barrier.l1_chain_id, &barrier.statement)
            .expect("encode native fourth argument");
        assert_eq!(native_proof[0], NATIVE_BARRIER_PROOF_VERSION);
        assert_eq!(
            &native_proof[129..161],
            barrier
                .statement
                .registry_digest(barrier.l1_chain_id)
                .as_slice()
        );

        let terminals = BTreeMap::from([(transfer_id, terminal(&source_lock, 20))]);
        let dispositions = BTreeMap::from([(transfer_id, disposition(&source_lock, 21))]);
        let resolution = BarrierResolutionInventory::build(
            &barrier,
            &terminals,
            &dispositions,
            &BTreeSet::new(),
        )
        .expect("resolution");
        let resolution_bytes = resolution.durable_bytes().expect("encode resolution");
        assert_eq!(
            BarrierResolutionInventory::decode_durable(&resolution_bytes)
                .expect("decode resolution"),
            resolution
        );
    }

    #[test]
    fn delayed_terminal_and_disposition_are_required_before_resolution() {
        let source_lock = lock(1, 11);
        let transfer_id = source_lock.transfer_id();
        let barrier = inventory(
            vec![source_lock.clone()],
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
        );
        assert_eq!(barrier.statement.unresolved_count, 1);
        assert!(matches!(
            BarrierResolutionInventory::build(
                &barrier,
                &BTreeMap::new(),
                &BTreeMap::new(),
                &BTreeSet::new()
            ),
            Err(DrainError::UnresolvedLiability(id)) if id == transfer_id
        ));

        let terminals = BTreeMap::from([(transfer_id, terminal(&source_lock, 20))]);
        assert!(matches!(
            BarrierResolutionInventory::build(
                &barrier,
                &terminals,
                &BTreeMap::new(),
                &BTreeSet::new()
            ),
            Err(DrainError::UnresolvedLiability(id)) if id == transfer_id
        ));

        let dispositions = BTreeMap::from([(transfer_id, disposition(&source_lock, 21))]);
        let resolution = BarrierResolutionInventory::build(
            &barrier,
            &terminals,
            &dispositions,
            &BTreeSet::new(),
        )
        .expect("delayed evidence completes resolution");
        assert_eq!(resolution.resolution.resolved_count, 1);
        assert_eq!(resolution.resolution.remaining_unresolved_count, 0);
        assert_eq!(
            resolution.resolution.remaining_unresolved_root,
            FAST_EMPTY_UNRESOLVED_ROOT
        );
        resolution.verify(&barrier).expect("derived roots verify");
    }

    #[test]
    fn policy_liability_prevents_finish_even_with_complete_evidence() {
        let source_lock = lock(1, 11);
        let transfer_id = source_lock.transfer_id();
        let terminals = BTreeMap::from([(transfer_id, terminal(&source_lock, 20))]);
        let dispositions = BTreeMap::from([(transfer_id, disposition(&source_lock, 21))]);
        let barrier = inventory(
            vec![source_lock],
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
        );
        let policy = BTreeSet::from([transfer_id]);
        assert_eq!(
            BarrierResolutionInventory::build(&barrier, &terminals, &dispositions, &policy),
            Err(DrainError::PolicyBlocked(transfer_id))
        );
    }

    #[test]
    fn proof_mutation_and_noninclusion_count_are_rejected() {
        let mut barrier = inventory(
            vec![lock(1, 11), lock(2, 12), lock(3, 13)],
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
        );
        barrier.locks[1].complete_proof.siblings[0] = B256::repeat_byte(77);
        assert_eq!(barrier.verify_complete(), Err(DrainError::WrongRoot));

        let mut truncated = inventory(
            vec![lock(1, 11), lock(2, 12)],
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeSet::new(),
        );
        truncated.locks.pop();
        assert!(matches!(
            truncated.verify_complete(),
            Err(DrainError::WrongRoot | DrainError::InvalidProof)
        ));
    }

    #[test]
    fn all_nine_barriers_are_required_in_registry_order() {
        let entries = (0..DRAIN_PEER_COUNT)
            .map(|index| {
                (
                    Address::repeat_byte((index + 1) as u8),
                    B256::repeat_byte((index + 20) as u8),
                    B256::repeat_byte((index + 40) as u8),
                )
            })
            .collect::<Vec<_>>();
        let first = barriers_hash(&entries).expect("nine peers");
        let mut reordered = entries.clone();
        reordered.swap(0, 1);
        assert_ne!(first, barriers_hash(&reordered).expect("nine peers"));
        assert_eq!(
            barriers_hash(&entries[..8]),
            Err(DrainError::WrongPeerCount)
        );
    }

    #[test]
    fn checkpoint_cannot_be_acknowledged_from_empty_runtime_state() {
        let image = CheckpointImage {
            l1_chain_id: L1_CHAIN_ID,
            statement: FastCheckpointStatement {
                portal: Address::repeat_byte(1),
                old_epoch: 7,
                next_epoch: 8,
                next_roster_hash: B256::repeat_byte(30),
                final_zone_height: U256::from(100),
                final_block_hash: B256::repeat_byte(31),
                final_withdrawal_batch_index: 9,
                final_settlement_hash: B256::repeat_byte(32),
                checkpoint_log_term: 4,
                checkpoint_log_index: 50,
                checkpoint_height: U256::from(100),
                checkpoint_block_hash: B256::repeat_byte(31),
                checkpoint_state_root: B256::repeat_byte(33),
            },
            canonical_head_hash: B256::repeat_byte(31),
            canonical_state_root: B256::repeat_byte(33),
            witness_root: B256::ZERO,
            outcomes_root: B256::ZERO,
            replay_barriers_root: B256::ZERO,
            raft_prefix: Vec::new(),
            transfer_history: Vec::new(),
            replay_witnesses: Vec::new(),
            replay_barriers: Vec::new(),
            fast_service_journal: Vec::new(),
            batch_boundaries: Vec::new(),
            replenishment_state: Vec::new(),
            consensus_snapshot: Vec::new(),
        };
        assert_eq!(image.validate(), Err(DrainError::InvalidCheckpoint));
    }
}
