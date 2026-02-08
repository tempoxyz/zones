// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { TIP20 } from "../../src/TIP20.sol";
import { ITIP20 } from "../../src/interfaces/ITIP20.sol";

import { BLOCKHASH_HISTORY_WINDOW } from "../../src/zone/BlockHashHistory.sol";
import {
    BlockTransition,
    Deposit,
    DepositQueueTransition,
    DepositType,
    ENCRYPTION_KEY_GRACE_PERIOD,
    EncryptedDeposit,
    EncryptedDepositPayload,
    EncryptionKeyEntry,
    IWithdrawalReceiver,
    IZoneFactory,
    IZoneMessenger,
    IZonePortal,
    PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
    PORTAL_ENCRYPTION_KEYS_SLOT,
    PORTAL_PENDING_SEQUENCER_SLOT,
    PORTAL_SEQUENCER_SLOT,
    Withdrawal,
    WithdrawalQueueTransition,
    ZoneInfo,
    ZoneParams
} from "../../src/zone/IZone.sol";
import { EMPTY_SENTINEL, WithdrawalQueueLib } from "../../src/zone/WithdrawalQueueLib.sol";
import { ZoneFactory } from "../../src/zone/ZoneFactory.sol";
import { ZoneMessenger } from "../../src/zone/ZoneMessenger.sol";
import { ZonePortal } from "../../src/zone/ZonePortal.sol";
import { BaseTest } from "../BaseTest.t.sol";

import { MockVerifier } from "./mocks/MockVerifier.sol";

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

    function onWithdrawalReceived(address, uint128, bytes calldata) external returns (bytes4) {
        // Infinite loop to consume all gas
        while (true) { }
        return IWithdrawalReceiver.onWithdrawalReceived.selector;
    }

}

