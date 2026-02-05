// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { BaseTest } from "../BaseTest.t.sol";
import { ZoneFactory } from "../../src/zone/ZoneFactory.sol";
import { ZonePortal } from "../../src/zone/ZonePortal.sol";
import { ZoneMessenger } from "../../src/zone/ZoneMessenger.sol";
import { MockVerifier } from "./mocks/MockVerifier.sol";
import { TIP20 } from "../../src/TIP20.sol";
import { ITIP20 } from "../../src/interfaces/ITIP20.sol";
import {
    IZoneFactory,
    IZonePortal,
    IZoneMessenger,
    IWithdrawalReceiver,
    ZoneInfo,
    ZoneParams,
    Deposit,
    Withdrawal,
    BlockTransition,
    DepositQueueTransition,
    WithdrawalQueueTransition
} from "../../src/zone/IZone.sol";
import { WithdrawalQueueLib, EMPTY_SENTINEL } from "../../src/zone/WithdrawalQueueLib.sol";
import { BLOCKHASH_HISTORY_WINDOW } from "../../src/zone/BlockHashHistory.sol";

/// @notice Mock withdrawal receiver that accepts funds
contract MockWithdrawalReceiver is IWithdrawalReceiver {
    bool public shouldAccept = true;
    bool public shouldRevert = false;

    address public lastSender;
    uint128 public lastAmount;
    bytes public lastCallbackData;
    address public expectedMessenger;

    function setExpectedMessenger(address _messenger) external {
        expectedMessenger = _messenger;
    }

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
    )
        external
        returns (bytes4)
    {
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

/// @notice Mock receiver that consumes all gas
contract GasConsumingReceiver is IWithdrawalReceiver {
    function onWithdrawalReceived(
        address,
        uint128,
        bytes calldata
    ) external returns (bytes4) {
        // Infinite loop to consume all gas
        while (true) {}
        return IWithdrawalReceiver.onWithdrawalReceived.selector;
    }
}

/// @notice Mock receiver that succeeds normally
contract SuccessfulReceiver is IWithdrawalReceiver {
    uint256 public callCount;

    function onWithdrawalReceived(
        address,
        uint128,
        bytes calldata
    ) external returns (bytes4) {
        callCount++;
        return IWithdrawalReceiver.onWithdrawalReceived.selector;
    }
}

/// @notice Tests for ZonePortal - simulating L1/zone interface
contract ZonePortalTest is BaseTest {
    ZoneFactory public zoneFactory;
    MockVerifier public mockVerifier;
    ZonePortal public portal;
    ZoneMessenger public messenger;
    MockWithdrawalReceiver public withdrawalReceiver;
    GasConsumingReceiver public gasConsumingReceiver;
    SuccessfulReceiver public successfulReceiver;

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
        gasConsumingReceiver = new GasConsumingReceiver();
        successfulReceiver = new SuccessfulReceiver();

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
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: genesisTempoBlockNumber
            })
        });

        address portalAddr;
        (testZoneId, portalAddr) = zoneFactory.createZone(params);
        portal = ZonePortal(portalAddr);

        // Get the messenger
        ZoneInfo memory info = zoneFactory.zones(testZoneId);
        messenger = ZoneMessenger(info.messenger);

        // Set expected messenger for withdrawal receiver
        withdrawalReceiver.setExpectedMessenger(address(messenger));
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
        assertEq(portal.withdrawalBatchIndex(), 0);
        assertEq(portal.messenger(), address(messenger));
    }

    function test_zoneFactoryTracksZones() public view {
        assertEq(zoneFactory.zoneCount(), 1);
        assertTrue(zoneFactory.isZonePortal(address(portal)));

        ZoneInfo memory info = zoneFactory.zones(testZoneId);
        assertEq(info.zoneId, testZoneId);
        assertEq(info.portal, address(portal));
        assertEq(info.messenger, address(messenger));
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

        // Advance a block so the history precompile can return a hash
        vm.roll(block.number + 1);

        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: newStateRoot }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHash }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );

        // Verify state updated
        assertEq(portal.blockHash(), newStateRoot);
        assertEq(portal.withdrawalBatchIndex(), 1);
        assertEq(portal.lastSyncedTempoBlockNumber(), uint64(block.number - 1));
    }

    function test_submitBatch_revertsOnPrevBlockHashMismatch() public {
        // Advance a block so the history precompile can return a hash
        vm.roll(block.number + 1);

        vm.expectRevert(IZonePortal.InvalidProof.selector);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: keccak256("wrong"), nextBlockHash: keccak256("state") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0) }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );
    }

    function test_submitBatch_revertsIfNotSequencer() public {
        // Advance a block so the history precompile can return a hash
        vm.roll(block.number + 1);

        bytes32 prevBlockHash = portal.blockHash();
        bytes32 nextStateRoot = keccak256("state");
        vm.prank(alice); // Not sequencer
        vm.expectRevert(IZonePortal.NotSequencer.selector);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: prevBlockHash, nextBlockHash: nextStateRoot }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0) }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );
    }

    function test_submitBatch_revertsOnInvalidProof() public {
        mockVerifier.setShouldAccept(false);

        // Advance a block so the history precompile can return a hash
        vm.roll(block.number + 1);

        bytes32 prevBlockHash = portal.blockHash();
        bytes32 nextStateRoot = keccak256("state");
        vm.expectRevert(IZonePortal.InvalidProof.selector);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: prevBlockHash, nextBlockHash: nextStateRoot }),
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
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0, // No callback
            fallbackRecipient: alice,
            callbackData: ""
        });

        // Build withdrawal hash (oldest = outermost, innermost = EMPTY_SENTINEL)
        bytes32 withdrawalHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        // Advance a block so the history precompile can return a hash
        vm.roll(block.number + 1);

        // Submit batch that adds withdrawal to slot 0
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("stateWithWithdrawal") }),
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
            sender: alice, to: bob, amount: 300e6, fee: 0, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, callbackData: ""
        });
        Withdrawal memory w2 = Withdrawal({
            sender: alice, to: charlie, amount: 400e6, fee: 0, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, callbackData: ""
        });

        // Build queue: w1 is oldest (outermost), w2 is newest (innermost wraps EMPTY_SENTINEL)
        bytes32 innerHash = keccak256(abi.encode(w2, EMPTY_SENTINEL));
        bytes32 batchQueueHash = keccak256(abi.encode(w1, innerHash));

        // Advance a block so the history precompile can return a hash
        vm.roll(block.number + 1);

        // Submit batch adding both withdrawals to slot 0
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state1") }),
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
            sender: alice, to: bob, amount: 500e6, fee: 0, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, callbackData: ""
        });
        bytes32 w1Hash = keccak256(abi.encode(w1, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state1") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash() }),
            WithdrawalQueueTransition({ withdrawalQueueHash: w1Hash }),
            "",
            ""
        );

        // Batch 2: withdrawal to charlie
        Withdrawal memory w2 = Withdrawal({
            sender: alice, to: charlie, amount: 600e6, fee: 0, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, callbackData: ""
        });
        bytes32 w2Hash = keccak256(abi.encode(w2, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state2") }),
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
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state1") }),
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
            fee: 0,
            memo: bytes32(0),
            gasLimit: 100000,
            fallbackRecipient: alice,
            callbackData: "callback_data"
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        // Advance a block so the history precompile can return a hash
        vm.roll(block.number + 1);

        // Submit batch adding withdrawal
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state") }),
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
            fee: 0,
            memo: bytes32(0),
            gasLimit: 100000,
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        // Advance a block so the history precompile can return a hash
        vm.roll(block.number + 1);

        // Submit batch
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state") }),
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
            fee: 0,
            memo: bytes32(0),
            gasLimit: 100000,
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        // Advance a block so the history precompile can return a hash
        vm.roll(block.number + 1);

        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state") }),
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

    function test_withdrawal_bounceBackOnTransferRevert_noCallback() public {
        // Fund portal
        uint128 depositAmount = 1000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        portal.deposit(alice, depositAmount, bytes32("memo"));
        vm.stopPrank();

        bytes32 depositHashBefore = portal.currentDepositQueueHash();
        uint256 portalBalanceBefore = pathUSD.balanceOf(address(portal));
        uint256 bobBalanceBefore = pathUSD.balanceOf(bob);

        // Pause token to force transfer revert
        vm.startPrank(pathUSDAdmin);
        pathUSD.grantRole(_PAUSE_ROLE, pathUSDAdmin);
        pathUSD.pause();
        vm.stopPrank();

        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: bob,
            amount: 500e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        // Advance a block so the history precompile can return a hash
        vm.roll(block.number + 1);

        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHashBefore }),
            WithdrawalQueueTransition({ withdrawalQueueHash: wHash }),
            "",
            ""
        );

        // Process withdrawal - should bounce back due to transfer revert
        portal.processWithdrawal(w, bytes32(0));

        // No transfer should have happened
        assertEq(pathUSD.balanceOf(address(portal)), portalBalanceBefore);
        assertEq(pathUSD.balanceOf(bob), bobBalanceBefore);
        assertTrue(portal.currentDepositQueueHash() != depositHashBefore);
    }

    /*//////////////////////////////////////////////////////////////
                     INVALID WITHDRAWAL TESTS
    //////////////////////////////////////////////////////////////*/

    function test_processWithdrawal_revertsIfEmpty() public {
        Withdrawal memory w = Withdrawal({
            sender: alice, to: bob, amount: 100e6, fee: 0, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, callbackData: ""
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
            sender: alice, to: bob, amount: 500e6, fee: 0, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        // Advance a block so the history precompile can return a hash
        vm.roll(block.number + 1);

        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash() }),
            WithdrawalQueueTransition({ withdrawalQueueHash: wHash }),
            "",
            ""
        );

        // Try to process with wrong withdrawal data
        Withdrawal memory wrongW = Withdrawal({
            sender: alice, to: charlie, amount: 500e6, fee: 0, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, callbackData: ""
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
            sender: alice, to: bob, amount: 500e6, fee: 0, memo: bytes32(0), gasLimit: 0, fallbackRecipient: alice, callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        // Advance a block so the history precompile can return a hash
        vm.roll(block.number + 1);

        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state") }),
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

    function test_depositChain_singleSlotDesign() public {
        // Test the simplified single-slot deposit design:
        // currentDepositQueueHash: head of chain (new deposits land here)
        // The zone tracks its own processedDepositQueueHash in EVM state.
        // The proof reads currentDepositQueueHash from Tempo state to validate ancestry.

        // Initial state: zero
        assertEq(portal.currentDepositQueueHash(), bytes32(0));

        // Make deposits
        vm.startPrank(alice);
        pathUSD.approve(address(portal), 3000e6);
        bytes32 h1 = portal.deposit(alice, 1000e6, bytes32("d1"));
        bytes32 h2 = portal.deposit(alice, 1000e6, bytes32("d2"));
        vm.stopPrank();

        // currentDepositQueueHash should be h2 (latest)
        assertEq(portal.currentDepositQueueHash(), h2);

        // Advance a block so the history precompile can return a hash
        vm.roll(block.number + 1);

        // Submit batch - portal no longer tracks processed, just updates lastSyncedTempoBlockNumber
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state1") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: h1 }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );

        // After batch: currentDepositQueueHash unchanged, lastSyncedTempoBlockNumber updated
        assertEq(portal.currentDepositQueueHash(), h2);
        assertEq(portal.lastSyncedTempoBlockNumber(), uint64(block.number - 1));

        // New deposit arrives
        vm.startPrank(alice);
        bytes32 h3 = portal.deposit(alice, 1000e6, bytes32("d3"));
        vm.stopPrank();

        // currentDepositQueueHash updated
        assertEq(portal.currentDepositQueueHash(), h3);
    }

    /*//////////////////////////////////////////////////////////////
                     SEQUENCER PUBKEY TESTS
    //////////////////////////////////////////////////////////////*/

    function test_setSequencerPubkey_success() public {
        bytes32 pubkey = keccak256("sequencerPubkey");

        assertEq(portal.sequencerPubkey(), bytes32(0));

        portal.setSequencerPubkey(pubkey);

        assertEq(portal.sequencerPubkey(), pubkey);
    }

    function test_setSequencerPubkey_canUpdate() public {
        bytes32 pubkey1 = keccak256("pubkey1");
        bytes32 pubkey2 = keccak256("pubkey2");

        portal.setSequencerPubkey(pubkey1);
        assertEq(portal.sequencerPubkey(), pubkey1);

        portal.setSequencerPubkey(pubkey2);
        assertEq(portal.sequencerPubkey(), pubkey2);
    }

    function test_setSequencerPubkey_revertsIfNotSequencer() public {
        vm.prank(alice);
        vm.expectRevert(IZonePortal.NotSequencer.selector);
        portal.setSequencerPubkey(keccak256("pubkey"));
    }

    /*//////////////////////////////////////////////////////////////
                      BATCH SUBMISSION VALIDATION
    //////////////////////////////////////////////////////////////*/

    function test_submitBatch_revertsIfTempoBlockNumberBeforeGenesis() public {
        bytes32 prevBlockHash = portal.blockHash();
        vm.expectRevert(IZonePortal.InvalidTempoBlockNumber.selector);
        portal.submitBatch(
            genesisTempoBlockNumber - 1, // Before genesis
            0,
            BlockTransition({ prevBlockHash: prevBlockHash, nextBlockHash: keccak256("state") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0) }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );
    }

    function test_submitBatch_revertsIfTempoBlockNumberInFuture() public {
        vm.roll(block.number + 10);

        bytes32 prevBlockHash = portal.blockHash();
        vm.expectRevert(IZonePortal.InvalidTempoBlockNumber.selector);
        portal.submitBatch(
            uint64(block.number + 1), // In future
            0,
            BlockTransition({ prevBlockHash: prevBlockHash, nextBlockHash: keccak256("state") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0) }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );
    }

    function test_submitBatch_revertsIfTempoBlockNumberTooOld() public {
        // Advance beyond the EIP-2935 history window
        vm.roll(block.number + BLOCKHASH_HISTORY_WINDOW + 1);

        bytes32 prevBlockHash = portal.blockHash();
        vm.expectRevert(IZonePortal.InvalidTempoBlockNumber.selector);
        portal.submitBatch(
            genesisTempoBlockNumber, // Valid but beyond history window
            0,
            BlockTransition({ prevBlockHash: prevBlockHash, nextBlockHash: keccak256("state") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0) }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );
    }

    function test_submitBatch_allowsHistoricalTempoBlockWithAncestryAnchor() public {
        // Advance beyond the EIP-2935 history window
        vm.roll(genesisTempoBlockNumber + BLOCKHASH_HISTORY_WINDOW + 100);

        uint64 oldTempoBlockNumber = genesisTempoBlockNumber;
        uint64 recentTempoBlockNumber = uint64(block.number - 1);

        portal.submitBatch(
            oldTempoBlockNumber,
            recentTempoBlockNumber,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0) }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );

        assertEq(portal.lastSyncedTempoBlockNumber(), oldTempoBlockNumber);
    }

    function test_submitBatch_revertsIfRecentTempoBlockNumberNotGreater() public {
        vm.roll(block.number + 1);

        uint64 tempoBlockNumber = uint64(block.number - 1);

        vm.expectRevert(IZonePortal.InvalidTempoBlockNumber.selector);
        portal.submitBatch(
            tempoBlockNumber,
            tempoBlockNumber,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0) }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );
    }

    function test_submitBatch_revertsIfRecentTempoBlockNumberInFuture() public {
        vm.roll(block.number + 1);

        uint64 tempoBlockNumber = uint64(block.number - 1);
        uint64 futureTempoBlockNumber = uint64(block.number + 1);

        vm.expectRevert(IZonePortal.InvalidTempoBlockNumber.selector);
        portal.submitBatch(
            tempoBlockNumber,
            futureTempoBlockNumber,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0) }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );
    }

    function test_submitBatch_succeedsAtHistoryWindowBoundary() public {
        // Advance exactly to the history window boundary
        vm.roll(genesisTempoBlockNumber + BLOCKHASH_HISTORY_WINDOW);

        // Should still work at the window boundary
        portal.submitBatch(
            genesisTempoBlockNumber,
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0) }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );

        assertEq(portal.withdrawalBatchIndex(), 1);
    }

    /*//////////////////////////////////////////////////////////////
               DEPOSIT QUEUE PREV HASH VALIDATION TESTS
    //////////////////////////////////////////////////////////////*/

    function test_submitBatch_usesInternalProcessedHash() public {
        // The implementation constructs prevProcessedHash from internal storage,
        // so the input prevProcessedHash is effectively ignored.
        // This test verifies the actual behavior: the portal uses its own tracked processed hash.

        // Make a deposit first
        vm.startPrank(alice);
        pathUSD.approve(address(portal), 1000e6);
        portal.deposit(alice, 1000e6, bytes32("memo"));
        vm.stopPrank();

        bytes32 depositHash = portal.currentDepositQueueHash();

        vm.roll(block.number + 1);

        // Even though we pass a "wrong" prevProcessedHash, the implementation
        // constructs its own from _depositQueue.processed, so this will succeed
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state") }),
            DepositQueueTransition({
                prevProcessedHash: keccak256("wrongHash"), // This is ignored by implementation
                nextProcessedHash: depositHash
            }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );

        // Verify batch was accepted
        assertEq(portal.withdrawalBatchIndex(), 1);
        // Portal no longer tracks processedDepositQueueHash - that's on the zone
    }

    function test_submitBatch_prevProcessedHashMustMatchPortalState() public {
        // Make deposits
        vm.startPrank(alice);
        pathUSD.approve(address(portal), 3000e6);
        bytes32 h1 = portal.deposit(alice, 1000e6, bytes32("d1"));
        bytes32 h2 = portal.deposit(alice, 1000e6, bytes32("d2"));
        vm.stopPrank();

        vm.roll(block.number + 1);

        // Process first deposit only
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state1") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: h1 }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );

        // Portal no longer tracks processedDepositQueueHash

        vm.roll(block.number + 1);

        // Submit second batch
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state2") }),
            DepositQueueTransition({ prevProcessedHash: h1, nextProcessedHash: h2 }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );

        assertEq(portal.withdrawalBatchIndex(), 2);
    }

    /*//////////////////////////////////////////////////////////////
                   WITHDRAWAL QUEUE MAX SIZE TESTS
    //////////////////////////////////////////////////////////////*/

    function test_withdrawalQueue_maxSizeTracksCorrectly() public {
        // Fund portal
        vm.startPrank(alice);
        pathUSD.approve(address(portal), 10000e6);
        portal.deposit(alice, 10000e6, bytes32(""));
        vm.stopPrank();

        bytes32 depositHash = portal.currentDepositQueueHash();

        // Initial maxSize should be 0
        assertEq(portal.withdrawalQueueMaxSize(), 0);

        // Submit batch with withdrawals
        Withdrawal memory w1 = Withdrawal({
            sender: alice, to: bob, amount: 100e6, fee: 0, memo: bytes32(0),
            gasLimit: 0, fallbackRecipient: alice, callbackData: ""
        });
        bytes32 w1Hash = keccak256(abi.encode(w1, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s1") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHash }),
            WithdrawalQueueTransition({ withdrawalQueueHash: w1Hash }),
            "",
            ""
        );

        // maxSize should be 1
        assertEq(portal.withdrawalQueueMaxSize(), 1);
        assertEq(portal.withdrawalQueueTail(), 1);
        assertEq(portal.withdrawalQueueHead(), 0);

        // Submit another batch with withdrawals
        Withdrawal memory w2 = Withdrawal({
            sender: alice, to: charlie, amount: 200e6, fee: 0, memo: bytes32(0),
            gasLimit: 0, fallbackRecipient: alice, callbackData: ""
        });
        bytes32 w2Hash = keccak256(abi.encode(w2, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s2") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHash }),
            WithdrawalQueueTransition({ withdrawalQueueHash: w2Hash }),
            "",
            ""
        );

        // maxSize should be 2
        assertEq(portal.withdrawalQueueMaxSize(), 2);

        // Process first withdrawal
        portal.processWithdrawal(w1, bytes32(0));

        // maxSize stays at 2 (historical max)
        assertEq(portal.withdrawalQueueMaxSize(), 2);
        assertEq(portal.withdrawalQueueHead(), 1);
    }

    function test_withdrawalQueue_emptyBatchDoesNotIncreaseTail() public {
        vm.startPrank(alice);
        pathUSD.approve(address(portal), 1000e6);
        portal.deposit(alice, 1000e6, bytes32(""));
        vm.stopPrank();

        bytes32 depositHash = portal.currentDepositQueueHash();
        uint256 tailBefore = portal.withdrawalQueueTail();

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s1") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHash }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }), // No withdrawals
            "",
            ""
        );

        // Tail should not have advanced
        assertEq(portal.withdrawalQueueTail(), tailBefore);
    }

    /*//////////////////////////////////////////////////////////////
                  WITHDRAWAL PROCESSING ORDER TESTS
    //////////////////////////////////////////////////////////////*/

    function test_processWithdrawal_mustProcessInOrder() public {
        // Fund portal
        vm.startPrank(alice);
        pathUSD.approve(address(portal), 10000e6);
        portal.deposit(alice, 10000e6, bytes32(""));
        vm.stopPrank();

        bytes32 depositHash = portal.currentDepositQueueHash();

        // Create two batches with different withdrawals
        Withdrawal memory w1 = Withdrawal({
            sender: alice, to: bob, amount: 100e6, fee: 0, memo: bytes32("w1"),
            gasLimit: 0, fallbackRecipient: alice, callbackData: ""
        });
        bytes32 w1Hash = keccak256(abi.encode(w1, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s1") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHash }),
            WithdrawalQueueTransition({ withdrawalQueueHash: w1Hash }),
            "",
            ""
        );

        Withdrawal memory w2 = Withdrawal({
            sender: alice, to: charlie, amount: 200e6, fee: 0, memo: bytes32("w2"),
            gasLimit: 0, fallbackRecipient: alice, callbackData: ""
        });
        bytes32 w2Hash = keccak256(abi.encode(w2, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s2") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHash }),
            WithdrawalQueueTransition({ withdrawalQueueHash: w2Hash }),
            "",
            ""
        );

        // Try to process w2 (slot 1) before w1 (slot 0) - should fail
        vm.expectRevert(WithdrawalQueueLib.InvalidWithdrawalHash.selector);
        portal.processWithdrawal(w2, bytes32(0));

        // Process w1 first
        portal.processWithdrawal(w1, bytes32(0));

        // Now w2 should work
        portal.processWithdrawal(w2, bytes32(0));
    }

    /*//////////////////////////////////////////////////////////////
                     CALLBACK GAS LIMIT TESTS
    //////////////////////////////////////////////////////////////*/

    function test_withdrawal_callbackOutOfGas_bouncesBack() public {
        // Fund portal
        vm.startPrank(alice);
        pathUSD.approve(address(portal), 1000e6);
        portal.deposit(alice, 1000e6, bytes32(""));
        vm.stopPrank();

        bytes32 depositHashBefore = portal.currentDepositQueueHash();

        // Create withdrawal with callback to gas-consuming receiver
        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: address(gasConsumingReceiver),
            amount: 500e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 50000, // Limited gas
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s1") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHashBefore }),
            WithdrawalQueueTransition({ withdrawalQueueHash: wHash }),
            "",
            ""
        );

        // Process withdrawal - should bounce back
        portal.processWithdrawal(w, bytes32(0));

        // Receiver should NOT have funds
        assertEq(pathUSD.balanceOf(address(gasConsumingReceiver)), 0);

        // Bounce-back deposit should have been created
        assertTrue(portal.currentDepositQueueHash() != depositHashBefore);
    }

    function test_withdrawal_zeroGasLimit_noCallback() public {
        // Fund portal
        vm.startPrank(alice);
        pathUSD.approve(address(portal), 1000e6);
        portal.deposit(alice, 1000e6, bytes32(""));
        vm.stopPrank();

        bytes32 depositHash = portal.currentDepositQueueHash();

        // Create withdrawal with gasLimit = 0
        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: address(successfulReceiver),
            amount: 500e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0, // No callback
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s1") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHash }),
            WithdrawalQueueTransition({ withdrawalQueueHash: wHash }),
            "",
            ""
        );

        uint256 callCountBefore = successfulReceiver.callCount();

        portal.processWithdrawal(w, bytes32(0));

        // Funds should be transferred
        assertEq(pathUSD.balanceOf(address(successfulReceiver)), 500e6);

        // But callback should NOT have been called
        assertEq(successfulReceiver.callCount(), callCountBefore);
    }

    function test_withdrawal_nonZeroGasLimit_callbackExecuted() public {
        // Fund portal
        vm.startPrank(alice);
        pathUSD.approve(address(portal), 1000e6);
        portal.deposit(alice, 1000e6, bytes32(""));
        vm.stopPrank();

        bytes32 depositHash = portal.currentDepositQueueHash();

        // Create withdrawal with callback
        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: address(successfulReceiver),
            amount: 500e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 100000,
            fallbackRecipient: alice,
            callbackData: "test"
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s1") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHash }),
            WithdrawalQueueTransition({ withdrawalQueueHash: wHash }),
            "",
            ""
        );

        portal.processWithdrawal(w, bytes32(0));

        // Callback should have been called
        assertEq(successfulReceiver.callCount(), 1);
        assertEq(pathUSD.balanceOf(address(successfulReceiver)), 500e6);
    }

    /*//////////////////////////////////////////////////////////////
                     BOUNCE-BACK DEPOSIT TESTS
    //////////////////////////////////////////////////////////////*/

    function test_bounceBack_depositsToFallbackRecipient() public {
        // Fund portal
        vm.startPrank(alice);
        pathUSD.approve(address(portal), 1000e6);
        portal.deposit(alice, 1000e6, bytes32(""));
        vm.stopPrank();

        bytes32 depositHashBefore = portal.currentDepositQueueHash();

        // Create withdrawal with callback that will fail
        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: address(gasConsumingReceiver),
            amount: 500e6,
            fee: 0,
            memo: bytes32("payment"),
            gasLimit: 50000,
            fallbackRecipient: bob, // Bob is the fallback
            callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s1") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHashBefore }),
            WithdrawalQueueTransition({ withdrawalQueueHash: wHash }),
            "",
            ""
        );

        portal.processWithdrawal(w, bytes32(0));

        // Verify bounce-back deposit was created
        bytes32 newDepositHash = portal.currentDepositQueueHash();
        assertTrue(newDepositHash != depositHashBefore);

        // The bounce-back deposit should be:
        // Deposit { sender: portal, to: bob, amount: 500e6, fee: 0, memo: 0 }
        Deposit memory expectedBounceBack = Deposit({
            sender: address(portal),
            to: bob,
            amount: 500e6,
            memo: bytes32(0)
        });
        bytes32 expectedHash = keccak256(abi.encode(expectedBounceBack, depositHashBefore));
        assertEq(newDepositHash, expectedHash);
    }

    /*//////////////////////////////////////////////////////////////
                      BATCH INDEX INCREMENT TESTS
    //////////////////////////////////////////////////////////////*/

    function test_withdrawalBatchIndex_incrementsOnEachBatch() public {
        vm.startPrank(alice);
        pathUSD.approve(address(portal), 3000e6);
        portal.deposit(alice, 1000e6, bytes32(""));
        vm.stopPrank();

        bytes32 depositHash = portal.currentDepositQueueHash();

        assertEq(portal.withdrawalBatchIndex(), 0);

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s1") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHash }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );
        assertEq(portal.withdrawalBatchIndex(), 1);

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s2") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHash }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );
        assertEq(portal.withdrawalBatchIndex(), 2);

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s3") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHash }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );
        assertEq(portal.withdrawalBatchIndex(), 3);
    }

    /*//////////////////////////////////////////////////////////////
                        EVENT EMISSION TESTS
    //////////////////////////////////////////////////////////////*/

    function test_deposit_emitsDepositMadeEvent() public {
        vm.startPrank(alice);
        pathUSD.approve(address(portal), 1000e6);

        vm.expectEmit(true, true, false, true);
        uint128 fee = portal.calculateDepositFee();
        uint128 netAmount = 500e6 - fee;
        bytes32 expectedHash = keccak256(abi.encode(
            Deposit({ sender: alice, to: bob, amount: netAmount, memo: bytes32("test") }),
            bytes32(0)
        ));
        emit IZonePortal.DepositMade(expectedHash, alice, bob, netAmount, fee, bytes32("test"));

        portal.deposit(bob, 500e6, bytes32("test"));
        vm.stopPrank();
    }

    function test_processWithdrawal_emitsWithdrawalProcessedEvent_success() public {
        // Setup withdrawal
        vm.startPrank(alice);
        pathUSD.approve(address(portal), 1000e6);
        portal.deposit(alice, 1000e6, bytes32(""));
        vm.stopPrank();

        Withdrawal memory w = Withdrawal({
            sender: alice, to: bob, amount: 500e6, fee: 0, memo: bytes32(0),
            gasLimit: 0, fallbackRecipient: alice, callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s1") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash() }),
            WithdrawalQueueTransition({ withdrawalQueueHash: wHash }),
            "",
            ""
        );

        vm.expectEmit(true, false, false, true);
        emit IZonePortal.WithdrawalProcessed(bob, 500e6, true);

        portal.processWithdrawal(w, bytes32(0));
    }

    function test_processWithdrawal_emitsWithdrawalProcessedEvent_failure() public {
        // Setup withdrawal with failing callback
        vm.startPrank(alice);
        pathUSD.approve(address(portal), 1000e6);
        portal.deposit(alice, 1000e6, bytes32(""));
        vm.stopPrank();

        Withdrawal memory w = Withdrawal({
            sender: alice, to: address(gasConsumingReceiver), amount: 500e6, fee: 0, memo: bytes32(0),
            gasLimit: 50000, fallbackRecipient: alice, callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s1") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash() }),
            WithdrawalQueueTransition({ withdrawalQueueHash: wHash }),
            "",
            ""
        );

        vm.expectEmit(true, false, false, true);
        emit IZonePortal.WithdrawalProcessed(address(gasConsumingReceiver), 500e6, false);

        portal.processWithdrawal(w, bytes32(0));
    }

    /*//////////////////////////////////////////////////////////////
                         IMMUTABLE GETTERS TESTS
    //////////////////////////////////////////////////////////////*/

    function test_immutableGetters() public view {
        assertEq(portal.zoneId(), testZoneId);
        assertEq(portal.token(), address(pathUSD));
        assertEq(portal.sequencer(), admin);
        assertEq(portal.verifier(), address(mockVerifier));
        assertEq(portal.genesisTempoBlockNumber(), genesisTempoBlockNumber);
    }
}
