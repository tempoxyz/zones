// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/// @title IVerifier
/// @notice Abstract verifier interface for ZK proofs or TEE attestations
interface IVerifier {
    /// @notice Verify a batch state transition
    /// @param checkpointedDepositsHash Where proof starts (from portal state)
    /// @param newProcessedDepositsHash Where zone processed up to (from batch)
    /// @param prevStateRoot Previous state root
    /// @param newStateRoot New state root after execution
    /// @param expectedQueue2 What proof assumed queue2 was
    /// @param updatedQueue2 Queue2 with new withdrawals added to innermost
    /// @param newWithdrawalsOnly New withdrawals only (if queue2 was empty)
    /// @param proof The validity proof or TEE attestation
    /// @return True if the proof is valid
    function verify(
        bytes32 checkpointedDepositsHash,
        bytes32 newProcessedDepositsHash,
        bytes32 prevStateRoot,
        bytes32 newStateRoot,
        bytes32 expectedQueue2,
        bytes32 updatedQueue2,
        bytes32 newWithdrawalsOnly,
        bytes calldata proof
    ) external view returns (bool);
}
