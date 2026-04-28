// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { Withdrawal } from "../../src/zone/IZone.sol";
import {
    EMPTY_SENTINEL,
    WITHDRAWAL_QUEUE_CAPACITY,
    WithdrawalQueue,
    WithdrawalQueueLib
} from "../../src/zone/WithdrawalQueueLib.sol";
import { Test } from "forge-std/Test.sol";

/// @title WithdrawalQueueHarness
/// @notice Test harness that wraps the library to convert memory to calldata
contract WithdrawalQueueHarness {

    using WithdrawalQueueLib for WithdrawalQueue;

    WithdrawalQueue internal queue;

    function enqueue(bytes32 withdrawalQueueHash) external {
        queue.enqueue(withdrawalQueueHash);
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

    function head() external view returns (uint256) {
        return queue.head;
    }

    function tail() external view returns (uint256) {
        return queue.tail;
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
    }

    /*//////////////////////////////////////////////////////////////
                          INITIAL STATE TESTS
    //////////////////////////////////////////////////////////////*/

    function test_initialState() public view {
        assertEq(harness.head(), 0);
        assertEq(harness.tail(), 0);
        assertFalse(harness.hasWithdrawals());
        assertEq(harness.length(), 0);
    }

    /*//////////////////////////////////////////////////////////////
                            ENQUEUE TESTS
    //////////////////////////////////////////////////////////////*/

    function test_enqueue_singleBatch() public {
        Withdrawal memory w = _makeWithdrawal(alice, bob, 100e6);
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        harness.enqueue(wHash);

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

        harness.enqueue(h1);
        assertEq(harness.tail(), 1);

        harness.enqueue(h2);
        assertEq(harness.tail(), 2);

        harness.enqueue(h3);
        assertEq(harness.tail(), 3);

        assertEq(harness.slots(0), h1);
        assertEq(harness.slots(1), h2);
        assertEq(harness.slots(2), h3);
        assertEq(harness.length(), 3);
    }

    function test_enqueue_emptyTransition_noOp() public {
        harness.enqueue(bytes32(0));

        assertEq(harness.head(), 0);
        assertEq(harness.tail(), 0);
        assertFalse(harness.hasWithdrawals());
    }

    function test_enqueue_mixedEmptyAndNonEmpty() public {
        bytes32 h1 = keccak256("batch1");
        bytes32 h2 = keccak256("batch2");

        harness.enqueue(h1);
        assertEq(harness.tail(), 1);

        // Empty batch - no change
        harness.enqueue(bytes32(0));
        assertEq(harness.tail(), 1);

        harness.enqueue(h2);
        assertEq(harness.tail(), 2);

        // Slots should be contiguous
        assertEq(harness.slots(0), h1);
        assertEq(harness.slots(1), h2);
    }

    function test_enqueue_revertsWhenFull() public {
        for (uint256 i = 0; i < WITHDRAWAL_QUEUE_CAPACITY; i++) {
            harness.enqueue(keccak256(abi.encode("b", i)));
        }
        assertEq(harness.length(), WITHDRAWAL_QUEUE_CAPACITY);

        vm.expectRevert(WithdrawalQueueLib.WithdrawalQueueFull.selector);
        harness.enqueue(keccak256("overflow"));
    }

    function test_enqueue_afterDequeueReuseSlots() public {
        Withdrawal memory w1 = _makeWithdrawal(alice, bob, 100e6);
        bytes32 h1 = keccak256(abi.encode(w1, EMPTY_SENTINEL));

        // Fill all slots
        harness.enqueue(h1);
        for (uint256 i = 1; i < WITHDRAWAL_QUEUE_CAPACITY; i++) {
            harness.enqueue(keccak256(abi.encode("b", i)));
        }
        assertEq(harness.length(), WITHDRAWAL_QUEUE_CAPACITY);

        // Dequeue first to free a slot
        harness.dequeue(w1, bytes32(0));
        assertEq(harness.length(), WITHDRAWAL_QUEUE_CAPACITY - 1);

        // Enqueue again — should succeed since we freed a slot
        bytes32 hNew = keccak256("new");
        harness.enqueue(hNew);
        assertEq(harness.length(), WITHDRAWAL_QUEUE_CAPACITY);

        // hNew should be written to slots[tail % capacity] = slots[CAPACITY % CAPACITY] = slots[0]
        assertEq(harness.slots(0), hNew);
    }

    /*//////////////////////////////////////////////////////////////
                            DEQUEUE TESTS
    //////////////////////////////////////////////////////////////*/

    function test_dequeue_singleWithdrawal() public {
        Withdrawal memory w = _makeWithdrawal(alice, bob, 100e6);
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        harness.enqueue(wHash);

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

        harness.enqueue(batchHash);

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

        harness.enqueue(h1);
        harness.enqueue(h2);

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
        harness.enqueue(h1);

        // Try to dequeue w2 (wrong withdrawal)
        vm.expectRevert(WithdrawalQueueLib.InvalidWithdrawalHash.selector);
        harness.dequeue(w2, bytes32(0));
    }

    function test_dequeue_revertsIfWrongRemainingQueue() public {
        Withdrawal memory w1 = _makeWithdrawal(alice, bob, 100e6);
        Withdrawal memory w2 = _makeWithdrawal(bob, charlie, 200e6);

        bytes32 innerHash = keccak256(abi.encode(w2, EMPTY_SENTINEL));
        bytes32 batchHash = keccak256(abi.encode(w1, innerHash));

        harness.enqueue(batchHash);

        // Try to dequeue with wrong remaining queue
        vm.expectRevert(WithdrawalQueueLib.InvalidWithdrawalHash.selector);
        harness.dequeue(w1, keccak256("wrongHash"));
    }

    /*//////////////////////////////////////////////////////////////
                      REVERT WHEN FULL TESTS
    //////////////////////////////////////////////////////////////*/

    function test_enqueue_revertsWhenFull_evenEmptyTransition() public {
        for (uint256 i = 0; i < WITHDRAWAL_QUEUE_CAPACITY; i++) {
            harness.enqueue(keccak256(abi.encode("b", i)));
        }

        vm.expectRevert(WithdrawalQueueLib.WithdrawalQueueFull.selector);
        harness.enqueue(bytes32(0));
    }

    function test_ringBuffer_multiCycleWraparound() public {
        Withdrawal[] memory ws = new Withdrawal[](4);
        ws[0] = _makeWithdrawal(alice, bob, 100e6);
        ws[1] = _makeWithdrawal(bob, charlie, 200e6);
        ws[2] = _makeWithdrawal(alice, charlie, 300e6);
        ws[3] = _makeWithdrawal(charlie, alice, 400e6);

        bytes32[] memory hs = new bytes32[](4);
        for (uint256 i = 0; i < 4; i++) {
            hs[i] = keccak256(abi.encode(ws[i], EMPTY_SENTINEL));
        }

        // Fill to capacity
        harness.enqueue(hs[0]);
        harness.enqueue(hs[1]);
        for (uint256 i = 2; i < WITHDRAWAL_QUEUE_CAPACITY; i++) {
            harness.enqueue(keccak256(abi.encode("fill", i)));
        }
        assertEq(harness.head(), 0);
        assertEq(harness.tail(), WITHDRAWAL_QUEUE_CAPACITY);

        // Dequeue first (head=1), enqueue C (tail=CAPACITY, slot 0)
        harness.dequeue(ws[0], bytes32(0));
        harness.enqueue(hs[2]);
        assertEq(harness.head(), 1);
        assertEq(harness.tail(), WITHDRAWAL_QUEUE_CAPACITY + 1);
        assertEq(harness.slots(0), hs[2]); // slot 0 reused

        // Dequeue second (head=2), enqueue D (tail=CAPACITY+1, slot 1)
        harness.dequeue(ws[1], bytes32(0));
        harness.enqueue(hs[3]);
        assertEq(harness.head(), 2);
        assertEq(harness.tail(), WITHDRAWAL_QUEUE_CAPACITY + 2);
        assertEq(harness.slots(1), hs[3]); // slot 1 reused

        // Verify wrapping worked by checking slot contents
        assertEq(harness.slots(0), hs[2]);
        assertEq(harness.slots(1), hs[3]);
        assertEq(harness.length(), WITHDRAWAL_QUEUE_CAPACITY);
    }

    /*//////////////////////////////////////////////////////////////
                        LENGTH & HAS WITHDRAWALS
    //////////////////////////////////////////////////////////////*/

    function test_length_accurate() public {
        assertEq(harness.length(), 0);

        harness.enqueue(keccak256("b1"));
        assertEq(harness.length(), 1);

        harness.enqueue(keccak256("b2"));
        assertEq(harness.length(), 2);
    }

    function test_hasWithdrawals_accurate() public {
        assertFalse(harness.hasWithdrawals());

        harness.enqueue(keccak256("b1"));
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
            token: address(0x100),
            senderTag: keccak256(abi.encodePacked(sender)),
            to: to,
            amount: amount,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: sender,
            callbackData: "",
            encryptedSender: "",
            bouncebackFee: 0
        });
    }

}