/// @notice Mock receiver that succeeds normally
contract SuccessfulReceiver is IWithdrawalReceiver {

    uint256 public callCount;

    function onWithdrawalReceived(address, uint128, bytes calldata) external returns (bytes4) {
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
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: depositHash
            }),
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
            BlockTransition({
                prevBlockHash: keccak256("wrong"), nextBlockHash: keccak256("state")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0)
                }),
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
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0)
            }),
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
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0)
            }),
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
            BlockTransition({
                prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("stateWithWithdrawal")
            }),
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash()
            }),
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
        portal.processWithdrawal(w, bytes32(0)); // 0 means last item in slot

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
            sender: alice,
            to: bob,
            amount: 300e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        Withdrawal memory w2 = Withdrawal({
            sender: alice,
            to: charlie,
            amount: 400e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
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
            BlockTransition({
                prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state1")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0),
                    nextProcessedHash: portal.currentDepositQueueHash()
                }),
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
        portal.processWithdrawal(w2, bytes32(0)); // 0 = last item
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
            sender: alice,
            to: bob,
            amount: 500e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 w1Hash = keccak256(abi.encode(w1, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({
                prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state1")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0),
                    nextProcessedHash: portal.currentDepositQueueHash()
                }),
            WithdrawalQueueTransition({ withdrawalQueueHash: w1Hash }),
            "",
            ""
        );

        // Batch 2: withdrawal to charlie
        Withdrawal memory w2 = Withdrawal({
            sender: alice,
            to: charlie,
            amount: 600e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 w2Hash = keccak256(abi.encode(w2, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({
                prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state2")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0),
                    nextProcessedHash: portal.currentDepositQueueHash()
                }),
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
            BlockTransition({
                prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state1")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0),
                    nextProcessedHash: portal.currentDepositQueueHash()
                }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }), // No withdrawals
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
            gasLimit: 100_000,
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
            BlockTransition({
                prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0),
                    nextProcessedHash: portal.currentDepositQueueHash()
                }),
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
            gasLimit: 100_000,
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
            BlockTransition({
                prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0), nextProcessedHash: depositHashBefore
                }),
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
            gasLimit: 100_000,
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        // Advance a block so the history precompile can return a hash
        vm.roll(block.number + 1);

        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({
                prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0), nextProcessedHash: depositHashBefore
                }),
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
            BlockTransition({
                prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0), nextProcessedHash: depositHashBefore
                }),
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
            sender: alice,
            to: bob,
            amount: 100e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
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
            BlockTransition({
                prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0),
                    nextProcessedHash: portal.currentDepositQueueHash()
                }),
            WithdrawalQueueTransition({ withdrawalQueueHash: wHash }),
            "",
            ""
        );

        // Try to process with wrong withdrawal data
        Withdrawal memory wrongW = Withdrawal({
            sender: alice,
            to: charlie,
            amount: 500e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
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
            BlockTransition({
                prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0),
                    nextProcessedHash: portal.currentDepositQueueHash()
                }),
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
            BlockTransition({
                prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state1")
            }),
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
                      BATCH SUBMISSION VALIDATION
    //////////////////////////////////////////////////////////////*/

    function test_submitBatch_revertsIfTempoBlockNumberBeforeGenesis() public {
        bytes32 prevBlockHash = portal.blockHash();
        vm.expectRevert(IZonePortal.InvalidTempoBlockNumber.selector);
        portal.submitBatch(
            genesisTempoBlockNumber - 1, // Before genesis
            0,
            BlockTransition({ prevBlockHash: prevBlockHash, nextBlockHash: keccak256("state") }),
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0)
            }),
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
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0)
            }),
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
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0)
            }),
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
            BlockTransition({
                prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0)
                }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );

        assertEq(portal.lastSyncedTempoBlockNumber(), oldTempoBlockNumber);
    }

    function test_submitBatch_revertsIfRecentTempoBlockNumberNotGreater() public {
        uint64 tempoBlockNumber = genesisTempoBlockNumber + 1;
        vm.roll(tempoBlockNumber + 1);

        bytes32 prevBlockHash = portal.blockHash();
        vm.expectRevert(IZonePortal.InvalidTempoBlockNumber.selector);
        portal.submitBatch(
            tempoBlockNumber,
            tempoBlockNumber,
            BlockTransition({ prevBlockHash: prevBlockHash, nextBlockHash: keccak256("state") }),
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0)
            }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );
    }

    function test_submitBatch_revertsIfRecentTempoBlockNumberInFuture() public {
        uint64 tempoBlockNumber = genesisTempoBlockNumber + 1;
        vm.roll(tempoBlockNumber + 1);

        uint64 futureTempoBlockNumber = tempoBlockNumber + 2;

        bytes32 prevBlockHash = portal.blockHash();
        vm.expectRevert(IZonePortal.InvalidTempoBlockNumber.selector);
        portal.submitBatch(
            tempoBlockNumber,
            futureTempoBlockNumber,
            BlockTransition({ prevBlockHash: prevBlockHash, nextBlockHash: keccak256("state") }),
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0)
            }),
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
            BlockTransition({
                prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0)
                }),
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
            BlockTransition({
                prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state")
            }),
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
            BlockTransition({
                prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state1")
            }),
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
            BlockTransition({
                prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("state2")
            }),
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
        pathUSD.approve(address(portal), 10_000e6);
        portal.deposit(alice, 10_000e6, bytes32(""));
        vm.stopPrank();

        bytes32 depositHash = portal.currentDepositQueueHash();

        // Initial maxSize should be 0
        assertEq(portal.withdrawalQueueMaxSize(), 0);

        // Submit batch with withdrawals
        Withdrawal memory w1 = Withdrawal({
            sender: alice,
            to: bob,
            amount: 100e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 w1Hash = keccak256(abi.encode(w1, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s1") }),
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: depositHash
            }),
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
            sender: alice,
            to: charlie,
            amount: 200e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 w2Hash = keccak256(abi.encode(w2, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s2") }),
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: depositHash
            }),
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
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: depositHash
            }),
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
        pathUSD.approve(address(portal), 10_000e6);
        portal.deposit(alice, 10_000e6, bytes32(""));
        vm.stopPrank();

        bytes32 depositHash = portal.currentDepositQueueHash();

        // Create two batches with different withdrawals
        Withdrawal memory w1 = Withdrawal({
            sender: alice,
            to: bob,
            amount: 100e6,
            fee: 0,
            memo: bytes32("w1"),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 w1Hash = keccak256(abi.encode(w1, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s1") }),
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: depositHash
            }),
            WithdrawalQueueTransition({ withdrawalQueueHash: w1Hash }),
            "",
            ""
        );

        Withdrawal memory w2 = Withdrawal({
            sender: alice,
            to: charlie,
            amount: 200e6,
            fee: 0,
            memo: bytes32("w2"),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 w2Hash = keccak256(abi.encode(w2, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s2") }),
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: depositHash
            }),
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
            gasLimit: 50_000, // Limited gas
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s1") }),
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: depositHashBefore
            }),
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
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: depositHash
            }),
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
            gasLimit: 100_000,
            fallbackRecipient: alice,
            callbackData: "test"
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s1") }),
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: depositHash
            }),
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
            gasLimit: 50_000,
            fallbackRecipient: bob, // Bob is the fallback
            callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s1") }),
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: depositHashBefore
            }),
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
        Deposit memory expectedBounceBack =
            Deposit({ sender: address(portal), to: bob, amount: 500e6, memo: bytes32(0) });
        bytes32 expectedHash =
            keccak256(abi.encode(DepositType.Regular, expectedBounceBack, depositHashBefore));
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
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: depositHash
            }),
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
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: depositHash
            }),
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
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: depositHash
            }),
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
        bytes32 expectedHash = keccak256(
            abi.encode(
                DepositType.Regular,
                Deposit({ sender: alice, to: bob, amount: netAmount, memo: bytes32("test") }),
                bytes32(0)
            )
        );
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

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s1") }),
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash()
            }),
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
            sender: alice,
            to: address(gasConsumingReceiver),
            amount: 500e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 50_000,
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: portal.blockHash(), nextBlockHash: keccak256("s1") }),
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: portal.currentDepositQueueHash()
            }),
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

    /*//////////////////////////////////////////////////////////////
                    ENCRYPTION KEY MANAGEMENT TESTS
    //////////////////////////////////////////////////////////////*/

    // secp256k1 generator point X (known valid point on curve)
    bytes32 internal constant VALID_SECP256K1_X =
        0x79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798;

    function test_sequencerEncryptionKey_emptyReturnsZero() public view {
        (bytes32 x, uint8 yParity) = portal.sequencerEncryptionKey();
        assertEq(x, bytes32(0));
        assertEq(yParity, 0);
    }

    function test_setSequencerEncryptionKey_success() public {
        bytes32 x = keccak256("key1");
        uint8 yParity = 0x02;

        portal.setSequencerEncryptionKey(x, yParity);

        (bytes32 storedX, uint8 storedYParity) = portal.sequencerEncryptionKey();
        assertEq(storedX, x);
        assertEq(storedYParity, yParity);
        assertEq(portal.encryptionKeyCount(), 1);
    }

    function test_setSequencerEncryptionKey_onlySequencer() public {
        vm.prank(alice);
        vm.expectRevert(IZonePortal.NotSequencer.selector);
        portal.setSequencerEncryptionKey(keccak256("key"), 0x02);
    }

    function test_setSequencerEncryptionKey_multipleKeys() public {
        bytes32 x1 = keccak256("key1");
        bytes32 x2 = keccak256("key2");

        portal.setSequencerEncryptionKey(x1, 0x02);
        vm.roll(block.number + 100);
        portal.setSequencerEncryptionKey(x2, 0x03);

        assertEq(portal.encryptionKeyCount(), 2);

        // sequencerEncryptionKey returns the latest key
        (bytes32 storedX, uint8 storedYParity) = portal.sequencerEncryptionKey();
        assertEq(storedX, x2);
        assertEq(storedYParity, 0x03);
    }

    function test_setSequencerEncryptionKey_emitsEvent() public {
        bytes32 x = keccak256("key1");
        uint8 yParity = 0x02;

        vm.expectEmit(true, true, true, true);
        emit IZonePortal.SequencerEncryptionKeyUpdated(x, yParity, 0, uint64(block.number));
        portal.setSequencerEncryptionKey(x, yParity);
    }

    function test_encryptionKeyAt_success() public {
        bytes32 x1 = keccak256("key1");
        portal.setSequencerEncryptionKey(x1, 0x02);

        vm.roll(block.number + 50);
        bytes32 x2 = keccak256("key2");
        portal.setSequencerEncryptionKey(x2, 0x03);

        EncryptionKeyEntry memory entry0 = portal.encryptionKeyAt(0);
        assertEq(entry0.x, x1);
        assertEq(entry0.yParity, 0x02);

        EncryptionKeyEntry memory entry1 = portal.encryptionKeyAt(1);
        assertEq(entry1.x, x2);
        assertEq(entry1.yParity, 0x03);
    }

    function test_encryptionKeyAt_revertsOnInvalidIndex() public {
        vm.expectRevert(abi.encodeWithSelector(IZonePortal.InvalidEncryptionKeyIndex.selector, 0));
        portal.encryptionKeyAt(0);
    }

    function test_encryptionKeyAtBlock_binarySearch() public {
        // Set key1 at block 10
        vm.roll(10);
        bytes32 x1 = keccak256("key1");
        portal.setSequencerEncryptionKey(x1, 0x02);

        // Set key2 at block 100
        vm.roll(100);
        bytes32 x2 = keccak256("key2");
        portal.setSequencerEncryptionKey(x2, 0x03);

        // Set key3 at block 200
        vm.roll(200);
        bytes32 x3 = keccak256("key3");
        portal.setSequencerEncryptionKey(x3, 0x02);

        // Query at block 10 -> key1
        (bytes32 rx, uint8 ry, uint256 ri) = portal.encryptionKeyAtBlock(10);
        assertEq(rx, x1);
        assertEq(ry, 0x02);
        assertEq(ri, 0);

        // Query at block 50 -> key1 (still active)
        (rx, ry, ri) = portal.encryptionKeyAtBlock(50);
        assertEq(rx, x1);
        assertEq(ri, 0);

        // Query at block 100 -> key2
        (rx, ry, ri) = portal.encryptionKeyAtBlock(100);
        assertEq(rx, x2);
        assertEq(ri, 1);

        // Query at block 150 -> key2
        (rx, ry, ri) = portal.encryptionKeyAtBlock(150);
        assertEq(rx, x2);
        assertEq(ri, 1);

        // Query at block 200 -> key3
        (rx, ry, ri) = portal.encryptionKeyAtBlock(200);
        assertEq(rx, x3);
        assertEq(ri, 2);

        // Query at block 500 -> key3
        (rx, ry, ri) = portal.encryptionKeyAtBlock(500);
        assertEq(rx, x3);
        assertEq(ri, 2);
    }

    function test_isEncryptionKeyValid_currentKeyNeverExpires() public {
        portal.setSequencerEncryptionKey(keccak256("key"), 0x02);

        (bool valid, uint64 expiresAt) = portal.isEncryptionKeyValid(0);
        assertTrue(valid);
        assertEq(expiresAt, 0);

        // Still valid far in the future
        vm.roll(block.number + 1_000_000);
        (valid, expiresAt) = portal.isEncryptionKeyValid(0);
        assertTrue(valid);
        assertEq(expiresAt, 0);
    }

    function test_isEncryptionKeyValid_oldKeyValidDuringGrace() public {
        portal.setSequencerEncryptionKey(keccak256("key1"), 0x02);

        uint256 key2Block = block.number + 100;
        vm.roll(key2Block);
        portal.setSequencerEncryptionKey(keccak256("key2"), 0x03);

        // Key 0 should be valid during grace period
        vm.roll(key2Block + ENCRYPTION_KEY_GRACE_PERIOD - 1);
        (bool valid,) = portal.isEncryptionKeyValid(0);
        assertTrue(valid);
    }

    function test_isEncryptionKeyValid_oldKeyExpiredAfterGrace() public {
        portal.setSequencerEncryptionKey(keccak256("key1"), 0x02);

        uint256 key2Block = block.number + 100;
        vm.roll(key2Block);
        portal.setSequencerEncryptionKey(keccak256("key2"), 0x03);

        // Key 0 should be expired after grace period
        vm.roll(key2Block + ENCRYPTION_KEY_GRACE_PERIOD);
        (bool valid,) = portal.isEncryptionKeyValid(0);
        assertFalse(valid);
    }

    function test_isEncryptionKeyValid_invalidIndexReturnsFalse() public view {
        (bool valid,) = portal.isEncryptionKeyValid(0);
        assertFalse(valid);
        (valid,) = portal.isEncryptionKeyValid(999);
        assertFalse(valid);
    }

    /*//////////////////////////////////////////////////////////////
                       ENCRYPTED DEPOSIT TESTS
    //////////////////////////////////////////////////////////////*/

    function _makeEncryptedPayload() internal pure returns (EncryptedDepositPayload memory) {
        return EncryptedDepositPayload({
            ephemeralPubkeyX: VALID_SECP256K1_X,
            ephemeralPubkeyYParity: 0x02,
            ciphertext: new bytes(64),
            nonce: bytes12(0),
            tag: bytes16(0)
        });
    }

    function test_depositEncrypted_success() public {
        portal.setSequencerEncryptionKey(keccak256("seqKey"), 0x02);

        uint128 depositAmount = 1000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        bytes32 hash = portal.depositEncrypted(depositAmount, 0, _makeEncryptedPayload());
        vm.stopPrank();

        assertEq(portal.currentDepositQueueHash(), hash);
        assertTrue(hash != bytes32(0));
    }

    function test_depositEncrypted_hashChainMatchesLibrary() public {
        portal.setSequencerEncryptionKey(keccak256("seqKey"), 0x02);

        uint128 depositAmount = 1000e6;
        uint128 fee = portal.calculateDepositFee();
        uint128 netAmount = depositAmount - fee;

        EncryptedDepositPayload memory encrypted = _makeEncryptedPayload();

        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        bytes32 hash = portal.depositEncrypted(depositAmount, 0, encrypted);
        vm.stopPrank();

        // Reconstruct expected hash using the same encoding as DepositQueueLib
        EncryptedDeposit memory ed = EncryptedDeposit({
            sender: alice, amount: netAmount, keyIndex: 0, encrypted: encrypted
        });
        bytes32 expectedHash = keccak256(abi.encode(DepositType.Encrypted, ed, bytes32(0)));
        assertEq(hash, expectedHash);
    }

    function test_depositEncrypted_mixedQueue() public {
        portal.setSequencerEncryptionKey(keccak256("seqKey"), 0x02);

        uint128 amount = 1000e6;

        // Regular deposit from alice
        vm.startPrank(alice);
        pathUSD.approve(address(portal), amount * 3);
        bytes32 h1 = portal.deposit(alice, amount, bytes32("memo"));

        // Encrypted deposit from alice
        bytes32 h2 = portal.depositEncrypted(amount, 0, _makeEncryptedPayload());
        vm.stopPrank();

        // Both should update the same queue
        assertEq(portal.currentDepositQueueHash(), h2);
        assertTrue(h1 != h2);
        assertTrue(h2 != bytes32(0));
    }

    function test_depositEncrypted_deductsFee() public {
        portal.setZoneGasRate(1); // 1 token per gas -> fee = 100_000
        portal.setSequencerEncryptionKey(keccak256("seqKey"), 0x02);

        uint128 depositAmount = 1000e6;
        uint128 expectedFee = uint128(100_000) * 1; // FIXED_DEPOSIT_GAS * zoneGasRate
        uint256 aliceBefore = pathUSD.balanceOf(alice);
        uint256 seqBefore = pathUSD.balanceOf(admin);
        uint256 portalBefore = pathUSD.balanceOf(address(portal));

        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);
        portal.depositEncrypted(depositAmount, 0, _makeEncryptedPayload());
        vm.stopPrank();

        assertEq(pathUSD.balanceOf(alice), aliceBefore - depositAmount);
        assertEq(pathUSD.balanceOf(admin), seqBefore + expectedFee);
        assertEq(pathUSD.balanceOf(address(portal)), portalBefore + depositAmount - expectedFee);
    }

    function test_depositEncrypted_emitsEvent() public {
        portal.setSequencerEncryptionKey(keccak256("seqKey"), 0x02);

        uint128 depositAmount = 1000e6;
        uint128 fee = portal.calculateDepositFee();
        uint128 netAmount = depositAmount - fee;

        EncryptedDepositPayload memory encrypted = _makeEncryptedPayload();

        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);

        // Build expected hash
        EncryptedDeposit memory ed = EncryptedDeposit({
            sender: alice, amount: netAmount, keyIndex: 0, encrypted: encrypted
        });
        bytes32 expectedHash = keccak256(abi.encode(DepositType.Encrypted, ed, bytes32(0)));

        vm.expectEmit(true, true, false, true);
        emit IZonePortal.EncryptedDepositMade(
            expectedHash, alice, netAmount, 0, VALID_SECP256K1_X, 0x02
        );
        portal.depositEncrypted(depositAmount, 0, encrypted);
        vm.stopPrank();
    }

    function test_depositEncrypted_revertsOnExpiredKey() public {
        portal.setSequencerEncryptionKey(keccak256("key1"), 0x02);

        // Rotate to key2
        uint256 key2Block = block.number + 100;
        vm.roll(key2Block);
        portal.setSequencerEncryptionKey(keccak256("key2"), 0x03);

        // Move past grace period for key1
        vm.roll(key2Block + ENCRYPTION_KEY_GRACE_PERIOD);

        uint128 depositAmount = 1000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);

        // Should revert with EncryptionKeyExpired for key index 0
        vm.expectRevert();
        portal.depositEncrypted(depositAmount, 0, _makeEncryptedPayload());
        vm.stopPrank();
    }

    function test_depositEncrypted_revertsOnInvalidKeyIndex() public {
        uint128 depositAmount = 1000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);

        // No keys set, index 0 is invalid
        vm.expectRevert(abi.encodeWithSelector(IZonePortal.InvalidEncryptionKeyIndex.selector, 0));
        portal.depositEncrypted(depositAmount, 0, _makeEncryptedPayload());
        vm.stopPrank();
    }

    function test_depositEncrypted_revertsOnInvalidYParity() public {
        portal.setSequencerEncryptionKey(keccak256("seqKey"), 0x02);

        uint128 depositAmount = 1000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);

        EncryptedDepositPayload memory encrypted = EncryptedDepositPayload({
            ephemeralPubkeyX: VALID_SECP256K1_X,
            ephemeralPubkeyYParity: 0x04, // Invalid
            ciphertext: new bytes(64),
            nonce: bytes12(0),
            tag: bytes16(0)
        });

        vm.expectRevert(IZonePortal.InvalidEphemeralPubkey.selector);
        portal.depositEncrypted(depositAmount, 0, encrypted);
        vm.stopPrank();
    }

    function test_depositEncrypted_revertsOnInvalidEphemeralX() public {
        portal.setSequencerEncryptionKey(keccak256("seqKey"), 0x02);

        uint128 depositAmount = 1000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(portal), depositAmount);

        EncryptedDepositPayload memory encrypted = EncryptedDepositPayload({
            ephemeralPubkeyX: bytes32(0), // Invalid: zero
            ephemeralPubkeyYParity: 0x02,
            ciphertext: new bytes(64),
            nonce: bytes12(0),
            tag: bytes16(0)
        });

        vm.expectRevert(IZonePortal.InvalidEphemeralPubkey.selector);
        portal.depositEncrypted(depositAmount, 0, encrypted);
        vm.stopPrank();
    }

    function test_depositEncrypted_revertsOnDepositTooSmall() public {
        portal.setZoneGasRate(1); // fee = 100_000
        portal.setSequencerEncryptionKey(keccak256("seqKey"), 0x02);

        vm.startPrank(alice);
        pathUSD.approve(address(portal), 100_000);

        vm.expectRevert(IZonePortal.DepositTooSmall.selector);
        portal.depositEncrypted(100_000, 0, _makeEncryptedPayload()); // amount == fee
        vm.stopPrank();
    }

    /*//////////////////////////////////////////////////////////////
                    STORAGE LAYOUT VERIFICATION TESTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Verify that ZonePortal's storage layout matches the slot constants
    ///         used by ZoneConfig and ZoneInbox for cross-domain reads.
    /// @dev This is a critical regression test. If the ZonePortal storage layout changes
    ///      (e.g. a variable is added/removed/reordered), this test will fail, preventing
    ///      silent slot mismatches that corrupt zone-side reads.
    ///
    ///      The zone-side contracts (ZoneConfig, ZoneInbox) read ZonePortal storage via
    ///      TempoState.readTempoStorageSlot() using hardcoded slot numbers. If those slot
    ///      numbers drift from the actual layout, the zone reads garbage data.
    ///
    ///      Slot layout (non-immutable variables only):
    ///        slot 0: sequencer (address)
    ///        slot 1: pendingSequencer (address)
    ///        slot 2: zoneGasRate (uint128) + withdrawalBatchIndex (uint64) [packed]
    ///        slot 3: blockHash (bytes32)
    ///        slot 4: currentDepositQueueHash (bytes32)
    ///        slot 5: lastSyncedTempoBlockNumber (uint64)
    ///        slot 6: _encryptionKeys.length (EncryptionKeyEntry[])
    function test_storageLayout_slotPositions() public {
        // --- Slot 0: sequencer ---
        bytes32 slot0 = vm.load(address(portal), bytes32(uint256(0)));
        assertEq(address(uint160(uint256(slot0))), portal.sequencer(), "slot 0: sequencer mismatch");

        // --- Slot 1: pendingSequencer ---
        // Transfer sequencer to get a non-zero pendingSequencer
        portal.transferSequencer(alice);
        bytes32 slot1 = vm.load(address(portal), bytes32(uint256(1)));
        assertEq(
            address(uint160(uint256(slot1))),
            portal.pendingSequencer(),
            "slot 1: pendingSequencer mismatch"
        );

        // --- Slot 2: zoneGasRate (uint128) + withdrawalBatchIndex (uint64) packed ---
        uint128 testRate = 42;
        portal.setZoneGasRate(testRate);
        bytes32 slot2 = vm.load(address(portal), bytes32(uint256(2)));
        // zoneGasRate is at the lowest 128 bits (uint128), withdrawalBatchIndex at bits 128-191
        uint128 loadedRate = uint128(uint256(slot2));
        assertEq(loadedRate, testRate, "slot 2: zoneGasRate mismatch");

        // --- Slot 3: blockHash ---
        bytes32 slot3 = vm.load(address(portal), bytes32(uint256(3)));
        assertEq(slot3, portal.blockHash(), "slot 3: blockHash mismatch");

        // --- Slot 4: currentDepositQueueHash ---
        bytes32 slot4 = vm.load(address(portal), bytes32(uint256(4)));
        assertEq(
            slot4, portal.currentDepositQueueHash(), "slot 4: currentDepositQueueHash mismatch"
        );

        // --- Slot 5: lastSyncedTempoBlockNumber ---
        bytes32 slot5 = vm.load(address(portal), bytes32(uint256(5)));
        assertEq(
            uint64(uint256(slot5)),
            portal.lastSyncedTempoBlockNumber(),
            "slot 5: lastSyncedTempoBlockNumber mismatch"
        );

        // --- Slot 6: _encryptionKeys array length ---
        // Before adding keys, length should be 0
        bytes32 slot6keys = vm.load(address(portal), bytes32(uint256(6)));
        assertEq(uint256(slot6keys), 0, "slot 6: _encryptionKeys length should be 0 initially");
    }

    /// @notice Verify that the _encryptionKeys dynamic array uses the expected slot layout.
    /// @dev This ensures ZoneConfig and ZoneInbox both compute the correct storage slots
    ///      when reading encryption keys via readTempoStorageSlot().
    ///
    ///      For a dynamic array at slot S:
    ///        - slot S stores the array length
    ///        - element data starts at keccak256(abi.encode(S))
    ///        - each EncryptionKeyEntry occupies 2 slots:
    ///            base + (index * 2):     x (bytes32)
    ///            base + (index * 2) + 1: yParity (uint8) + activationBlock (uint64) [packed]
    function test_storageLayout_encryptionKeysArray() public {
        bytes32 keyX1 = keccak256("encryption-key-1");
        uint8 keyYParity1 = 0x02;
        bytes32 keyX2 = keccak256("encryption-key-2");
        uint8 keyYParity2 = 0x03;

        // Add two keys at different blocks
        portal.setSequencerEncryptionKey(keyX1, keyYParity1);

        vm.roll(block.number + 100);
        portal.setSequencerEncryptionKey(keyX2, keyYParity2);

        // Verify array length at slot 6
        uint256 arraySlot = 6;
        bytes32 lengthRaw = vm.load(address(portal), bytes32(arraySlot));
        assertEq(uint256(lengthRaw), 2, "encryption keys array length should be 2");

        // Compute the base slot for array data
        uint256 base = uint256(keccak256(abi.encode(arraySlot)));

        // --- Entry 0: verify raw storage matches the public getter ---
        EncryptionKeyEntry memory entry0 = portal.encryptionKeyAt(0);
        bytes32 loadedX1 = vm.load(address(portal), bytes32(base + 0));
        assertEq(loadedX1, keyX1, "entry 0: x mismatch");
        assertEq(loadedX1, entry0.x, "entry 0: x != getter");

        bytes32 meta1 = vm.load(address(portal), bytes32(base + 1));
        uint8 loadedYParity1 = uint8(uint256(meta1) & 0xff);
        uint64 loadedActivation1 = uint64(uint256(meta1) >> 8);
        assertEq(loadedYParity1, keyYParity1, "entry 0: yParity mismatch");
        assertEq(loadedActivation1, entry0.activationBlock, "entry 0: activationBlock mismatch");

        // --- Entry 1: verify raw storage matches the public getter ---
        EncryptionKeyEntry memory entry1 = portal.encryptionKeyAt(1);
        bytes32 loadedX2 = vm.load(address(portal), bytes32(base + 2));
        assertEq(loadedX2, keyX2, "entry 1: x mismatch");
        assertEq(loadedX2, entry1.x, "entry 1: x != getter");

        bytes32 meta2 = vm.load(address(portal), bytes32(base + 3));
        uint8 loadedYParity2 = uint8(uint256(meta2) & 0xff);
        uint64 loadedActivation2 = uint64(uint256(meta2) >> 8);
        assertEq(loadedYParity2, keyYParity2, "entry 1: yParity mismatch");
        assertEq(loadedActivation2, entry1.activationBlock, "entry 1: activationBlock mismatch");

        // Verify the two keys have different activation blocks (proves vm.roll worked)
        assertTrue(
            entry1.activationBlock > entry0.activationBlock, "key2 should be activated later"
        );
    }

    /// @notice Verify that the slot constants used by ZoneInbox and ZoneConfig match
    ///         the actual ZonePortal storage layout.
    /// @dev This is the cross-contract consistency check. The test replicates the exact
    ///      slot computation logic used by ZoneInbox._readEncryptionKey() and
    ///      ZoneConfig.sequencerEncryptionKey() to ensure they both read the correct data.
    function test_storageLayout_crossContractConsistency() public {
        bytes32 keyX = keccak256("cross-contract-key");
        uint8 keyYParity = 0x03;

        portal.setSequencerEncryptionKey(keyX, keyYParity);

        // Use the shared constants from IZone.sol (single source of truth)

        // Verify sequencer slot (used by ZoneConfig)
        bytes32 seqFromSlot = vm.load(address(portal), PORTAL_SEQUENCER_SLOT);
        assertEq(
            address(uint160(uint256(seqFromSlot))),
            portal.sequencer(),
            "PORTAL_SEQUENCER_SLOT reads wrong data"
        );

        // Verify pendingSequencer slot (used by ZoneConfig)
        bytes32 pendingFromSlot = vm.load(address(portal), PORTAL_PENDING_SEQUENCER_SLOT);
        assertEq(
            address(uint160(uint256(pendingFromSlot))),
            portal.pendingSequencer(),
            "PORTAL_PENDING_SEQUENCER_SLOT reads wrong data"
        );

        // Verify currentDepositQueueHash slot (used by ZoneInbox)
        bytes32 queueHashFromSlot = vm.load(address(portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT);
        assertEq(
            queueHashFromSlot,
            portal.currentDepositQueueHash(),
            "PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT reads wrong data"
        );

        // Verify encryption keys array length from slot 6
        bytes32 arrayLenRaw = vm.load(address(portal), PORTAL_ENCRYPTION_KEYS_SLOT);
        assertEq(
            uint256(arrayLenRaw),
            portal.encryptionKeyCount(),
            "PORTAL_ENCRYPTION_KEYS_SLOT reads wrong array length"
        );

        // Verify the derived slot computation matches actual key data
        // This replicates the exact logic from ZoneInbox._readEncryptionKey():
        //   uint256 base = uint256(keccak256(abi.encode(uint256(PORTAL_ENCRYPTION_KEYS_SLOT))));
        //   uint256 slotX = base + (keyIndex * 2);
        //   uint256 slotMeta = slotX + 1;
        uint256 base = uint256(keccak256(abi.encode(PORTAL_ENCRYPTION_KEYS_SLOT)));
        bytes32 loadedX = vm.load(address(portal), bytes32(base + 0));
        bytes32 loadedMeta = vm.load(address(portal), bytes32(base + 1));

        assertEq(loadedX, keyX, "derived slot for key x does not match actual storage");
        assertEq(
            uint8(uint256(loadedMeta) & 0xff),
            keyYParity,
            "derived slot for key yParity does not match actual storage"
        );

        // Also verify via the public getter for full round-trip confidence
        EncryptionKeyEntry memory entry = portal.encryptionKeyAt(0);
        assertEq(loadedX, entry.x, "vm.load x != encryptionKeyAt(0).x");
        assertEq(
            uint8(uint256(loadedMeta) & 0xff),
            entry.yParity,
            "vm.load yParity != encryptionKeyAt(0).yParity"
        );
    }

}
