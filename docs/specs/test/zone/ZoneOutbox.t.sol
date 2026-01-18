// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { Test } from "forge-std/Test.sol";
import { ZoneOutbox } from "../../src/zone/ZoneOutbox.sol";
import { MockZoneGasToken } from "./mocks/MockZoneGasToken.sol";
import { Withdrawal } from "../../src/zone/IZone.sol";
import { EMPTY_SENTINEL } from "../../src/zone/WithdrawalQueueLib.sol";

/// @title ZoneOutboxTest
/// @notice Tests for ZoneOutbox batch() functionality and withdrawal storage
contract ZoneOutboxTest is Test {
    ZoneOutbox public outbox;
    MockZoneGasToken public gasToken;

    address public sequencer = address(0x1);
    address public alice = address(0x200);
    address public bob = address(0x300);

    function setUp() public {
        gasToken = new MockZoneGasToken("Zone USD", "zUSD");
        outbox = new ZoneOutbox(address(gasToken), sequencer);

        // Grant burner role to outbox
        gasToken.setBurner(address(outbox), true);

        // Give alice and bob tokens
        gasToken.setMinter(address(this), true);
        gasToken.mint(alice, 10_000e6);
        gasToken.mint(bob, 10_000e6);
    }

    /*//////////////////////////////////////////////////////////////
                          STORAGE TESTS
    //////////////////////////////////////////////////////////////*/

    function test_requestWithdrawal_storesInArray() public {
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(alice, 500e6, bytes32("memo"), 0, address(0), "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 1);

        vm.startPrank(bob);
        gasToken.approve(address(outbox), 300e6);
        outbox.requestWithdrawal(bob, 300e6, bytes32(0), 0, address(0), "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 2);
    }

    /*//////////////////////////////////////////////////////////////
                          BATCH TESTS
    //////////////////////////////////////////////////////////////*/

    function test_batch_emptyQueue_returnsZero() public {
        vm.prank(sequencer);
        bytes32 hash = outbox.batch(100);

        assertEq(hash, bytes32(0));
    }

    function test_batch_zeroCount_returnsZero() public {
        // Add a withdrawal
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(alice, 500e6, bytes32(0), 0, address(0), "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 1);

        // Batch with count=0 should return 0 and not process
        vm.prank(sequencer);
        bytes32 hash = outbox.batch(0);

        assertEq(hash, bytes32(0));
        assertEq(outbox.pendingWithdrawalsCount(), 1);
    }

    function test_batch_singleWithdrawal_correctHash() public {
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(alice, 500e6, bytes32("memo"), 0, address(0), "");
        vm.stopPrank();

        // Expected hash
        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: alice,
            amount: 500e6,
            memo: bytes32("memo"),
            gasLimit: 0,
            fallbackRecipient: address(0),
            callbackData: ""
        });
        bytes32 expectedHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.prank(sequencer);
        bytes32 hash = outbox.batch(100);

        assertEq(hash, expectedHash);
    }

    function test_batch_multipleWithdrawals_correctHashChain() public {
        // Alice withdraws
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(alice, 500e6, bytes32(0), 0, address(0), "");
        vm.stopPrank();

        // Bob withdraws
        vm.startPrank(bob);
        gasToken.approve(address(outbox), 300e6);
        outbox.requestWithdrawal(bob, 300e6, bytes32(0), 0, address(0), "");
        vm.stopPrank();

        // Build expected hash (oldest = outermost)
        // w0 = alice's withdrawal (first, oldest)
        // w1 = bob's withdrawal (second, newest)
        // Hash chain: hash(w0, hash(w1, EMPTY_SENTINEL))
        Withdrawal memory w0 = Withdrawal({
            sender: alice,
            to: alice,
            amount: 500e6,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: address(0),
            callbackData: ""
        });
        Withdrawal memory w1 = Withdrawal({
            sender: bob,
            to: bob,
            amount: 300e6,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: address(0),
            callbackData: ""
        });

        bytes32 innerHash = keccak256(abi.encode(w1, EMPTY_SENTINEL));
        bytes32 expectedHash = keccak256(abi.encode(w0, innerHash));

        vm.prank(sequencer);
        bytes32 hash = outbox.batch(100);

        assertEq(hash, expectedHash);
    }

    function test_batch_clearsStorage() public {
        // Add withdrawals
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 1000e6);
        outbox.requestWithdrawal(alice, 500e6, bytes32(0), 0, address(0), "");
        outbox.requestWithdrawal(alice, 300e6, bytes32(0), 0, address(0), "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 2);

        // Batch all
        vm.prank(sequencer);
        outbox.batch(type(uint256).max);

        assertEq(outbox.pendingWithdrawalsCount(), 0);
    }

    function test_batch_partialBatch_processesOnlyCount() public {
        // Add 3 withdrawals
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 1500e6);
        outbox.requestWithdrawal(alice, 500e6, bytes32("w1"), 0, address(0), "");
        outbox.requestWithdrawal(alice, 500e6, bytes32("w2"), 0, address(0), "");
        outbox.requestWithdrawal(alice, 500e6, bytes32("w3"), 0, address(0), "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 3);

        // Batch only 2 (should process w2 and w3, leaving w1)
        vm.prank(sequencer);
        bytes32 hash = outbox.batch(2);

        // Should have 1 left (w1)
        assertEq(outbox.pendingWithdrawalsCount(), 1);

        // Expected hash for w2 and w3 (w2 is oldest of the two)
        Withdrawal memory w2 = Withdrawal({
            sender: alice,
            to: alice,
            amount: 500e6,
            memo: bytes32("w2"),
            gasLimit: 0,
            fallbackRecipient: address(0),
            callbackData: ""
        });
        Withdrawal memory w3 = Withdrawal({
            sender: alice,
            to: alice,
            amount: 500e6,
            memo: bytes32("w3"),
            gasLimit: 0,
            fallbackRecipient: address(0),
            callbackData: ""
        });

        bytes32 innerHash = keccak256(abi.encode(w3, EMPTY_SENTINEL));
        bytes32 expectedHash = keccak256(abi.encode(w2, innerHash));

        assertEq(hash, expectedHash);
    }

    function test_batch_emitsEvent() public {
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(alice, 500e6, bytes32(0), 0, address(0), "");
        vm.stopPrank();

        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: alice,
            amount: 500e6,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: address(0),
            callbackData: ""
        });
        bytes32 expectedHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.expectEmit(true, false, false, true);
        emit ZoneOutbox.BatchWithdrawals(expectedHash, 1);

        vm.prank(sequencer);
        outbox.batch(100);
    }

    /*//////////////////////////////////////////////////////////////
                          ACCESS CONTROL TESTS
    //////////////////////////////////////////////////////////////*/

    function test_batch_onlySequencer() public {
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(alice, 500e6, bytes32(0), 0, address(0), "");
        vm.stopPrank();

        // Non-sequencer should revert
        vm.prank(alice);
        vm.expectRevert(ZoneOutbox.OnlySequencer.selector);
        outbox.batch(100);

        // Sequencer should succeed
        vm.prank(sequencer);
        bytes32 hash = outbox.batch(100);
        assertTrue(hash != bytes32(0));
    }

    /*//////////////////////////////////////////////////////////////
                    WITHDRAWAL WITH CALLBACK TESTS
    //////////////////////////////////////////////////////////////*/

    function test_batch_withdrawalWithCallback_correctHash() public {
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(
            bob,            // to
            500e6,          // amount
            bytes32("pay"), // memo
            100000,         // gasLimit
            alice,          // fallbackRecipient
            "callback_data"
        );
        vm.stopPrank();

        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: bob,
            amount: 500e6,
            memo: bytes32("pay"),
            gasLimit: 100000,
            fallbackRecipient: alice,
            callbackData: "callback_data"
        });
        bytes32 expectedHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.prank(sequencer);
        bytes32 hash = outbox.batch(100);

        assertEq(hash, expectedHash);
    }
}
