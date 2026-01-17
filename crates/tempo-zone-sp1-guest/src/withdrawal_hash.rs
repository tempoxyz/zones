//! Withdrawal hash chain computation.
//!
//! Computes keccak256(abi.encode(withdrawal, tail_hash)) following the exact
//! Solidity encoding for compatibility with on-chain verification.
//!
//! Hash chain structure (oldest withdrawal outermost for O(1) pop):
//! ```text
//! queue = keccak256(abi.encode(w1, keccak256(abi.encode(w2, keccak256(abi.encode(w3, bytes32(0)))))))
//! ```
//! Where w1 is oldest (first to be popped) and w3 is newest.

use alloy_primitives::{B256, keccak256};
use alloy_sol_types::{SolType, sol_data};

use crate::types::Withdrawal;

/// ABI type for withdrawal encoding matching Solidity:
/// ```solidity
/// struct Withdrawal {
///     address sender;
///     address to;
///     uint128 amount;
///     bytes32 memo;
///     uint64 gasLimit;
///     address fallbackRecipient;
///     bytes data;
/// }
/// ```
/// Followed by bytes32 tailHash for the hash chain.
type WithdrawalHashTuple = (
    sol_data::Address,        // sender
    sol_data::Address,        // to
    sol_data::Uint<128>,      // amount
    sol_data::FixedBytes<32>, // memo
    sol_data::Uint<64>,       // gasLimit
    sol_data::Address,        // fallbackRecipient
    sol_data::Bytes,          // data
    sol_data::FixedBytes<32>, // tailHash
);

/// Compute the hash of a single withdrawal with a tail hash.
///
/// This matches the Solidity: `keccak256(abi.encode(withdrawal, tailHash))`
fn hash_withdrawal(withdrawal: &Withdrawal, tail_hash: B256) -> B256 {
    // Convert U128 to u128 for ABI encoding
    let amount: u128 = withdrawal.amount.to();
    let encoded = WithdrawalHashTuple::abi_encode_params(&(
        withdrawal.sender,
        withdrawal.to,
        amount,
        withdrawal.memo,
        withdrawal.gas_limit,
        withdrawal.fallback_recipient,
        withdrawal.data.clone(),
        tail_hash,
    ));
    keccak256(&encoded)
}

/// Compute withdrawal queue hashes for a batch.
///
/// Returns a tuple of:
/// - `updated_withdrawal_queue2`: The queue hash after appending all withdrawals to `expected_queue2`
/// - `new_withdrawal_queue_only`: The hash of only the new withdrawals (starting from zero hash)
///
/// The hash chain is built with the oldest withdrawal outermost for O(1) pop:
/// ```text
/// queue = hash(w_oldest, hash(w_next, hash(w_newest, tail)))
/// ```
///
/// To achieve this, we iterate in reverse: newest first (innermost), oldest last (outermost).
pub fn compute_withdrawal_hashes(
    withdrawals: &[Withdrawal],
    expected_queue2: B256,
) -> (B256, B256) {
    // Start with the expected queue hash for the updated queue
    let mut updated_queue = expected_queue2;
    // Start with zero hash for the new-only queue
    let mut new_only_queue = B256::ZERO;

    // Iterate in reverse: newest withdrawal becomes innermost, oldest becomes outermost
    for withdrawal in withdrawals.iter().rev() {
        // Build the new-only queue (just the new withdrawals)
        new_only_queue = hash_withdrawal(withdrawal, new_only_queue);
    }

    // For updated queue, we need to wrap expected_queue2 inside the new withdrawals
    // The new withdrawals should be appended (become poppable after existing ones drain)
    // So: hash(existing_oldest, hash(..., hash(new_oldest, hash(new_next, hash(new_newest, 0)))))
    // But we only have the hash of existing queue, not the individual withdrawals.
    // The correct approach: new withdrawals wrap around expected_queue2 as their innermost tail.
    // This means the oldest NEW withdrawal becomes outermost of the new portion.
    for withdrawal in withdrawals.iter().rev() {
        updated_queue = hash_withdrawal(withdrawal, updated_queue);
    }

    (updated_queue, new_only_queue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, Bytes, U128};

    fn make_withdrawal(to_byte: u8, amount: u128) -> Withdrawal {
        Withdrawal {
            sender: Address::repeat_byte(0xAA),
            to: Address::repeat_byte(to_byte),
            amount: U128::from(amount),
            memo: B256::ZERO,
            gas_limit: 0,
            fallback_recipient: Address::ZERO,
            data: Bytes::new(),
        }
    }

    #[test]
    fn test_empty_withdrawals() {
        let expected = B256::repeat_byte(0x42);
        let (updated, new_only) = compute_withdrawal_hashes(&[], expected);

        // With no withdrawals, updated should equal expected
        assert_eq!(updated, expected);
        // New-only should be zero
        assert_eq!(new_only, B256::ZERO);
    }

    #[test]
    fn test_single_withdrawal() {
        let withdrawal = make_withdrawal(0x01, 1000);

        let expected = B256::ZERO;
        let (updated, new_only) = compute_withdrawal_hashes(&[withdrawal], expected);

        // With zero expected, both should be equal
        assert_eq!(updated, new_only);
        // Should not be zero (we added a withdrawal)
        assert_ne!(updated, B256::ZERO);
    }

    #[test]
    fn test_multiple_withdrawals() {
        let withdrawals = vec![make_withdrawal(0x01, 1000), make_withdrawal(0x02, 2000)];

        let expected = B256::repeat_byte(0x42);
        let (updated, new_only) = compute_withdrawal_hashes(&withdrawals, expected);

        // Updated and new_only should differ because they have different starting points
        assert_ne!(updated, new_only);
    }

    #[test]
    fn test_hash_chain_order() {
        // Verify that oldest withdrawal is outermost (first to be popped)
        let w1 = make_withdrawal(0x01, 1000); // oldest
        let w2 = make_withdrawal(0x02, 2000);
        let w3 = make_withdrawal(0x03, 3000); // newest

        let (_, queue) = compute_withdrawal_hashes(&[w1.clone(), w2.clone(), w3.clone()], B256::ZERO);

        // The queue should be: hash(w1, hash(w2, hash(w3, 0)))
        // Build it manually to verify
        let inner = hash_withdrawal(&w3, B256::ZERO);
        let middle = hash_withdrawal(&w2, inner);
        let outer = hash_withdrawal(&w1, middle);

        assert_eq!(queue, outer, "oldest withdrawal should be outermost");
    }
}
