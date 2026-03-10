// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { BlockTransition, DepositQueueTransition, IVerifier } from "./IZone.sol";

/// @title ISP1Verifier
/// @notice Interface for Succinct's on-chain SP1 proof verifier
interface ISP1Verifier {

    /// @notice Verify an SP1 proof
    /// @param programVKey The verification key of the SP1 program
    /// @param publicValues The public values committed by the proof
    /// @param proofBytes The encoded SP1 proof
    function verifyProof(bytes32 programVKey, bytes calldata publicValues, bytes calldata proofBytes) external view;

}

/// @title SP1ZoneVerifier
/// @notice Verifies zone batch proofs using Succinct's SP1 zkVM.
///         Implements the IVerifier interface so it can be used as a drop-in
///         replacement for the stub verifier.
contract SP1ZoneVerifier is IVerifier {

    /// @notice Succinct's on-chain SP1 proof verifier contract
    ISP1Verifier public immutable sp1Verifier;

    /// @notice SP1 verification key for the zone batch program
    bytes32 public zoneBatchVkey;

    /// @notice Address authorized to update the verification key
    address public owner;

    error Unauthorized();
    error VerificationFailed();

    event VkeyUpdated(bytes32 indexed oldVkey, bytes32 indexed newVkey);

    constructor(address _sp1Verifier, bytes32 _zoneBatchVkey) {
        sp1Verifier = ISP1Verifier(_sp1Verifier);
        zoneBatchVkey = _zoneBatchVkey;
        owner = msg.sender;
    }

    /// @notice Update the SP1 verification key (e.g., after recompiling the guest program)
    function setVkey(bytes32 _zoneBatchVkey) external {
        if (msg.sender != owner) revert Unauthorized();
        bytes32 oldVkey = zoneBatchVkey;
        zoneBatchVkey = _zoneBatchVkey;
        emit VkeyUpdated(oldVkey, _zoneBatchVkey);
    }

    /// @inheritdoc IVerifier
    function verify(
        uint64 tempoBlockNumber,
        uint64 anchorBlockNumber,
        bytes32 anchorBlockHash,
        uint64 expectedWithdrawalBatchIndex,
        address sequencer,
        BlockTransition calldata blockTransition,
        DepositQueueTransition calldata depositQueueTransition,
        bytes32 withdrawalQueueHash,
        bytes calldata,
        bytes calldata proof
    )
        external
        view
        override
        returns (bool)
    {
        // Encode the public values that the SP1 proof committed to.
        // This must match the BatchOutput encoding in the guest program.
        bytes memory publicValues = abi.encode(
            tempoBlockNumber,
            anchorBlockNumber,
            anchorBlockHash,
            expectedWithdrawalBatchIndex,
            sequencer,
            blockTransition.prevBlockHash,
            blockTransition.nextBlockHash,
            depositQueueTransition.prevProcessedHash,
            depositQueueTransition.nextProcessedHash,
            withdrawalQueueHash
        );

        sp1Verifier.verifyProof(zoneBatchVkey, publicValues, proof);

        return true;
    }

}
