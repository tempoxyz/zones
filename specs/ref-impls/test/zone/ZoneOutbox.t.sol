// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    DepositPayload,
    IZoneOutbox,
    IZonePortal,
    LastBatch,
    PORTAL_ACCESS_MODE_SLOT,
    PORTAL_IS_SEQUENCER_SLOT,
    PendingWithdrawal,
    Withdrawal,
    ZONE_INBOX,
    ZONE_TX_CONTEXT
} from "../../src/interfaces/IZone.sol";
import { EMPTY_SENTINEL } from "../../src/libraries/WithdrawalQueueLib.sol";
import { ZoneInbox } from "../../src/zone/ZoneInbox.sol";
import { ZoneOutbox } from "../../src/zone/ZoneOutbox.sol";
import { MockTempoState } from "../mocks/MockTempoState.sol";
import { GatewayCallbackData, GatewayFlow } from "../mocks/MockZoneGateway.sol";
import { MockZoneToken } from "../mocks/MockZoneToken.sol";
import { MockZoneTxContext } from "../mocks/MockZoneTxContext.sol";
import { Test } from "forge-std/Test.sol";

contract ZeroTxContext {

    function currentTxHash() external pure returns (bytes32) {
        return bytes32(0);
    }

}

/// @title ZoneOutboxTest
/// @notice Tests for ZoneOutbox finalizeWithdrawalBatch() functionality and withdrawal storage
contract ZoneOutboxTest is Test {

    uint128 internal constant TEST_MAX_TEMPO_GAS_RATE = 1e18;

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
    address public callbackTarget = address(0x500);

    bytes32 constant GENESIS_TEMPO_BLOCK_HASH = keccak256("tempoGenesis");
    uint64 constant GENESIS_TEMPO_BLOCK_NUMBER = 1;

    function setUp() public {
        MockZoneTxContext mockTxContext = new MockZoneTxContext();
        vm.etch(ZONE_TX_CONTEXT, address(mockTxContext).code);

        zoneToken = new MockZoneToken("Zone USD", "zUSD");
        tempoState =
            new MockTempoState(sequencer, GENESIS_TEMPO_BLOCK_HASH, GENESIS_TEMPO_BLOCK_NUMBER);
        tempoState.setMockStorageValue(
            mockPortal,
            keccak256(abi.encode(sequencer, PORTAL_IS_SEQUENCER_SLOT)),
            bytes32(uint256(1))
        );
        tempoState.setMockTokenEnabled(mockPortal, address(zoneToken), true);
        tempoState.setMockMaxTempoGasRate(mockPortal, TEST_MAX_TEMPO_GAS_RATE);
        tempoState.setMockAccountAllowed(mockPortal, sequencer, true);
        tempoState.setMockAccountAllowed(mockPortal, alice, true);
        tempoState.setMockAccountAllowed(mockPortal, bob, true);
        tempoState.setMockAccountAllowed(mockPortal, charlie, true);
        tempoState.setMockZoneGateway(mockPortal, callbackTarget, true);
        _setModes(true, true);
        inbox = new ZoneInbox(mockPortal, address(tempoState));
        outbox = new ZoneOutbox(mockPortal, address(tempoState));

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
        return
            keccak256(abi.encodePacked(sender, txContext.txHashFor(txSequence), uint64(txSequence)));
    }

    function _setModes(bool accessMode, bool gatewayMode) internal {
        uint256 modes;
        if (accessMode) modes |= 1;
        if (gatewayMode) modes |= 1 << 8;
        tempoState.setMockStorageValue(mockPortal, PORTAL_ACCESS_MODE_SLOT, bytes32(modes));
    }

    function _setMaxTempoGasRate(uint128 rate) internal {
        tempoState.setMockMaxTempoGasRate(mockPortal, rate);
    }

    function _withdrawal(
        uint256 txSequence,
        address sender,
        address to,
        uint128 amount,
        bytes32 memo,
        uint64 gasLimit,
        address,
        /* zoneFallbackRecipient */
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
            memo: memo,
            gasLimit: gasLimit,
            fallbackNonce: uint64(txSequence),
            callbackData: callbackData,
            encryptedSender: ""
        });
    }

    function _validRevealTo() internal pure returns (bytes memory) {
        return hex"0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
    }

    function _callbackData(
        GatewayFlow flow,
        address tempoRefundRecipient
    )
        internal
        view
        returns (bytes memory)
    {
        bytes memory gatewayData = abi.encode(
            GatewayCallbackData({
                flow: flow,
                outputToken: address(zoneToken),
                keyIndex: 0,
                encrypted: DepositPayload({
                    ephemeralPubkeyX: bytes32(uint256(1)),
                    ephemeralPubkeyYParity: 0x02,
                    ciphertext: new bytes(64),
                    nonce: bytes12(0),
                    tag: bytes16(0)
                }),
                minVaultAssets: 0,
                minVaultShares: 0,
                minOutputAmount: 0,
                actionId: bytes32(0),
                tempoRefundRecipient: tempoRefundRecipient
            })
        );
        return abi.encode(keccak256(gatewayData), gatewayData);
    }

    function _emptyEncryptedSenders(uint256 count)
        internal
        view
        returns (bytes[] memory encryptedSenders)
    {
        encryptedSenders = new bytes[](count);
    }

    function _pendingWithdrawalsCount() internal returns (uint256 count) {
        vm.prank(sequencer);
        count = outbox.pendingWithdrawalsCount();
    }

    function _getPendingWithdrawals() internal returns (PendingWithdrawal[] memory pending) {
        vm.prank(sequencer);
        pending = outbox.getPendingWithdrawals();
    }

    function _finalizeWithdrawalBatch(uint256 count) internal returns (bytes32) {
        return _finalizeWithdrawalBatchAs(sequencer, count);
    }

    function test_enqueueDepositBounceBack_queuesRevokedTempoRefundRecipient() public {
        uint128 amount = 1000e6;
        tempoState.setMockAccountAllowed(mockPortal, bob, false);

        vm.expectEmit(true, true, false, true);
        emit IZoneOutbox.WithdrawalRequested(
            0, address(0), address(zoneToken), bob, amount, 0, bytes32(0), 0, 0, "", ""
        );

        vm.prank(ZONE_INBOX);
        outbox.enqueueDepositBounceBack(address(zoneToken), amount, bob);

        Withdrawal memory expected = Withdrawal({
            token: address(zoneToken),
            senderTag: keccak256(abi.encodePacked(address(0), bytes32(0))),
            to: bob,
            amount: amount,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackNonce: 0,
            callbackData: "",
            encryptedSender: ""
        });

        bytes32 expectedHash = keccak256(abi.encode(expected, EMPTY_SENTINEL));
        assertEq(_finalizeWithdrawalBatch(1), expectedHash);
    }

    function test_enqueueDepositBounceBack_revertsUnlessInbox() public {
        vm.expectRevert(ZoneOutbox.OnlyZoneInbox.selector);
        outbox.enqueueDepositBounceBack(address(zoneToken), 1000e6, bob);
    }

    function _finalizeWithdrawalBatchAs(address caller, uint256 count) internal returns (bytes32) {
        if (count == type(uint256).max) {
            count = _pendingWithdrawalsCount();
        }
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

        assertEq(_pendingWithdrawalsCount(), 1);

        vm.startPrank(bob);
        zoneToken.approve(address(outbox), 300e6);
        outbox.requestWithdrawal(address(zoneToken), bob, 300e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(_pendingWithdrawalsCount(), 2);
    }

    function test_requestWithdrawal_assignsMonotonicFallbackNonces() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 1000e6);
        outbox.requestWithdrawal(address(zoneToken), bob, 100e6, bytes32(0), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), bob, 100e6, bytes32(0), 0, charlie, "");
        vm.stopPrank();

        PendingWithdrawal[] memory pending = _getPendingWithdrawals();
        assertEq(pending[0].fallbackNonce, 1);
        assertEq(pending[1].fallbackNonce, 2);
        assertEq(outbox.lastFallbackNonce(), 2);
    }

    function test_consumeFallbackRecipient_resolvesAndDeletesMapping() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(address(zoneToken), bob, 100e6, bytes32(0), 0, charlie, "");
        vm.stopPrank();

        vm.prank(ZONE_INBOX);
        assertEq(outbox.consumeFallbackRecipient(1), charlie);

        vm.expectRevert(ZoneOutbox.InvalidFallbackRecipient.selector);
        vm.prank(ZONE_INBOX);
        outbox.consumeFallbackRecipient(1);
    }

    function test_consumeFallbackRecipient_revertsUnlessInbox() public {
        vm.expectRevert(ZoneOutbox.OnlyZoneInbox.selector);
        outbox.consumeFallbackRecipient(1);
    }

    function test_getPendingWithdrawals_returnsPendingInFifoOrder() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 800e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32("first"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), bob, 300e6, bytes32("second"), 0, alice, "");
        vm.stopPrank();

        PendingWithdrawal[] memory pending = _getPendingWithdrawals();
        assertEq(pending.length, 2);
        assertEq(pending[0].sender, alice);
        assertEq(pending[0].txHash, txContext.txHashFor(1));
        assertEq(pending[0].to, alice);
        assertEq(pending[0].amount, 500e6);
        assertEq(pending[0].memo, bytes32("first"));
        assertEq(pending[1].sender, alice);
        assertEq(pending[1].txHash, txContext.txHashFor(2));
        assertEq(pending[1].to, bob);
        assertEq(pending[1].amount, 300e6);
        assertEq(pending[1].memo, bytes32("second"));
    }

    function test_pendingWithdrawalGetters_revertUnlessSequencerOrSystem() public {
        vm.expectRevert(ZoneOutbox.OnlySequencer.selector);
        vm.prank(alice);
        outbox.pendingWithdrawalsCount();

        vm.expectRevert(ZoneOutbox.OnlySequencer.selector);
        vm.prank(alice);
        outbox.getPendingWithdrawals();

        vm.prank(address(0));
        assertEq(outbox.pendingWithdrawalsCount(), 0);

        vm.prank(address(0));
        assertEq(outbox.getPendingWithdrawals().length, 0);
    }

    function test_requestWithdrawal_revertsWhenTokenNotEnabled() public {
        MockZoneToken disabledToken = new MockZoneToken("Disabled USD", "dUSD");
        disabledToken.setMinter(address(this), true);
        disabledToken.setBurner(address(outbox), true);
        disabledToken.mint(alice, 1000e6);

        vm.startPrank(alice);
        disabledToken.approve(address(outbox), 500e6);
        vm.expectRevert(IZonePortal.TokenNotEnabled.selector);
        outbox.requestWithdrawal(address(disabledToken), bob, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(_pendingWithdrawalsCount(), 0);
        assertEq(disabledToken.balanceOf(alice), 1000e6);
    }

    function test_requestWithdrawal_openAccessStillEnforcesGatewayRegistration() public {
        address outsider = address(0x999);
        _setModes(false, true);

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 1000e6);
        outbox.requestWithdrawal(address(zoneToken), outsider, 500e6, bytes32(0), 0, alice, "");
        vm.expectRevert(IZonePortal.InvalidCallbackTarget.selector);
        outbox.requestWithdrawal(address(zoneToken), outsider, 500e6, bytes32(0), 1, alice, "");
        vm.stopPrank();
    }

    function test_requestWithdrawal_openGatewayStillEnforcesAccountAllowlist() public {
        address outsider = address(0x999);
        _setModes(true, false);
        tempoState.setMockAccountAllowed(mockPortal, callbackTarget, true);

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 1500e6);
        outbox.requestWithdrawal(address(zoneToken), outsider, 500e6, bytes32(0), 1, alice, "");
        outbox.requestWithdrawal(
            address(zoneToken), callbackTarget, 500e6, bytes32(0), 0, alice, ""
        );
        vm.expectRevert(abi.encodeWithSelector(IZonePortal.AccountNotAllowed.selector, outsider));
        outbox.requestWithdrawal(address(zoneToken), outsider, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();
    }

    function test_requestWithdrawal_revertsOnInvalidCurrentTxHash() public {
        ZeroTxContext zeroTxContext = new ZeroTxContext();
        vm.etch(ZONE_TX_CONTEXT, address(zeroTxContext).code);

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        vm.expectRevert(ZoneOutbox.InvalidCurrentTxHash.selector);
        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(_pendingWithdrawalsCount(), 0);
    }

    /*//////////////////////////////////////////////////////////////
                       FINALIZE BATCH TESTS
    //////////////////////////////////////////////////////////////*/

    function test_finalizeWithdrawalBatch_emptyQueue_returnsZero() public {
        bytes32 hash = _finalizeWithdrawalBatch(0);

        // Still emits event with zero count
        assertEq(hash, bytes32(0));
    }

    function test_finalizeWithdrawalBatch_zeroCountWithPending_reverts() public {
        // Add a withdrawal
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(_pendingWithdrawalsCount(), 1);

        bytes[] memory encryptedSenders = new bytes[](0);

        vm.prank(sequencer);
        vm.expectRevert(abi.encodeWithSelector(ZoneOutbox.InvalidWithdrawalCount.selector, 0, 1));
        outbox.finalizeWithdrawalBatch(0, uint64(block.number), encryptedSenders);
        assertEq(_pendingWithdrawalsCount(), 1);
    }

    function test_finalizeWithdrawalBatch_singleWithdrawal_correctHash() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32("memo"), 0, alice, "");
        vm.stopPrank();

        // Expected hash
        Withdrawal memory w = _withdrawal(1, alice, alice, 500e6, bytes32("memo"), 0, alice, "");
        bytes32 expectedHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        bytes32 hash = _finalizeWithdrawalBatch(type(uint256).max);

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

        bytes32 hash = _finalizeWithdrawalBatch(type(uint256).max);

        assertEq(hash, expectedHash);
    }

    function test_finalizeWithdrawalBatch_clearsStorage() public {
        // Add withdrawals
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 1000e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32(0), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 300e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(_pendingWithdrawalsCount(), 2);

        // Batch all
        _finalizeWithdrawalBatch(type(uint256).max);

        assertEq(_pendingWithdrawalsCount(), 0);
    }

    function test_finalizeWithdrawalBatch_partialBatch_reverts() public {
        // Add 3 withdrawals
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 1500e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32("w1"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32("w2"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32("w3"), 0, alice, "");
        vm.stopPrank();

        assertEq(_pendingWithdrawalsCount(), 3);

        bytes[] memory encryptedSenders = new bytes[](2);
        vm.prank(sequencer);
        vm.expectRevert(abi.encodeWithSelector(ZoneOutbox.InvalidWithdrawalCount.selector, 2, 3));
        outbox.finalizeWithdrawalBatch(2, uint64(block.number), encryptedSenders);
        assertEq(_pendingWithdrawalsCount(), 3);
    }

    function test_finalizeWithdrawalBatch_exactCountProcessesAllInFifoOrder() public {
        // Add 4 withdrawals in order
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 4000e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 100e6, bytes32("w1"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 200e6, bytes32("w2"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 300e6, bytes32("w3"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 400e6, bytes32("w4"), 0, alice, "");
        vm.stopPrank();

        assertEq(_pendingWithdrawalsCount(), 4);

        bytes32 hash = _finalizeWithdrawalBatch(type(uint256).max);

        Withdrawal memory w1 = _withdrawal(1, alice, alice, 100e6, bytes32("w1"), 0, alice, "");
        Withdrawal memory w2 = _withdrawal(2, alice, alice, 200e6, bytes32("w2"), 0, alice, "");
        Withdrawal memory w3 = _withdrawal(3, alice, alice, 300e6, bytes32("w3"), 0, alice, "");
        Withdrawal memory w4 = _withdrawal(4, alice, alice, 400e6, bytes32("w4"), 0, alice, "");
        bytes32 hash4 = keccak256(abi.encode(w4, EMPTY_SENTINEL));
        bytes32 hash3 = keccak256(abi.encode(w3, hash4));
        bytes32 hash2 = keccak256(abi.encode(w2, hash3));
        bytes32 expectedHash = keccak256(abi.encode(w1, hash2));

        assertEq(hash, expectedHash);
        assertEq(_pendingWithdrawalsCount(), 0);
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

        _finalizeWithdrawalBatch(type(uint256).max);
    }

    function test_finalizeWithdrawalBatch_writesLastBatchToState() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        Withdrawal memory w = _withdrawal(1, alice, alice, 500e6, bytes32(0), 0, alice, "");
        bytes32 expectedHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        _finalizeWithdrawalBatch(type(uint256).max);

        // Verify lastBatch storage was written correctly
        LastBatch memory batch = outbox.lastBatch();
        assertEq(batch.withdrawalQueueHash, expectedHash);
        assertEq(batch.withdrawalBatchIndex, 1);

        // The proof system reads LastBatch directly from fixed storage slots.
        assertEq(vm.load(address(outbox), bytes32(uint256(1))), expectedHash);
        assertEq(uint64(uint256(vm.load(address(outbox), bytes32(uint256(2))))), 1);
    }

    function test_finalizeWithdrawalBatch_writesLastFinalizedTimestamp() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        vm.warp(1234);
        _finalizeWithdrawalBatch(type(uint256).max);

        assertEq(outbox.lastFinalizedTimestamp(), 1234);
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
        bytes[] memory encryptedSenders = _emptyEncryptedSenders(1);
        vm.startPrank(alice);
        vm.expectRevert(ZoneOutbox.OnlySequencer.selector);
        outbox.finalizeWithdrawalBatch(1, uint64(block.number), encryptedSenders);
        vm.stopPrank();

        // Any additional active member has the same authority.
        tempoState.setMockStorageValue(
            mockPortal, keccak256(abi.encode(bob, PORTAL_IS_SEQUENCER_SLOT)), bytes32(uint256(1))
        );
        vm.prank(bob);
        bytes32 hash = outbox.finalizeWithdrawalBatch(1, uint64(block.number), encryptedSenders);
        assertTrue(hash != bytes32(0));
    }

    function test_finalizeWithdrawalBatch_revertsOnInvalidBlockNumber() public {
        bytes[] memory encryptedSenders = new bytes[](0);

        vm.prank(sequencer);
        vm.expectRevert(ZoneOutbox.InvalidBlockNumber.selector);
        outbox.finalizeWithdrawalBatch(0, uint64(block.number + 1), encryptedSenders);
    }

    /*//////////////////////////////////////////////////////////////
                    WITHDRAWAL WITH CALLBACK TESTS
    //////////////////////////////////////////////////////////////*/

    function test_requestWithdrawal_plainRejectsZoneGatewayRecipient() public {
        vm.prank(alice);
        vm.expectRevert(IZonePortal.InvalidCallbackTarget.selector);
        outbox.requestWithdrawal(
            address(zoneToken), callbackTarget, 500e6, bytes32(0), 0, alice, ""
        );
    }

    function test_requestWithdrawal_callbackRejectsNonGatewayRecipient() public {
        vm.prank(alice);
        vm.expectRevert(IZonePortal.InvalidCallbackTarget.selector);
        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 1, alice, "");
    }

    function test_requestWithdrawal_callbackDataIsOpaque() public {
        uint256 balanceBefore = zoneToken.balanceOf(alice);
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(
            address(zoneToken),
            callbackTarget,
            500e6,
            bytes32(0),
            100_000,
            alice,
            _callbackData(GatewayFlow.Deposit, alice)
        );
        vm.stopPrank();

        assertEq(zoneToken.balanceOf(alice), balanceBefore - 500e6);
        assertEq(_pendingWithdrawalsCount(), 1);
    }

    function test_requestWithdrawal_rejectsUnallowedPlainRecipient() public {
        address outsider = address(0x600);
        vm.prank(alice);
        vm.expectRevert(abi.encodeWithSelector(IZonePortal.AccountNotAllowed.selector, outsider));
        outbox.requestWithdrawal(address(zoneToken), outsider, 500e6, bytes32(0), 0, alice, "");
    }

    function test_requestWithdrawal_allowsUnlistedZoneFallbackRecipient() public {
        address outsider = address(0x600);
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, outsider, "");
        vm.stopPrank();

        vm.prank(ZONE_INBOX);
        assertEq(outbox.consumeFallbackRecipient(1), outsider);
    }

    function test_finalizeWithdrawalBatch_withdrawalWithCallback_correctHash() public {
        bytes memory callbackData = _callbackData(GatewayFlow.Deposit, alice);
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(
            address(zoneToken), // token
            callbackTarget, // to
            500e6, // amount
            bytes32("pay"), // memo
            100_000, // gasLimit
            alice, // zoneFallbackRecipient
            callbackData
        );
        vm.stopPrank();

        Withdrawal memory w = _withdrawal(
            1, alice, callbackTarget, 500e6, bytes32("pay"), 100_000, alice, callbackData
        );
        bytes32 expectedHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        bytes32 hash = _finalizeWithdrawalBatch(type(uint256).max);

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
        assertEq(_pendingWithdrawalsCount(), 0);

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 3000e6);

        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();
        assertEq(_pendingWithdrawalsCount(), 1);

        vm.startPrank(alice);
        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();
        assertEq(_pendingWithdrawalsCount(), 2);

        // Finalize clears them
        _finalizeWithdrawalBatch(type(uint256).max);
        assertEq(_pendingWithdrawalsCount(), 0);
    }

    /*//////////////////////////////////////////////////////////////
                      TOKEN TRANSFER TESTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Verifies the sender is debited `amount + fee` when a gas rate is configured.
    /// @dev With the default zero gas rate the fee is always zero, so `amount + fee` reads
    ///      the same as `amount`; a non-zero rate and gas limit make the fee observable.
    ///      The expected fee hardcodes WITHDRAWAL_BASE_GAS (50_000) so a mutated base-gas
    ///      constant is also caught.
    function test_requestWithdrawal_burnsAmountPlusFee() public {
        uint128 rate = 3;
        uint64 gasLimit = 100_000;
        vm.prank(sequencer);
        outbox.setTempoGasRate(rate);

        uint128 amount = 500e6;
        uint128 expectedFee = uint128(50_000 + gasLimit) * rate;
        assertGt(expectedFee, 0);

        uint256 aliceBefore = zoneToken.balanceOf(alice);
        uint256 supplyBefore = zoneToken.totalSupply();

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), amount + expectedFee);
        bytes memory callbackData = _callbackData(GatewayFlow.Deposit, alice);
        outbox.requestWithdrawal(
            address(zoneToken), callbackTarget, amount, bytes32(0), gasLimit, alice, callbackData
        );
        vm.stopPrank();

        assertEq(zoneToken.balanceOf(alice), aliceBefore - amount - expectedFee);
        assertEq(zoneToken.totalSupply(), supplyBefore - amount - expectedFee);
    }

    /// @notice Callback data exactly at the maximum size is accepted (boundary is inclusive).
    /// @dev Guards `data.length > MAX` against `>=`/`==` mutants, which would reject MAX bytes.
    function test_requestWithdrawal_callbackDataAtMaxSize_succeeds() public {
        bytes memory data = new bytes(outbox.MAX_CALLBACK_DATA_SIZE());

        uint256 supplyBefore = zoneToken.totalSupply();
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, data);
        vm.stopPrank();

        assertEq(zoneToken.totalSupply(), supplyBefore - 500e6);
    }

    /// @notice Callback data one byte over the maximum reverts.
    function test_requestWithdrawal_callbackDataAboveMax_reverts() public {
        bytes memory data = new bytes(outbox.MAX_CALLBACK_DATA_SIZE() + 1);

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        vm.expectRevert(ZoneOutbox.CallbackDataTooLarge.selector);
        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, data);
        vm.stopPrank();
    }

    /// @notice Finalizing with a count above the true pending count reverts.
    function test_finalizeWithdrawalBatch_countAbovePending_reverts() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 4 * 500e6);
        for (uint256 i = 0; i < 4; i++) {
            outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "");
        }
        vm.stopPrank();

        bytes[] memory senders = _emptyEncryptedSenders(5);
        vm.prank(sequencer);
        vm.expectRevert(abi.encodeWithSelector(ZoneOutbox.InvalidWithdrawalCount.selector, 5, 4));
        outbox.finalizeWithdrawalBatch(5, uint64(block.number), senders);
        assertEq(_pendingWithdrawalsCount(), 4);
    }

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

        // gasLimit = 0, zoneFallbackRecipient = alice is fine
        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(_pendingWithdrawalsCount(), 1);
    }

    function test_requestWithdrawal_callbackNeedsFallback_reverts() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);

        // zoneFallbackRecipient = address(0) reverts
        vm.expectRevert(ZoneOutbox.InvalidFallbackRecipient.selector);
        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, address(0), "");
        vm.stopPrank();
    }

    function test_requestWithdrawal_callbackWithValidFallback_ok() public {
        bytes memory callbackData = _callbackData(GatewayFlow.Redeem, alice);
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);

        // gasLimit > 0 with valid fallback
        outbox.requestWithdrawal(
            address(zoneToken), callbackTarget, 500e6, bytes32(0), 100_000, alice, callbackData
        );
        vm.stopPrank();

        assertEq(_pendingWithdrawalsCount(), 1);
    }

    function test_requestWithdrawal_revertsWhenGasLimitTooHigh() public {
        uint64 highGasLimit = outbox.MAX_WITHDRAWAL_GAS_LIMIT() + 1;
        assertEq(outbox.MAX_WITHDRAWAL_GAS_LIMIT(), 10_000_000);

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);

        vm.expectRevert(ZoneOutbox.GasLimitTooHigh.selector);
        outbox.requestWithdrawal(
            address(zoneToken), bob, 500e6, bytes32(0), highGasLimit, alice, "callback"
        );
        vm.stopPrank();
    }

    function test_calculateWithdrawalFee_revertsWhenGasLimitTooHigh() public {
        uint64 highGasLimit = outbox.MAX_WITHDRAWAL_GAS_LIMIT() + 1;
        assertEq(outbox.MAX_WITHDRAWAL_GAS_LIMIT(), 10_000_000);

        vm.expectRevert(ZoneOutbox.GasLimitTooHigh.selector);
        outbox.calculateWithdrawalFee(highGasLimit);
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

    function test_finalizeWithdrawalBatch_partialBatchDoesNotLeaveRemainder_reverts() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 5000e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 100e6, bytes32("w1"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 200e6, bytes32("w2"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 300e6, bytes32("w3"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 400e6, bytes32("w4"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 500e6, bytes32("w5"), 0, alice, "");
        vm.stopPrank();

        assertEq(_pendingWithdrawalsCount(), 5);

        bytes[] memory encryptedSenders = new bytes[](2);
        vm.prank(sequencer);
        vm.expectRevert(abi.encodeWithSelector(ZoneOutbox.InvalidWithdrawalCount.selector, 2, 5));
        outbox.finalizeWithdrawalBatch(2, uint64(block.number), encryptedSenders);
        assertEq(_pendingWithdrawalsCount(), 5);
    }

    function test_finalizeWithdrawalBatch_countLargerThanPending_reverts() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 1000e6);
        outbox.requestWithdrawal(address(zoneToken), alice, 100e6, bytes32("w1"), 0, alice, "");
        outbox.requestWithdrawal(address(zoneToken), alice, 200e6, bytes32("w2"), 0, alice, "");
        vm.stopPrank();

        bytes[] memory encryptedSenders = new bytes[](1000);
        vm.prank(sequencer);
        vm.expectRevert(abi.encodeWithSelector(ZoneOutbox.InvalidWithdrawalCount.selector, 1000, 2));
        outbox.finalizeWithdrawalBatch(1000, uint64(block.number), encryptedSenders);

        assertEq(_pendingWithdrawalsCount(), 2);
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
        bytes memory callbackData = _callbackData(GatewayFlow.Deposit, charlie);
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 1000e6);
        outbox.requestWithdrawal(
            address(zoneToken), // token
            callbackTarget, // to
            500e6, // amount
            bytes32("payment123"), // memo
            50_000, // gasLimit
            charlie, // zoneFallbackRecipient
            callbackData // data
        );
        vm.stopPrank();

        // Finalize and verify hash includes all fields
        Withdrawal memory expected = _withdrawal(
            1, alice, callbackTarget, 500e6, bytes32("payment123"), 50_000, charlie, callbackData
        );
        bytes32 expectedHash = keccak256(abi.encode(expected, EMPTY_SENTINEL));

        bytes32 hash = _finalizeWithdrawalBatch(1);

        assertEq(hash, expectedHash);
    }

    /*//////////////////////////////////////////////////////////////
                          ZERO AMOUNT TESTS
    //////////////////////////////////////////////////////////////*/

    function test_requestWithdrawal_zeroAmount_reverts() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 0);
        vm.expectRevert(ZoneOutbox.ZeroAmountWithdrawal.selector);
        outbox.requestWithdrawal(address(zoneToken), bob, 0, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(_pendingWithdrawalsCount(), 0);
        assertEq(outbox.lastFallbackNonce(), 0);
    }

    /*//////////////////////////////////////////////////////////////
                    WITHDRAWAL REQUESTED EVENT TESTS
    //////////////////////////////////////////////////////////////*/

    function test_requestWithdrawal_emitsEvent() public {
        bytes memory callbackData = _callbackData(GatewayFlow.Deposit, charlie);
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);

        uint128 expectedFee = outbox.calculateWithdrawalFee(50_000);
        vm.expectEmit(true, true, false, true);
        emit IZoneOutbox.WithdrawalRequested(
            0, // index
            alice, // sender
            address(zoneToken), // token
            callbackTarget, // to
            500e6, // amount
            expectedFee, // fee
            bytes32("memo"),
            50_000, // gasLimit
            1, // fallbackNonce
            callbackData,
            ""
        );

        outbox.requestWithdrawal(
            address(zoneToken),
            callbackTarget,
            500e6,
            bytes32("memo"),
            50_000,
            charlie,
            callbackData
        );
        vm.stopPrank();
    }

    /*//////////////////////////////////////////////////////////////
                        IMMUTABLE GETTERS TESTS
    //////////////////////////////////////////////////////////////*/

    function test_immutableGetters() public view {
        assertEq(outbox.tempoPortal(), mockPortal);
        assertEq(address(outbox.tempoState()), address(tempoState));
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

        assertEq(_pendingWithdrawalsCount(), numWithdrawals);

        bytes32 hash = _finalizeWithdrawalBatch(type(uint256).max);

        assertTrue(hash != bytes32(0));
        assertEq(_pendingWithdrawalsCount(), 0);
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
        assertEq(_pendingWithdrawalsCount(), 10);
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

        assertEq(_pendingWithdrawalsCount(), 4);
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

    /// @notice Sequencer updates the Tempo gas rate and emits the new value.
    function test_setTempoGasRate_sequencerCanSetAndEmit() public {
        uint128 rate = 7;

        vm.prank(sequencer);
        vm.expectEmit(false, false, false, true);
        emit IZoneOutbox.TempoGasRateUpdated(rate);
        outbox.setTempoGasRate(rate);

        assertEq(outbox.tempoGasRate(), rate);
    }

    /// @notice Only the sequencer can update the Tempo gas rate.
    function test_setTempoGasRate_onlySequencer() public {
        vm.prank(alice);
        vm.expectRevert(ZoneOutbox.OnlySequencer.selector);
        outbox.setTempoGasRate(1);
    }

    function test_setTempoGasRate_revertsAboveAdminMaximum() public {
        _setMaxTempoGasRate(6);

        vm.prank(sequencer);
        vm.expectRevert(ZoneOutbox.GasFeeRateTooHigh.selector);
        outbox.setTempoGasRate(7);
    }

    function test_setTempoGasRate_acceptsAdminMaximum() public {
        _setMaxTempoGasRate(6);

        vm.prank(sequencer);
        outbox.setTempoGasRate(6);
        assertEq(outbox.tempoGasRate(), 6);
    }

    function test_setTempoGasRate_zeroAdminMaximumDisablesNonzeroRates() public {
        _setMaxTempoGasRate(0);

        vm.startPrank(sequencer);
        vm.expectRevert(ZoneOutbox.GasFeeRateTooHigh.selector);
        outbox.setTempoGasRate(1);
        outbox.setTempoGasRate(0);
        vm.stopPrank();

        assertEq(outbox.tempoGasRate(), 0);
    }

    /// @notice Withdrawal fee matches base plus callback gas times Tempo gas rate.
    function testFuzz_calculateWithdrawalFee(uint64 gasLimit, uint128 tempoGasRate) public {
        tempoGasRate = uint128(bound(tempoGasRate, 0, TEST_MAX_TEMPO_GAS_RATE));
        uint64 maxGasLimit = outbox.MAX_WITHDRAWAL_GAS_LIMIT();

        vm.prank(sequencer);
        outbox.setTempoGasRate(tempoGasRate);

        if (gasLimit > maxGasLimit) {
            vm.expectRevert(ZoneOutbox.GasLimitTooHigh.selector);
            outbox.calculateWithdrawalFee(gasLimit);
        } else {
            uint128 expected = uint128(outbox.WITHDRAWAL_BASE_GAS() + gasLimit) * tempoGasRate;
            assertEq(outbox.calculateWithdrawalFee(gasLimit), expected);
        }
    }

    /// @notice Zero-count finalization with pending withdrawals reverts.
    function test_finalizeWithdrawalBatch_zeroCountWithPending_doesNotAdvance() public {
        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        bytes[] memory encryptedSenders = new bytes[](0);
        vm.prank(sequencer);
        vm.expectRevert(abi.encodeWithSelector(ZoneOutbox.InvalidWithdrawalCount.selector, 0, 1));
        outbox.finalizeWithdrawalBatch(0, uint64(block.number), encryptedSenders);
        assertEq(_pendingWithdrawalsCount(), 1);
        assertEq(outbox.lastBatch().withdrawalBatchIndex, 0);
    }

    /// @notice Zero gas limit withdrawals still store callback data in the hash.
    function test_requestWithdrawal_zeroGasLimitStoresCallbackData() public {
        bytes memory data = "simple-with-data";

        vm.startPrank(alice);
        zoneToken.approve(address(outbox), 500e6);
        outbox.requestWithdrawal(address(zoneToken), bob, 500e6, bytes32("memo"), 0, alice, data);
        vm.stopPrank();

        Withdrawal memory w = _withdrawal(1, alice, bob, 500e6, bytes32("memo"), 0, alice, data);
        assertEq(_finalizeWithdrawalBatch(1), keccak256(abi.encode(w, EMPTY_SENTINEL)));
    }

    /// @notice Finalized withdrawal hashes chain in reverse dequeue order.
    function testFuzz_finalizeWithdrawalBatch_hashChainOrder(uint8 rawCount) public {
        uint256 count = bound(rawCount, 1, 8);
        address[3] memory senders = [alice, bob, charlie];
        Withdrawal[] memory withdrawals = new Withdrawal[](count);

        for (uint256 i = 0; i < count; i++) {
            address sender = senders[i % senders.length];
            uint128 amount = uint128((i + 1) * 10e6);
            bytes32 memo = bytes32(i + 1);

            vm.startPrank(sender);
            zoneToken.approve(address(outbox), amount);
            outbox.requestWithdrawal(address(zoneToken), sender, amount, memo, 0, alice, "");
            vm.stopPrank();

            withdrawals[i] = _withdrawal(i + 1, sender, sender, amount, memo, 0, alice, "");
        }

        bytes32 expectedHash = EMPTY_SENTINEL;
        for (uint256 i = count; i > 0; i--) {
            expectedHash = keccak256(abi.encode(withdrawals[i - 1], expectedHash));
        }

        assertEq(_finalizeWithdrawalBatch(count), expectedHash);
        assertEq(_pendingWithdrawalsCount(), 0);
    }

}
