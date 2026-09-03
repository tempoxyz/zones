//! Durable checker coordinates and runtime status.

use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};

use crate::accounting::State;

/// Exact canonical block coordinate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) struct BlockRef {
    pub(crate) number: u64,
    pub(crate) hash: B256,
}

impl BlockRef {
    pub(crate) const fn new(number: u64, hash: B256) -> Self {
        Self { number, hash }
    }
}

impl From<alloy_eips::BlockNumHash> for BlockRef {
    fn from(value: alloy_eips::BlockNumHash) -> Self {
        Self::new(value.number, value.hash)
    }
}

impl From<BlockRef> for alloy_eips::BlockNumHash {
    fn from(value: BlockRef) -> Self {
        Self::new(value.number, value.hash)
    }
}

/// Authenticated initial state used to create or rebuild the checker database.
pub(crate) struct Checkpoint {
    pub(crate) identity: Identity,
    pub(crate) zone: BlockRef,
    pub(crate) tempo: BlockRef,
    pub(crate) state: State,
}

/// Immutable Zone and Portal identity bound to one database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Identity {
    /// L1 chain the Portal is deployed on.
    pub(crate) l1_chain_id: u64,
    /// Chain ID of the local Zone, rejecting cross-chain databases.
    pub(crate) zone_chain_id: u64,
    pub(crate) zone_id: u32,
    pub(crate) portal: Address,
    /// Coordinate of the Portal creation block.
    pub(crate) creation: BlockRef,
}

/// Current deterministic finding retained after verification stops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Finding {
    pub(crate) zone: BlockRef,
    pub(crate) summary: String,
}

/// Durable checker coverage state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Status {
    Verifying,
    /// Verification has stopped at this deterministic finding.
    Diverged {
        finding: Finding,
    },
}

/// Singleton mutable database metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Metadata {
    pub(crate) identity: Identity,
    /// Last Zone block the checker has fully verified.
    pub(crate) verified_zone: BlockRef,
    /// Last Tempo/L1 block imported by the verified Zone tip.
    pub(crate) imported_tempo: BlockRef,
    /// Latest canonical Zone tip observed, which may be ahead of `verified_zone`.
    pub(crate) observed_zone: BlockRef,
    pub(crate) status: Status,
}
