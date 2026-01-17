// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { IVerifier } from "../../../src/zone/IZone.sol";

/// @title MockVerifier
/// @notice Mock verifier for testing that always accepts proofs (configurable)
contract MockVerifier is IVerifier {
    bool public shouldAccept = true;

    // Track last verify call for test assertions
    bytes32 public lastPrevProcessedDepositQueueHash;
    bytes32 public lastPrevPendingDepositQueueHash;
    bytes32 public lastNextProcessedDepositQueueHash;
    bytes32 public lastPrevStateRoot;
    bytes32 public lastNextStateRoot;
    bytes32 public lastPrevPendingWithdrawalQueueHash;
    bytes32 public lastNextPendingWithdrawalQueueHashIfFull;
    bytes32 public lastNextPendingWithdrawalQueueHashIfEmpty;

    function setShouldAccept(bool _shouldAccept) external {
        shouldAccept = _shouldAccept;
    }

    function verify(
        bytes32 prevProcessedDepositQueueHash,
        bytes32 prevPendingDepositQueueHash,
        bytes32 nextProcessedDepositQueueHash,
        bytes32 prevStateRoot,
        bytes32 nextStateRoot,
        bytes32 prevPendingWithdrawalQueueHash,
        bytes32 nextPendingWithdrawalQueueHashIfFull,
        bytes32 nextPendingWithdrawalQueueHashIfEmpty,
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
        bytes32 prevProcessedDepositQueueHash,
        bytes32 prevPendingDepositQueueHash,
        bytes32 nextProcessedDepositQueueHash,
        bytes32 prevStateRoot,
        bytes32 nextStateRoot,
        bytes32 prevPendingWithdrawalQueueHash,
        bytes32 nextPendingWithdrawalQueueHashIfFull,
        bytes32 nextPendingWithdrawalQueueHashIfEmpty,
        bytes calldata,
        bytes calldata
    ) external returns (bool) {
        lastPrevProcessedDepositQueueHash = prevProcessedDepositQueueHash;
        lastPrevPendingDepositQueueHash = prevPendingDepositQueueHash;
        lastNextProcessedDepositQueueHash = nextProcessedDepositQueueHash;
        lastPrevStateRoot = prevStateRoot;
        lastNextStateRoot = nextStateRoot;
        lastPrevPendingWithdrawalQueueHash = prevPendingWithdrawalQueueHash;
        lastNextPendingWithdrawalQueueHashIfFull = nextPendingWithdrawalQueueHashIfFull;
        lastNextPendingWithdrawalQueueHashIfEmpty = nextPendingWithdrawalQueueHashIfEmpty;

        return shouldAccept;
    }
}
