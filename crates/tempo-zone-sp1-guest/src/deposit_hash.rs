//! Deposit hash chain computation.
//!
//! Computes hash chains for deposits following the exact Solidity encoding
//! for compatibility with on-chain verification.
//!
//! Note: The deposit queue uses DepositQueueMessage which wraps Deposit with a kind field.
//! This module computes the hash for the Deposit struct itself.

use alloy_primitives::{B256, keccak256};
use alloy_sol_types::{SolType, sol_data};

use crate::types::Deposit;

/// ABI type for deposit encoding matching Solidity:
/// ```solidity
/// struct Deposit {
///     bytes32 l1BlockHash;
///     uint64 l1BlockNumber;
///     uint64 l1Timestamp;
///     address sender;
///     address to;
///     uint128 amount;
///     bytes32 memo;
/// }
/// ```
/// Followed by bytes32 tailHash for the hash chain.
type DepositHashTuple = (
    sol_data::FixedBytes<32>, // l1BlockHash
    sol_data::Uint<64>,       // l1BlockNumber
    sol_data::Uint<64>,       // l1Timestamp
    sol_data::Address,        // sender
    sol_data::Address,        // to
    sol_data::Uint<128>,      // amount
    sol_data::FixedBytes<32>, // memo
    sol_data::FixedBytes<32>, // tailHash
);

/// Compute the hash of a single deposit with a tail hash.
///
/// This matches the Solidity: `keccak256(abi.encode(deposit, tailHash))`
fn hash_deposit(deposit: &Deposit, tail_hash: B256) -> B256 {
    let amount: u128 = deposit.amount.to();
    let encoded = DepositHashTuple::abi_encode_params(&(
        deposit.l1_block_hash,
        deposit.l1_block_number,
        deposit.l1_timestamp,
        deposit.sender,
        deposit.to,
        amount,
        deposit.memo,
        tail_hash,
    ));
    keccak256(&encoded)
}

/// Compute the new processed deposit queue hash after consuming deposits.
///
/// Starting from `current_processed_hash`, appends each consumed deposit to
/// produce the new processed queue hash.
///
/// # Arguments
/// * `deposits_consumed` - Deposits consumed in this batch, in order
/// * `current_processed_hash` - The processed queue hash before this batch
///
/// # Returns
/// The new processed queue hash after consuming all deposits
pub fn compute_new_processed_hash(
    deposits_consumed: &[Deposit],
    current_processed_hash: B256,
) -> B256 {
    let mut hash = current_processed_hash;

    for deposit in deposits_consumed {
        hash = hash_deposit(deposit, hash);
    }

    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U128};

    fn make_deposit(sender_byte: u8, to_byte: u8, amount: u128) -> Deposit {
        Deposit {
            l1_block_hash: B256::ZERO,
            l1_block_number: 1,
            l1_timestamp: 1000,
            sender: Address::repeat_byte(sender_byte),
            to: Address::repeat_byte(to_byte),
            amount: U128::from(amount),
            memo: B256::ZERO,
        }
    }

    #[test]
    fn test_no_deposits() {
        let current = B256::repeat_byte(0x42);
        let new_hash = compute_new_processed_hash(&[], current);

        // With no deposits, hash should remain unchanged
        assert_eq!(new_hash, current);
    }

    #[test]
    fn test_single_deposit() {
        let deposit = make_deposit(0x01, 0x02, 1000);

        let current = B256::ZERO;
        let new_hash = compute_new_processed_hash(&[deposit], current);

        // Should produce a non-zero hash
        assert_ne!(new_hash, B256::ZERO);
    }

    #[test]
    fn test_multiple_deposits() {
        let deposits = vec![make_deposit(0x01, 0x02, 1000), make_deposit(0x03, 0x04, 2000)];

        let current = B256::ZERO;
        let new_hash = compute_new_processed_hash(&deposits, current);

        // Verify it's deterministic
        let new_hash2 = compute_new_processed_hash(&deposits, current);
        assert_eq!(new_hash, new_hash2);
    }

    #[test]
    fn test_order_matters() {
        let deposit1 = make_deposit(0x01, 0x02, 1000);
        let deposit2 = make_deposit(0x03, 0x04, 2000);

        let current = B256::ZERO;
        let hash_1_2 = compute_new_processed_hash(&[deposit1.clone(), deposit2.clone()], current);
        let hash_2_1 = compute_new_processed_hash(&[deposit2, deposit1], current);

        // Order should produce different hashes
        assert_ne!(hash_1_2, hash_2_1);
    }
}
