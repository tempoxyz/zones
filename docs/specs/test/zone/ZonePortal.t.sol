// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { BaseTest } from "../BaseTest.t.sol";
import { ZoneFactory } from "../../src/zone/ZoneFactory.sol";
import { ZonePortal } from "../../src/zone/ZonePortal.sol";
import { MockVerifier } from "./mocks/MockVerifier.sol";
import { TIP20 } from "../../src/TIP20.sol";
import { ITIP20 } from "../../src/interfaces/ITIP20.sol";
import {
    IZoneFactory,
    IZonePortal,
    IExitReceiver,
    ZoneInfo,
    Deposit,
    Withdrawal,
    StateTransition,
    DepositQueueTransition,
    WithdrawalQueueTransition
} from "../../src/zone/IZone.sol";
import { WithdrawalQueueLib } from "../../src/zone/WithdrawalQueueLib.sol";

/// @notice Mock exit receiver that accepts funds
contract MockExitReceiver is IExitReceiver {
    bool public shouldAccept = true;
    bool public shouldRevert = false;

    address public lastSender;
    uint128 public lastAmount;
    bytes public lastData;

    function setShouldAccept(bool _shouldAccept) external {
        shouldAccept = _shouldAccept;
    }

    function setShouldRevert(bool _shouldRevert) external {
        shouldRevert = _shouldRevert;
    }

    function onExitReceived(
        address sender,
        uint128 amount,
        bytes calldata data
    ) external returns (bytes4) {
        lastSender = sender;
        lastAmount = amount;
        lastData = data;

        if (shouldRevert) {
            revert("MockExitReceiver: intentional revert");
        }

        if (shouldAccept) {
            return IExitReceiver.onExitReceived.selector;
        } else {
            return bytes4(0xdeadbeef); // Wrong selector
        }
    }
}

