// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { DepositQueueMessage } from "./IZone.sol";

/// @title DepositQueue
/// @notice 3-slot queue for L1→L2 deposits with proof ceiling pattern
struct DepositQueue {
    bytes32 processed;  // where proofs start (last proven state)
    bytes32 pending;    // stable target ceiling for current proof
    bytes32 current;    // head of queue (new messages land here)
}

/// @title DepositQueueLib
/// @notice Library for managing the deposit queue hash chain
/// @dev Deposits are inserted on-chain and popped (consumed) by proofs.
///      The 3-slot design handles the race where new deposits arrive while a proof is in-flight:
///      - `processed`: where the last proof ended
///      - `pending`: snapshot of `current` at proof start (stable ceiling)
///      - `current`: head where new deposits land
library DepositQueueLib {
    error InvalidState();

    /// @notice Insert a new message into the queue (on-chain operation)
    /// @dev Hash chain: newHash = keccak256(abi.encode(message, prevHash))
    /// @param q The deposit queue
    /// @param message The deposit queue message to insert
    /// @return newCurrent The new current queue hash
    function insert(
        DepositQueue storage q,
        DepositQueueMessage memory message
    ) internal returns (bytes32 newCurrent) {
        newCurrent = keccak256(abi.encode(message, q.current));
        q.current = newCurrent;
    }

    /// @notice Consume deposits via proof (proof operation)
    /// @dev Called when a batch proof is submitted. Validates that the expected state
    ///      matches the actual state (ensuring the proof was generated against correct state).
    ///      After this call:
    ///      - `processed = newProcessed` (advance to where we actually processed)
    ///      - `pending = current` (snapshot new target for next proof)
    /// @param q The deposit queue
    /// @param expectedProcessed The processed value the proof was generated against (must match current)
    /// @param expectedPending The pending value the proof was generated against (must match current)
    /// @param newProcessed Where the proof processed up to
    function popWithProof(
        DepositQueue storage q,
        bytes32 expectedProcessed,
        bytes32 expectedPending,
        bytes32 newProcessed
    ) internal {
        // Verify the proof was generated against the correct state
        if (q.processed != expectedProcessed) revert InvalidState();
        if (q.pending != expectedPending) revert InvalidState();

        q.processed = newProcessed;
        q.pending = q.current;
    }

    /// @notice Get the current state for proof generation
    /// @param q The deposit queue
    /// @return processed Where proofs start from
    /// @return pending Stable ceiling for the current proof
    /// @return current Head of queue (for reference, not used in proofs)
    function getState(
        DepositQueue storage q
    ) internal view returns (bytes32 processed, bytes32 pending, bytes32 current) {
        return (q.processed, q.pending, q.current);
    }
}
