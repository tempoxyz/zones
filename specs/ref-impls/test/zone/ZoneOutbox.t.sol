// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { IZoneOutbox, LastBatch, Withdrawal, ZONE_TX_CONTEXT } from "../../src/zone/IZone.sol";
import { EMPTY_SENTINEL } from "../../src/zone/WithdrawalQueueLib.sol";
import { ZoneConfig } from "../../src/zone/ZoneConfig.sol";
import { ZoneInbox } from "../../src/zone/ZoneInbox.sol";
import { ZoneOutbox } from "../../src/zone/ZoneOutbox.sol";
import { MockTIP403Registry } from "./mocks/MockTIP403Registry.sol";
import { MockTempoState } from "./mocks/MockTempoState.sol";
import { MockZoneToken } from "./mocks/MockZoneToken.sol";
import { MockZoneTxContext } from "./mocks/MockZoneTxContext.sol";
import { Test } from "forge-std/Test.sol";
import { StdPrecompiles } from "tempo-std/StdPrecompiles.sol";

/// @title ZoneOutboxTest
/// @notice Tests for ZoneOutbox finalizeWithdrawalBatch() functionality and withdrawal storage
contract ZoneOutboxTest is Test {

    ZoneConfig public config;
    ZoneOutbox public outbox;
    ZoneInbox public inbox;
    MockZoneToken public zoneToken;
    MockTempoState public tempoState;
    MockZoneTxContext public txContext = MockZoneTxContext(ZONE_TX_CONTEXT);

    address public sequencer = address(0x1);
    address public alice = address(0x200);
    address public bob = address(0x300);
    address public charlie = address(0x400);
    address public mockPortal = address(0x400);

    bytes32 constant GENESIS_TEMPO_BLOCK_HASH = keccak256("tempoGenesis");
    uint64 constant GENESIS_TEMPO_BLOCK_NUMBER = 1;

    function setUp() public {
        MockZoneTxContext mockTxContext = new MockZoneTxContext();
        vm.etch(ZONE_TX_CONTEXT, address(mockTxContext).code);

        // Install permissive TIP-403 registry so withdrawal requests succeed
        // without policy configuration in this isolated unit test setup.
        MockTIP403Registry mockTIP403Registry = new MockTIP403Registry();
        vm.etch(StdPrecompiles.TIP403_REGISTRY_ADDRESS, address(mockTIP403Registry).code);

        zoneToken = new MockZoneToken("Zone USD", "zUSD");
        tempoState =
            new MockTempoState(sequencer, GENESIS_TEMPO_BLOCK_HASH, GENESIS_TEMPO_BLOCK_NUMBER);
        config = new ZoneConfig(mockPortal, address(tempoState));
        tempoState.setMockStorageValue(
            mockPortal, bytes32(uint256(0)), bytes32(uint256(uint160(sequencer)))
        );
        outbox = new ZoneOutbox(address(config), mockPortal, address(tempoState));
        inbox = new ZoneInbox(address(config), mockPortal, address(tempoState), address(outbox));

        // Grant minter role to inbox and burner role to outbox
        zoneToken.setMinter(address(inbox), true);
        zoneToken.setBurner(address(outbox), true);

        // Give alice and bob tokens
        zoneToken.setMinter(address(this), true);
        zoneToken.mint(alice, 10_000e6);
        zoneToken.mint(bob, 10_000e6);
        zoneToken.mint(charlie, 10_000e6);
    }

    function _senderTag(address sender, uint256 txSequence) internal view returns (bytes32) {
        return keccak256(abi.encodePacked(sender, txContext.txHashFor(txSequence)));
    }

    function _withdrawal(
        uint256 txSequence,
        address sender,
        address to,
        uint128 amount,
        bytes32 memo,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes memory callbackData
    )
        internal
        view
        returns (Withdrawal memory)
    {
        return Withdrawal({
            token: address(zoneToken),
            senderTag: _senderTag(sender, txSequence),
            to: to,
            amount: amount,
            fee: 0,
            memo: memo,
            gasLimit: gasLimit,
            fallbackRecipient: fallbackRecipient,
            callbackData: callbackData,
            encryptedSender: "",
            bouncebackFee: 0
        });
    }

    function _validRevealTo() internal pure returns (bytes memory) {
        return hex"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
    }

    function _emptyEncryptedSenders(uint256 count)
        internal
        view
        returns (bytes[] memory encryptedSenders)
    {
        uint256 pending = outbox.pendingWithdrawalsCount();
        if (count > pending) {
            count = pending;
        }
        encryptedSenders = new bytes[](count);
    }

    function _finalizeWithdrawalBatch(uint256 count) internal returns (bytes32) {
        return _finalizeWithdrawalBatchAs(sequencer, count);
    }

    function _finalizeWithdrawalBatchAs(address caller, uint256 count) internal returns (bytes32) {
        vm.startPrank(caller);
        bytes32 hash = outbox.finalizeWithdrawalBatch(
            count, uint64(block.number), _emptyEncryptedSenders(count)
        );
        vm.stopPrank();
        return hash;
    }

    /*//////////////////////////////////////////////////////////////
                          STORAGE TESTS
    //////////////////////////////////////////////////////////////*/

    function test_requestWithdrawal_storesInArray() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32("memo"), 0, alice, "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 1);

        vm.startPrank(bob);
        zoneToken.approve(address(outbox), 300e6);
        outbox.requestWithdrawal(address(zoneToken), bob, 300e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 2);
    }

    /*//////////////////////////////////////////////////////////////
                       FINALIZE BATCH TESTS
    //////////////////////////////////////////////////////////////*/

    function test_finalizeWithdrawalBatch_emptyQueue_returnsZero() public {
        bytes32 hash = _finalizeWithdrawalBatch(100);

        // Still emits event with zero count
        assertEq(hash, bytes32(0));
    }

    function test_finalizeWithdrawalBatch_zeroCount_returnsZero() public {
        // Add a withdrawal
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 1);

        // finalizeWithdrawalBatch with count=0 should return 0 and not process withdrawals
        bytes32 hash = _finalizeWithdrawalBatch(0);

        assertEq(hash, bytes32(0));
        assertEq(outbox.pendingWithdrawalsCount(), 1);
    }

    function test_finalizeWithdrawalBatch_singleWithdrawal_correctHash() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32("memo"), 0, alice, "");
        vm.stopPrank();

        // Expected hash
        Withdrawal memory w = _withdrawal(1, alice, alice, 500e6, bytes32("memo"), 0, alice, "");
        bytes32 expectedHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        bytes32 hash = _finalizeWithdrawalBatch(100);

        assertEq(hash, expectedHash);
    }

    function test_finalizeWithdrawalBatch_multipleWithdrawals_correctHashChain() public {
        // Alice withdraws
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        // Bob withdraws
        vm.startPrank(bob);
        zoneToken.approve(address(outbox), 300e6);
        outbox.requestWithdrawal(address(zoneToken), bob, 300e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        // Build expected hash (oldest = outermost)
        // w0 = alice's withdrawal (first, oldest)
        // w1 = bob's withdrawal (second, newest)
        // Hash chain: hash(w0, hash(w1, EMPTY_SENTINEL))
        Withdrawal memory w0 = _withdrawal(1, alice, alice, 500e6, bytes32(0), 0, alice, "");
        Withdrawal memory w1 = _withdrawal(2, bob, bob, 300e6, bytes32(0), 0, alice, "");

        bytes32 innerHash = keccak256(abi.encode(w1, EMPTY_SENTINEL));
        bytes32 expectedHash = keccak256(abi.encode(w0, innerHash));

        bytes32 hash = _finalizeWithdrawalBatch(100);

        assertEq(hash, expectedHash);
    }

    function test_finalizeWithdrawalBatch_clearsStorage() public {
        // Add withdrawals
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 1000e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32(0), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 300e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 2);

        // Batch all
        _finalizeWithdrawalBatch(type(uint256).max);

        assertEq(outbox.pendingWithdrawalsCount(), 0);
    }

    function test_finalizeWithdrawalBatch_partialBatch_processesOnlyCount() public {
        // Add 3 withdrawals
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 1500e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32("w1"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32("w2"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32("w3"), 0, alice, "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 3);

        // Batch only 2 (should process w1 and w2, leaving w3)
        bytes32 hash = _finalizeWithdrawalBatch(2);

        // Should have 1 left (w3)
        assertEq(outbox.pendingWithdrawalsCount(), 1);

        // Expected hash for w1 and w2 (w1 is oldest of the two)
        Withdrawal memory w1 = _withdrawal(1, alice, alice, 500e6, bytes32("w1"), 0, alice, "");
        Withdrawal memory w2 = _withdrawal(2, alice, alice, 500e6, bytes32("w2"), 0, alice, "");

        bytes32 innerHash = keccak256(abi.encode(w2, EMPTY_SENTINEL));
        bytes32 expectedHash = keccak256(abi.encode(w1, innerHash));

        assertEq(hash, expectedHash);
    }

    function test_finalizeWithdrawalBatch_partialBatches_fifoOrder() public {
        // Add 4 withdrawals in order
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 4000e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 100e6, bytes32("w1"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 200e6, bytes32("w2"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 300e6, bytes32("w3"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 400e6, bytes32("w4"), 0, alice, "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 4);

        // First batch takes w1, w2
        bytes32 hash1 = _finalizeWithdrawalBatch(2);

        Withdrawal memory w1 = _withdrawal(1, alice, alice, 100e6, bytes32("w1"), 0, alice, "");
        Withdrawal memory w2 = _withdrawal(2, alice, alice, 200e6, bytes32("w2"), 0, alice, "");
        bytes32 innerHash1 = keccak256(abi.encode(w2, EMPTY_SENTINEL));
        bytes32 expectedHash1 = keccak256(abi.encode(w1, innerHash1));

        assertEq(hash1, expectedHash1);
        assertEq(outbox.pendingWithdrawalsCount(), 2);

        // Second batch takes w3, w4
        bytes32 hash2 = _finalizeWithdrawalBatch(2);

        Withdrawal memory w3 = _withdrawal(3, alice, alice, 300e6, bytes32("w3"), 0, alice, "");
        Withdrawal memory w4 = _withdrawal(4, alice, alice, 400e6, bytes32("w4"), 0, alice, "");
        bytes32 innerHash2 = keccak256(abi.encode(w4, EMPTY_SENTINEL));
        bytes32 expectedHash2 = keccak256(abi.encode(w3, innerHash2));

        assertEq(hash2, expectedHash2);
        assertEq(outbox.pendingWithdrawalsCount(), 0);
    }

    function test_finalizeWithdrawalBatch_emitsEvent() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        Withdrawal memory w = _withdrawal(1, alice, alice, 500e6, bytes32(0), 0, alice, "");
        bytes32 expectedHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        // New event format: BatchFinalized(withdrawalQueueHash, withdrawalBatchIndex)
        vm.expectEmit(true, false, false, true);
        emit IZoneOutbox.BatchFinalized(
            expectedHash,
            1 // withdrawalBatchIndex increments to 1 on first finalize
        );

        _finalizeWithdrawalBatch(100);
    }

    function test_finalizeWithdrawalBatch_writesLastBatchToState() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        Withdrawal memory w = _withdrawal(1, alice, alice, 500e6, bytes32(0), 0, alice, "");
        bytes32 expectedHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        _finalizeWithdrawalBatch(100);

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
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        // Non-sequencer should revert
        bytes[] memory encryptedSenders = _emptyEncryptedSenders(100);
        vm.startPrank(alice);
        vm.expectRevert(ZoneOutbox.OnlySequencer.selector);
        outbox.finalizeWithdrawalBatch(100, uint64(block.number), encryptedSenders);
        vm.stopPrank();

        // Sequencer should succeed
        bytes32 hash = _finalizeWithdrawalBatch(100);
        assertTrue(hash != bytes32(0));
    }

    /*//////////////////////////////////////////////////////////////
                    WITHDRAWAL WITH CALLBACK TESTS
    //////////////////////////////////////////////////////////////*/

    function test_finalizeWithdrawalBatch_withdrawalWithCallback_correctHash() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(
            address(zoneToken), // token
            bob, // to
            500e6, // amount
            bytes32("pay"), // memo
            100_000, // gasLimit
            alice, // fallbackRecipient
            "callback_data"
        );
        vm.stopPrank();

        Withdrawal memory w =
            _withdrawal(1, alice, bob, 500e6, bytes32("pay"), 100_000, alice, "callback_data");
        bytes32 expectedHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        bytes32 hash = _finalizeWithdrawalBatch(100);

        assertEq(hash, expectedHash);
    }

    /*//////////////////////////////////////////////////////////////
                    WITHDRAWAL INDEX TRACKING TESTS
    //////////////////////////////////////////////////////////////*/

    function test_nextWithdrawalIndex_incrementsCorrectly() public {
        assertEq(outbox.nextWithdrawalIndex(), 0);

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 3000e6);

        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "");
        assertEq(outbox.nextWithdrawalIndex(), 1);

        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "");
        assertEq(outbox.nextWithdrawalIndex(), 2);

        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "");
        assertEq(outbox.nextWithdrawalIndex(), 3);

        vm.stopPrank();
    }

    function test_nextWithdrawalIndex_persistsAcrossBatches() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 5000e6);

        // First batch
        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "");

        vm.stopPrank();

        _finalizeWithdrawalBatch(type(uint256).max);

        // Second batch
        vm.startPrank(alice);
        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "");

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

        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "");
        assertEq(outbox.pendingWithdrawalsCount(), 1);

        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "");
        assertEq(outbox.pendingWithdrawalsCount(), 2);

        vm.stopPrank();

        // Finalize clears them
        _finalizeWithdrawalBatch(type(uint256).max);
        assertEq(outbox.pendingWithdrawalsCount(), 0);
    }

    /*//////////////////////////////////////////////////////////////
                      TOKEN TRANSFER TESTS
    //////////////////////////////////////////////////////////////*/

    function test_requestWithdrawal_transfersFromSender() public {
        uint256 aliceBalanceBefore = zoneToken.balanceOf(alice);

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(zoneToken.balanceOf(alice), aliceBalanceBefore - 500e6);
    }

    function test_requestWithdrawal_burnsTokens() public {
        uint256 totalSupplyBefore = zoneToken.totalSupply();

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(zoneToken.totalSupply(), totalSupplyBefore - 500e6);
    }

    function test_requestWithdrawal_revertsOnInsufficientBalance() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 200_000e6);

        vm.expectRevert(MockZoneToken.InsufficientBalance.selector);
        outbox.requestWithdrawal(address(zoneToken), bob, 200_000e6, bytes32(0), 0, alice, "");
        vm.stopPrank();
    }

    function test_requestWithdrawal_revertsOnInsufficientAllowance() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 100e6);

        vm.expectRevert(MockZoneToken.InsufficientAllowance.selector);
        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();
    }

    /*//////////////////////////////////////////////////////////////
                   FALLBACK RECIPIENT VALIDATION TESTS
    //////////////////////////////////////////////////////////////*/

    function test_requestWithdrawal_noCallbackNeedsFallback_ok() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);

        // gasLimit = 0, fallbackRecipient = alice is fine
        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 1);
    }

    function test_requestWithdrawal_callbackNeedsFallback_reverts() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);

        // fallbackRecipient = address(0) reverts
        vm.expectRevert(ZoneOutbox.InvalidFallbackRecipient.selector);
        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, address(0), "");
        vm.stopPrank();
    }

    function test_requestWithdrawal_callbackWithValidFallback_ok() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);

        // gasLimit > 0 with valid fallback
        outbox.requestWithdrawal(
            address(zoneToken), bob, 500e6, bytes32(0), 100_000, alice, "callback"
        );
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 1);
    }

    function test_requestWithdrawal_validRevealTo_ok() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(
            address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "", _validRevealTo()
        );
        vm.stopPrank();

        bytes[] memory encryptedSenders = new bytes[](1);
        encryptedSenders[0] = new bytes(outbox.AUTHENTICATED_WITHDRAWAL_CIPHERTEXT_LENGTH());

        vm.prank(sequencer);
        bytes32 hash = outbox.finalizeWithdrawalBatch(1, uint64(block.number), encryptedSenders);
        assertTrue(hash != bytes32(0));
    }

    function test_requestWithdrawal_invalidRevealToLength_reverts() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);

        vm.expectRevert(ZoneOutbox.InvalidRevealTo.selector);
        outbox.requestWithdrawal(
            address(zoneToken),
            bob,
            500e6,
            bytes32(0),
            0,
            alice,
            "",
            hex"0211111111111111111111111111111111111111111111111111111111111111"
        );
        vm.stopPrank();
    }

    function test_requestWithdrawal_invalidRevealToPrefix_reverts() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);

        vm.expectRevert(ZoneOutbox.InvalidRevealTo.selector);
        outbox.requestWithdrawal(
            address(zoneToken),
            bob,
            500e6,
            bytes32(0),
            0,
            alice,
            "",
            hex"041111111111111111111111111111111111111111111111111111111111111111"
        );
        vm.stopPrank();
    }

    function test_requestWithdrawal_invalidRevealToPoint_reverts() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);

        vm.expectRevert(ZoneOutbox.InvalidRevealTo.selector);
        outbox.requestWithdrawal(
            address(zoneToken),
            bob,
            500e6,
            bytes32(0),
            0,
            alice,
            "",
            hex"020000000000000000000000000000000000000000000000000000000000000005"
        );
        vm.stopPrank();
    }

    function test_finalizeWithdrawalBatch_encryptedSenderCountMismatch_reverts() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        bytes[] memory encryptedSenders = new bytes[](0);

        vm.prank(sequencer);
        vm.expectRevert(
            abi.encodeWithSelector(ZoneOutbox.InvalidEncryptedSenderCount.selector, 0, 1)
        );
        outbox.finalizeWithdrawalBatch(1, uint64(block.number), encryptedSenders);
    }

    function test_finalizeWithdrawalBatch_encryptedSenderLengthMismatch_reverts() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(
            address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "", _validRevealTo()
        );
        vm.stopPrank();

        bytes[] memory encryptedSenders = new bytes[](1);
        encryptedSenders[0] = hex"1234";

        vm.expectRevert(
            abi.encodeWithSelector(
                ZoneOutbox.InvalidEncryptedSenderLength.selector,
                uint256(2),
                outbox.AUTHENTICATED_WITHDRAWAL_CIPHERTEXT_LENGTH()
            )
        );
        vm.prank(sequencer);
        outbox.finalizeWithdrawalBatch(1, uint64(block.number), encryptedSenders);
    }

    /*//////////////////////////////////////////////////////////////
                       FINALIZE BATCH TESTS
    //////////////////////////////////////////////////////////////*/

    function test_finalizeWithdrawalBatch_hashChainOrder() public {
        // Add three withdrawals
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 3000e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 100e6, bytes32("w1"), 0, alice, "");
        vm.stopPrank();

        vm.startPrank(bob);
        zoneToken.approve(address(outbox), 3000e6);
        outbox.requestWithdrawal(address(zoneToken), bob, 200e6, bytes32("w2"), 0, alice, "");
        vm.stopPrank();

        vm.startPrank(charlie);
        zoneToken.approve(address(outbox), 3000e6);
        outbox.requestWithdrawal(address(zoneToken), charlie, 300e6, bytes32("w3"), 0, alice, "");
        vm.stopPrank();

        // Build expected hash (oldest = outermost)
        Withdrawal memory w1 = _withdrawal(1, alice, alice, 100e6, bytes32("w1"), 0, alice, "");
        Withdrawal memory w2 = _withdrawal(2, bob, bob, 200e6, bytes32("w2"), 0, alice, "");
        Withdrawal memory w3 = _withdrawal(3, charlie, charlie, 300e6, bytes32("w3"), 0, alice, "");

        // Hash chain: w1 outermost, w3 innermost wrapping EMPTY_SENTINEL
        bytes32 innermost = keccak256(abi.encode(w3, EMPTY_SENTINEL));
        bytes32 middle = keccak256(abi.encode(w2, innermost));
        bytes32 expectedHash = keccak256(abi.encode(w1, middle));

        bytes32 hash = _finalizeWithdrawalBatch(type(uint256).max);

        assertEq(hash, expectedHash);
    }

    function test_finalizeWithdrawalBatch_partialBatch_leavesRemainder() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 5000e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 100e6, bytes32("w1"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 200e6, bytes32("w2"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 300e6, bytes32("w3"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 400e6, bytes32("w4"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32("w5"), 0, alice, "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 5);

        // Process only 2 (oldest first: w1 and w2)
        _finalizeWithdrawalBatch(2);

        // 3 should remain: w3, w4, w5
        assertEq(outbox.pendingWithdrawalsCount(), 3);
    }

    function test_finalizeWithdrawalBatch_countLargerThanPending() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 1000e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 100e6, bytes32("w1"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 200e6, bytes32("w2"), 0, alice, "");
        vm.stopPrank();

        // Process with large count
        _finalizeWithdrawalBatch(1000);

        assertEq(outbox.pendingWithdrawalsCount(), 0);
    }

    function test_finalizeWithdrawalBatch_consecutiveBatches() public {
        // First batch
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 10_000e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 100e6, bytes32("b1w1"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 200e6, bytes32("b1w2"), 0, alice, "");
        vm.stopPrank();

        bytes32 hash1 = _finalizeWithdrawalBatch(type(uint256).max);
        assertTrue(hash1 != bytes32(0));

        // Second batch
        vm.startPrank(alice);
        outbox.requestWithdrawal(address(zoneToken), alice, 300e6, bytes32("b2w1"), 0, alice, "");
        vm.stopPrank();

        bytes32 hash2 = _finalizeWithdrawalBatch(type(uint256).max);
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
            address(zoneToken), // token
            bob, // to
            500e6, // amount
            bytes32("payment123"), // memo
            50_000, // gasLimit
            charlie, // fallbackRecipient
            "callbackData" // data
        );
        vm.stopPrank();

        // Finalize and verify hash includes all fields
        Withdrawal memory expected = _withdrawal(
            1, alice, bob, 500e6, bytes32("payment123"), 50_000, charlie, "callbackData"
        );
        bytes32 expectedHash = keccak256(abi.encode(expected, EMPTY_SENTINEL));

        bytes32 hash = _finalizeWithdrawalBatch(1);

        assertEq(hash, expectedHash);
    }

    /*//////////////////////////////////////////////////////////////
                          ZERO AMOUNT TESTS
    //////////////////////////////////////////////////////////////*/

    function test_requestWithdrawal_zeroAmount() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 0);
        outbox.requestWithdrawal(address(zoneToken), bob, 0, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), 1);

        // Should still produce valid hash
        Withdrawal memory w = _withdrawal(1, alice, bob, 0, bytes32(0), 0, alice, "");
        bytes32 expectedHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        bytes32 hash = _finalizeWithdrawalBatch(1);

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
            address(zoneToken), // token
            bob, // to
            500e6, // amount
            expectedFee, // fee
            bytes32("memo"),
            50_000, // gasLimit
            charlie, // fallbackRecipient
            "data",
            ""
        );

        outbox.requestWithdrawal(
            address(zoneToken), bob, 500e6, bytes32("memo"), 50_000, charlie, "data"
        );
        vm.stopPrank();
    }

    /*//////////////////////////////////////////////////////////////
                        IMMUTABLE GETTERS TESTS
    //////////////////////////////////////////////////////////////*/

    function test_immutableGetters() public view {
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
            outbox.requestWithdrawal(address(zoneToken), bob, 100e6, bytes32(i), 0, alice, "");
        }
        vm.stopPrank();

        assertEq(outbox.pendingWithdrawalsCount(), numWithdrawals);

        bytes32 hash = _finalizeWithdrawalBatch(type(uint256).max);

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
            outbox.requestWithdrawal(address(zoneToken), alice, 10e6, bytes32(0), 0, alice, "");
        }
        vm.stopPrank();
        assertEq(outbox.pendingWithdrawalsCount(), 10);
    }

    function test_maxWithdrawalsPerBlock_enforcesLimit() public {
        vm.prank(sequencer);
        outbox.setMaxWithdrawalsPerBlock(3);

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 1000e6);

        outbox.requestWithdrawal(address(zoneToken), alice, 10e6, bytes32(0), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 10e6, bytes32(0), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 10e6, bytes32(0), 0, alice, "");

        vm.expectRevert(ZoneOutbox.TooManyWithdrawalsThisBlock.selector);
        outbox.requestWithdrawal(address(zoneToken), alice, 10e6, bytes32(0), 0, alice, "");
        vm.stopPrank();
    }

    function test_maxWithdrawalsPerBlock_resetsOnNewBlock() public {
        vm.prank(sequencer);
        outbox.setMaxWithdrawalsPerBlock(2);

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 1000e6);

        outbox.requestWithdrawal(address(zoneToken), alice, 10e6, bytes32(0), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 10e6, bytes32(0), 0, alice, "");

        vm.expectRevert(ZoneOutbox.TooManyWithdrawalsThisBlock.selector);
        outbox.requestWithdrawal(address(zoneToken), alice, 10e6, bytes32(0), 0, alice, "");

        vm.roll(block.number + 1);

        outbox.requestWithdrawal(address(zoneToken), alice, 10e6, bytes32(0), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 10e6, bytes32(0), 0, alice, "");

        vm.expectRevert(ZoneOutbox.TooManyWithdrawalsThisBlock.selector);
        outbox.requestWithdrawal(address(zoneToken), alice, 10e6, bytes32(0), 0, alice, "");
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
