// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { Withdrawal, WithdrawalQueueTransition } from "../../src/zone/IZone.sol";
import {
    EMPTY_SENTINEL,
    WithdrawalQueue,
    WithdrawalQueueLib
} from "../../src/zone/WithdrawalQueueLib.sol";
import { Test } from "forge-std/Test.sol";

/// @title WithdrawalQueueHarness
/// @notice Test harness that wraps the library to convert memory to calldata
contract WithdrawalQueueHarness {

    using WithdrawalQueueLib for WithdrawalQueue;

    WithdrawalQueue internal queue;

    function enqueue(WithdrawalQueueTransition memory transition) external {
        queue.enqueue(transition);
    }

    function dequeue(Withdrawal calldata withdrawal, bytes32 remainingQueue) external {
        queue.dequeue(withdrawal, remainingQueue);
    }

    function hasWithdrawals() external view returns (bool) {
        return queue.hasWithdrawals();
    }

    function length() external view returns (uint256) {
        return queue.length();
    }

    function isFull() external view returns (bool) {
        return queue.isFull();
    }

    function head() external view returns (uint256) {
        return queue.head;
    }

    function tail() external view returns (uint256) {
        return queue.tail;
    }

    function capacity() external view returns (uint256) {
        return queue.capacity;
    }

    function setCapacity(uint256 cap) external {
        queue.capacity = cap;
    }

    function slots(uint256 index) external view returns (bytes32) {
        return queue.slots[index];
    }

}

