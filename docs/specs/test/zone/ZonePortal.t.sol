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
    BatchCommitment
} from "../../src/zone/IZone.sol";

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

/// @notice Tests for ZonePortal - simulating L1/L2 interface
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
                         DEPOSIT TESTS (L1 -> L2)
    //////////////////////////////////////////////////////////////*/

    function test_deposit_updatesHashChain() public {
        uint128 depositAmount = 1000e6;

        // Approve and deposit
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        bytes32 hash1 = portal.deposit(alice, depositAmount, bytes32("memo1"));
        vm.stopPrank();

        // Verify hash chain updated
        assertEq(portal.currentDepositsHash(), hash1);
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
        assertEq(portal.currentDepositsHash(), hash2);
        assertTrue(hash2 != hash1);

        // Verify total escrow
        assertEq(pathUSD.balanceOf(address(portal)), amount1 + amount2);
    }

    function test_deposit_hashChainStructure() public {
        // Verify the hash chain is built correctly: newest deposits wrap the outside
        uint128 amount = 1000e6;

        vm.startPrank(alice);
        pathUSD.approve(address(portal), amount * 3);

        // Initial state: currentDepositsHash = 0
        bytes32 initialHash = portal.currentDepositsHash();
        assertEq(initialHash, bytes32(0));

        // After deposit 1
        portal.deposit(alice, amount, bytes32("d1"));
        bytes32 hash1 = portal.currentDepositsHash();

        // After deposit 2: hash2 = keccak256(abi.encode(deposit2, hash1))
        portal.deposit(alice, amount, bytes32("d2"));
        bytes32 hash2 = portal.currentDepositsHash();

        // After deposit 3: hash3 = keccak256(abi.encode(deposit3, hash2))
        portal.deposit(alice, amount, bytes32("d3"));
        bytes32 hash3 = portal.currentDepositsHash();

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
        BatchCommitment memory commitment = BatchCommitment({
            newProcessedDepositsHash: depositHash,
            newStateRoot: newStateRoot
        });

        portal.submitBatch(
            commitment,
            bytes32(0), // expectedQueue2
            bytes32(0), // updatedQueue2
            bytes32(0), // newWithdrawalsOnly
            "", // verifierData
            ""  // proof
        );

        // Verify state updated
        assertEq(portal.stateRoot(), newStateRoot);
        assertEq(portal.batchIndex(), 1);
        assertEq(portal.processedDepositsHash(), depositHash);
        assertEq(portal.pendingDepositsHash(), depositHash); // snapshot of current
    }

    function test_submitBatch_revertsIfNotSequencer() public {
        BatchCommitment memory commitment = BatchCommitment({
            newProcessedDepositsHash: bytes32(0),
            newStateRoot: keccak256("state")
        });

        vm.prank(alice); // Not sequencer
        vm.expectRevert(IZonePortal.NotSequencer.selector);
        portal.submitBatch(commitment, bytes32(0), bytes32(0), bytes32(0), "", "");
    }

    function test_submitBatch_revertsOnInvalidProof() public {
        mockVerifier.setShouldAccept(false);

        BatchCommitment memory commitment = BatchCommitment({
            newProcessedDepositsHash: bytes32(0),
            newStateRoot: keccak256("state")
        });

        vm.expectRevert(IZonePortal.InvalidProof.selector);
        portal.submitBatch(commitment, bytes32(0), bytes32(0), bytes32(0), "", "");
    }

    /*//////////////////////////////////////////////////////////////
                    WITHDRAWAL QUEUE TESTS (L2 -> L1)
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
            gasLimit: 0, // No callback
            fallbackRecipient: alice,
            data: ""
        });

        // Build withdrawal hash (oldest = outermost for FIFO processing)
        bytes32 withdrawalHash = keccak256(abi.encode(w, bytes32(0)));

        // Submit batch that adds withdrawal to queue2
        // When expectedQueue2 matches current state, we use updatedQueue2
        BatchCommitment memory commitment = BatchCommitment({
            newProcessedDepositsHash: portal.currentDepositsHash(),
            newStateRoot: keccak256("stateWithWithdrawal")
        });

        portal.submitBatch(
            commitment,
            bytes32(0),      // expectedQueue2 (matches current queue2 = 0)
            withdrawalHash,  // updatedQueue2 (used since expectedQueue2 matches)
            withdrawalHash,  // newWithdrawalsOnly (not used in this case)
            "",
            ""
        );

        // Queue2 should now have the withdrawal
        assertEq(portal.withdrawalQueue2(), withdrawalHash);

        // Process the withdrawal (queue1 is empty, so it will swap in queue2)
        uint256 bobBalanceBefore = pathUSD.balanceOf(bob);
        portal.processWithdrawal(w, bytes32(0));

        // Bob should have received funds
        assertEq(pathUSD.balanceOf(bob), bobBalanceBefore + 500e6);
        // Queues should be empty
        assertEq(portal.withdrawalQueue1(), bytes32(0));
        assertEq(portal.withdrawalQueue2(), bytes32(0));
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
            sender: alice, to: bob, amount: 300e6, gasLimit: 0, fallbackRecipient: alice, data: ""
        });
        Withdrawal memory w2 = Withdrawal({
            sender: alice, to: charlie, amount: 400e6, gasLimit: 0, fallbackRecipient: alice, data: ""
        });

        // Build queue: w1 is oldest (outermost), w2 is newest (innermost)
        bytes32 innerHash = keccak256(abi.encode(w2, bytes32(0)));
        bytes32 queue2Hash = keccak256(abi.encode(w1, innerHash));

        // Submit batch adding both withdrawals
        // expectedQueue2 = 0 matches current state, so use updatedQueue2
        BatchCommitment memory commitment = BatchCommitment({
            newProcessedDepositsHash: portal.currentDepositsHash(),
            newStateRoot: keccak256("state1")
        });

        portal.submitBatch(commitment, bytes32(0), queue2Hash, queue2Hash, "", "");
        assertEq(portal.withdrawalQueue2(), queue2Hash);

        // Process w1 (oldest) - this will swap queue2 into queue1 first
        uint256 bobBalanceBefore = pathUSD.balanceOf(bob);
        portal.processWithdrawal(w1, innerHash);
        assertEq(pathUSD.balanceOf(bob), bobBalanceBefore + 300e6);

        // Queue1 should now have w2
        assertEq(portal.withdrawalQueue1(), innerHash);
        assertEq(portal.withdrawalQueue2(), bytes32(0));

        // Process w2
        uint256 charlieBalanceBefore = pathUSD.balanceOf(charlie);
        portal.processWithdrawal(w2, bytes32(0));
        assertEq(pathUSD.balanceOf(charlie), charlieBalanceBefore + 400e6);

        // Both queues empty
        assertEq(portal.withdrawalQueue1(), bytes32(0));
        assertEq(portal.withdrawalQueue2(), bytes32(0));
    }

    function test_withdrawalQueue_raceConditionHandling() public {
        // Test the race condition scenario from the spec:
        // 1. Proof generation starts when queue2 = X (non-empty)
        // 2. Sequencer drains queue1, triggering swap: queue1 = X, queue2 = 0
        // 3. Proof submits expecting queue2 = X, but it's now 0

        uint128 depositAmount = 3000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        portal.deposit(alice, depositAmount, bytes32("memo"));
        vm.stopPrank();

        // First batch: add withdrawal w1 to queue2
        Withdrawal memory w1 = Withdrawal({
            sender: alice, to: bob, amount: 500e6, gasLimit: 0, fallbackRecipient: alice, data: ""
        });
        bytes32 w1Hash = keccak256(abi.encode(w1, bytes32(0)));

        BatchCommitment memory commit1 = BatchCommitment({
            newProcessedDepositsHash: portal.currentDepositsHash(),
            newStateRoot: keccak256("state1")
        });
        portal.submitBatch(commit1, bytes32(0), w1Hash, w1Hash, "", "");

        // queue2 = w1Hash
        assertEq(portal.withdrawalQueue2(), w1Hash);

        // Process w1: queue2 swaps to queue1, then drained
        portal.processWithdrawal(w1, bytes32(0));
        // Now both queues are empty
        assertEq(portal.withdrawalQueue1(), bytes32(0));
        assertEq(portal.withdrawalQueue2(), bytes32(0));

        // Second batch was generated when queue2 = w1Hash
        // But now queue2 = 0 (swap occurred)
        Withdrawal memory w2 = Withdrawal({
            sender: alice, to: charlie, amount: 600e6, gasLimit: 0, fallbackRecipient: alice, data: ""
        });
        bytes32 w2Hash = keccak256(abi.encode(w2, bytes32(0)));

        // Proof expected queue2 = w1Hash, generated updatedQueue2 with w2 added
        // But since queue2 is now 0, we use newWithdrawalsOnly
        bytes32 updatedQueue2 = keccak256(abi.encode(w2, w1Hash)); // w2 added to w1Hash

        BatchCommitment memory commit2 = BatchCommitment({
            newProcessedDepositsHash: portal.currentDepositsHash(),
            newStateRoot: keccak256("state2")
        });

        // This should use newWithdrawalsOnly since queue2 != expectedQueue2 but queue2 == 0
        portal.submitBatch(commit2, w1Hash, updatedQueue2, w2Hash, "", "");

        // queue2 should now have w2 only (newWithdrawalsOnly path)
        assertEq(portal.withdrawalQueue2(), w2Hash);
    }

    function test_withdrawalQueue_revertsOnUnexpectedState() public {
        // If queue2 != expectedQueue2 AND queue2 != 0, should revert
        uint128 depositAmount = 2000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        portal.deposit(alice, depositAmount, bytes32("memo"));
        vm.stopPrank();

        // Add withdrawal to queue2
        Withdrawal memory w1 = Withdrawal({
            sender: alice, to: bob, amount: 500e6, gasLimit: 0, fallbackRecipient: alice, data: ""
        });
        bytes32 w1Hash = keccak256(abi.encode(w1, bytes32(0)));

        BatchCommitment memory commit = BatchCommitment({
            newProcessedDepositsHash: portal.currentDepositsHash(),
            newStateRoot: keccak256("state1")
        });
        portal.submitBatch(commit, bytes32(0), w1Hash, w1Hash, "", "");

        // Now queue2 = w1Hash
        // Try to submit with wrong expectedQueue2
        bytes32 wrongExpected = keccak256("wrong");
        vm.expectRevert(IZonePortal.UnexpectedQueue2State.selector);
        portal.submitBatch(commit, wrongExpected, bytes32(0), bytes32(0), "", "");
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
            gasLimit: 100000,
            fallbackRecipient: alice,
            data: "callback_data"
        });
        bytes32 wHash = keccak256(abi.encode(w, bytes32(0)));

        // Submit batch adding withdrawal
        BatchCommitment memory commit = BatchCommitment({
            newProcessedDepositsHash: portal.currentDepositsHash(),
            newStateRoot: keccak256("state")
        });
        portal.submitBatch(commit, bytes32(0), wHash, wHash, "", "");

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

        bytes32 depositHashBefore = portal.currentDepositsHash();

        // Set receiver to revert
        exitReceiver.setShouldRevert(true);

        // Create withdrawal with callback
        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: address(exitReceiver),
            amount: 500e6,
            gasLimit: 100000,
            fallbackRecipient: alice,
            data: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, bytes32(0)));

        // Submit batch
        BatchCommitment memory commit = BatchCommitment({
            newProcessedDepositsHash: depositHashBefore,
            newStateRoot: keccak256("state")
        });
        portal.submitBatch(commit, bytes32(0), wHash, wHash, "", "");

        // Process withdrawal - should bounce back
        portal.processWithdrawal(w, bytes32(0));

        // Receiver should NOT have funds (they stayed in portal)
        assertEq(pathUSD.balanceOf(address(exitReceiver)), 0);

        // Deposit hash should have changed (bounce-back added a deposit)
        assertTrue(portal.currentDepositsHash() != depositHashBefore);
    }

    function test_withdrawal_bounceBackOnWrongSelector() public {
        // Fund portal
        uint128 depositAmount = 1000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        portal.deposit(alice, depositAmount, bytes32("memo"));
        vm.stopPrank();

        bytes32 depositHashBefore = portal.currentDepositsHash();

        // Set receiver to return wrong selector
        exitReceiver.setShouldAccept(false);

        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: address(exitReceiver),
            amount: 500e6,
            gasLimit: 100000,
            fallbackRecipient: alice,
            data: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, bytes32(0)));

        BatchCommitment memory commit = BatchCommitment({
            newProcessedDepositsHash: depositHashBefore,
            newStateRoot: keccak256("state")
        });
        portal.submitBatch(commit, bytes32(0), wHash, wHash, "", "");

        // Process withdrawal - should bounce back due to wrong selector
        portal.processWithdrawal(w, bytes32(0));

        // Funds should still be in portal (bounce-back)
        assertEq(pathUSD.balanceOf(address(exitReceiver)), 0);
        assertTrue(portal.currentDepositsHash() != depositHashBefore);
    }

    /*//////////////////////////////////////////////////////////////
                     INVALID WITHDRAWAL TESTS
    //////////////////////////////////////////////////////////////*/

    function test_processWithdrawal_revertsIfEmpty() public {
        Withdrawal memory w = Withdrawal({
            sender: alice, to: bob, amount: 100e6, gasLimit: 0, fallbackRecipient: alice, data: ""
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
            sender: alice, to: bob, amount: 500e6, gasLimit: 0, fallbackRecipient: alice, data: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, bytes32(0)));

        BatchCommitment memory commit = BatchCommitment({
            newProcessedDepositsHash: portal.currentDepositsHash(),
            newStateRoot: keccak256("state")
        });
        portal.submitBatch(commit, bytes32(0), wHash, wHash, "", "");

        // Try to process with wrong withdrawal data
        Withdrawal memory wrongW = Withdrawal({
            sender: alice, to: charlie, amount: 500e6, gasLimit: 0, fallbackRecipient: alice, data: ""
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
            sender: alice, to: bob, amount: 500e6, gasLimit: 0, fallbackRecipient: alice, data: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, bytes32(0)));

        BatchCommitment memory commit = BatchCommitment({
            newProcessedDepositsHash: portal.currentDepositsHash(),
            newStateRoot: keccak256("state")
        });
        portal.submitBatch(commit, bytes32(0), wHash, wHash, "", "");

        vm.prank(alice); // Not sequencer
        vm.expectRevert(IZonePortal.NotSequencer.selector);
        portal.processWithdrawal(w, bytes32(0));
    }

    /*//////////////////////////////////////////////////////////////
                         DEPOSIT CHAIN TESTS
    //////////////////////////////////////////////////////////////*/

    function test_depositChain_threeSlotDesign() public {
        // Test the 3-slot deposit design:
        // processedDepositsHash: where proofs start
        // pendingDepositsHash: stable target for proofs
        // currentDepositsHash: head of chain (new deposits land here)

        // Initial state: all zero
        assertEq(portal.processedDepositsHash(), bytes32(0));
        assertEq(portal.pendingDepositsHash(), bytes32(0));
        assertEq(portal.currentDepositsHash(), bytes32(0));

        // Make deposits
        vm.startPrank(alice);
        pathUSD.approve(address(portal), 3000e6);
        bytes32 h1 = portal.deposit(alice, 1000e6, bytes32("d1"));
        bytes32 h2 = portal.deposit(alice, 1000e6, bytes32("d2"));
        vm.stopPrank();

        // currentDepositsHash should be h2 (latest)
        assertEq(portal.currentDepositsHash(), h2);
        // processed and pending still zero (no batch yet)
        assertEq(portal.processedDepositsHash(), bytes32(0));
        assertEq(portal.pendingDepositsHash(), bytes32(0));

        // Submit batch processing only first deposit
        BatchCommitment memory commit1 = BatchCommitment({
            newProcessedDepositsHash: h1,
            newStateRoot: keccak256("state1")
        });
        portal.submitBatch(commit1, bytes32(0), bytes32(0), bytes32(0), "", "");

        // After batch:
        // processedDepositsHash = h1 (where we processed to)
        // pendingDepositsHash = h2 (snapshot of currentDepositsHash)
        // currentDepositsHash = h2 (unchanged by batch)
        assertEq(portal.processedDepositsHash(), h1);
        assertEq(portal.pendingDepositsHash(), h2);
        assertEq(portal.currentDepositsHash(), h2);

        // New deposit arrives
        vm.startPrank(alice);
        bytes32 h3 = portal.deposit(alice, 1000e6, bytes32("d3"));
        vm.stopPrank();

        // currentDepositsHash updated, others unchanged
        assertEq(portal.currentDepositsHash(), h3);
        assertEq(portal.processedDepositsHash(), h1);
        assertEq(portal.pendingDepositsHash(), h2);

        // Submit batch processing up to h2
        BatchCommitment memory commit2 = BatchCommitment({
            newProcessedDepositsHash: h2,
            newStateRoot: keccak256("state2")
        });
        portal.submitBatch(commit2, bytes32(0), bytes32(0), bytes32(0), "", "");

        // Now:
        // processedDepositsHash = h2 (advanced)
        // pendingDepositsHash = h3 (snapshot of current)
        // currentDepositsHash = h3 (unchanged)
        assertEq(portal.processedDepositsHash(), h2);
        assertEq(portal.pendingDepositsHash(), h3);
        assertEq(portal.currentDepositsHash(), h3);
    }
}
