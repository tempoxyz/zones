// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { IVerifier } from "../../../src/zone/IZone.sol";

/// @title MockVerifier
/// @notice Mock verifier for testing that always accepts proofs (configurable)
contract MockVerifier is IVerifier {
    bool public shouldAccept = true;

    // Track last verify call for test assertions
    bytes32 public lastProcessedDepositQueueHash;
    bytes32 public lastPendingDepositQueueHash;
    bytes32 public lastNewProcessedDepositQueueHash;
    bytes32 public lastPrevStateRoot;
    bytes32 public lastNewStateRoot;
    bytes32 public lastExpectedQueue2;
    bytes32 public lastUpdatedQueue2;
    bytes32 public lastNewWithdrawalsOnly;

    function setShouldAccept(bool _shouldAccept) external {
        shouldAccept = _shouldAccept;
    }

    function verify(
        bytes32 processedDepositQueueHash,
        bytes32 pendingDepositQueueHash,
        bytes32 newProcessedDepositQueueHash,
        bytes32 prevStateRoot,
        bytes32 newStateRoot,
        bytes32 expectedWithdrawalQueue2,
        bytes32 updatedWithdrawalQueue2,
        bytes32 newWithdrawalQueueOnly,
        bytes calldata, // verifierData
        bytes calldata  // proof
    ) external view returns (bool) {
        // Store last call params (via pure reads, no state modification in view)
        // In production, we'd use events or separate tracking
        // For testing, we just return the configured result
        return shouldAccept;
    }

    /// @notice Non-view version for testing that records call parameters
    function verifyAndRecord(
        bytes32 processedDepositQueueHash,
        bytes32 pendingDepositQueueHash,
        bytes32 newProcessedDepositQueueHash,
        bytes32 prevStateRoot,
        bytes32 newStateRoot,
        bytes32 expectedWithdrawalQueue2,
        bytes32 updatedWithdrawalQueue2,
        bytes32 newWithdrawalQueueOnly,
        bytes calldata,
        bytes calldata
    ) external returns (bool) {
        lastProcessedDepositQueueHash = processedDepositQueueHash;
        lastPendingDepositQueueHash = pendingDepositQueueHash;
        lastNewProcessedDepositQueueHash = newProcessedDepositQueueHash;
        lastPrevStateRoot = prevStateRoot;
        lastNewStateRoot = newStateRoot;
        lastExpectedQueue2 = expectedWithdrawalQueue2;
        lastUpdatedQueue2 = updatedWithdrawalQueue2;
        lastNewWithdrawalsOnly = newWithdrawalQueueOnly;

        return shouldAccept;
    }
}