/// @notice Tests for ZonePortal - simulating L1/zone interface
contract ZonePortalTest is BaseTest {
    ZoneFactory public zoneFactory;
    MockVerifier public mockVerifier;
    ZonePortal public portal;
    MockExitReceiver public exitReceiver;

    uint64 public testZoneId;
    bytes32 public constant GENESIS_STATE_ROOT = keccak256("genesis");

    function setUp() public override {
        super.setUp();

        // Deploy zone infrastructure
        zoneFactory = new ZoneFactory();
        mockVerifier = new MockVerifier();
        exitReceiver = new MockExitReceiver();

        // Grant issuer role and mint tokens for tests
        vm.startPrank(pathUSDAdmin);
        pathUSD.grantRole(_ISSUER_ROLE, pathUSDAdmin);
        pathUSD.mint(admin, 1_000_000e6);
        pathUSD.mint(alice, 100_000e6);
        pathUSD.mint(bob, 100_000e6);
        vm.stopPrank();

        // Create a zone
        IZoneFactory.CreateZoneParams memory params = IZoneFactory.CreateZoneParams({
            token: address(pathUSD),
            sequencer: admin, // admin is the sequencer for tests
            verifier: address(mockVerifier),
            genesisStateRoot: GENESIS_STATE_ROOT
        });

        address portalAddr;
        (testZoneId, portalAddr) = zoneFactory.createZone(params);
        portal = ZonePortal(portalAddr);
    }

    /*//////////////////////////////////////////////////////////////
                            ZONE CREATION TESTS
    //////////////////////////////////////////////////////////////*/

    function test_zoneCreation() public view {
        assertEq(portal.zoneId(), testZoneId);
        assertEq(portal.token(), address(pathUSD));
        assertEq(portal.sequencer(), admin);
        assertEq(portal.verifier(), address(mockVerifier));
        assertEq(portal.stateRoot(), GENESIS_STATE_ROOT);
        assertEq(portal.batchIndex(), 0);
    }

    function test_zoneFactoryTracksZones() public view {
        assertEq(zoneFactory.zoneCount(), 1);
        assertTrue(zoneFactory.isZonePortal(address(portal)));

        ZoneInfo memory info = zoneFactory.zones(testZoneId);
        assertEq(info.zoneId, testZoneId);
        assertEq(info.portal, address(portal));
        assertEq(info.token, address(pathUSD));
    }

    /*//////////////////////////////////////////////////////////////
                         DEPOSIT TESTS (L1 -> ZONE)
    //////////////////////////////////////////////////////////////*/

    function test_deposit_updatesHashChain() public {
        uint128 depositAmount = 1000e6;

        // Approve and deposit
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        bytes32 hash1 = portal.deposit(alice, depositAmount, bytes32("memo1"));
        vm.stopPrank();

        // Verify hash chain updated
        assertEq(portal.currentDepositQueueHash(), hash1);
        assertTrue(hash1 != bytes32(0));

        // Verify tokens escrowed
        assertEq(pathUSD.balanceOf(address(portal)), depositAmount);
    }

    function test_deposit_multipleDepositsChain() public {
        uint128 amount1 = 1000e6;
        uint128 amount2 = 2000e6;

        // First deposit from alice
        vm.startPrank(alice);
        pathUSD.approve(address(portal), amount1);
        bytes32 hash1 = portal.deposit(alice, amount1, bytes32("memo1"));
        vm.stopPrank();

        // Second deposit from bob
        vm.startPrank(bob);
        pathUSD.approve(address(portal), amount2);
        bytes32 hash2 = portal.deposit(bob, amount2, bytes32("memo2"));
        vm.stopPrank();

        // Hash chain should have updated
        assertEq(portal.currentDepositQueueHash(), hash2);
        assertTrue(hash2 != hash1);

        // Verify total escrow
        assertEq(pathUSD.balanceOf(address(portal)), amount1 + amount2);
    }

    function test_deposit_hashChainStructure() public {
        // Verify the hash chain is built correctly: newest deposits wrap the outside
        uint128 amount = 1000e6;

        vm.startPrank(alice);
        pathUSD.approve(address(portal), amount * 3);

        // Initial state: currentDepositQueueHash = 0
        bytes32 initialHash = portal.currentDepositQueueHash();
        assertEq(initialHash, bytes32(0));

        // After deposit 1
        portal.deposit(alice, amount, bytes32("d1"));
        bytes32 hash1 = portal.currentDepositQueueHash();

        // After deposit 2: hash2 = keccak256(abi.encode(message2, hash1))
        portal.deposit(alice, amount, bytes32("d2"));
        bytes32 hash2 = portal.currentDepositQueueHash();

        // After deposit 3: hash3 = keccak256(abi.encode(message3, hash2))
        portal.deposit(alice, amount, bytes32("d3"));
        bytes32 hash3 = portal.currentDepositQueueHash();

        vm.stopPrank();

        // Each hash should be different (chain is growing)
        assertTrue(hash1 != hash2);
        assertTrue(hash2 != hash3);
        assertTrue(hash1 != hash3);
    }

    /*//////////////////////////////////////////////////////////////
                       BATCH SUBMISSION TESTS
    //////////////////////////////////////////////////////////////*/

    function test_submitBatch_updatesState() public {
        // Setup: make a deposit
        uint128 depositAmount = 1000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        bytes32 depositHash = portal.deposit(alice, depositAmount, bytes32("memo"));
        vm.stopPrank();

        // Submit a batch (as sequencer)
        bytes32 newStateRoot = keccak256("newState");

        portal.submitBatch(
            StateTransition({ prevStateRoot: bytes32(0), nextStateRoot: newStateRoot }),
            DepositQueueTransition({ prevSnapshotHash: bytes32(0), prevProcessedHash: bytes32(0), nextProcessedHash: depositHash }),
            WithdrawalQueueTransition({ prevPendingHash: bytes32(0), nextPendingHashIfFull: bytes32(0), nextPendingHashIfEmpty: bytes32(0) }),
            "",
            ""
        );

        // Verify state updated
        assertEq(portal.stateRoot(), newStateRoot);
        assertEq(portal.batchIndex(), 1);
        assertEq(portal.processedDepositQueueHash(), depositHash);
        assertEq(portal.snapshotDepositQueueHash(), depositHash); // snapshot of current
    }

    function test_submitBatch_revertsIfNotSequencer() public {
        bytes32 nextStateRoot = keccak256("state");
        vm.prank(alice); // Not sequencer
        vm.expectRevert(IZonePortal.NotSequencer.selector);
        portal.submitBatch(
            StateTransition({ prevStateRoot: bytes32(0), nextStateRoot: nextStateRoot }),
            DepositQueueTransition({ prevSnapshotHash: bytes32(0), prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0) }),
            WithdrawalQueueTransition({ prevPendingHash: bytes32(0), nextPendingHashIfFull: bytes32(0), nextPendingHashIfEmpty: bytes32(0) }),
            "",
            ""
        );
    }

    function test_submitBatch_revertsOnInvalidProof() public {
        mockVerifier.setShouldAccept(false);

        bytes32 nextStateRoot = keccak256("state");
        vm.expectRevert(IZonePortal.InvalidProof.selector);
        portal.submitBatch(
            StateTransition({ prevStateRoot: bytes32(0), nextStateRoot: nextStateRoot }),
            DepositQueueTransition({ prevSnapshotHash: bytes32(0), prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0) }),
            WithdrawalQueueTransition({ prevPendingHash: bytes32(0), nextPendingHashIfFull: bytes32(0), nextPendingHashIfEmpty: bytes32(0) }),
            "",
            ""
        );
    }

    /*//////////////////////////////////////////////////////////////
                    WITHDRAWAL QUEUE TESTS (ZONE -> L1)
    //////////////////////////////////////////////////////////////*/

    function test_withdrawalQueue_simpleWithdrawal() public {
        // Setup: deposit funds to portal for withdrawal
        uint128 depositAmount = 1000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        portal.deposit(alice, depositAmount, bytes32("memo"));
        vm.stopPrank();

        // Create a withdrawal and add to queue via batch
        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: bob,
            amount: 500e6,
            memo: bytes32(0),
            gasLimit: 0, // No callback
            fallbackRecipient: alice,
            data: ""
        });

        // Build withdrawal hash (oldest = outermost for FIFO processing)
        bytes32 withdrawalHash = keccak256(abi.encode(w, bytes32(0)));

        // Submit batch that adds withdrawal to pending
        // When prevPendingHash matches current pending queue, use nextPendingHashIfFull.
        portal.submitBatch(
            StateTransition({ prevStateRoot: bytes32(0), nextStateRoot: keccak256("stateWithWithdrawal") }),
            DepositQueueTransition({ prevSnapshotHash: bytes32(0), prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash() }),
            WithdrawalQueueTransition({ prevPendingHash: bytes32(0), nextPendingHashIfFull: withdrawalHash, nextPendingHashIfEmpty: withdrawalHash }),
            "",
            ""
        );

        // Pending queue should now have the withdrawal
        assertEq(portal.pendingWithdrawalQueueHash(), withdrawalHash);

        // Process the withdrawal (active is empty, so it will swap in pending)
        uint256 bobBalanceBefore = pathUSD.balanceOf(bob);
        portal.processWithdrawal(w, bytes32(0));

        // Bob should have received funds
        assertEq(pathUSD.balanceOf(bob), bobBalanceBefore + 500e6);
        // Queues should be empty
        assertEq(portal.activeWithdrawalQueueHash(), bytes32(0));
        assertEq(portal.pendingWithdrawalQueueHash(), bytes32(0));
    }

    function test_withdrawalQueue_twoQueueSwap() public {
        // Setup: deposit funds
        uint128 depositAmount = 2000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        portal.deposit(alice, depositAmount, bytes32("memo"));
        vm.stopPrank();

        // Create two withdrawals
        Withdrawal memory w1 = Withdrawal({
            sender: alice, to: bob, amount: 300e6, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, data: ""
        });
        Withdrawal memory w2 = Withdrawal({
            sender: alice, to: charlie, amount: 400e6, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, data: ""
        });

        // Build queue: w1 is oldest (outermost), w2 is newest (innermost)
        bytes32 innerHash = keccak256(abi.encode(w2, bytes32(0)));
        bytes32 pendingQueueHash = keccak256(abi.encode(w1, innerHash));

        // Submit batch adding both withdrawals
        // prevPendingHash = 0 matches current state, so use nextPendingHashIfFull
        portal.submitBatch(
            StateTransition({ prevStateRoot: bytes32(0), nextStateRoot: keccak256("state1") }),
            DepositQueueTransition({ prevSnapshotHash: bytes32(0), prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash() }),
            WithdrawalQueueTransition({ prevPendingHash: bytes32(0), nextPendingHashIfFull: pendingQueueHash, nextPendingHashIfEmpty: pendingQueueHash }),
            "",
            ""
        );
        assertEq(portal.pendingWithdrawalQueueHash(), pendingQueueHash);

        // Process w1 (oldest) - this will swap pending into active first
        uint256 bobBalanceBefore = pathUSD.balanceOf(bob);
        portal.processWithdrawal(w1, innerHash);
        assertEq(pathUSD.balanceOf(bob), bobBalanceBefore + 300e6);

        // Active queue should now have w2
        assertEq(portal.activeWithdrawalQueueHash(), innerHash);
        assertEq(portal.pendingWithdrawalQueueHash(), bytes32(0));

        // Process w2
        uint256 charlieBalanceBefore = pathUSD.balanceOf(charlie);
        portal.processWithdrawal(w2, bytes32(0));
        assertEq(pathUSD.balanceOf(charlie), charlieBalanceBefore + 400e6);

        // Both queues empty
        assertEq(portal.activeWithdrawalQueueHash(), bytes32(0));
        assertEq(portal.pendingWithdrawalQueueHash(), bytes32(0));
    }

    function test_withdrawalQueue_raceConditionHandling() public {
        // Test the race condition scenario from the spec:
        // 1. Proof generation starts when pending = X (non-empty)
        // 2. Sequencer drains active, triggering swap: active = X, pending = 0
        // 3. Proof submits expecting pending = X, but it's now 0

        uint128 depositAmount = 3000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        portal.deposit(alice, depositAmount, bytes32("memo"));
        vm.stopPrank();

        // First batch: add withdrawal w1 to pending
        Withdrawal memory w1 = Withdrawal({
            sender: alice, to: bob, amount: 500e6, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, data: ""
        });
        bytes32 w1Hash = keccak256(abi.encode(w1, bytes32(0)));

        portal.submitBatch(
            StateTransition({ prevStateRoot: bytes32(0), nextStateRoot: keccak256("state1") }),
            DepositQueueTransition({ prevSnapshotHash: bytes32(0), prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash() }),
            WithdrawalQueueTransition({ prevPendingHash: bytes32(0), nextPendingHashIfFull: w1Hash, nextPendingHashIfEmpty: w1Hash }),
            "",
            ""
        );

        // pending = w1Hash
        assertEq(portal.pendingWithdrawalQueueHash(), w1Hash);

        // Process w1: pending swaps to active, then drained
        portal.processWithdrawal(w1, bytes32(0));
        // Now both queues are empty
        assertEq(portal.activeWithdrawalQueueHash(), bytes32(0));
        assertEq(portal.pendingWithdrawalQueueHash(), bytes32(0));

        // Second batch was generated when pending = w1Hash
        // But now pending = 0 (swap occurred)
        Withdrawal memory w2 = Withdrawal({
            sender: alice, to: charlie, amount: 600e6, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, data: ""
        });
        bytes32 w2Hash = keccak256(abi.encode(w2, bytes32(0)));

        // Proof expected pending = w1Hash, generated nextPendingHashIfFull with w2 added.
        // But since pending is now 0, we use nextPendingHashIfEmpty.
        bytes32 nextPendingHashIfFull = keccak256(abi.encode(w2, w1Hash)); // w2 added to w1Hash

        // This should use nextPendingHashIfEmpty since pending != prevPending but pending == 0
        portal.submitBatch(
            StateTransition({ prevStateRoot: bytes32(0), nextStateRoot: keccak256("state2") }),
            DepositQueueTransition({ prevSnapshotHash: bytes32(0), prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash() }),
            WithdrawalQueueTransition({ prevPendingHash: w1Hash, nextPendingHashIfFull: nextPendingHashIfFull, nextPendingHashIfEmpty: w2Hash }),
            "",
            ""
        );

        // Pending should now have w2 only (nextPending...IfEmpty path)
        assertEq(portal.pendingWithdrawalQueueHash(), w2Hash);
    }

    function test_withdrawalQueue_revertsOnUnexpectedState() public {
        // If pending != prevPendingWithdrawalQueueHash AND pending != 0, should revert
        uint128 depositAmount = 2000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        portal.deposit(alice, depositAmount, bytes32("memo"));
        vm.stopPrank();

        // Add withdrawal to pending via first batch
        Withdrawal memory w1 = Withdrawal({
            sender: alice, to: bob, amount: 500e6, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, data: ""
        });
        bytes32 w1Hash = keccak256(abi.encode(w1, bytes32(0)));

        portal.submitBatch(
            StateTransition({ prevStateRoot: bytes32(0), nextStateRoot: keccak256("state1") }),
            DepositQueueTransition({ prevSnapshotHash: bytes32(0), prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash() }),
            WithdrawalQueueTransition({ prevPendingHash: bytes32(0), nextPendingHashIfFull: w1Hash, nextPendingHashIfEmpty: w1Hash }),
            "",
            ""
        );

        // Verify pending is now w1Hash
        assertEq(portal.pendingWithdrawalQueueHash(), w1Hash, "pending should be w1Hash after first batch");

        // Try to submit with wrong prevPendingHash (neither matches pending nor is pending zero)
        bytes32 wrongExpected = keccak256("wrong");
        assertTrue(wrongExpected != w1Hash, "wrongExpected must differ from w1Hash");
        assertTrue(wrongExpected != bytes32(0), "wrongExpected must not be zero");

        // Cache values before expectRevert (view calls count as "next call")
        bytes32 currentDeposit = portal.currentDepositQueueHash();
        bytes32 nextState = keccak256("state2");

        vm.expectRevert(WithdrawalQueueLib.UnexpectedPendingQueueHash.selector);
        portal.submitBatch(
            StateTransition({ prevStateRoot: bytes32(0), nextStateRoot: nextState }),
            DepositQueueTransition({ prevSnapshotHash: bytes32(0), prevProcessedHash: bytes32(0), nextProcessedHash: currentDeposit }),
            WithdrawalQueueTransition({ prevPendingHash: wrongExpected, nextPendingHashIfFull: bytes32(0), nextPendingHashIfEmpty: bytes32(0) }),
            "",
            ""
        );
    }

    /*//////////////////////////////////////////////////////////////
                     CALLBACK & BOUNCE-BACK TESTS
    //////////////////////////////////////////////////////////////*/

    function test_withdrawal_withCallback() public {
        // Fund portal
        uint128 depositAmount = 1000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        portal.deposit(alice, depositAmount, bytes32("memo"));
        vm.stopPrank();

        // Create withdrawal with callback
        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: address(exitReceiver),
            amount: 500e6,
            memo: bytes32(0),
            gasLimit: 100000,
            fallbackRecipient: alice,
            data: "callback_data"
        });
        bytes32 wHash = keccak256(abi.encode(w, bytes32(0)));

        // Submit batch adding withdrawal
        portal.submitBatch(
            StateTransition({ prevStateRoot: bytes32(0), nextStateRoot: keccak256("state") }),
            DepositQueueTransition({ prevSnapshotHash: bytes32(0), prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash() }),
            WithdrawalQueueTransition({ prevPendingHash: bytes32(0), nextPendingHashIfFull: wHash, nextPendingHashIfEmpty: wHash }),
            "",
            ""
        );

        // Process withdrawal
        portal.processWithdrawal(w, bytes32(0));

        // Receiver should have gotten funds and callback
        assertEq(pathUSD.balanceOf(address(exitReceiver)), 500e6);
        assertEq(exitReceiver.lastSender(), alice);
        assertEq(exitReceiver.lastAmount(), 500e6);
        assertEq(exitReceiver.lastData(), "callback_data");
    }

    function test_withdrawal_bounceBackOnRevert() public {
        // Fund portal
        uint128 depositAmount = 1000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        portal.deposit(alice, depositAmount, bytes32("memo"));
        vm.stopPrank();

        bytes32 depositHashBefore = portal.currentDepositQueueHash();

        // Set receiver to revert
        exitReceiver.setShouldRevert(true);

        // Create withdrawal with callback
        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: address(exitReceiver),
            amount: 500e6,
            memo: bytes32(0),
            gasLimit: 100000,
            fallbackRecipient: alice,
            data: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, bytes32(0)));

        // Submit batch
        portal.submitBatch(
            StateTransition({ prevStateRoot: bytes32(0), nextStateRoot: keccak256("state") }),
            DepositQueueTransition({ prevSnapshotHash: bytes32(0), prevProcessedHash: bytes32(0), nextProcessedHash: depositHashBefore }),
            WithdrawalQueueTransition({ prevPendingHash: bytes32(0), nextPendingHashIfFull: wHash, nextPendingHashIfEmpty: wHash }),
            "",
            ""
        );

        // Process withdrawal - should bounce back
        portal.processWithdrawal(w, bytes32(0));

        // Receiver should NOT have funds (they stayed in portal)
        assertEq(pathUSD.balanceOf(address(exitReceiver)), 0);

        // Deposit hash should have changed (bounce-back added a deposit)
        assertTrue(portal.currentDepositQueueHash() != depositHashBefore);
    }

    function test_withdrawal_bounceBackOnWrongSelector() public {
        // Fund portal
        uint128 depositAmount = 1000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        portal.deposit(alice, depositAmount, bytes32("memo"));
        vm.stopPrank();

        bytes32 depositHashBefore = portal.currentDepositQueueHash();

        // Set receiver to return wrong selector
        exitReceiver.setShouldAccept(false);

        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: address(exitReceiver),
            amount: 500e6,
            memo: bytes32(0),
            gasLimit: 100000,
            fallbackRecipient: alice,
            data: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, bytes32(0)));

        portal.submitBatch(
            StateTransition({ prevStateRoot: bytes32(0), nextStateRoot: keccak256("state") }),
            DepositQueueTransition({ prevSnapshotHash: bytes32(0), prevProcessedHash: bytes32(0), nextProcessedHash: depositHashBefore }),
            WithdrawalQueueTransition({ prevPendingHash: bytes32(0), nextPendingHashIfFull: wHash, nextPendingHashIfEmpty: wHash }),
            "",
            ""
        );

        // Process withdrawal - should bounce back due to wrong selector
        portal.processWithdrawal(w, bytes32(0));

        // Funds should still be in portal (bounce-back)
        assertEq(pathUSD.balanceOf(address(exitReceiver)), 0);
        assertTrue(portal.currentDepositQueueHash() != depositHashBefore);
    }

    /*//////////////////////////////////////////////////////////////
                     INVALID WITHDRAWAL TESTS
    //////////////////////////////////////////////////////////////*/

    function test_processWithdrawal_revertsIfEmpty() public {
        Withdrawal memory w = Withdrawal({
            sender: alice, to: bob, amount: 100e6, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, data: ""
        });

        vm.expectRevert(IZonePortal.NoWithdrawals.selector);
        portal.processWithdrawal(w, bytes32(0));
    }

    function test_processWithdrawal_revertsIfInvalid() public {
        // Fund and create withdrawal
        uint128 depositAmount = 1000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        portal.deposit(alice, depositAmount, bytes32("memo"));
        vm.stopPrank();

        Withdrawal memory w = Withdrawal({
            sender: alice, to: bob, amount: 500e6, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, data: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, bytes32(0)));

        portal.submitBatch(
            StateTransition({ prevStateRoot: bytes32(0), nextStateRoot: keccak256("state") }),
            DepositQueueTransition({ prevSnapshotHash: bytes32(0), prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash() }),
            WithdrawalQueueTransition({ prevPendingHash: bytes32(0), nextPendingHashIfFull: wHash, nextPendingHashIfEmpty: wHash }),
            "",
            ""
        );

        // Try to process with wrong withdrawal data
        Withdrawal memory wrongW = Withdrawal({
            sender: alice, to: charlie, amount: 500e6, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, data: ""
        });

        vm.expectRevert(IZonePortal.InvalidWithdrawal.selector);
        portal.processWithdrawal(wrongW, bytes32(0));
    }

    function test_processWithdrawal_revertsIfNotSequencer() public {
        uint128 depositAmount = 1000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        portal.deposit(alice, depositAmount, bytes32("memo"));
        vm.stopPrank();

        Withdrawal memory w = Withdrawal({
            sender: alice, to: bob, amount: 500e6, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, data: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, bytes32(0)));

        portal.submitBatch(
            StateTransition({ prevStateRoot: bytes32(0), nextStateRoot: keccak256("state") }),
            DepositQueueTransition({ prevSnapshotHash: bytes32(0), prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash() }),
            WithdrawalQueueTransition({ prevPendingHash: bytes32(0), nextPendingHashIfFull: wHash, nextPendingHashIfEmpty: wHash }),
            "",
            ""
        );

        vm.prank(alice); // Not sequencer
        vm.expectRevert(IZonePortal.NotSequencer.selector);
        portal.processWithdrawal(w, bytes32(0));
    }

    /*//////////////////////////////////////////////////////////////
                         DEPOSIT CHAIN TESTS
    //////////////////////////////////////////////////////////////*/

    function test_depositChain_threeSlotDesign() public {
        // Test the 3-slot deposit design:
        // processedDepositQueueHash: where proofs start
        // snapshotDepositQueueHash: stable target for proofs
        // currentDepositQueueHash: head of chain (new messages land here)

        // Initial state: all zero
        assertEq(portal.processedDepositQueueHash(), bytes32(0));
        assertEq(portal.snapshotDepositQueueHash(), bytes32(0));
        assertEq(portal.currentDepositQueueHash(), bytes32(0));

        // Make deposits
        vm.startPrank(alice);
        pathUSD.approve(address(portal), 3000e6);
        bytes32 h1 = portal.deposit(alice, 1000e6, bytes32("d1"));
        bytes32 h2 = portal.deposit(alice, 1000e6, bytes32("d2"));
        vm.stopPrank();

        // currentDepositQueueHash should be h2 (latest)
        assertEq(portal.currentDepositQueueHash(), h2);
        // processed and pending still zero (no batch yet)
        assertEq(portal.processedDepositQueueHash(), bytes32(0));
        assertEq(portal.snapshotDepositQueueHash(), bytes32(0));

        // Submit batch processing only first deposit
        portal.submitBatch(
            StateTransition({ prevStateRoot: bytes32(0), nextStateRoot: keccak256("state1") }),
            DepositQueueTransition({ prevSnapshotHash: bytes32(0), prevProcessedHash: bytes32(0), nextProcessedHash: h1 }),
            WithdrawalQueueTransition({ prevPendingHash: bytes32(0), nextPendingHashIfFull: bytes32(0), nextPendingHashIfEmpty: bytes32(0) }),
            "",
            ""
        );

        // After batch:
        // processedDepositQueueHash = h1 (where we processed to)
        // snapshotDepositQueueHash = h2 (snapshot of currentDepositQueueHash)
        // currentDepositQueueHash = h2 (unchanged by batch)
        assertEq(portal.processedDepositQueueHash(), h1);
        assertEq(portal.snapshotDepositQueueHash(), h2);
        assertEq(portal.currentDepositQueueHash(), h2);

        // New deposit arrives
        vm.startPrank(alice);
        bytes32 h3 = portal.deposit(alice, 1000e6, bytes32("d3"));
        vm.stopPrank();

        // currentDepositQueueHash updated, others unchanged
        assertEq(portal.currentDepositQueueHash(), h3);
        assertEq(portal.processedDepositQueueHash(), h1);
        assertEq(portal.snapshotDepositQueueHash(), h2);

        // Submit batch processing up to h2
        portal.submitBatch(
            StateTransition({ prevStateRoot: bytes32(0), nextStateRoot: keccak256("state2") }),
            DepositQueueTransition({ prevSnapshotHash: bytes32(0), prevProcessedHash: bytes32(0), nextProcessedHash: h2 }),
            WithdrawalQueueTransition({ prevPendingHash: bytes32(0), nextPendingHashIfFull: bytes32(0), nextPendingHashIfEmpty: bytes32(0) }),
            "",
            ""
        );

        // Now:
        // processedDepositQueueHash = h2 (advanced)
        // snapshotDepositQueueHash = h3 (snapshot of current)
        // currentDepositQueueHash = h3 (unchanged)
        assertEq(portal.processedDepositQueueHash(), h2);
        assertEq(portal.snapshotDepositQueueHash(), h3);
        assertEq(portal.currentDepositQueueHash(), h3);
    }
}
