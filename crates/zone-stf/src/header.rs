//! Zone header construction and hashing.
//!
//! The zone block hash is computed from the simplified zone header:
//! `parentHash`, `beneficiary`, `stateRoot`, `transactionsRoot`, `receiptsRoot`,
//! `number`, `timestamp`.
//!
//! The header is RLP-encoded as a list and hashed with keccak256 to produce
//! the block hash.

use alloy_primitives::{B256, keccak256};
use alloy_rlp::{Encodable, RlpEncodable};

use crate::types::ZoneHeader;

/// RLP-encodable zone header for block hash computation.
///
/// This mirrors the simplified zone header defined in the prover spec.
/// Fields are ordered to match the canonical zone header encoding.
#[derive(RlpEncodable)]
struct RlpZoneHeader {
    parent_hash: B256,
    beneficiary: alloy_primitives::Address,
    state_root: B256,
    transactions_root: B256,
    receipts_root: B256,
    number: u64,
    timestamp: u64,
}

impl ZoneHeader {
    /// Compute the block hash for this zone header.
    ///
    /// The hash is `keccak256(rlp([parentHash, beneficiary, stateRoot,
    /// transactionsRoot, receiptsRoot, number, timestamp]))`.
    pub fn block_hash(&self) -> B256 {
        let rlp_header = RlpZoneHeader {
            parent_hash: self.parent_hash,
            beneficiary: self.beneficiary,
            state_root: self.state_root,
            transactions_root: self.transactions_root,
            receipts_root: self.receipts_root,
            number: self.number,
            timestamp: self.timestamp,
        };

        let mut buf = Vec::new();
        rlp_header.encode(&mut buf);
        keccak256(&buf)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256};

    use super::*;

    #[test]
    fn test_zone_header_hash_deterministic() {
        let header = ZoneHeader {
            parent_hash: B256::ZERO,
            beneficiary: Address::ZERO,
            state_root: B256::ZERO,
            transactions_root: B256::ZERO,
            receipts_root: B256::ZERO,
            number: 1,
            timestamp: 1000,
        };

        let hash1 = header.block_hash();
        let hash2 = header.block_hash();
        assert_eq!(hash1, hash2);
        assert_ne!(hash1, B256::ZERO);
    }
}
