// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { IZoneOutbox, LastBatch, Withdrawal } from "../../src/zone/IZone.sol";
import { EMPTY_SENTINEL } from "../../src/zone/WithdrawalQueueLib.sol";
import { ZoneConfig } from "../../src/zone/ZoneConfig.sol";
import { ZoneInbox } from "../../src/zone/ZoneInbox.sol";
import { ZoneOutbox } from "../../src/zone/ZoneOutbox.sol";
import { MockTempoState } from "./mocks/MockTempoState.sol";
import { MockZoneToken } from "./mocks/MockZoneToken.sol";
import { Test } from "forge-std/Test.sol";

/// @title ZoneOutboxTest
/// @notice Tests for ZoneOutbox finalizeWithdrawalBatch() functionality and withdrawal storage
contract ZoneOutboxTest is Test {

    ZoneConfig public config;
    ZoneOutbox public outbox;
    ZoneInbox public inbox;
    MockZoneToken public zoneToken;
    MockTempoState public tempoState;

    address public sequencer = address(0x1);
    address public alice = address(0x200);
    address public bob = address(0x300);
    address public charlie = address(0x400);
    address public mockPortal = address(0x400);

    bytes32 constant GENESIS_TEMPO_BLOCK_HASH = keccak256("tempoGenesis");
    uint64 constant GENESIS_TEMPO_BLOCK_NUMBER = 1;

    function setUp() public {
        zoneToken = new MockZoneToken("Zone USD", "zUSD");
        tempoState =
            new MockTempoState(sequencer, GENESIS_TEMPO_BLOCK_HASH, GENESIS_TEMPO_BLOCK_NUMBER);
        config = new ZoneConfig(address(zoneToken), mockPortal, address(tempoState));
        tempoState.setMockStorageValue(
            mockPortal, bytes32(uint256(0)), bytes32(uint256(uint160(sequencer)))
        );
        inbox = new ZoneInbox(address(config), mockPortal, address(tempoState), address(zoneToken));
        outbox = new ZoneOutbox(address(config), address(zoneToken));

        // Grant minter role to inbox and burner role to outbox
        zoneToken.setMinter(address(inbox), true);
        zoneToken.setBurner(address(outbox), true);

        // Give alice and bob tokens
        zoneToken.setMinter(address(this), true);
        zoneToken.mint(alice, 10_000e6);
        zoneToken.mint(bob, 10_000e6);
        zoneToken.mint(charlie, 10_000e6);
    }

    /*//////////////////////////////////////////////////////////////
                          STORAGE TESTS
    //////////////////////////////////////////////////////////////*/

    function test_requestWithdrawal_storesInArray() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(alice, 500e6, bytes32("memo"), 0, alice, "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 1);

        vm.startPrank(bob);
        zoneToken.approve(address(outbox), 300e6);
        outbox.requestWithdrawal(bob, 300e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 2);
    }

    /*//////////////////////////////////////////////////////////////
                       FINALIZE BATCH TESTS
    //////////////////////////////////////////////////////////////*/

    function test_finalizeWithdrawalBatch_emptyQueue_returnsZero() public {
        vm.prank(sequencer);
        bytes32 hash = outbox.finalizeWithdrawalBatch(100, uint64(block.number));

        // Still emits event with zero count
        assertEq(hash, bytes32(0));
    }

    function test_finalizeWithdrawalBatch_zeroCount_returnsZero() public {
        // Add a withdrawal
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(alice, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 1);

        // finalizeWithdrawalBatch with count=0 should return 0 and not process withdrawals
        vm.prank(sequencer);
        bytes32 hash = outbox.finalizeWithdrawalBatch(0, uint64(block.number));

        assertEq(hash, bytes32(0));
        assertEq(outbox.pendingWithdrawalsCount(), 1);
    }

    function test_finalizeWithdrawalBatch_singleWithdrawal_correctHash() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(alice, 500e6, bytes32("memo"), 0, alice, "");
        vm.stopPrank();

        // Expected hash
        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: alice,
            amount: 500e6,
            fee: 0,
            memo: bytes32("memo"),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 expectedHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.prank(sequencer);
        bytes32 hash = outbox.finalizeWithdrawalBatch(100, uint64(block.number));

        assertEq(hash, expectedHash);
    }

    function test_finalizeWithdrawalBatch_multipleWithdrawals_correctHashChain() public {
        // Alice withdraws
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(alice, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        // Bob withdraws
        vm.startPrank(bob);
        zoneToken.approve(address(outbox), 300e6);
        outbox.requestWithdrawal(bob, 300e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        // Build expected hash (oldest = outermost)
        // w0 = alice's withdrawal (first, oldest)
        // w1 = bob's withdrawal (second, newest)
        // Hash chain: hash(w0, hash(w1, EMPTY_SENTINEL))
        Withdrawal memory w0 = Withdrawal({
            sender: alice,
            to: alice,
            amount: 500e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        Withdrawal memory w1 = Withdrawal({
            sender: bob,
            to: bob,
            amount: 300e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });

        bytes32 innerHash = keccak256(abi.encode(w1, EMPTY_SENTINEL));
        bytes32 expectedHash = keccak256(abi.encode(w0, innerHash));

        vm.prank(sequencer);
        bytes32 hash = outbox.finalizeWithdrawalBatch(100, uint64(block.number));

        assertEq(hash, expectedHash);
    }

    function test_finalizeWithdrawalBatch_clearsStorage() public {
        // Add withdrawals
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 1000e6);
        outbox.requestWithdrawal(alice, 500e6, bytes32(0), 0, alice, "");
        outbox.requestWithdrawal(alice, 300e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 2);

        // Batch all
        vm.prank(sequencer);
        outbox.finalizeWithdrawalBatch(type(uint256).max, uint64(block.number));

        assertEq(outbox.pendingWithdrawalsCount(), 0);
    }

    function test_finalizeWithdrawalBatch_partialBatch_processesOnlyCount() public {
        // Add 3 withdrawals
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 1500e6);
        outbox.requestWithdrawal(alice, 500e6, bytes32("w1"), 0, alice, "");
        outbox.requestWithdrawal(alice, 500e6, bytes32("w2"), 0, alice, "");
        outbox.requestWithdrawal(alice, 500e6, bytes32("w3"), 0, alice, "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 3);

        // Batch only 2 (should process w1 and w2, leaving w3)
        vm.prank(sequencer);
        bytes32 hash = outbox.finalizeWithdrawalBatch(2, uint64(block.number));

        // Should have 1 left (w3)
        assertEq(outbox.pendingWithdrawalsCount(), 1);

        // Expected hash for w1 and w2 (w1 is oldest of the two)
        Withdrawal memory w1 = Withdrawal({
            sender: alice,
            to: alice,
            amount: 500e6,
            fee: 0,
            memo: bytes32("w1"),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        Withdrawal memory w2 = Withdrawal({
            sender: alice,
            to: alice,
            amount: 500e6,
            fee: 0,
            memo: bytes32("w2"),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });

        bytes32 innerHash = keccak256(abi.encode(w2, EMPTY_SENTINEL));
        bytes32 expectedHash = keccak256(abi.encode(w1, innerHash));

        assertEq(hash, expectedHash);
    }

    function test_finalizeWithdrawalBatch_partialBatches_fifoOrder() public {
        // Add 4 withdrawals in order
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 4000e6);
        outbox.requestWithdrawal(alice, 100e6, bytes32("w1"), 0, alice, "");
        outbox.requestWithdrawal(alice, 200e6, bytes32("w2"), 0, alice, "");
        outbox.requestWithdrawal(alice, 300e6, bytes32("w3"), 0, alice, "");
        outbox.requestWithdrawal(alice, 400e6, bytes32("w4"), 0, alice, "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 4);

        // First batch takes w1, w2
        vm.prank(sequencer);
        bytes32 hash1 = outbox.finalizeWithdrawalBatch(2, uint64(block.number));

        Withdrawal memory w1 = Withdrawal({
            sender: alice,
            to: alice,
            amount: 100e6,
            fee: 0,
            memo: bytes32("w1"),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        Withdrawal memory w2 = Withdrawal({
            sender: alice,
            to: alice,
            amount: 200e6,
            fee: 0,
            memo: bytes32("w2"),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 innerHash1 = keccak256(abi.encode(w2, EMPTY_SENTINEL));
        bytes32 expectedHash1 = keccak256(abi.encode(w1, innerHash1));

        assertEq(hash1, expectedHash1);
        assertEq(outbox.pendingWithdrawalsCount(), 2);

        // Second batch takes w3, w4
        vm.prank(sequencer);
        bytes32 hash2 = outbox.finalizeWithdrawalBatch(2, uint64(block.number));

        Withdrawal memory w3 = Withdrawal({
            sender: alice,
            to: alice,
            amount: 300e6,
            fee: 0,
            memo: bytes32("w3"),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        Withdrawal memory w4 = Withdrawal({
            sender: alice,
            to: alice,
            amount: 400e6,
            fee: 0,
            memo: bytes32("w4"),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 innerHash2 = keccak256(abi.encode(w4, EMPTY_SENTINEL));
        bytes32 expectedHash2 = keccak256(abi.encode(w3, innerHash2));

        assertEq(hash2, expectedHash2);
        assertEq(outbox.pendingWithdrawalsCount(), 0);
    }

    function test_finalizeWithdrawalBatch_emitsEvent() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(alice, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: alice,
            amount: 500e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 expectedHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        // New event format: BatchFinalized(withdrawalQueueHash, withdrawalBatchIndex)
        vm.expectEmit(true, false, false, true);
        emit IZoneOutbox.BatchFinalized(
            expectedHash,
            1 // withdrawalBatchIndex increments to 1 on first finalize
        );

        vm.prank(sequencer);
        outbox.finalizeWithdrawalBatch(100, uint64(block.number));
    }

    function test_finalizeWithdrawalBatch_writesLastBatchToState() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(alice, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: alice,
            amount: 500e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 expectedHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.prank(sequencer);
        outbox.finalizeWithdrawalBatch(100, uint64(block.number));

        // Verify lastBatch storage was written correctly
        LastBatch memory batch = outbox.lastBatch();
        assertEq(batch.withdrawalQueueHash, expectedHash);
        assertEq(batch.withdrawalBatchIndex, 1);
        assertEq(outbox.withdrawalBatchIndex(), batch.withdrawalBatchIndex);
    }

    /*//////////////////////////////////////////////////////////////
                          ACCESS CONTROL TESTS
    //////////////////////////////////////////////////////////////*/

    function test_finalizeWithdrawalBatch_onlySequencer() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(alice, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        // Non-sequencer should revert
        vm.prank(alice);
        vm.expectRevert(ZoneOutbox.OnlySequencer.selector);
        outbox.finalizeWithdrawalBatch(100, uint64(block.number));

        // Sequencer should succeed
        vm.prank(sequencer);
        bytes32 hash = outbox.finalizeWithdrawalBatch(100, uint64(block.number));
        assertTrue(hash != bytes32(0));
    }

    /*//////////////////////////////////////////////////////////////
                    WITHDRAWAL WITH CALLBACK TESTS
    //////////////////////////////////////////////////////////////*/

    function test_finalizeWithdrawalBatch_withdrawalWithCallback_correctHash() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(
            bob, // to
            500e6, // amount
            bytes32("pay"), // memo
            100_000, // gasLimit
            alice, // fallbackRecipient
            "callback_data"
        );
        vm.stopPrank();

        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: bob,
            amount: 500e6,
            fee: 0,
            memo: bytes32("pay"),
            gasLimit: 100_000,
            fallbackRecipient: alice,
            callbackData: "callback_data"
        });
        bytes32 expectedHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.prank(sequencer);
        bytes32 hash = outbox.finalizeWithdrawalBatch(100, uint64(block.number));

        assertEq(hash, expectedHash);
    }

    /*//////////////////////////////////////////////////////////////
                    WITHDRAWAL INDEX TRACKING TESTS
    //////////////////////////////////////////////////////////////*/

    function test_nextWithdrawalIndex_incrementsCorrectly() public {
        assertEq(outbox.nextWithdrawalIndex(), 0);

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 3000e6);

        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, alice, "");
        assertEq(outbox.nextWithdrawalIndex(), 1);

        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, alice, "");
        assertEq(outbox.nextWithdrawalIndex(), 2);

        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, alice, "");
        assertEq(outbox.nextWithdrawalIndex(), 3);

        vm.stopPrank();
    }

    function test_nextWithdrawalIndex_persistsAcrossBatches() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 5000e6);

        // First batch
        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, alice, "");
        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, alice, "");

        vm.stopPrank();

        vm.prank(sequencer);
        outbox.finalizeWithdrawalBatch(type(uint256).max, uint64(block.number));

        // Second batch
        vm.startPrank(alice);
        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, alice, "");

        assertEq(outbox.nextWithdrawalIndex(), 3);
        vm.stopPrank();
    }

    /*//////////////////////////////////////////////////////////////
                      PENDING WITHDRAWALS TESTS
    //////////////////////////////////////////////////////////////*/

    function test_pendingWithdrawalsCount_tracksCorrectly() public {
        assertEq(outbox.pendingWithdrawalsCount(), 0);

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 3000e6);

        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, alice, "");
        assertEq(outbox.pendingWithdrawalsCount(), 1);

        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, alice, "");
        assertEq(outbox.pendingWithdrawalsCount(), 2);

        vm.stopPrank();

        // Finalize clears them
        vm.prank(sequencer);
        outbox.finalizeWithdrawalBatch(type(uint256).max, uint64(block.number));
        assertEq(outbox.pendingWithdrawalsCount(), 0);
    }

    /*//////////////////////////////////////////////////////////////
                      TOKEN TRANSFER TESTS
    //////////////////////////////////////////////////////////////*/

    function test_requestWithdrawal_transfersFromSender() public {
        uint256 aliceBalanceBefore = zoneToken.balanceOf(alice);

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(zoneToken.balanceOf(alice), aliceBalanceBefore - 500e6);
    }

    function test_requestWithdrawal_burnsTokens() public {
        uint256 totalSupplyBefore = zoneToken.totalSupply();

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(zoneToken.totalSupply(), totalSupplyBefore - 500e6);
    }

    function test_requestWithdrawal_revertsOnInsufficientBalance() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 200_000e6);

        vm.expectRevert(MockZoneToken.InsufficientBalance.selector);
        outbox.requestWithdrawal(bob, 200_000e6, bytes32(0), 0, alice, "");
        vm.stopPrank();
    }

    function test_requestWithdrawal_revertsOnInsufficientAllowance() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 100e6);

        vm.expectRevert(MockZoneToken.InsufficientAllowance.selector);
        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();
    }

    /*//////////////////////////////////////////////////////////////
                   FALLBACK RECIPIENT VALIDATION TESTS
    //////////////////////////////////////////////////////////////*/

    function test_requestWithdrawal_noCallbackNeedsFallback_ok() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);

        // gasLimit = 0, fallbackRecipient = alice is fine
        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 1);
    }

    function test_requestWithdrawal_callbackNeedsFallback_reverts() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);

        // fallbackRecipient = address(0) reverts
        vm.expectRevert(ZoneOutbox.InvalidFallbackRecipient.selector);
        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 0, address(0), "");
        vm.stopPrank();
    }

    function test_requestWithdrawal_callbackWithValidFallback_ok() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);

        // gasLimit > 0 with valid fallback
        outbox.requestWithdrawal(bob, 500e6, bytes32(0), 100_000, alice, "callback");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 1);
    }

    /*//////////////////////////////////////////////////////////////
                       FINALIZE BATCH TESTS
    //////////////////////////////////////////////////////////////*/

    function test_finalizeWithdrawalBatch_hashChainOrder() public {
        // Add three withdrawals
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 3000e6);
        outbox.requestWithdrawal(alice, 100e6, bytes32("w1"), 0, alice, "");
        vm.stopPrank();

        vm.startPrank(bob);
        zoneToken.approve(address(outbox), 3000e6);
        outbox.requestWithdrawal(bob, 200e6, bytes32("w2"), 0, alice, "");
        vm.stopPrank();

        vm.startPrank(charlie);
        zoneToken.approve(address(outbox), 3000e6);
        outbox.requestWithdrawal(charlie, 300e6, bytes32("w3"), 0, alice, "");
        vm.stopPrank();

        // Build expected hash (oldest = outermost)
        Withdrawal memory w1 = Withdrawal({
            sender: alice,
            to: alice,
            amount: 100e6,
            fee: 0,
            memo: bytes32("w1"),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        Withdrawal memory w2 = Withdrawal({
            sender: bob,
            to: bob,
            amount: 200e6,
            fee: 0,
            memo: bytes32("w2"),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        Withdrawal memory w3 = Withdrawal({
            sender: charlie,
            to: charlie,
            amount: 300e6,
            fee: 0,
            memo: bytes32("w3"),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });

        // Hash chain: w1 outermost, w3 innermost wrapping EMPTY_SENTINEL
        bytes32 innermost = keccak256(abi.encode(w3, EMPTY_SENTINEL));
        bytes32 middle = keccak256(abi.encode(w2, innermost));
        bytes32 expectedHash = keccak256(abi.encode(w1, middle));

        vm.prank(sequencer);
        bytes32 hash = outbox.finalizeWithdrawalBatch(type(uint256).max, uint64(block.number));

        assertEq(hash, expectedHash);
    }

    function test_finalizeWithdrawalBatch_partialBatch_leavesRemainder() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 5000e6);
        outbox.requestWithdrawal(alice, 100e6, bytes32("w1"), 0, alice, "");
        outbox.requestWithdrawal(alice, 200e6, bytes32("w2"), 0, alice, "");
        outbox.requestWithdrawal(alice, 300e6, bytes32("w3"), 0, alice, "");
        outbox.requestWithdrawal(alice, 400e6, bytes32("w4"), 0, alice, "");
        outbox.requestWithdrawal(alice, 500e6, bytes32("w5"), 0, alice, "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 5);

        // Process only 2 (oldest first: w1 and w2)
        vm.prank(sequencer);
        outbox.finalizeWithdrawalBatch(2, uint64(block.number));

        // 3 should remain: w3, w4, w5
        assertEq(outbox.pendingWithdrawalsCount(), 3);
    }

    function test_finalizeWithdrawalBatch_countLargerThanPending() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 1000e6);
        outbox.requestWithdrawal(alice, 100e6, bytes32("w1"), 0, alice, "");
        outbox.requestWithdrawal(alice, 200e6, bytes32("w2"), 0, alice, "");
        vm.stopPrank();

        // Process with large count
        vm.prank(sequencer);
        outbox.finalizeWithdrawalBatch(1000, uint64(block.number));

        assertEq(outbox.pendingWithdrawalsCount(), 0);
    }

    function test_finalizeWithdrawalBatch_consecutiveBatches() public {
        // First batch
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 10_000e6);
        outbox.requestWithdrawal(alice, 100e6, bytes32("b1w1"), 0, alice, "");
        outbox.requestWithdrawal(alice, 200e6, bytes32("b1w2"), 0, alice, "");
        vm.stopPrank();

        vm.prank(sequencer);
        bytes32 hash1 = outbox.finalizeWithdrawalBatch(type(uint256).max, uint64(block.number));
        assertTrue(hash1 != bytes32(0));

        // Second batch
        vm.startPrank(alice);
        outbox.requestWithdrawal(alice, 300e6, bytes32("b2w1"), 0, alice, "");
        vm.stopPrank();

        vm.prank(sequencer);
        bytes32 hash2 = outbox.finalizeWithdrawalBatch(type(uint256).max, uint64(block.number));
        assertTrue(hash2 != bytes32(0));

        // Hashes should be different
        assertTrue(hash1 != hash2);
    }

    /*//////////////////////////////////////////////////////////////
                        WITHDRAWAL STRUCT TESTS
    //////////////////////////////////////////////////////////////*/

    function test_requestWithdrawal_capturesAllFields() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 1000e6);
        outbox.requestWithdrawal(
            bob, // to
            500e6, // amount
            bytes32("payment123"), // memo
            50_000, // gasLimit
            charlie, // fallbackRecipient
            "callbackData" // data
        );
        vm.stopPrank();

        // Finalize and verify hash includes all fields
        Withdrawal memory expected = Withdrawal({
            sender: alice,
            to: bob,
            amount: 500e6,
            fee: 0,
            memo: bytes32("payment123"),
            gasLimit: 50_000,
            fallbackRecipient: charlie,
            callbackData: "callbackData"
        });
        bytes32 expectedHash = keccak256(abi.encode(expected, EMPTY_SENTINEL));

        vm.prank(sequencer);
        bytes32 hash = outbox.finalizeWithdrawalBatch(1, uint64(block.number));

        assertEq(hash, expectedHash);
    }

    /*//////////////////////////////////////////////////////////////
                          ZERO AMOUNT TESTS
    //////////////////////////////////////////////////////////////*/

    function test_requestWithdrawal_zeroAmount() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 0);
        outbox.requestWithdrawal(bob, 0, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 1);

        // Should still produce valid hash
        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: bob,
            amount: 0,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 expectedHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.prank(sequencer);
        bytes32 hash = outbox.finalizeWithdrawalBatch(1, uint64(block.number));

        assertEq(hash, expectedHash);
    }

    /*//////////////////////////////////////////////////////////////
                    WITHDRAWAL REQUESTED EVENT TESTS
    //////////////////////////////////////////////////////////////*/

    function test_requestWithdrawal_emitsEvent() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);

        uint128 expectedFee = outbox.calculateWithdrawalFee(50_000);
        vm.expectEmit(true, true, false, true);
        emit IZoneOutbox.WithdrawalRequested(
            0, // index
            alice, // sender
            bob, // to
            500e6, // amount
            expectedFee, // fee
            bytes32("memo"),
            50_000, // gasLimit
            charlie, // fallbackRecipient
            "data"
        );

        outbox.requestWithdrawal(bob, 500e6, bytes32("memo"), 50_000, charlie, "data");
        vm.stopPrank();
    }

    /*//////////////////////////////////////////////////////////////
                        IMMUTABLE GETTERS TESTS
    //////////////////////////////////////////////////////////////*/

    function test_immutableGetters() public view {
        assertEq(address(outbox.zoneToken()), address(zoneToken));
        assertEq(address(outbox.config()), address(config));
        assertEq(config.sequencer(), sequencer);
    }

    /*//////////////////////////////////////////////////////////////
                    LARGE WITHDRAWAL BATCH TESTS
    //////////////////////////////////////////////////////////////*/

    function test_finalizeWithdrawalBatch_manyWithdrawals() public {
        uint256 numWithdrawals = 50;

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), numWithdrawals * 100e6);

        for (uint256 i = 0; i < numWithdrawals; i++) {
            outbox.requestWithdrawal(bob, 100e6, bytes32(i), 0, alice, "");
        }
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), numWithdrawals);

        vm.prank(sequencer);
        bytes32 hash = outbox.finalizeWithdrawalBatch(type(uint256).max, uint64(block.number));

        assertTrue(hash != bytes32(0));
        assertEq(outbox.pendingWithdrawalsCount(), 0);
    }

    /*//////////////////////////////////////////////////////////////
                     MAX WITHDRAWALS PER BLOCK TESTS
    //////////////////////////////////////////////////////////////*/

    function test_setMaxWithdrawalsPerBlock_onlySequencer() public {
        vm.prank(alice);
        vm.expectRevert(ZoneOutbox.OnlySequencer.selector);
        outbox.setMaxWithdrawalsPerBlock(10);
    }

    function test_setMaxWithdrawalsPerBlock_sequencerCanSet() public {
        vm.prank(sequencer);
        outbox.setMaxWithdrawalsPerBlock(5);
        assertEq(outbox.maxWithdrawalsPerBlock(), 5);
    }

    function test_setMaxWithdrawalsPerBlock_zeroMeansUnlimited() public {
        vm.prank(sequencer);
        outbox.setMaxWithdrawalsPerBlock(0);
        assertEq(outbox.maxWithdrawalsPerBlock(), 0);

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 1000e6);
        for (uint256 i = 0; i < 10; i++) {
            outbox.requestWithdrawal(alice, 10e6, bytes32(0), 0, alice, "");
        }
        vm.stopPrank();
        assertEq(outbox.pendingWithdrawalsCount(), 10);
    }

    function test_maxWithdrawalsPerBlock_enforcesLimit() public {
        vm.prank(sequencer);
        outbox.setMaxWithdrawalsPerBlock(3);

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 1000e6);

        outbox.requestWithdrawal(alice, 10e6, bytes32(0), 0, alice, "");
        outbox.requestWithdrawal(alice, 10e6, bytes32(0), 0, alice, "");
        outbox.requestWithdrawal(alice, 10e6, bytes32(0), 0, alice, "");

        vm.expectRevert(ZoneOutbox.TooManyWithdrawalsThisBlock.selector);
        outbox.requestWithdrawal(alice, 10e6, bytes32(0), 0, alice, "");
        vm.stopPrank();
    }

    function test_maxWithdrawalsPerBlock_resetsOnNewBlock() public {
        vm.prank(sequencer);
        outbox.setMaxWithdrawalsPerBlock(2);

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 1000e6);

        outbox.requestWithdrawal(alice, 10e6, bytes32(0), 0, alice, "");
        outbox.requestWithdrawal(alice, 10e6, bytes32(0), 0, alice, "");

        vm.expectRevert(ZoneOutbox.TooManyWithdrawalsThisBlock.selector);
        outbox.requestWithdrawal(alice, 10e6, bytes32(0), 0, alice, "");

        vm.roll(block.number + 1);

        outbox.requestWithdrawal(alice, 10e6, bytes32(0), 0, alice, "");
        outbox.requestWithdrawal(alice, 10e6, bytes32(0), 0, alice, "");

        vm.expectRevert(ZoneOutbox.TooManyWithdrawalsThisBlock.selector);
        outbox.requestWithdrawal(alice, 10e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 4);
    }

    function test_maxWithdrawalsPerBlock_canBeUpdated() public {
        vm.startPrank(sequencer);
        outbox.setMaxWithdrawalsPerBlock(1);
        assertEq(outbox.maxWithdrawalsPerBlock(), 1);

        outbox.setMaxWithdrawalsPerBlock(100);
        assertEq(outbox.maxWithdrawalsPerBlock(), 100);

        outbox.setMaxWithdrawalsPerBlock(0);
        assertEq(outbox.maxWithdrawalsPerBlock(), 0);
        vm.stopPrank();
    }

}
