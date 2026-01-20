// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { Test } from "forge-std/Test.sol";
import { ZoneOutbox } from "../../src/zone/ZoneOutbox.sol";
import { ZoneInbox } from "../../src/zone/ZoneInbox.sol";
import { MockZoneGasToken } from "./mocks/MockZoneGasToken.sol";
import { MockTempoState } from "./mocks/MockTempoState.sol";
import { Withdrawal } from "../../src/zone/IZone.sol";
import { EMPTY_SENTINEL } from "../../src/zone/WithdrawalQueueLib.sol";

/// @title ZoneOutboxExtendedTest
/// @notice Extended tests for ZoneOutbox covering edge cases
contract ZoneOutboxExtendedTest is Test {
    ZoneOutbox public outbox;
    ZoneInbox public inbox;
    MockZoneGasToken public gasToken;
    MockTempoState public tempoState;

    address public sequencer = address(0x1);
    address public alice = address(0x200);
    address public bob = address(0x300);
    address public charlie = address(0x400);
    address public mockPortal = address(0x500);

    bytes32 constant GENESIS_TEMPO_BLOCK_HASH = keccak256("tempoGenesis");
    uint64 constant GENESIS_TEMPO_BLOCK_NUMBER = 1;

    function setUp() public {
        gasToken = new MockZoneGasToken("Zone USD", "zUSD");
        tempoState = new MockTempoState(sequencer, GENESIS_TEMPO_BLOCK_HASH, GENESIS_TEMPO_BLOCK_NUMBER);
        inbox = new ZoneInbox(mockPortal, address(tempoState), address(gasToken), sequencer);
        outbox = new ZoneOutbox(address(gasToken), sequencer, address(inbox), address(tempoState));

        gasToken.setMinter(address(inbox), true);
        gasToken.setBurner(address(outbox), true);

        // Give test accounts tokens
        gasToken.setMinter(address(this), true);
        gasToken.mint(alice, 100_000e6);
        gasToken.mint(bob, 100_000e6);
        gasToken.mint(charlie, 100_000e6);
    }

    /*//////////////////////////////////////////////////////////////
                     WITHDRAWAL INDEX TRACKING TESTS
    //////////////////////////////////////////////////////////////*/

    function test_nextWithdrawalIndex_incrementsCorrectly() public {
        assertEq(outbox.nextWithdrawalIndex(), 0);

        vm.startPrank(alice);
        gasToken.approve(address(outbox), 3000e6);

        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, address(0), "");
        assertEq(outbox.nextWithdrawalIndex(), 1);

        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, address(0), "");
        assertEq(outbox.nextWithdrawalIndex(), 2);

        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, address(0), "");
        assertEq(outbox.nextWithdrawalIndex(), 3);

        vm.stopPrank();
    }

    function test_nextWithdrawalIndex_persistsAcrossBatches() public {
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 5000e6);

        // First batch
        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, address(0), "");
        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, address(0), "");

        vm.stopPrank();

        vm.prank(sequencer);
        outbox.finalizeBatch(type(uint256).max);

        // Second batch
        vm.startPrank(alice);
        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, address(0), "");

        assertEq(outbox.nextWithdrawalIndex(), 3);
        vm.stopPrank();
    }

    /*//////////////////////////////////////////////////////////////
                      PENDING WITHDRAWALS TESTS
    //////////////////////////////////////////////////////////////*/

    function test_pendingWithdrawalsCount_tracksCorrectly() public {
        assertEq(outbox.pendingWithdrawalsCount(), 0);

        vm.startPrank(alice);
        gasToken.approve(address(outbox), 3000e6);

        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, address(0), "");
        assertEq(outbox.pendingWithdrawalsCount(), 1);

        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, address(0), "");
        assertEq(outbox.pendingWithdrawalsCount(), 2);

        vm.stopPrank();

        // Finalize clears them
        vm.prank(sequencer);
        outbox.finalizeBatch(type(uint256).max);
        assertEq(outbox.pendingWithdrawalsCount(), 0);
    }

    /*//////////////////////////////////////////////////////////////
                      TOKEN TRANSFER TESTS
    //////////////////////////////////////////////////////////////*/

    function test_requestWithdrawal_transfersFromSender() public {
        uint256 aliceBalanceBefore = gasToken.balanceOf(alice);

        vm.startPrank(alice);
        gasToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, address(0), "");
        vm.stopPrank();

        assertEq(gasToken.balanceOf(alice), aliceBalanceBefore - 500e6);
    }

    function test_requestWithdrawal_burnsTokens() public {
        uint256 totalSupplyBefore = gasToken.totalSupply();

        vm.startPrank(alice);
        gasToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, address(0), "");
        vm.stopPrank();

        assertEq(gasToken.totalSupply(), totalSupplyBefore - 500e6);
    }

    function test_requestWithdrawal_revertsOnInsufficientBalance() public {
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 200_000e6);

        vm.expectRevert(MockZoneGasToken.InsufficientBalance.selector);
        outbox.requestWithdrawal(bob, 200_000e6, bytes32(0), 0, address(0), "");
        vm.stopPrank();
    }

    function test_requestWithdrawal_revertsOnInsufficientAllowance() public {
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 100e6);

        vm.expectRevert(MockZoneGasToken.InsufficientAllowance.selector);
        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, address(0), "");
        vm.stopPrank();
    }

    /*//////////////////////////////////////////////////////////////
                   FALLBACK RECIPIENT VALIDATION TESTS
    //////////////////////////////////////////////////////////////*/

    function test_requestWithdrawal_noCallbackNeedsFallback_ok() public {
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 500e6);

        // gasLimit = 0, fallbackRecipient = address(0) is fine
        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, address(0), "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 1);
    }

    function test_requestWithdrawal_callbackNeedsFallback_reverts() public {
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 500e6);

        // gasLimit > 0, fallbackRecipient = address(0) reverts
        vm.expectRevert(ZoneOutbox.InvalidFallbackRecipient.selector);
        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 100000, address(0), "");
        vm.stopPrank();
    }

    function test_requestWithdrawal_callbackWithValidFallback_ok() public {
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 500e6);

        // gasLimit > 0 with valid fallback
        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 100000, alice, "callback");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 1);
    }

    /*//////////////////////////////////////////////////////////////
                       FINALIZE BATCH TESTS
    //////////////////////////////////////////////////////////////*/

    function test_finalizeBatch_hashChainOrder() public {
        // Add three withdrawals
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 3000e6);
        outbox.requestWithdrawal(alice, 100e6, bytes32("w1"), 0, address(0), "");
        vm.stopPrank();

        vm.startPrank(bob);
        gasToken.approve(address(outbox), 3000e6);
        outbox.requestWithdrawal(bob, 200e6, bytes32("w2"), 0, address(0), "");
        vm.stopPrank();

        vm.startPrank(charlie);
        gasToken.approve(address(outbox), 3000e6);
        outbox.requestWithdrawal(charlie, 300e6, bytes32("w3"), 0, address(0), "");
        vm.stopPrank();

        // Build expected hash (oldest = outermost)
        Withdrawal memory w1 = Withdrawal({
            sender: alice,
            to: alice,
            amount: 100e6,
            memo: bytes32("w1"),
            gasLimit: 0,
            fallbackRecipient: address(0),
            callbackData: ""
        });
        Withdrawal memory w2 = Withdrawal({
            sender: bob,
            to: bob,
            amount: 200e6,
            memo: bytes32("w2"),
            gasLimit: 0,
            fallbackRecipient: address(0),
            callbackData: ""
        });
        Withdrawal memory w3 = Withdrawal({
            sender: charlie,
            to: charlie,
            amount: 300e6,
            memo: bytes32("w3"),
            gasLimit: 0,
            fallbackRecipient: address(0),
            callbackData: ""
        });

        // Hash chain: w1 outermost, w3 innermost wrapping EMPTY_SENTINEL
        bytes32 innermost = keccak256(abi.encode(w3, EMPTY_SENTINEL));
        bytes32 middle = keccak256(abi.encode(w2, innermost));
        bytes32 expectedHash = keccak256(abi.encode(w1, middle));

        vm.prank(sequencer);
        bytes32 hash = outbox.finalizeBatch(type(uint256).max);

        assertEq(hash, expectedHash);
    }

    function test_finalizeBatch_partialBatch_leavesRemainder() public {
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 5000e6);
        outbox.requestWithdrawal(alice, 100e6, bytes32("w1"), 0, address(0), "");
        outbox.requestWithdrawal(alice, 200e6, bytes32("w2"), 0, address(0), "");
        outbox.requestWithdrawal(alice, 300e6, bytes32("w3"), 0, address(0), "");
        outbox.requestWithdrawal(alice, 400e6, bytes32("w4"), 0, address(0), "");
        outbox.requestWithdrawal(alice, 500e6, bytes32("w5"), 0, address(0), "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 5);

        // Process only 2 (pops from end: w5 and w4)
        vm.prank(sequencer);
        outbox.finalizeBatch(2);

        // 3 should remain: w1, w2, w3
        assertEq(outbox.pendingWithdrawalsCount(), 3);
    }

    function test_finalizeBatch_countLargerThanPending() public {
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 1000e6);
        outbox.requestWithdrawal(alice, 100e6, bytes32("w1"), 0, address(0), "");
        outbox.requestWithdrawal(alice, 200e6, bytes32("w2"), 0, address(0), "");
        vm.stopPrank();

        // Process with large count
        vm.prank(sequencer);
        outbox.finalizeBatch(1000);

        assertEq(outbox.pendingWithdrawalsCount(), 0);
    }

    function test_finalizeBatch_consecutiveBatches() public {
        // First batch
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 10000e6);
        outbox.requestWithdrawal(alice, 100e6, bytes32("b1w1"), 0, address(0), "");
        outbox.requestWithdrawal(alice, 200e6, bytes32("b1w2"), 0, address(0), "");
        vm.stopPrank();

        vm.prank(sequencer);
        bytes32 hash1 = outbox.finalizeBatch(type(uint256).max);
        assertTrue(hash1 != bytes32(0));

        // Second batch
        vm.startPrank(alice);
        outbox.requestWithdrawal(alice, 300e6, bytes32("b2w1"), 0, address(0), "");
        vm.stopPrank();

        vm.prank(sequencer);
        bytes32 hash2 = outbox.finalizeBatch(type(uint256).max);
        assertTrue(hash2 != bytes32(0));

        // Hashes should be different
        assertTrue(hash1 != hash2);
    }

    /*//////////////////////////////////////////////////////////////
                        WITHDRAWAL STRUCT TESTS
    //////////////////////////////////////////////////////////////*/

    function test_requestWithdrawal_capturesAllFields() public {
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 1000e6);
        outbox.requestWithdrawal(
            bob,                    // to
            500e6,                  // amount
            bytes32("payment123"), // memo
            50000,                  // gasLimit
            charlie,                // fallbackRecipient
            "callbackData"          // data
        );
        vm.stopPrank();

        // Finalize and verify hash includes all fields
        Withdrawal memory expected = Withdrawal({
            sender: alice,
            to: bob,
            amount: 500e6,
            memo: bytes32("payment123"),
            gasLimit: 50000,
            fallbackRecipient: charlie,
            callbackData: "callbackData"
        });
        bytes32 expectedHash = keccak256(abi.encode(expected, EMPTY_SENTINEL));

        vm.prank(sequencer);
        bytes32 hash = outbox.finalizeBatch(1);

        assertEq(hash, expectedHash);
    }

    /*//////////////////////////////////////////////////////////////
                          ZERO AMOUNT TESTS
    //////////////////////////////////////////////////////////////*/

    function test_requestWithdrawal_zeroAmount() public {
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 0);
        outbox.requestWithdrawal(bob, 0, bytes32(0), 0, address(0), "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 1);

        // Should still produce valid hash
        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: bob,
            amount: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: address(0),
            callbackData: ""
        });
        bytes32 expectedHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.prank(sequencer);
        bytes32 hash = outbox.finalizeBatch(1);

        assertEq(hash, expectedHash);
    }

    /*//////////////////////////////////////////////////////////////
                    WITHDRAWAL REQUESTED EVENT TESTS
    //////////////////////////////////////////////////////////////*/

    function test_requestWithdrawal_emitsEvent() public {
        vm.startPrank(alice);
        gasToken.approve(address(outbox), 500e6);

        vm.expectEmit(true, true, false, true);
        emit ZoneOutbox.WithdrawalRequested(
            0,          // index
            alice,      // sender
            bob,        // to
            500e6,      // amount
            bytes32("memo"),
            50000,      // gasLimit
            charlie,    // fallbackRecipient
            "data"
        );

        outbox.requestWithdrawal(bob, 500e6, bytes32("memo"), 50000, charlie, "data");
        vm.stopPrank();
    }

    /*//////////////////////////////////////////////////////////////
                        IMMUTABLE GETTERS TESTS
    //////////////////////////////////////////////////////////////*/

    function test_immutableGetters() public view {
        assertEq(address(outbox.gasToken()), address(gasToken));
        assertEq(outbox.sequencer(), sequencer);
        assertEq(address(outbox.zoneInbox()), address(inbox));
        assertEq(address(outbox.tempoState()), address(tempoState));
    }

    /*//////////////////////////////////////////////////////////////
                    LARGE WITHDRAWAL BATCH TESTS
    //////////////////////////////////////////////////////////////*/

    function test_finalizeBatch_manyWithdrawals() public {
        uint256 numWithdrawals = 50;

        vm.startPrank(alice);
        gasToken.approve(address(outbox), numWithdrawals * 100e6);

        for (uint256 i = 0; i < numWithdrawals; i++) {
            outbox.requestWithdrawal(bob, 100e6, bytes32(i), 0, address(0), "");
        }
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), numWithdrawals);

        vm.prank(sequencer);
        bytes32 hash = outbox.finalizeBatch(type(uint256).max);

        assertTrue(hash != bytes32(0));
        assertEq(outbox.pendingWithdrawalsCount(), 0);
    }
}
