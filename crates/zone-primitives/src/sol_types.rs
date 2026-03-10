//! Solidity-compatible types used in proof public values and on-chain verification.

use alloy_primitives::B256;
use serde::{Deserialize, Serialize};

/// Zone block hash transition (prev -> next).
///
/// Mirrors the Solidity `BlockTransition` struct.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BlockTransition {
    pub prev_block_hash: B256,
    pub next_block_hash: B256,
}

/// Deposit queue processing transition.
///
/// Mirrors the Solidity `DepositQueueTransition` struct.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DepositQueueTransition {
    pub prev_processed_hash: B256,
    pub next_processed_hash: B256,
}
