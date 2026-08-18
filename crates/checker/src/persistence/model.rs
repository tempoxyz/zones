//! Durable checker coordinates and runtime status.

use alloy_primitives::{Address, B256};
use serde::{Deserialize, Serialize};

use crate::accounting::BlockDelta;

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

/// Immutable Zone and Portal identity bound to one database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Identity {
    pub(crate) l1_chain_id: u64,
    pub(crate) zone_chain_id: u64,
    pub(crate) zone_id: u32,
    pub(crate) portal: Address,
    pub(crate) creation: BlockRef,
}

/// Durable checker coverage state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Status {
    Verifying,
    Diverged {
        first_unchecked: BlockRef,
        observed_through: BlockRef,
    },
}

/// Singleton mutable database metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Metadata {
    pub(crate) identity: Identity,
    pub(crate) verified_zone: BlockRef,
    pub(crate) imported_tempo: BlockRef,
    pub(crate) observed_zone: BlockRef,
    pub(crate) status: Status,
}

/// One retained verified transition and its exact previous values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DeltaRecord {
    pub(crate) zone: BlockRef,
    pub(crate) parent: BlockRef,
    pub(crate) imported_tempo: BlockRef,
    pub(crate) imported_tempo_parent: BlockRef,
    pub(crate) delta: BlockDelta,
}

/// Current deterministic finding retained after verification stops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Finding {
    pub(crate) zone: BlockRef,
    pub(crate) summary: String,
}
