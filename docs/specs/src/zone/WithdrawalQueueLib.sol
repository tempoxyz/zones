// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { Withdrawal } from "./IZone.sol";

/// @title WithdrawalQueue
/// @notice 2-slot queue for L2→L1 withdrawals with swap pattern
struct WithdrawalQueue {
    bytes32 queue1;  // active queue (being drained on-chain)
    bytes32 queue2;  // pending queue (being filled by proofs)
}

/// @title WithdrawalQueueLib
/// @notice Library for managing the withdrawal queue hash chain
/// @dev Withdrawals are inserted by proofs and popped on-chain by the sequencer.
///      The 2-queue design handles the race where the sequencer drains queue1 while a proof is in-flight:
///      - `queue1`: active queue being processed
///      - `queue2`: pending queue receiving new withdrawals from proofs
///      When queue1 empties, swap in queue2.
library WithdrawalQueueLib {
    error NoWithdrawals();
    error InvalidWithdrawal();
    error UnexpectedQueue2State();

    /// @notice Pop the next withdrawal from the queue (on-chain operation)
    /// @dev Verifies the withdrawal is at the head of queue1 and advances the queue.
    ///      If queue1 is empty, automatically swaps in queue2 first.
    /// @param q The withdrawal queue
    /// @param w The withdrawal to pop (must be at head of queue)
    /// @param remainingQueue The hash of the remaining queue after this withdrawal
    function pop(
        WithdrawalQueue storage q,
        Withdrawal calldata w,
        bytes32 remainingQueue
    ) internal {
        // Swap in queue2 if queue1 is empty
        if (q.queue1 == bytes32(0)) {
            if (q.queue2 == bytes32(0)) revert NoWithdrawals();
            q.queue1 = q.queue2;
            q.queue2 = bytes32(0);
        }

        // Verify this is the head of queue1
        // Queue structure: oldest withdrawal at outermost layer for O(1) removal
        if (keccak256(abi.encode(w, remainingQueue)) != q.queue1) {
            revert InvalidWithdrawal();
        }

        // Advance queue1
        if (remainingQueue == bytes32(0)) {
            // Queue1 exhausted, swap in queue2
            q.queue1 = q.queue2;
            q.queue2 = bytes32(0);
        } else {
            q.queue1 = remainingQueue;
        }
    }

    /// @notice Insert new withdrawals via proof (proof operation)
    /// @dev Called when a batch proof is submitted. The proof computed new withdrawals
    ///      and provides two outputs to handle the race condition:
    ///      - `updated`: new withdrawals appended to the expected queue2
    ///      - `fresh`: new withdrawals as a fresh queue (if queue2 was swapped away)
    ///
    ///      The race condition:
    ///      1. Proof starts, sees queue2 = X
    ///      2. Sequencer drains queue1, triggers swap: queue1 = X, queue2 = 0
    ///      3. Proof submits expecting queue2 = X, but it's now 0
    ///
    ///      Solution: proof provides both outputs, we use the appropriate one.
    /// @param q The withdrawal queue
    /// @param expected What queue2 was when the proof started
    /// @param updated New withdrawals appended to expected (innermost = newest)
    /// @param fresh New withdrawals as standalone queue (if queue2 was empty/swapped)
    function insertWithProof(
        WithdrawalQueue storage q,
        bytes32 expected,
        bytes32 updated,
        bytes32 fresh
    ) internal {
        if (q.queue2 == expected) {
            // No swap happened during proving, use the appended version
            q.queue2 = updated;
        } else if (q.queue2 == bytes32(0)) {
            // Swap happened during proving, queue2 is now empty, use fresh
            q.queue2 = fresh;
        } else {
            // Unexpected state: queue2 is neither expected nor empty
            revert UnexpectedQueue2State();
        }
    }

    /// @notice Get the current state for proof generation
    /// @param q The withdrawal queue
    /// @return queue1 Active queue being drained
    /// @return queue2 Pending queue being filled
    function getState(
        WithdrawalQueue storage q
    ) internal view returns (bytes32 queue1, bytes32 queue2) {
        return (q.queue1, q.queue2);
    }

    /// @notice Check if there are any pending withdrawals
    /// @param q The withdrawal queue
    /// @return True if either queue has withdrawals
    function hasWithdrawals(WithdrawalQueue storage q) internal view returns (bool) {
        return q.queue1 != bytes32(0) || q.queue2 != bytes32(0);
    }
}
