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
    IWithdrawalReceiver,
    ZoneInfo,
    Deposit,
    Withdrawal,
    BlockTransition,
    DepositQueueTransition,
    WithdrawalQueueTransition
} from "../../src/zone/IZone.sol";
import { WithdrawalQueueLib, EMPTY_SENTINEL } from "../../src/zone/WithdrawalQueueLib.sol";

/// @notice Mock withdrawal receiver that accepts funds
contract MockWithdrawalReceiver is IWithdrawalReceiver {
    bool public shouldAccept = true;
    bool public shouldRevert = false;

    address public lastSender;
    uint128 public lastAmount;
    bytes public lastCallbackData;

    function setShouldAccept(bool _shouldAccept) external {
        shouldAccept = _shouldAccept;
    }

    function setShouldRevert(bool _shouldRevert) external {
        shouldRevert = _shouldRevert;
    }

    function onWithdrawalReceived(
        address sender,
        uint128 amount,
        bytes calldata callbackData
    ) external returns (bytes4) {
        lastSender = sender;
        lastAmount = amount;
        lastCallbackData = callbackData;

        if (shouldRevert) {
            revert("MockWithdrawalReceiver: intentional revert");
        }

        if (shouldAccept) {
            return IWithdrawalReceiver.onWithdrawalReceived.selector;
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
    MockWithdrawalReceiver public withdrawalReceiver;

    uint64 public testZoneId;
    bytes32 public constant GENESIS_BLOCK_HASH = keccak256("genesis");
    bytes32 public constant GENESIS_TEMPO_BLOCK_HASH = keccak256("tempoGenesis");
    uint64 public genesisTempoBlockNumber;

    function setUp() public override {
        super.setUp();

        // Deploy zone infrastructure
        zoneFactory = new ZoneFactory();
        mockVerifier = new MockVerifier();
        withdrawalReceiver = new MockWithdrawalReceiver();

        // Grant issuer role and mint tokens for tests
        vm.startPrank(pathUSDAdmin);
        pathUSD.grantRole(_ISSUER_ROLE, pathUSDAdmin);
        pathUSD.mint(admin, 1_000_000e6);
        pathUSD.mint(alice, 100_000e6);
        pathUSD.mint(bob, 100_000e6);
        vm.stopPrank();

        // Record genesis block number for Tempo
        genesisTempoBlockNumber = uint64(block.number);

        // Create a zone
        IZoneFactory.CreateZoneParams memory params = IZoneFactory.CreateZoneParams({
            token: address(pathUSD),
            sequencer: admin, // admin is the sequencer for tests
            verifier: address(mockVerifier),
            genesisBlockHash: GENESIS_BLOCK_HASH,
            genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
            genesisTempoBlockNumber: genesisTempoBlockNumber
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
        assertEq(portal.blockHash(), GENESIS_BLOCK_HASH);
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

        // Advance a block so we can use blockhash
        vm.roll(block.number + 1);

        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: newStateRoot }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHash }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );

        // Verify state updated
        assertEq(portal.blockHash(), newStateRoot);
        assertEq(portal.batchIndex(), 1);
        assertEq(portal.processedDepositQueueHash(), depositHash);
    }

    function test_submitBatch_revertsIfNotSequencer() public {
        // Advance a block so we can use blockhash
        vm.roll(block.number + 1);

        bytes32 nextStateRoot = keccak256("state");
        vm.prank(alice); // Not sequencer
        vm.expectRevert(IZonePortal.NotSequencer.selector);
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: nextStateRoot }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0) }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );
    }

    function test_submitBatch_revertsOnInvalidProof() public {
        mockVerifier.setShouldAccept(false);

        // Advance a block so we can use blockhash
        vm.roll(block.number + 1);

        bytes32 nextStateRoot = keccak256("state");
        vm.expectRevert(IZonePortal.InvalidProof.selector);
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: nextStateRoot }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0) }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );
    }

    /*//////////////////////////////////////////////////////////////
                    WITHDRAWAL QUEUE TESTS (ZONE -> TEMPO)
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
            callbackData: ""
        });

        // Build withdrawal hash (oldest = outermost, innermost = EMPTY_SENTINEL)
        bytes32 withdrawalHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        // Advance a block so we can use blockhash
        vm.roll(block.number + 1);

        // Submit batch that adds withdrawal to slot 0
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("stateWithWithdrawal") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash() }),
            WithdrawalQueueTransition({ withdrawalQueueHash: withdrawalHash }),
            "",
            ""
        );

        // Slot 0 should now have the withdrawal, tail advanced to 1
        assertEq(portal.withdrawalQueueSlot(0), withdrawalHash);
        assertEq(portal.withdrawalQueueHead(), 0);
        assertEq(portal.withdrawalQueueTail(), 1);

        // Process the withdrawal
        uint256 bobBalanceBefore = pathUSD.balanceOf(bob);
        portal.processWithdrawal(w, bytes32(0));  // 0 means last item in slot

        // Bob should have received funds
        assertEq(pathUSD.balanceOf(bob), bobBalanceBefore + 500e6);
        // Slot should be cleared (back to EMPTY_SENTINEL), head advanced to 1
        assertEq(portal.withdrawalQueueSlot(0), EMPTY_SENTINEL);
        assertEq(portal.withdrawalQueueHead(), 1);
    }

    function test_withdrawalQueue_multipleWithdrawalsInBatch() public {
        // Setup: deposit funds
        uint128 depositAmount = 2000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        portal.deposit(alice, depositAmount, bytes32("memo"));
        vm.stopPrank();

        // Create two withdrawals in the same batch
        Withdrawal memory w1 = Withdrawal({
            sender: alice, to: bob, amount: 300e6, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, callbackData: ""
        });
        Withdrawal memory w2 = Withdrawal({
            sender: alice, to: charlie, amount: 400e6, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, callbackData: ""
        });

        // Build queue: w1 is oldest (outermost), w2 is newest (innermost wraps EMPTY_SENTINEL)
        bytes32 innerHash = keccak256(abi.encode(w2, EMPTY_SENTINEL));
        bytes32 batchQueueHash = keccak256(abi.encode(w1, innerHash));

        // Advance a block so we can use blockhash
        vm.roll(block.number + 1);

        // Submit batch adding both withdrawals to slot 0
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("state1") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash() }),
            WithdrawalQueueTransition({ withdrawalQueueHash: batchQueueHash }),
            "",
            ""
        );
        assertEq(portal.withdrawalQueueSlot(0), batchQueueHash);
        assertEq(portal.withdrawalQueueTail(), 1);

        // Process w1 (oldest)
        uint256 bobBalanceBefore = pathUSD.balanceOf(bob);
        portal.processWithdrawal(w1, innerHash);
        assertEq(pathUSD.balanceOf(bob), bobBalanceBefore + 300e6);

        // Slot 0 should now have w2's hash, head still at 0
        assertEq(portal.withdrawalQueueSlot(0), innerHash);
        assertEq(portal.withdrawalQueueHead(), 0);

        // Process w2 (last in slot)
        uint256 charlieBalanceBefore = pathUSD.balanceOf(charlie);
        portal.processWithdrawal(w2, bytes32(0));  // 0 = last item
        assertEq(pathUSD.balanceOf(charlie), charlieBalanceBefore + 400e6);

        // Slot 0 cleared, head advanced
        assertEq(portal.withdrawalQueueSlot(0), EMPTY_SENTINEL);
        assertEq(portal.withdrawalQueueHead(), 1);
    }

    function test_withdrawalQueue_multipleBatches() public {
        // Test that multiple batches get their own slots
        uint128 depositAmount = 3000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        portal.deposit(alice, depositAmount, bytes32("memo"));
        vm.stopPrank();

        // Batch 1: withdrawal to bob
        Withdrawal memory w1 = Withdrawal({
            sender: alice, to: bob, amount: 500e6, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, callbackData: ""
        });
        bytes32 w1Hash = keccak256(abi.encode(w1, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("state1") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash() }),
            WithdrawalQueueTransition({ withdrawalQueueHash: w1Hash }),
            "",
            ""
        );

        // Batch 2: withdrawal to charlie
        Withdrawal memory w2 = Withdrawal({
            sender: alice, to: charlie, amount: 600e6, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, callbackData: ""
        });
        bytes32 w2Hash = keccak256(abi.encode(w2, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("state2") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash() }),
            WithdrawalQueueTransition({ withdrawalQueueHash: w2Hash }),
            "",
            ""
        );

        // Verify slots
        assertEq(portal.withdrawalQueueSlot(0), w1Hash);
        assertEq(portal.withdrawalQueueSlot(1), w2Hash);
        assertEq(portal.withdrawalQueueHead(), 0);
        assertEq(portal.withdrawalQueueTail(), 2);

        // Process w1 from slot 0
        portal.processWithdrawal(w1, bytes32(0));
        assertEq(portal.withdrawalQueueHead(), 1);

        // Process w2 from slot 1
        portal.processWithdrawal(w2, bytes32(0));
        assertEq(portal.withdrawalQueueHead(), 2);
    }

    function test_withdrawalQueue_batchWithNoWithdrawals() public {
        // Test that batches with no withdrawals don't affect the queue
        uint128 depositAmount = 1000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        portal.deposit(alice, depositAmount, bytes32("memo"));
        vm.stopPrank();

        uint256 tailBefore = portal.withdrawalQueueTail();

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("state1") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash() }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),  // No withdrawals
            "",
            ""
        );

        // Tail should not have advanced
        assertEq(portal.withdrawalQueueTail(), tailBefore);
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
            to: address(withdrawalReceiver),
            amount: 500e6,
            memo: bytes32(0),
            gasLimit: 100000,
            fallbackRecipient: alice,
            callbackData: "callback_data"
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        // Advance a block so we can use blockhash
        vm.roll(block.number + 1);

        // Submit batch adding withdrawal
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("state") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash() }),
            WithdrawalQueueTransition({ withdrawalQueueHash: wHash }),
            "",
            ""
        );

        // Process withdrawal (0 = last item in slot)
        portal.processWithdrawal(w, bytes32(0));

        // Receiver should have gotten funds and callback
        assertEq(pathUSD.balanceOf(address(withdrawalReceiver)), 500e6);
        assertEq(withdrawalReceiver.lastSender(), alice);
        assertEq(withdrawalReceiver.lastAmount(), 500e6);
        assertEq(withdrawalReceiver.lastCallbackData(), "callback_data");
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
        withdrawalReceiver.setShouldRevert(true);

        // Create withdrawal with callback
        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: address(withdrawalReceiver),
            amount: 500e6,
            memo: bytes32(0),
            gasLimit: 100000,
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        // Advance a block so we can use blockhash
        vm.roll(block.number + 1);

        // Submit batch
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("state") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHashBefore }),
            WithdrawalQueueTransition({ withdrawalQueueHash: wHash }),
            "",
            ""
        );

        // Process withdrawal - should bounce back
        portal.processWithdrawal(w, bytes32(0));

        // Receiver should NOT have funds (they stayed in portal)
        assertEq(pathUSD.balanceOf(address(withdrawalReceiver)), 0);

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
        withdrawalReceiver.setShouldAccept(false);

        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: address(withdrawalReceiver),
            amount: 500e6,
            memo: bytes32(0),
            gasLimit: 100000,
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        // Advance a block so we can use blockhash
        vm.roll(block.number + 1);

        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("state") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHashBefore }),
            WithdrawalQueueTransition({ withdrawalQueueHash: wHash }),
            "",
            ""
        );

        // Process withdrawal - should bounce back due to wrong selector
        portal.processWithdrawal(w, bytes32(0));

        // Funds should still be in portal (bounce-back)
        assertEq(pathUSD.balanceOf(address(withdrawalReceiver)), 0);
        assertTrue(portal.currentDepositQueueHash() != depositHashBefore);
    }

    /*//////////////////////////////////////////////////////////////
                     INVALID WITHDRAWAL TESTS
    //////////////////////////////////////////////////////////////*/

    function test_processWithdrawal_revertsIfEmpty() public {
        Withdrawal memory w = Withdrawal({
            sender: alice, to: bob, amount: 100e6, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, callbackData: ""
        });

        vm.expectRevert(WithdrawalQueueLib.NoWithdrawalsInQueue.selector);
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
            sender: alice, to: bob, amount: 500e6, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        // Advance a block so we can use blockhash
        vm.roll(block.number + 1);

        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("state") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash() }),
            WithdrawalQueueTransition({ withdrawalQueueHash: wHash }),
            "",
            ""
        );

        // Try to process with wrong withdrawal data
        Withdrawal memory wrongW = Withdrawal({
            sender: alice, to: charlie, amount: 500e6, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, callbackData: ""
        });

        vm.expectRevert(WithdrawalQueueLib.InvalidWithdrawalHash.selector);
        portal.processWithdrawal(wrongW, bytes32(0));
    }

    function test_processWithdrawal_revertsIfNotSequencer() public {
        uint128 depositAmount = 1000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        portal.deposit(alice, depositAmount, bytes32("memo"));
        vm.stopPrank();

        Withdrawal memory w = Withdrawal({
            sender: alice, to: bob, amount: 500e6, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        // Advance a block so we can use blockhash
        vm.roll(block.number + 1);

        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("state") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash() }),
            WithdrawalQueueTransition({ withdrawalQueueHash: wHash }),
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

    function test_depositChain_twoSlotDesign() public {
        // Test the 2-slot deposit design:
        // processedDepositQueueHash: where proofs have processed up to
        // currentDepositQueueHash: head of chain (new deposits land here)
        // The proof reads currentDepositQueueHash from Tempo state to validate ancestry.

        // Initial state: both zero
        assertEq(portal.processedDepositQueueHash(), bytes32(0));
        assertEq(portal.currentDepositQueueHash(), bytes32(0));

        // Make deposits
        vm.startPrank(alice);
        pathUSD.approve(address(portal), 3000e6);
        bytes32 h1 = portal.deposit(alice, 1000e6, bytes32("d1"));
        bytes32 h2 = portal.deposit(alice, 1000e6, bytes32("d2"));
        vm.stopPrank();

        // currentDepositQueueHash should be h2 (latest)
        assertEq(portal.currentDepositQueueHash(), h2);
        // processed still zero (no batch yet)
        assertEq(portal.processedDepositQueueHash(), bytes32(0));

        // Advance a block so we can use blockhash
        vm.roll(block.number + 1);

        // Submit batch processing only first deposit
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("state1") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: h1 }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );

        // After batch:
        // processedDepositQueueHash = h1 (where we processed to)
        // currentDepositQueueHash = h2 (unchanged)
        assertEq(portal.processedDepositQueueHash(), h1);
        assertEq(portal.currentDepositQueueHash(), h2);

        // New deposit arrives
        vm.startPrank(alice);
        bytes32 h3 = portal.deposit(alice, 1000e6, bytes32("d3"));
        vm.stopPrank();

        // currentDepositQueueHash updated, processed unchanged
        assertEq(portal.currentDepositQueueHash(), h3);
        assertEq(portal.processedDepositQueueHash(), h1);

        // Advance a block so we can use blockhash
        vm.roll(block.number + 1);

        // Submit batch processing up to h2
        // Note: prevProcessedHash must match current processed (h1)
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("state2") }),
            DepositQueueTransition({ prevProcessedHash: h1, nextProcessedHash: h2 }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );

        // Now:
        // processedDepositQueueHash = h2 (advanced)
        // currentDepositQueueHash = h3 (unchanged)
        assertEq(portal.processedDepositQueueHash(), h2);
        assertEq(portal.currentDepositQueueHash(), h3);
    }
}