/// @title WithdrawalQueueLibTest
/// @notice Direct tests for WithdrawalQueueLib functionality
contract WithdrawalQueueLibTest is Test {

    WithdrawalQueueHarness internal harness;

    address public alice = address(0x200);
    address public bob = address(0x300);
    address public charlie = address(0x400);

    function setUp() public {
        harness = new WithdrawalQueueHarness();
        harness.setCapacity(256);
    }

    /*//////////////////////////////////////////////////////////////
                          INITIAL STATE TESTS
    //////////////////////////////////////////////////////////////*/

    function test_initialState() public view {
        assertEq(harness.head(), 0);
        assertEq(harness.tail(), 0);
        assertEq(harness.capacity(), 256);
        assertFalse(harness.hasWithdrawals());
        assertEq(harness.length(), 0);
    }

    /*//////////////////////////////////////////////////////////////
                            ENQUEUE TESTS
    //////////////////////////////////////////////////////////////*/

    function test_enqueue_singleBatch() public {
        Withdrawal memory w = _makeWithdrawal(alice, bob, 100e6);
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: wHash }));

        assertEq(harness.head(), 0);
        assertEq(harness.tail(), 1);
        assertEq(harness.slots(0), wHash);
        assertTrue(harness.hasWithdrawals());
        assertEq(harness.length(), 1);
    }

    function test_enqueue_multipleBatches() public {
        bytes32 h1 = keccak256("batch1");
        bytes32 h2 = keccak256("batch2");
        bytes32 h3 = keccak256("batch3");

        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: h1 }));
        assertEq(harness.tail(), 1);

        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: h2 }));
        assertEq(harness.tail(), 2);

        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: h3 }));
        assertEq(harness.tail(), 3);

        assertEq(harness.slots(0), h1);
        assertEq(harness.slots(1), h2);
        assertEq(harness.slots(2), h3);
        assertEq(harness.length(), 3);
    }

    function test_enqueue_emptyTransition_noOp() public {
        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }));

        assertEq(harness.head(), 0);
        assertEq(harness.tail(), 0);
        assertFalse(harness.hasWithdrawals());
    }

    function test_enqueue_mixedEmptyAndNonEmpty() public {
        bytes32 h1 = keccak256("batch1");
        bytes32 h2 = keccak256("batch2");

        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: h1 }));
        assertEq(harness.tail(), 1);

        // Empty batch - no change
        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }));
        assertEq(harness.tail(), 1);

        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: h2 }));
        assertEq(harness.tail(), 2);

        // Slots should be contiguous
        assertEq(harness.slots(0), h1);
        assertEq(harness.slots(1), h2);
    }

    function test_enqueue_revertsWhenFull() public {
        harness.setCapacity(2);

        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: keccak256("b1") }));
        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: keccak256("b2") }));

        vm.expectRevert(WithdrawalQueueLib.WithdrawalQueueFull.selector);
        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: keccak256("b3") }));
    }

    function test_enqueue_afterDequeueReuseSlots() public {
        harness.setCapacity(2);

        Withdrawal memory w1 = _makeWithdrawal(alice, bob, 100e6);
        bytes32 h1 = keccak256(abi.encode(w1, EMPTY_SENTINEL));
        bytes32 h2 = keccak256("b2");

        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: h1 }));
        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: h2 }));

        // Dequeue first to free a slot
        harness.dequeue(w1, bytes32(0));
        assertEq(harness.length(), 1);

        // Enqueue again — should succeed since length is now 1 < capacity 2
        bytes32 h3 = keccak256("b3");
        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: h3 }));
        assertEq(harness.length(), 2);

        // h3 should be written to slots[tail % capacity] = slots[2 % 2] = slots[0]
        assertEq(harness.slots(0), h3);
    }

    /*//////////////////////////////////////////////////////////////
                            DEQUEUE TESTS
    //////////////////////////////////////////////////////////////*/

    function test_dequeue_singleWithdrawal() public {
        Withdrawal memory w = _makeWithdrawal(alice, bob, 100e6);
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: wHash }));

        harness.dequeue(w, bytes32(0));

        assertEq(harness.head(), 1);
        assertEq(harness.tail(), 1);
        assertEq(harness.slots(0), EMPTY_SENTINEL);
        assertFalse(harness.hasWithdrawals());
    }

    function test_dequeue_multipleWithdrawalsInBatch() public {
        Withdrawal memory w1 = _makeWithdrawal(alice, bob, 100e6);
        Withdrawal memory w2 = _makeWithdrawal(bob, charlie, 200e6);

        // Build queue: w1 outermost, w2 innermost (wraps EMPTY_SENTINEL)
        bytes32 innerHash = keccak256(abi.encode(w2, EMPTY_SENTINEL));
        bytes32 batchHash = keccak256(abi.encode(w1, innerHash));

        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: batchHash }));

        // Dequeue w1
        harness.dequeue(w1, innerHash);
        assertEq(harness.head(), 0); // Still on slot 0
        assertEq(harness.slots(0), innerHash);

        // Dequeue w2
        harness.dequeue(w2, bytes32(0));
        assertEq(harness.head(), 1);
        assertEq(harness.slots(0), EMPTY_SENTINEL);
    }

    function test_dequeue_multipleSlots() public {
        Withdrawal memory w1 = _makeWithdrawal(alice, bob, 100e6);
        Withdrawal memory w2 = _makeWithdrawal(bob, charlie, 200e6);

        bytes32 h1 = keccak256(abi.encode(w1, EMPTY_SENTINEL));
        bytes32 h2 = keccak256(abi.encode(w2, EMPTY_SENTINEL));

        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: h1 }));
        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: h2 }));

        // Dequeue from slot 0
        harness.dequeue(w1, bytes32(0));
        assertEq(harness.head(), 1);
        assertEq(harness.length(), 1);

        // Dequeue from slot 1
        harness.dequeue(w2, bytes32(0));
        assertEq(harness.head(), 2);
        assertEq(harness.length(), 0);
    }

    function test_dequeue_revertsIfEmpty() public {
        Withdrawal memory w = _makeWithdrawal(alice, bob, 100e6);

        vm.expectRevert(WithdrawalQueueLib.NoWithdrawalsInQueue.selector);
        harness.dequeue(w, bytes32(0));
    }

    function test_dequeue_revertsIfInvalidHash() public {
        Withdrawal memory w1 = _makeWithdrawal(alice, bob, 100e6);
        Withdrawal memory w2 = _makeWithdrawal(bob, charlie, 200e6);

        bytes32 h1 = keccak256(abi.encode(w1, EMPTY_SENTINEL));
        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: h1 }));

        // Try to dequeue w2 (wrong withdrawal)
        vm.expectRevert(WithdrawalQueueLib.InvalidWithdrawalHash.selector);
        harness.dequeue(w2, bytes32(0));
    }

    function test_dequeue_revertsIfWrongRemainingQueue() public {
        Withdrawal memory w1 = _makeWithdrawal(alice, bob, 100e6);
        Withdrawal memory w2 = _makeWithdrawal(bob, charlie, 200e6);

        bytes32 innerHash = keccak256(abi.encode(w2, EMPTY_SENTINEL));
        bytes32 batchHash = keccak256(abi.encode(w1, innerHash));

        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: batchHash }));

        // Try to dequeue with wrong remaining queue
        vm.expectRevert(WithdrawalQueueLib.InvalidWithdrawalHash.selector);
        harness.dequeue(w1, keccak256("wrongHash"));
    }

    /*//////////////////////////////////////////////////////////////
                            IS FULL TESTS
    //////////////////////////////////////////////////////////////*/

    function test_isFull() public {
        harness.setCapacity(2);

        assertFalse(harness.isFull());

        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: keccak256("b1") }));
        assertFalse(harness.isFull());

        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: keccak256("b2") }));
        assertTrue(harness.isFull());

        // Dequeue one — no longer full
        Withdrawal memory w = _makeWithdrawal(alice, bob, 100e6);
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));
        // We need a properly encoded slot for dequeue, so let's re-do this cleanly
        harness.setCapacity(256); // reset to avoid issues
        // Reset by deploying new harness
        harness = new WithdrawalQueueHarness();
        harness.setCapacity(2);

        Withdrawal memory w1 = _makeWithdrawal(alice, bob, 100e6);
        bytes32 h1 = keccak256(abi.encode(w1, EMPTY_SENTINEL));
        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: h1 }));
        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: keccak256("b2") }));
        assertTrue(harness.isFull());

        harness.dequeue(w1, bytes32(0));
        assertFalse(harness.isFull());
    }

    function test_enqueue_revertsWhenFull_evenEmptyTransition() public {
        harness.setCapacity(2);

        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: keccak256("b1") }));
        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: keccak256("b2") }));

        vm.expectRevert(WithdrawalQueueLib.WithdrawalQueueFull.selector);
        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }));
    }

    function test_enqueue_revertsWhenCapacityZero() public {
        WithdrawalQueueHarness zeroCapHarness = new WithdrawalQueueHarness();

        vm.expectRevert(WithdrawalQueueLib.WithdrawalQueueFull.selector);
        zeroCapHarness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: keccak256("b1") }));
    }

    function test_ringBuffer_multiCycleWraparound() public {
        harness.setCapacity(2);

        Withdrawal memory w1 = _makeWithdrawal(alice, bob, 100e6);
        Withdrawal memory w2 = _makeWithdrawal(bob, charlie, 200e6);
        Withdrawal memory w3 = _makeWithdrawal(alice, charlie, 300e6);
        Withdrawal memory w4 = _makeWithdrawal(charlie, alice, 400e6);

        bytes32 h1 = keccak256(abi.encode(w1, EMPTY_SENTINEL));
        bytes32 h2 = keccak256(abi.encode(w2, EMPTY_SENTINEL));
        bytes32 h3 = keccak256(abi.encode(w3, EMPTY_SENTINEL));
        bytes32 h4 = keccak256(abi.encode(w4, EMPTY_SENTINEL));

        // Fill: A, B
        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: h1 }));
        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: h2 }));
        assertEq(harness.head(), 0);
        assertEq(harness.tail(), 2);

        // Dequeue A (head=1), enqueue C (tail=2, slot 0)
        harness.dequeue(w1, bytes32(0));
        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: h3 }));
        assertEq(harness.head(), 1);
        assertEq(harness.tail(), 3);
        assertEq(harness.slots(0), h3); // slot 0 reused

        // Dequeue B (head=2), enqueue D (tail=3, slot 1)
        harness.dequeue(w2, bytes32(0));
        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: h4 }));
        assertEq(harness.head(), 2);
        assertEq(harness.tail(), 4);
        assertEq(harness.slots(1), h4); // slot 1 reused

        // Dequeue C,D
        harness.dequeue(w3, bytes32(0));
        harness.dequeue(w4, bytes32(0));
        assertEq(harness.head(), 4);
        assertEq(harness.tail(), 4);
        assertFalse(harness.hasWithdrawals());
    }

    /*//////////////////////////////////////////////////////////////
                        LENGTH & HAS WITHDRAWALS
    //////////////////////////////////////////////////////////////*/

    function test_length_accurate() public {
        assertEq(harness.length(), 0);

        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: keccak256("b1") }));
        assertEq(harness.length(), 1);

        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: keccak256("b2") }));
        assertEq(harness.length(), 2);
    }

    function test_hasWithdrawals_accurate() public {
        assertFalse(harness.hasWithdrawals());

        harness.enqueue(WithdrawalQueueTransition({ withdrawalQueueHash: keccak256("b1") }));
        assertTrue(harness.hasWithdrawals());
    }

    /*//////////////////////////////////////////////////////////////
                            HELPER FUNCTIONS
    //////////////////////////////////////////////////////////////*/

    function _makeWithdrawal(
        address sender,
        address to,
        uint128 amount
    )
        internal
        pure
        returns (Withdrawal memory)
    {
        return Withdrawal({
            sender: sender,
            to: to,
            amount: amount,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: sender,
            callbackData: ""
        });
    }

}
