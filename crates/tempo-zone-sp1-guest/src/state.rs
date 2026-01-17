//! State transition validation.
//!
//! Validates that the state transition from `prev_state_root` to `new_state_root`
//! is correct given the transactions in the batch.

use alloy_primitives::B256;

use crate::types::BatchInput;

/// Validate the state transition and return the new state root.
///
/// In mock mode (feature = "mock"), this simply returns the witness's new_state_root.
/// In production mode, this will perform stateless EVM verification.
#[cfg(feature = "mock")]
pub fn validate_and_compute_state_root(input: &BatchInput) -> B256 {
    // In mock mode, trust the witness
    // This is only for development and testing
    input.witness.new_state_root
}

/// Validate the state transition and return the new state root.
///
/// This is the production implementation that will perform stateless EVM
/// verification to ensure the state transition is valid.
#[cfg(not(feature = "mock"))]
pub fn validate_and_compute_state_root(input: &BatchInput) -> B256 {
    // TODO: Implement stateless EVM verification
    //
    // This will:
    // 1. Verify state proofs for all accessed accounts/storage
    // 2. Re-execute transactions using the witness data
    // 3. Compute the new state root from the modified state
    // 4. Verify it matches the claimed new_state_root
    //
    // For now, we trust the witness in all modes until stateless
    // execution is implemented.
    //
    // SECURITY: This is a placeholder. In production, this MUST
    // perform full stateless verification.

    // Placeholder: trust the witness
    // TODO: Remove this once stateless verification is implemented
    let _ = input.prev_state_root; // Acknowledge we should use this
    input.witness.new_state_root
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{StateWitness, BatchInput};
    use alloy_primitives::B256;

    fn mock_input(prev_root: B256, new_root: B256) -> BatchInput {
        BatchInput {
            processed_deposit_queue_hash: B256::ZERO,
            pending_deposit_queue_hash: B256::ZERO,
            deposits_consumed: vec![],
            prev_state_root: prev_root,
            expected_withdrawal_queue2: B256::ZERO,
            withdrawals: vec![],
            witness: StateWitness {
                new_state_root: new_root,
            },
        }
    }

    #[test]
    fn test_returns_witness_state_root() {
        let prev_root = B256::repeat_byte(0x11);
        let new_root = B256::repeat_byte(0x22);
        let input = mock_input(prev_root, new_root);

        let result = validate_and_compute_state_root(&input);
        assert_eq!(result, new_root);
    }
}
