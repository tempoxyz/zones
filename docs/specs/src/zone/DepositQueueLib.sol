// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { DepositQueueMessage, DepositQueueTransition } from "./IZone.sol";

/// @title DepositQueue
/// @notice 3-slot queue for L1→L2 deposits with proof ceiling pattern
struct DepositQueue {
    bytes32 processed;  // where proofs start (last proven state)
    bytes32 snapshot;   // stable target ceiling for current proof
    bytes32 current;    // head of queue (new messages land here)
}

/// @title DepositQueueLib
/// @notice Library for managing the deposit queue hash chain
/// @dev Deposits are inserted on-chain and popped (consumed) by proofs.
///      The 3-slot design handles the race where new deposits arrive while a proof is in-flight:
///      - `processed`: where the last proof ended
///      - `snapshot`: snapshot of `current` at proof start (stable ceiling)
///      - `current`: head where new deposits land
library DepositQueueLib {
    error InvalidState();

    /// @notice Enqueue a new message into the queue (on-chain operation)
    /// @dev Hash chain: newHash = keccak256(abi.encode(message, prevHash))
    /// @param q The deposit queue
    /// @param message The deposit queue message to enqueue
    /// @return newHeadQueueHash The new head queue hash
    function enqueue(
        DepositQueue storage q,
        DepositQueueMessage memory message
    ) internal returns (bytes32 newHeadQueueHash) {
        newHeadQueueHash = keccak256(abi.encode(message, q.current));
        q.current = newHeadQueueHash;
    }

    /// @notice Dequeue deposits via proof (proof operation)
    /// @dev Called when a batch proof is submitted. Validates that the expected state
    ///      matches the actual state (ensuring the proof was generated against correct state).
    ///      After this call:
    ///      - `processed = nextProcessed` (advance to where we actually processed)
    ///      - `snapshot = current` (snapshot new target for next proof)
    /// @param q The deposit queue
    /// @param transition The deposit queue transition containing prev/next state hashes
    function dequeueWithProof(
        DepositQueue storage q,
        DepositQueueTransition memory transition
    ) internal {
        // Verify the proof was generated against the correct state
        if (q.snapshot != transition.prevSnapshotHash) revert InvalidState();
        if (q.processed != transition.prevProcessedHash) revert InvalidState();

        q.processed = transition.nextProcessedHash;
        q.snapshot = q.current;
    }

    /// @notice Get the current state for proof generation
    /// @param q The deposit queue
    /// @return processed Where proofs start from
    /// @return snapshot Stable ceiling for the current proof
    /// @return current Head of queue (for reference, not used in proofs)
    function getState(
        DepositQueue storage q
    ) internal view returns (bytes32 processed, bytes32 snapshot, bytes32 current) {
        return (q.processed, q.snapshot, q.current);
    }
}
