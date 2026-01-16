// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/// @title IVerifier
/// @notice Abstract verifier interface for ZK proofs or TEE attestations
interface IVerifier {
    /// @notice Verify a batch commitment
    /// @param batchCommitment The hash of the batch commitment
    /// @param proof The proof or attestation data
    /// @return True if the proof is valid
    function verify(bytes32 batchCommitment, bytes calldata proof) external view returns (bool);
}
