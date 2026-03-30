//! Merkle Patricia Trie (MPT) proof verification.
//!
//! Provides verification functions for both zone state proofs (account and storage)
//! and Tempo state reads (via the deduplicated node pool).

use alloy_primitives::{B256, Bytes, U256, keccak256};
use alloy_trie::{TrieAccount, proof::verify_proof};
use nybbles::Nibbles;

use crate::types::ProverError;

/// Verify an account MPT proof against the state root.
///
/// Verifies that the account at `address` exists in the state trie rooted at
/// `state_root`, with the given `nonce`, `balance`, `storage_root`, and `code_hash`.
pub fn verify_account_proof(
    state_root: B256,
    address: alloy_primitives::Address,
    nonce: u64,
    balance: U256,
    storage_root: B256,
    code_hash: B256,
    proof: &[Bytes],
) -> Result<(), ProverError> {
    // The key in the account trie is keccak256(address), unpacked to nibbles.
    let key = Nibbles::unpack(keccak256(address));

    // The value is RLP([nonce, balance, storageRoot, codeHash]).
    let account = TrieAccount {
        nonce,
        balance,
        storage_root,
        code_hash,
    };
    let mut encoded = Vec::new();
    alloy_rlp::Encodable::encode(&account, &mut encoded);

    verify_proof(state_root, key, Some(encoded), proof)
        .map_err(|e| ProverError::InvalidProof(format!("account proof for {address}: {e}")))?;

    Ok(())
}

/// Verify a storage MPT proof for a single slot.
///
/// Verifies that `slot` has `value` in the storage trie rooted at `storage_root`.
pub fn verify_storage_proof(
    storage_root: B256,
    slot: U256,
    value: U256,
    proof: &[Bytes],
) -> Result<(), ProverError> {
    // The key in the storage trie is keccak256(slot as B256), unpacked to nibbles.
    let slot_b256 = B256::from(slot);
    let key = Nibbles::unpack(keccak256(slot_b256));

    // The value is RLP-encoded as a scalar (leading zeros stripped).
    // For zero values, the proof shows absence (None).
    let expected_value = if value.is_zero() {
        None
    } else {
        let mut encoded = Vec::new();
        alloy_rlp::Encodable::encode(&value, &mut encoded);
        Some(encoded)
    };

    verify_proof(storage_root, key, expected_value, proof)
        .map_err(|e| ProverError::InvalidProof(format!("storage proof for slot {slot}: {e}")))?;

    Ok(())
}

/// Verify an account absence proof against the state root.
///
/// Verifies that the account at `address` does **not** exist in the state trie
/// rooted at `state_root`. The proof is an exclusion proof (the path terminates
/// at a branch/extension that diverges from the target key).
pub fn verify_account_absence_proof(
    state_root: B256,
    address: alloy_primitives::Address,
    proof: &[Bytes],
) -> Result<(), ProverError> {
    let key = Nibbles::unpack(keccak256(address));

    // Passing `None` as expected_value verifies that the key is absent.
    verify_proof(state_root, key, None, proof)
        .map_err(|e| ProverError::InvalidProof(format!("absence proof for {address}: {e}")))?;

    Ok(())
}

/// The empty storage trie root hash.
pub fn empty_storage_root() -> B256 {
    alloy_trie::EMPTY_ROOT_HASH
}

/// Verify a node in the deduplicated pool.
///
/// Checks that `keccak256(rlp_data) == claimed_hash`.
pub fn verify_pool_node(claimed_hash: B256, rlp_data: &[u8]) -> Result<(), ProverError> {
    let actual_hash = keccak256(rlp_data);
    if actual_hash != claimed_hash {
        return Err(ProverError::InvalidProof(format!(
            "node pool hash mismatch: claimed={claimed_hash}, actual={actual_hash}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_verify_pool_node() {
        let data = b"hello world";
        let hash = keccak256(data);
        assert!(verify_pool_node(hash, data).is_ok());
        assert!(verify_pool_node(B256::ZERO, data).is_err());
    }
}
