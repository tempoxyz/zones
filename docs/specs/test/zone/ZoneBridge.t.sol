// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { BaseTest } from "../BaseTest.t.sol";
import { ZoneFactory } from "../../src/zone/ZoneFactory.sol";
import { ZonePortal } from "../../src/zone/ZonePortal.sol";
import { ZoneDepositQueue } from "../../src/zone/ZoneDepositQueue.sol";
import { ZoneOutbox } from "../../src/zone/ZoneOutbox.sol";
import { MockVerifier } from "./mocks/MockVerifier.sol";
import { MockZoneGasToken } from "./mocks/MockZoneGasToken.sol";
import { TIP20 } from "../../src/TIP20.sol";
import {
    IZoneFactory,
    IZonePortal,
    IExitReceiver,
    DepositQueueMessage,
    DepositQueueMessageKind,
    Deposit,
    Withdrawal,
    StateTransition,
    DepositQueueTransition,
    WithdrawalQueueTransition
} from "../../src/zone/IZone.sol";

/// @notice Mock exit receiver for callback tests
contract MockExitReceiver is IExitReceiver {
    bool public shouldAccept = true;
    address public lastSender;
    uint128 public lastAmount;
    bytes public lastData;

    function setShouldAccept(bool _accept) external { shouldAccept = _accept; }

    function onExitReceived(
        address sender,
        uint128 amount,
        bytes calldata data
    ) external returns (bytes4) {
        lastSender = sender;
        lastAmount = amount;
        lastData = data;
        return shouldAccept ? IExitReceiver.onExitReceived.selector : bytes4(0);
    }
}

