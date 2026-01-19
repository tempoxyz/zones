// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { Deposit, DepositQueueTransition } from "./IZone.sol";

/// @title DepositQueue
/// @notice 2-slot queue for L1→zone deposits
/// @dev The zone has access to L1 state via TempoState, so it can verify deposits
///      against the actual currentDepositQueueHash at any proven Tempo block.
///      No "ceiling" slot is needed since the proof handles ancestry validation.
struct DepositQueue {
    bytes32 processed;  // where proofs have processed up to
    bytes32 current;    // head of queue (new deposits land here)
}

/// @title DepositQueueLib
/// @notice Library for managing the deposit queue hash chain
/// @dev Deposits are inserted on-chain and consumed by proofs.
///      The proof verifies deposits were processed correctly by reading
///      currentDepositQueueHash from Tempo state at the proven block.
library DepositQueueLib {
    error UnexpectedProcessedHash();

    /// @notice Enqueue a new deposit into the queue (on-chain operation)
    /// @dev Hash chain: newHash = keccak256(abi.encode(deposit, prevHash))
    /// @param queue The deposit queue
    /// @param depositData The deposit to enqueue
    /// @return newHeadQueueHash The new head queue hash
    function enqueue(
        DepositQueue storage queue,
        Deposit memory depositData
    ) internal returns (bytes32 newHeadQueueHash) {
        newHeadQueueHash = keccak256(abi.encode(depositData, queue.current));
        queue.current = newHeadQueueHash;
    }

    /// @notice Update processed hash after proof verification
    /// @dev Called when a batch proof is submitted. The proof itself validates
    ///      that nextProcessedHash is valid by reading Tempo state.
    /// @param queue The deposit queue
    /// @param transition The deposit queue transition containing prev/next hashes
    function dequeueWithProof(
        DepositQueue storage queue,
        DepositQueueTransition memory transition
    ) internal {
        // Verify the proof was generated against the correct processed state
        if (queue.processed != transition.prevProcessedHash) revert UnexpectedProcessedHash();

        // Update processed to where the proof processed up to
        queue.processed = transition.nextProcessedHash;
    }

    /// @notice Get the current state
    /// @param queue The deposit queue
    /// @return processed Where proofs have processed up to
    /// @return current Head of queue (new deposits land here)
    function getState(
        DepositQueue storage queue
    ) internal view returns (bytes32 processed, bytes32 current) {
        return (queue.processed, queue.current);
    }
}
