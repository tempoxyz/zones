// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { IVerifier } from "../../../src/zone/IZone.sol";

/// @title MockVerifier
/// @notice Mock verifier for testing that always accepts proofs (configurable)
contract MockVerifier is IVerifier {
    bool public shouldAccept = true;

    // Track last verify call for test assertions
    bytes32 public lastProcessedDepositsHash;
    bytes32 public lastPendingDepositsHash;
    bytes32 public lastNewProcessedDepositsHash;
    bytes32 public lastPrevStateRoot;
    bytes32 public lastNewStateRoot;
    bytes32 public lastExpectedQueue2;
    bytes32 public lastUpdatedQueue2;
    bytes32 public lastNewWithdrawalsOnly;

    function setShouldAccept(bool _shouldAccept) external {
        shouldAccept = _shouldAccept;
    }

    function verify(
        bytes32 processedDepositsHash,
        bytes32 pendingDepositsHash,
        bytes32 newProcessedDepositsHash,
        bytes32 prevStateRoot,
        bytes32 newStateRoot,
        bytes32 expectedQueue2,
        bytes32 updatedQueue2,
        bytes32 newWithdrawalsOnly,
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
        bytes32 processedDepositsHash,
        bytes32 pendingDepositsHash,
        bytes32 newProcessedDepositsHash,
        bytes32 prevStateRoot,
        bytes32 newStateRoot,
        bytes32 expectedQueue2,
        bytes32 updatedQueue2,
        bytes32 newWithdrawalsOnly,
        bytes calldata,
        bytes calldata
    ) external returns (bool) {
        lastProcessedDepositsHash = processedDepositsHash;
        lastPendingDepositsHash = pendingDepositsHash;
        lastNewProcessedDepositsHash = newProcessedDepositsHash;
        lastPrevStateRoot = prevStateRoot;
        lastNewStateRoot = newStateRoot;
        lastExpectedQueue2 = expectedQueue2;
        lastUpdatedQueue2 = updatedQueue2;
        lastNewWithdrawalsOnly = newWithdrawalsOnly;

        return shouldAccept;
    }
}