/// @title ZoneBridgeTest
/// @notice Tests the full L1<->zone state machine with mocked message passing
/// @dev Simulates sequencer relaying data between chains asynchronously
contract ZoneBridgeTest is BaseTest {
    /*//////////////////////////////////////////////////////////////
                              L1 CONTRACTS
    //////////////////////////////////////////////////////////////*/

    ZoneFactory public l1Factory;
    ZonePortal public l1Portal;
    MockVerifier public l1Verifier;

    /*//////////////////////////////////////////////////////////////
                             ZONE CONTRACTS
    //////////////////////////////////////////////////////////////*/

    MockZoneGasToken public l2GasToken;
    ZoneDepositQueue public l2DepositQueue;
    ZoneOutbox public l2Outbox;

    /*//////////////////////////////////////////////////////////////
                             TEST HELPERS
    //////////////////////////////////////////////////////////////*/

    MockExitReceiver public exitReceiver;
    uint64 public zoneId;

    bytes32 constant GENESIS_STATE_ROOT = keccak256("genesis");

    /// @notice Represents an observed deposit from L1 (simulating sequencer watching events)
    struct ObservedDeposit {
        Deposit deposit;
        bytes32 newCurrentDepositQueueHash;
    }

    /// @notice Represents an observed withdrawal from zone events
    struct ObservedWithdrawal {
        uint64 index;
        Withdrawal withdrawal;
    }

    /// @notice Sequencer's pending deposit observations
    ObservedDeposit[] internal pendingDeposits;

    /// @notice Sequencer's observed withdrawals for current batch
    ObservedWithdrawal[] internal pendingWithdrawals;

    /// @notice Track zone state root (in reality computed by prover)
    bytes32 internal l2StateRoot;

    function setUp() public override {
        super.setUp();

        // === Deploy L1 Contracts ===
        l1Factory = new ZoneFactory();
        l1Verifier = new MockVerifier();
        exitReceiver = new MockExitReceiver();

        // Fund test accounts on L1
        vm.startPrank(pathUSDAdmin);
        pathUSD.grantRole(_ISSUER_ROLE, pathUSDAdmin);
        pathUSD.mint(alice, 100_000e6);
        pathUSD.mint(bob, 100_000e6);
        vm.stopPrank();

        // Create zone on L1
        IZoneFactory.CreateZoneParams memory params = IZoneFactory.CreateZoneParams({
            token: address(pathUSD),
            sequencer: admin,
            verifier: address(l1Verifier),
            genesisStateRoot: GENESIS_STATE_ROOT
        });
        address portalAddr;
        (zoneId, portalAddr) = l1Factory.createZone(params);
        l1Portal = ZonePortal(portalAddr);

        // === Deploy zone contracts ===
        // Gas token on zone (same concept as pathUSD, deployed at "same address" conceptually)
        l2GasToken = new MockZoneGasToken("Zone USD", "zUSD");

        // Zone deposit queue (processes deposit queue messages)
        l2DepositQueue = new ZoneDepositQueue(portalAddr, address(l2GasToken), admin);
        l2GasToken.setMinter(address(l2DepositQueue), true);

        // Zone outbox (handles withdrawals)
        l2Outbox = new ZoneOutbox(address(l2GasToken));
        l2GasToken.setBurner(address(l2Outbox), true);

        // Initialize zone state root
        l2StateRoot = GENESIS_STATE_ROOT;
    }

    /*//////////////////////////////////////////////////////////////
                       SEQUENCER SIMULATION HELPERS
    //////////////////////////////////////////////////////////////*/

    /// @notice Simulate sequencer observing a deposit event on L1
    function _sequencerObserveDeposit(
        address sender,
        address to,
        uint128 amount,
        bytes32 memo
    ) internal returns (bytes32 newHash) {
        // Record the deposit with L1 block info
        Deposit memory d = Deposit({
            l1ParentBlockHash: blockhash(block.number - 1),
            l1BlockNumber: uint64(block.number),
            l1Timestamp: uint64(block.timestamp),
            sender: sender,
            to: to,
            amount: amount,
            memo: memo
        });

        // Calculate the new hash (matches what L1 portal computes)
        bytes32 prevHash = pendingDeposits.length > 0
            ? pendingDeposits[pendingDeposits.length - 1].newCurrentDepositQueueHash
            : l2DepositQueue.processedDepositQueueHash();

        DepositQueueMessage memory m = DepositQueueMessage({
            kind: DepositQueueMessageKind.Deposit,
            data: abi.encode(d)
        });
        newHash = keccak256(abi.encode(m, prevHash));

        pendingDeposits.push(ObservedDeposit({
            deposit: d,
            newCurrentDepositQueueHash: newHash
        }));
    }

    /// @notice Simulate sequencer relaying deposits to the zone (system transaction)
    function _sequencerRelayDepositsToL2() internal returns (bytes32 newProcessedHash) {
        if (pendingDeposits.length == 0) return l2DepositQueue.processedDepositQueueHash();

        // Build deposit queue message array
        DepositQueueMessage[] memory messages = new DepositQueueMessage[](pendingDeposits.length);
        for (uint256 i = 0; i < pendingDeposits.length; i++) {
            messages[i] = DepositQueueMessage({
                kind: DepositQueueMessageKind.Deposit,
                data: abi.encode(pendingDeposits[i].deposit)
            });
        }

        // Get expected final hash
        newProcessedHash = pendingDeposits[pendingDeposits.length - 1].newCurrentDepositQueueHash;

        // Process on zone (sequencer calls as system tx)
        l2DepositQueue.processDepositQueue(messages, newProcessedHash);

        // Clear pending
        delete pendingDeposits;

        // Update zone state root (simulated)
        l2StateRoot = keccak256(abi.encode(l2StateRoot, "messages", newProcessedHash));
    }

    /// @notice Simulate sequencer observing a withdrawal event on the zone
    function _sequencerObserveWithdrawal(
        uint64 index,
        address sender,
        address to,
        uint128 amount,
        bytes32 memo,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes memory data
    ) internal {
        pendingWithdrawals.push(ObservedWithdrawal({
            index: index,
            withdrawal: Withdrawal({
                sender: sender,
                to: to,
                amount: amount,
                memo: memo,
                gasLimit: gasLimit,
                fallbackRecipient: fallbackRecipient,
                data: data
            })
        }));
    }

    /// @notice Build withdrawal queue hash from observed events (oldest = outermost)
    function _buildWithdrawalQueueHash() internal view returns (bytes32 queueHash) {
        // Build from newest to oldest (so oldest ends up outermost)
        queueHash = bytes32(0);
        for (uint256 i = pendingWithdrawals.length; i > 0; ) {
            unchecked { i--; }
            queueHash = keccak256(abi.encode(pendingWithdrawals[i].withdrawal, queueHash));
        }
    }

    /// @notice Simulate sequencer building and submitting a batch to L1
    function _sequencerSubmitBatch(bytes32 newProcessedDepositQueueHash) internal {
        // Build withdrawal queue hash from observed events
        bytes32 withdrawalQueueHash = _buildWithdrawalQueueHash();

        // Get current L1 pending queue state
        bytes32 prevPendingHash = l1Portal.pendingWithdrawalQueueHash();

        // Submit to L1
        l1Portal.submitBatch(
            StateTransition({ prevStateRoot: bytes32(0), nextStateRoot: l2StateRoot }),
            DepositQueueTransition({ prevSnapshotHash: bytes32(0), prevProcessedHash: bytes32(0), nextProcessedHash: newProcessedDepositQueueHash }),
            WithdrawalQueueTransition({ prevPendingHash: prevPendingHash, nextPendingHashIfFull: withdrawalQueueHash, nextPendingHashIfEmpty: withdrawalQueueHash }),
            "",
            ""
        );

        // Clear pending withdrawals (they're now in L1 queue)
        delete pendingWithdrawals;
    }

    /// @notice Get withdrawal from pending list by index
    function _getWithdrawalByIndex(uint64 targetIndex) internal view returns (Withdrawal memory) {
        for (uint256 i = 0; i < pendingWithdrawals.length; i++) {
            if (pendingWithdrawals[i].index == targetIndex) {
                return pendingWithdrawals[i].withdrawal;
            }
        }
        revert("withdrawal not found");
    }

    /*//////////////////////////////////////////////////////////////
                    FULL STATE MACHINE TESTS
    //////////////////////////////////////////////////////////////*/

    function test_fullFlow_depositAndWithdraw() public {
        // === STEP 1: Alice deposits on L1 ===
        uint128 depositAmount = 1000e6;
        vm.startPrank(alice);
        pathUSD.approve(address(l1Portal), depositAmount);
        bytes32 l1DepositHash = l1Portal.deposit(alice, depositAmount, bytes32("hello zone"));
        vm.stopPrank();

        // Verify L1 state
        assertEq(l1Portal.currentDepositQueueHash(), l1DepositHash);
        assertEq(pathUSD.balanceOf(address(l1Portal)), depositAmount);

        // === STEP 2: Sequencer observes deposit (simulated event watching) ===
        _sequencerObserveDeposit(alice, alice, depositAmount, bytes32("hello zone"));

        // === STEP 3: Sequencer relays deposit to zone (system transaction) ===
        bytes32 newProcessedHash = _sequencerRelayDepositsToL2();

        // Verify zone state
        assertEq(l2GasToken.balanceOf(alice), depositAmount);
        assertEq(l2DepositQueue.processedDepositQueueHash(), newProcessedHash);
        assertEq(l2GasToken.totalSupply(), depositAmount);

        // === STEP 4: Submit batch to L1 (no withdrawals yet) ===
        _sequencerSubmitBatch(newProcessedHash);

        // Verify L1 batch state updated
        assertEq(l1Portal.batchIndex(), 1);
        assertEq(l1Portal.processedDepositQueueHash(), newProcessedHash);
        assertEq(l1Portal.stateRoot(), l2StateRoot);

        // === STEP 5: Alice requests withdrawal on zone ===
        uint128 withdrawAmount = 400e6;
        vm.startPrank(alice);
        l2GasToken.approve(address(l2Outbox), withdrawAmount);
        l2Outbox.requestWithdrawal(
            alice,          // to (back to self on L1)
            withdrawAmount,
            bytes32(0),     // memo
            0,              // no callback
            address(0),     // no fallback needed
            ""
        );
        vm.stopPrank();

        // Verify zone state - tokens burned
        assertEq(l2GasToken.balanceOf(alice), depositAmount - withdrawAmount);

        // === STEP 6: Sequencer observes withdrawal event ===
        _sequencerObserveWithdrawal(0, alice, alice, withdrawAmount, bytes32(0), 0, address(0), "");

        // Update zone state root
        l2StateRoot = keccak256(abi.encode(l2StateRoot, "withdrawal", 0));

        // === STEP 7: Submit batch with withdrawal ===
        _sequencerSubmitBatch(newProcessedHash);

        // Verify L1 queue updated
        assertEq(l1Portal.batchIndex(), 2);
        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: alice,
            amount: withdrawAmount,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: address(0),
            data: ""
        });
        bytes32 expectedQueueHash = keccak256(abi.encode(w, bytes32(0)));
        assertEq(l1Portal.pendingWithdrawalQueueHash(), expectedQueueHash);

        // === STEP 8: Sequencer processes withdrawal on L1 ===
        uint256 aliceL1BalanceBefore = pathUSD.balanceOf(alice);
        l1Portal.processWithdrawal(w, bytes32(0));

        // Verify Alice received funds on L1
        assertEq(pathUSD.balanceOf(alice), aliceL1BalanceBefore + withdrawAmount);

        // Verify queues are empty
        assertEq(l1Portal.activeWithdrawalQueueHash(), bytes32(0));
        assertEq(l1Portal.pendingWithdrawalQueueHash(), bytes32(0));
    }

    function test_fullFlow_multipleDepositsAndWithdrawals() public {
        // === Alice and Bob both deposit ===
        vm.startPrank(alice);
        pathUSD.approve(address(l1Portal), 5000e6);
        l1Portal.deposit(alice, 2000e6, bytes32("alice1"));
        vm.stopPrank();

        vm.startPrank(bob);
        pathUSD.approve(address(l1Portal), 5000e6);
        l1Portal.deposit(bob, 3000e6, bytes32("bob1"));
        vm.stopPrank();

        // Sequencer observes and relays
        _sequencerObserveDeposit(alice, alice, 2000e6, bytes32("alice1"));
        _sequencerObserveDeposit(bob, bob, 3000e6, bytes32("bob1"));
        bytes32 processedHash = _sequencerRelayDepositsToL2();

        // Verify zone balances
        assertEq(l2GasToken.balanceOf(alice), 2000e6);
        assertEq(l2GasToken.balanceOf(bob), 3000e6);

        // Submit batch
        _sequencerSubmitBatch(processedHash);

        // === Both request withdrawals ===
        vm.startPrank(alice);
        l2GasToken.approve(address(l2Outbox), 500e6);
        l2Outbox.requestWithdrawal(alice, 500e6, bytes32(0), 0, address(0), "");
        vm.stopPrank();

        vm.startPrank(bob);
        l2GasToken.approve(address(l2Outbox), 1000e6);
        l2Outbox.requestWithdrawal(bob, 1000e6, bytes32(0), 0, address(0), "");
        vm.stopPrank();

        // Sequencer observes withdrawals
        _sequencerObserveWithdrawal(0, alice, alice, 500e6, bytes32(0), 0, address(0), "");
        _sequencerObserveWithdrawal(1, bob, bob, 1000e6, bytes32(0), 0, address(0), "");

        // Submit batch with both withdrawals
        l2StateRoot = keccak256(abi.encode(l2StateRoot, "withdrawals"));
        _sequencerSubmitBatch(processedHash);

        // Build expected queue hash (oldest = outermost)
        Withdrawal memory w0 = Withdrawal({
            sender: alice, to: alice, amount: 500e6, memo: bytes32(0), gasLimit: 0, fallbackRecipient: address(0), data: ""
        });
        Withdrawal memory w1 = Withdrawal({
            sender: bob, to: bob, amount: 1000e6, memo: bytes32(0), gasLimit: 0, fallbackRecipient: address(0), data: ""
        });
        bytes32 innerHash = keccak256(abi.encode(w1, bytes32(0)));
        bytes32 queueHash = keccak256(abi.encode(w0, innerHash));
        assertEq(l1Portal.pendingWithdrawalQueueHash(), queueHash);

        // Process withdrawals in order
        uint256 aliceBefore = pathUSD.balanceOf(alice);
        uint256 bobBefore = pathUSD.balanceOf(bob);

        l1Portal.processWithdrawal(w0, innerHash);
        assertEq(pathUSD.balanceOf(alice), aliceBefore + 500e6);

        l1Portal.processWithdrawal(w1, bytes32(0));
        assertEq(pathUSD.balanceOf(bob), bobBefore + 1000e6);
    }

    function test_fullFlow_withdrawalWithCallback() public {
        // Setup: deposit to zone
        vm.startPrank(alice);
        pathUSD.approve(address(l1Portal), 1000e6);
        l1Portal.deposit(alice, 1000e6, bytes32(""));
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));
        bytes32 processedHash = _sequencerRelayDepositsToL2();
        _sequencerSubmitBatch(processedHash);

        // Request withdrawal with callback
        vm.startPrank(alice);
        l2GasToken.approve(address(l2Outbox), 500e6);
        l2Outbox.requestWithdrawal(
            address(exitReceiver),  // to: receiver contract
            500e6,
            bytes32(0),             // memo
            100000,                 // gasLimit for callback
            alice,                  // fallbackRecipient on zone
            "callback_data"
        );
        vm.stopPrank();

        // Sequencer observes and submits
        _sequencerObserveWithdrawal(0, alice, address(exitReceiver), 500e6, bytes32(0), 100000, alice, "callback_data");
        l2StateRoot = keccak256(abi.encode(l2StateRoot, "callback_withdrawal"));
        _sequencerSubmitBatch(processedHash);

        // Process withdrawal
        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: address(exitReceiver),
            amount: 500e6,
            memo: bytes32(0),
            gasLimit: 100000,
            fallbackRecipient: alice,
            data: "callback_data"
        });
        l1Portal.processWithdrawal(w, bytes32(0));

        // Verify callback was executed
        assertEq(pathUSD.balanceOf(address(exitReceiver)), 500e6);
        assertEq(exitReceiver.lastSender(), alice);
        assertEq(exitReceiver.lastAmount(), 500e6);
        assertEq(exitReceiver.lastData(), "callback_data");
    }

    function test_fullFlow_bounceBackOnCallbackFailure() public {
        // Setup: deposit to zone
        vm.startPrank(alice);
        pathUSD.approve(address(l1Portal), 1000e6);
        l1Portal.deposit(alice, 1000e6, bytes32(""));
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));
        bytes32 processedHash = _sequencerRelayDepositsToL2();
        _sequencerSubmitBatch(processedHash);

        // Request withdrawal with callback that will fail
        exitReceiver.setShouldAccept(false);
        vm.startPrank(alice);
        l2GasToken.approve(address(l2Outbox), 500e6);
        l2Outbox.requestWithdrawal(
            address(exitReceiver),
            500e6,
            bytes32(0),  // memo
            100000,
            alice,  // fallback recipient
            ""
        );
        vm.stopPrank();

        // Sequencer observes and submits
        _sequencerObserveWithdrawal(0, alice, address(exitReceiver), 500e6, bytes32(0), 100000, alice, "");
        l2StateRoot = keccak256(abi.encode(l2StateRoot, "failing_callback"));
        _sequencerSubmitBatch(processedHash);

        bytes32 depositHashBefore = l1Portal.currentDepositQueueHash();

        // Process withdrawal - callback will fail, triggering bounce-back
        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: address(exitReceiver),
            amount: 500e6,
            memo: bytes32(0),
            gasLimit: 100000,
            fallbackRecipient: alice,
            data: ""
        });
        l1Portal.processWithdrawal(w, bytes32(0));

        // Verify receiver did NOT get funds (transfer reverted)
        assertEq(pathUSD.balanceOf(address(exitReceiver)), 0);

        // Verify bounce-back deposit was created
        assertTrue(l1Portal.currentDepositQueueHash() != depositHashBefore);
    }

    function test_fullFlow_transferOnL2() public {
        // Deposit to Alice
        vm.startPrank(alice);
        pathUSD.approve(address(l1Portal), 1000e6);
        l1Portal.deposit(alice, 1000e6, bytes32(""));
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));
        _sequencerRelayDepositsToL2();

        // Alice transfers to Bob on zone
        vm.prank(alice);
        l2GasToken.transfer(bob, 300e6);

        // Verify zone balances
        assertEq(l2GasToken.balanceOf(alice), 700e6);
        assertEq(l2GasToken.balanceOf(bob), 300e6);

        // Bob withdraws on zone
        vm.startPrank(bob);
        l2GasToken.approve(address(l2Outbox), 300e6);
        l2Outbox.requestWithdrawal(bob, 300e6, bytes32(0), 0, address(0), "");
        vm.stopPrank();

        // Verify Bob's zone balance debited
        assertEq(l2GasToken.balanceOf(bob), 0);
        assertEq(l2Outbox.nextWithdrawalIndex(), 1);
    }

    function test_fullFlow_partialDepositProcessing() public {
        // Make multiple deposits
        vm.startPrank(alice);
        pathUSD.approve(address(l1Portal), 3000e6);
        l1Portal.deposit(alice, 1000e6, bytes32("d1"));
        l1Portal.deposit(alice, 1000e6, bytes32("d2"));
        l1Portal.deposit(alice, 1000e6, bytes32("d3"));
        vm.stopPrank();

        // Sequencer observes all
        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32("d1"));
        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32("d2"));
        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32("d3"));

        // But only processes first two
        DepositQueueMessage[] memory firstTwo = new DepositQueueMessage[](2);
        firstTwo[0] = DepositQueueMessage({
            kind: DepositQueueMessageKind.Deposit,
            data: abi.encode(pendingDeposits[0].deposit)
        });
        firstTwo[1] = DepositQueueMessage({
            kind: DepositQueueMessageKind.Deposit,
            data: abi.encode(pendingDeposits[1].deposit)
        });
        bytes32 partialHash = pendingDeposits[1].newCurrentDepositQueueHash;

        l2DepositQueue.processDepositQueue(firstTwo, partialHash);

        // Verify zone state
        assertEq(l2GasToken.balanceOf(alice), 2000e6); // Only 2 processed
        assertEq(l2DepositQueue.processedDepositQueueHash(), partialHash);

        // Submit batch with partial processing
        l2StateRoot = keccak256(abi.encode(l2StateRoot, "partial"));
        l1Portal.submitBatch(
            StateTransition({ prevStateRoot: bytes32(0), nextStateRoot: l2StateRoot }),
            DepositQueueTransition({ prevSnapshotHash: bytes32(0), prevProcessedHash: bytes32(0), nextProcessedHash: partialHash }),
            WithdrawalQueueTransition({ prevPendingHash: bytes32(0), nextPendingHashIfFull: bytes32(0), nextPendingHashIfEmpty: bytes32(0) }),
            "",
            ""
        );

        // L1 should show partial processing
        assertEq(l1Portal.processedDepositQueueHash(), partialHash);
        assertEq(l1Portal.snapshotDepositQueueHash(), l1Portal.currentDepositQueueHash());
    }

    function test_l2_insufficientBalanceReverts() public {
        // Deposit to Alice
        vm.startPrank(alice);
        pathUSD.approve(address(l1Portal), 1000e6);
        l1Portal.deposit(alice, 1000e6, bytes32(""));
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));
        _sequencerRelayDepositsToL2();

        // Alice tries to withdraw more than balance
        vm.startPrank(alice);
        l2GasToken.approve(address(l2Outbox), 2000e6);
        vm.expectRevert(MockZoneGasToken.InsufficientBalance.selector);
        l2Outbox.requestWithdrawal(alice, 2000e6, bytes32(0), 0, address(0), "");
        vm.stopPrank();
    }

    function test_l2_transferInsufficientBalance() public {
        // Deposit to Alice
        vm.startPrank(alice);
        pathUSD.approve(address(l1Portal), 1000e6);
        l1Portal.deposit(alice, 1000e6, bytes32(""));
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));
        _sequencerRelayDepositsToL2();

        // Alice tries to transfer more than balance
        vm.prank(alice);
        vm.expectRevert(MockZoneGasToken.InsufficientBalance.selector);
        l2GasToken.transfer(bob, 2000e6);
    }

    function test_l2_invalidDepositChainReverts() public {
        // Deposit on L1
        vm.startPrank(alice);
        pathUSD.approve(address(l1Portal), 1000e6);
        l1Portal.deposit(alice, 1000e6, bytes32(""));
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));

        // Try to process with wrong expected hash
        DepositQueueMessage[] memory messages = new DepositQueueMessage[](1);
        messages[0] = DepositQueueMessage({
            kind: DepositQueueMessageKind.Deposit,
            data: abi.encode(pendingDeposits[0].deposit)
        });

        vm.expectRevert(ZoneDepositQueue.InvalidDepositQueueChain.selector);
        l2DepositQueue.processDepositQueue(messages, bytes32("wrong hash"));
    }

    function test_l2_callbackRequiresFallbackRecipient() public {
        // Deposit to Alice
        vm.startPrank(alice);
        pathUSD.approve(address(l1Portal), 1000e6);
        l1Portal.deposit(alice, 1000e6, bytes32(""));
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));
        _sequencerRelayDepositsToL2();

        // Try callback without fallback recipient
        vm.startPrank(alice);
        l2GasToken.approve(address(l2Outbox), 500e6);
        vm.expectRevert(ZoneOutbox.InvalidFallbackRecipient.selector);
        l2Outbox.requestWithdrawal(
            address(exitReceiver),
            500e6,
            bytes32(0), // memo
            100000,     // gasLimit > 0 requires fallback
            address(0), // invalid!
            ""
        );
        vm.stopPrank();
    }

    function test_l2_onlySequencerCanProcessDeposits() public {
        vm.startPrank(alice);
        pathUSD.approve(address(l1Portal), 1000e6);
        l1Portal.deposit(alice, 1000e6, bytes32(""));
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));

        DepositQueueMessage[] memory messages = new DepositQueueMessage[](1);
        messages[0] = DepositQueueMessage({
            kind: DepositQueueMessageKind.Deposit,
            data: abi.encode(pendingDeposits[0].deposit)
        });
        bytes32 expectedHash = pendingDeposits[0].newCurrentDepositQueueHash;

        // Non-sequencer tries to process
        vm.prank(alice);
        vm.expectRevert(ZoneDepositQueue.OnlySequencer.selector);
        l2DepositQueue.processDepositQueue(messages, expectedHash);
    }
}
