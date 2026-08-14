//! Durable checker records persisted in the local database.

use std::sync::Arc;

use crate::{
    CheckerBlockedReason,
    kernel::{Finding as FindingDetails, State, StateDelta},
};
use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};

/// A block coordinate used in durable chain records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct BlockNumHash {
    pub number: u64,
    pub hash: B256,
}

impl From<BlockNumHash> for alloy_eips::BlockNumHash {
    fn from(value: BlockNumHash) -> Self {
        Self::new(value.number, value.hash)
    }
}

/// The Zone and imported Tempo tips represented by a checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChainCut {
    pub zone: BlockNumHash,
    pub tempo: BlockNumHash,
}

/// Immutable chain and Portal identity bound to one persistence database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Identity {
    pub l1_chain_id: u64,
    pub zone_chain_id: u64,
    pub zone_id: u32,
    pub portal: Address,
    pub creation_block: B256,
    pub creation_height: u64,
}

/// Stable checkpoint key derived from its Zone tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct CheckpointId {
    pub height: u64,
    pub hash: B256,
}

impl From<BlockNumHash> for CheckpointId {
    fn from(value: BlockNumHash) -> Self {
        Self {
            height: value.number,
            hash: value.hash,
        }
    }
}

/// Whether checker coverage reaches the observed Zone tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Coverage {
    /// The verified and observed Zone tips agree.
    Complete,
    /// Canonical Zone history is retained locally but has not been verified yet.
    Recovering,
    /// An active finding prevents verification of the observed suffix.
    Gap {
        first_unchecked: BlockNumHash,
        observed_through: BlockNumHash,
    },
}

/// Mutable durable pointers and coverage state for the active checker history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Metadata {
    pub identity: Identity,
    /// Oldest checkpoint from which local reorg recovery is supported.
    pub recovery_checkpoint: CheckpointId,
    pub active_checkpoint: CheckpointId,
    /// Last Zone block represented by the durable checker state.
    pub verified_zone_tip: BlockNumHash,
    pub imported_tempo_tip: BlockNumHash,
    /// Latest canonical Zone head observed from the local node.
    pub observed_zone_tip: BlockNumHash,
    pub active_finding: Option<FindingKey>,
    /// Number of previously active findings removed from the canonical branch by reorgs.
    pub cleared_findings: u64,
    pub coverage: Coverage,
    /// Why the checker stopped verifying new work.
    pub blocked: Option<CheckerBlockedReason>,
}

/// One value stored in the metadata table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum MetaValue {
    Version(u32),
    Metadata(Box<Metadata>),
}

/// A durable state snapshot at a chain cut.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Checkpoint {
    pub cut: ChainCut,
    pub state: State,
}

/// One verified Zone transition and its imported Tempo advancement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct JournalEntry {
    pub zone: BlockNumHash,
    pub parent: BlockNumHash,
    pub imported_tempo: BlockNumHash,
    pub imported_tempo_parent: BlockNumHash,
    pub delta: StateDelta,
}

/// Stable coordinate for one durable checker finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct FindingKey {
    pub zone: BlockNumHash,
    pub operation: u32,
    pub code: u16,
}

/// Durable evidence for a checker divergence at one Zone coordinate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Finding {
    pub zone: BlockNumHash,
    pub parent: BlockNumHash,
    pub imported_tempo: Option<BlockNumHash>,
    pub imported_tempo_parent: Option<BlockNumHash>,
    pub details: FindingDetails,
    pub evidence_len: u32,
    pub evidence_digest: B256,
    pub summary: String,
}

/// The currently reconstructed durable checker state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub meta: Metadata,
    pub state: Arc<State>,
}
