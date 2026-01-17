//! State root computation for Privacy Zone.
//!
//! Computes a deterministic state root from bundle state changes.
//! Uses a simple hash-based approach (not a full Merkle Patricia Trie).
//!
//! For a production L2, this would use proper sparse Merkle trie computation.
//! This simplified version provides a deterministic commitment that can be
//! verified and is sufficient for the validium architecture.

use alloy_primitives::{keccak256, B256};
use reth_revm::db::BundleState;

/// Compute a state root from the previous state root and bundle state changes.
///
/// This uses a simple incremental hash: new_root = keccak256(prev_root || changes_hash)
/// where changes_hash is a deterministic hash of all account and storage changes.
pub fn compute_state_root(prev_root: B256, bundle: &BundleState) -> B256 {
    let changes_hash = compute_changes_hash(bundle);
    
    let mut data = Vec::with_capacity(64);
    data.extend_from_slice(prev_root.as_slice());
    data.extend_from_slice(changes_hash.as_slice());
    keccak256(&data)
}

/// Compute a deterministic hash of all state changes in a bundle.
///
/// Sorts changes by address to ensure determinism.
fn compute_changes_hash(bundle: &BundleState) -> B256 {
    let mut data = Vec::new();
    
    // Sort accounts by address for determinism
    let mut accounts: Vec<_> = bundle.state.iter().collect();
    accounts.sort_by_key(|(addr, _)| *addr);
    
    for (address, account) in accounts {
        // Hash account address
        data.extend_from_slice(address.as_slice());
        
        // Hash account info changes
        if let Some(info) = &account.info {
            data.extend_from_slice(&info.balance.to_be_bytes::<32>());
            data.extend_from_slice(&info.nonce.to_be_bytes());
            data.extend_from_slice(info.code_hash.as_slice());
        } else {
            // Account was destroyed
            data.extend_from_slice(&[0u8; 32]);
        }
        
        // Hash storage changes (sorted by slot for determinism)
        let mut storage: Vec<_> = account.storage.iter().collect();
        storage.sort_by_key(|(slot, _)| *slot);
        
        for (slot, value) in storage {
            data.extend_from_slice(&slot.to_be_bytes::<32>());
            data.extend_from_slice(&value.present_value.to_be_bytes::<32>());
        }
    }
    
    if data.is_empty() {
        // No changes, return zero hash
        B256::ZERO
    } else {
        keccak256(&data)
    }
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

/// Merge multiple bundle states into one.
///
/// This is useful when processing multiple transactions and combining their state changes.
pub fn merge_bundles(bundles: Vec<BundleState>) -> BundleState {
    let mut result = BundleState::default();
    for bundle in bundles {
        result.extend(bundle);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_revm::{
        db::states::{bundle_account::BundleAccount, StorageSlot},
        state::AccountInfo,
    };
    use std::collections::HashMap;

    fn make_bundle_with_account(address: Address, balance: U256) -> BundleState {
        let mut state = HashMap::new();
        let account = BundleAccount {
            info: Some(AccountInfo {
                balance,
                nonce: 1,
                ..Default::default()
            }),
            original_info: None,
            storage: HashMap::new(),
            status: reth_revm::db::AccountStatus::Changed,
        };
        state.insert(address, account);
        
        BundleState {
            state,
            contracts: Default::default(),
            reverts: Default::default(),
            state_size: 0,
            reverts_size: 0,
        }
    }

    #[test]
    fn test_compute_state_root_deterministic() {
        let prev_root = B256::repeat_byte(0x01);
        let bundle = make_bundle_with_account(
            Address::repeat_byte(0x02),
            U256::from(1000),
        );
        
        let root1 = compute_state_root(prev_root, &bundle);
        let root2 = compute_state_root(prev_root, &bundle);
        
        assert_eq!(root1, root2);
        assert_ne!(root1, prev_root);
    }

    #[test]
    fn test_compute_state_root_different_changes() {
        let prev_root = B256::repeat_byte(0x01);
        
        let bundle1 = make_bundle_with_account(
            Address::repeat_byte(0x02),
            U256::from(1000),
        );
        let bundle2 = make_bundle_with_account(
            Address::repeat_byte(0x02),
            U256::from(2000),
        );
        
        let root1 = compute_state_root(prev_root, &bundle1);
        let root2 = compute_state_root(prev_root, &bundle2);
        
        assert_ne!(root1, root2);
    }

    #[test]
    fn test_empty_bundle() {
        let prev_root = B256::repeat_byte(0x01);
        let bundle = BundleState::default();
        
        let root = compute_state_root(prev_root, &bundle);
        
        // Empty bundle should still produce a valid (different) root
        assert_ne!(root, B256::ZERO);
    }

    #[test]
    fn test_transactions_root_empty() {
        let root = compute_transactions_root(&[]);
        assert_eq!(root, B256::ZERO);
    }

    #[test]
    fn test_transactions_root_deterministic() {
        let hashes = vec![
            B256::repeat_byte(0x01),
            B256::repeat_byte(0x02),
        ];
        
        let root1 = compute_transactions_root(&hashes);
        let root2 = compute_transactions_root(&hashes);
        
        assert_eq!(root1, root2);
    }
}
