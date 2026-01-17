//! State root computation for Privacy Zone.
//!
//! Computes a deterministic state root from deposit and transaction data.
//! Uses a simple hash-based approach for the validium architecture.

use alloy_primitives::{keccak256, B256};

/// Compute a simple state root from prev_root, deposits_hash, and transactions_root.
///
/// This represents the state changes from processing deposits in a block.
/// new_root = keccak256(prev_root || deposits_hash || transactions_root)
pub fn compute_simple_state_root(
    prev_root: B256,
    deposits_hash: B256,
    transactions_root: B256,
) -> B256 {
    let mut data = Vec::with_capacity(96);
    data.extend_from_slice(prev_root.as_slice());
    data.extend_from_slice(deposits_hash.as_slice());
    data.extend_from_slice(transactions_root.as_slice());
    keccak256(&data)
}

/// Compute transactions root from a list of transaction hashes.
pub fn compute_transactions_root(tx_hashes: &[B256]) -> B256 {
    if tx_hashes.is_empty() {
        return B256::ZERO;
    }

    let mut data = Vec::with_capacity(tx_hashes.len() * 32);
    for hash in tx_hashes {
        data.extend_from_slice(hash.as_slice());
    }
    keccak256(&data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_simple_state_root_deterministic() {
        let prev_root = B256::repeat_byte(0x01);
        let deposits_hash = B256::repeat_byte(0x02);
        let transactions_root = B256::repeat_byte(0x03);

        let root1 = compute_simple_state_root(prev_root, deposits_hash, transactions_root);
        let root2 = compute_simple_state_root(prev_root, deposits_hash, transactions_root);

        assert_eq!(root1, root2);
        assert_ne!(root1, prev_root);
    }

    #[test]
    fn test_compute_simple_state_root_different_inputs() {
        let prev_root = B256::repeat_byte(0x01);

        let root1 = compute_simple_state_root(prev_root, B256::repeat_byte(0x02), B256::ZERO);
        let root2 = compute_simple_state_root(prev_root, B256::repeat_byte(0x03), B256::ZERO);

        assert_ne!(root1, root2);
    }

    #[test]
    fn test_transactions_root_empty() {
        let root = compute_transactions_root(&[]);
        assert_eq!(root, B256::ZERO);
    }

    #[test]
    fn test_transactions_root_deterministic() {
        let hashes = vec![B256::repeat_byte(0x01), B256::repeat_byte(0x02)];

        let root1 = compute_transactions_root(&hashes);
        let root2 = compute_transactions_root(&hashes);

        assert_eq!(root1, root2);
    }
}
