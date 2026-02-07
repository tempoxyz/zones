// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { TIP20 } from "../../src/TIP20.sol";
import {
    BlockTransition,
    Deposit,
    DepositQueueTransition,
    IWithdrawalReceiver,
    IZoneFactory,
    IZoneInbox,
    IZonePortal,
    Withdrawal,
    WithdrawalQueueTransition,
    ZoneParams
} from "../../src/zone/IZone.sol";
import { EMPTY_SENTINEL } from "../../src/zone/WithdrawalQueueLib.sol";
import { ZoneFactory } from "../../src/zone/ZoneFactory.sol";
import { ZoneInbox } from "../../src/zone/ZoneInbox.sol";
import { ZoneOutbox } from "../../src/zone/ZoneOutbox.sol";
import { ZonePortal } from "../../src/zone/ZonePortal.sol";
import { BaseTest } from "../BaseTest.t.sol";
import { MockTempoState } from "./mocks/MockTempoState.sol";
import { MockVerifier } from "./mocks/MockVerifier.sol";
import { MockZoneGasToken } from "./mocks/MockZoneGasToken.sol";

/// @notice Mock withdrawal receiver for callback tests
contract MockWithdrawalReceiver is IWithdrawalReceiver {

    bool public shouldAccept = true;
    address public lastSender;
    uint128 public lastAmount;
    bytes public lastCallbackData;

    function setShouldAccept(bool _accept) external {
        shouldAccept = _accept;
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
        return shouldAccept ? IWithdrawalReceiver.onWithdrawalReceived.selector : bytes4(0);
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
    MockTempoState public l2TempoState;
    ZoneInbox public l2Inbox;
    ZoneOutbox public l2Outbox;

    /*//////////////////////////////////////////////////////////////
                             TEST HELPERS
    //////////////////////////////////////////////////////////////*/

    MockWithdrawalReceiver public withdrawalReceiver;
    uint64 public zoneId;

    bytes32 constant GENESIS_BLOCK_HASH = keccak256("genesis");
    bytes32 constant GENESIS_TEMPO_BLOCK_HASH = keccak256("tempoGenesis");
    uint64 public genesisTempoBlockNumber;

    /// @notice Storage slot for currentDepositQueueHash in ZonePortal
    /// @dev Layout: sequencerPubkey(0), withdrawalBatchIndex(1), blockHash(2), currentDepositQueueHash(3)
    bytes32 internal constant CURRENT_DEPOSIT_QUEUE_HASH_SLOT = bytes32(uint256(5));

    /// @notice Represents an observed deposit from Tempo (simulating sequencer watching events)
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

    /// @notice Track zone block hash (in reality from block header)
    bytes32 internal l2BlockHash;

    function setUp() public override {
        super.setUp();

        // === Deploy L1 Contracts ===
        l1Factory = new ZoneFactory();
        l1Verifier = new MockVerifier();
        withdrawalReceiver = new MockWithdrawalReceiver();

        // Fund test accounts on L1
        vm.startPrank(pathUSDAdmin);
        pathUSD.grantRole(_ISSUER_ROLE, pathUSDAdmin);
        pathUSD.mint(alice, 100_000e6);
        pathUSD.mint(bob, 100_000e6);
        vm.stopPrank();

        // Record genesis block number for Tempo
        genesisTempoBlockNumber = uint64(block.number);

        // Create zone on L1
        IZoneFactory.CreateZoneParams memory params = IZoneFactory.CreateZoneParams({
            token: address(pathUSD),
            sequencer: admin,
            verifier: address(l1Verifier),
            zoneParams: ZoneParams({
                genesisBlockHash: GENESIS_BLOCK_HASH,
                genesisTempoBlockHash: GENESIS_TEMPO_BLOCK_HASH,
                genesisTempoBlockNumber: genesisTempoBlockNumber
            })
        });
        address portalAddr;
        (zoneId, portalAddr) = l1Factory.createZone(params);
        l1Portal = ZonePortal(portalAddr);

        // === Deploy zone contracts ===
        // Gas token on zone (same concept as pathUSD, deployed at "same address" conceptually)
        l2GasToken = new MockZoneGasToken("Zone USD", "zUSD");

        // TempoState mock for testing
        l2TempoState = new MockTempoState(admin, GENESIS_TEMPO_BLOCK_HASH, genesisTempoBlockNumber);

        // Zone inbox (advances Tempo state and processes deposits)
        l2Inbox = new ZoneInbox(portalAddr, address(l2TempoState), address(l2GasToken), admin);
        l2GasToken.setMinter(address(l2Inbox), true);

        // Zone outbox (handles withdrawals)
        l2Outbox = new ZoneOutbox(address(l2GasToken), admin);
        l2GasToken.setBurner(address(l2Outbox), true);

        // Initialize zone block hash
        l2BlockHash = GENESIS_BLOCK_HASH;
    }

    /*//////////////////////////////////////////////////////////////
                       SEQUENCER SIMULATION HELPERS
    //////////////////////////////////////////////////////////////*/

    /// @notice Simulate sequencer observing a deposit event on Tempo
    function _sequencerObserveDeposit(
        address sender,
        address to,
        uint128 amount,
        bytes32 memo
    )
        internal
        returns (bytes32 newHash)
    {
        // Record the deposit
        Deposit memory d = Deposit({ sender: sender, to: to, amount: amount, memo: memo });

        // Calculate the new hash (matches what Tempo portal computes)
        bytes32 prevHash = pendingDeposits.length > 0
            ? pendingDeposits[pendingDeposits.length - 1].newCurrentDepositQueueHash
            : l2Inbox.processedDepositQueueHash();

        newHash = keccak256(abi.encode(d, prevHash));

        pendingDeposits.push(ObservedDeposit({ deposit: d, newCurrentDepositQueueHash: newHash }));
    }

    /// @notice Simulate sequencer relaying deposits to the zone (sequencer-only call)
    function _sequencerRelayDepositsToL2() internal returns (bytes32 newProcessedHash) {
        if (pendingDeposits.length == 0) return l2Inbox.processedDepositQueueHash();

        // Build deposits array
        Deposit[] memory deposits = new Deposit[](pendingDeposits.length);
        for (uint256 i = 0; i < pendingDeposits.length; i++) {
            deposits[i] = pendingDeposits[i].deposit;
        }

        // Get expected final hash
        newProcessedHash = pendingDeposits[pendingDeposits.length - 1].newCurrentDepositQueueHash;

        // Set up mock: TempoState will return this hash when reading from portal
        l2TempoState.setMockStorageValue(
            address(l1Portal), CURRENT_DEPOSIT_QUEUE_HASH_SLOT, newProcessedHash
        );

        // Process on zone via advanceTempo (sequencer-only call)
        // Empty header since MockTempoState just advances block number
        vm.prank(admin);
        l2Inbox.advanceTempo("", deposits);

        // Clear pending
        delete pendingDeposits;

        // Update zone block hash (simulated)
        l2BlockHash = keccak256(abi.encode(l2BlockHash, "deposits", newProcessedHash));
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
    )
        internal
    {
        pendingWithdrawals.push(
            ObservedWithdrawal({
                index: index,
                withdrawal: Withdrawal({
                    sender: sender,
                    to: to,
                    amount: amount,
                    fee: 0,
                    memo: memo,
                    gasLimit: gasLimit,
                    fallbackRecipient: fallbackRecipient,
                    callbackData: data
                })
            })
        );
    }

    /// @notice Build withdrawal queue hash from observed events (oldest = outermost)
    /// @dev Only used for verification in tests, actual hash is built by l2Outbox.finalizeWithdrawalBatch()
    function _buildWithdrawalQueueHash() internal view returns (bytes32 queueHash) {
        if (pendingWithdrawals.length == 0) return bytes32(0);

        // Build from newest to oldest (so oldest ends up outermost)
        // Innermost element wraps EMPTY_SENTINEL
        queueHash = EMPTY_SENTINEL;
        for (uint256 i = pendingWithdrawals.length; i > 0;) {
            unchecked {
                i--;
            }
            queueHash = keccak256(abi.encode(pendingWithdrawals[i].withdrawal, queueHash));
        }
    }

    /// @notice Simulate sequencer building and submitting a batch to Tempo
    function _sequencerSubmitBatch(bytes32 newProcessedDepositQueueHash) internal {
        // Sequencer calls finalizeWithdrawalBatch() on zone outbox to get withdrawal hash on-chain
        vm.prank(admin);
        bytes32 withdrawalQueueHash = l2Outbox.finalizeWithdrawalBatch(type(uint256).max);

        // Advance a block so the history precompile can return a hash
        vm.roll(block.number + 1);

        // Submit to Tempo
        l1Portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({ prevBlockHash: l1Portal.blockHash(), nextBlockHash: l2BlockHash }),
            DepositQueueTransition({
                prevProcessedHash: bytes32(0), nextProcessedHash: newProcessedDepositQueueHash
            }),
            WithdrawalQueueTransition({ withdrawalQueueHash: withdrawalQueueHash }),
            "",
            ""
        );

        // Clear pending withdrawals observation (they're now in Tempo queue)
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

        // === STEP 3: Sequencer relays deposit to zone (sequencer-only call) ===
        bytes32 newProcessedHash = _sequencerRelayDepositsToL2();

        // Verify zone state
        assertEq(l2GasToken.balanceOf(alice), depositAmount);
        assertEq(l2Inbox.processedDepositQueueHash(), newProcessedHash);
        assertEq(l2GasToken.totalSupply(), depositAmount);

        // === STEP 4: Submit batch to L1 (no withdrawals yet) ===
        _sequencerSubmitBatch(newProcessedHash);

        // Verify L1 batch state updated
        assertEq(l1Portal.withdrawalBatchIndex(), 1);
        assertEq(l1Portal.blockHash(), l2BlockHash);

        // === STEP 5: Alice requests withdrawal on zone ===
        uint128 withdrawAmount = 400e6;
        vm.startPrank(alice);
        l2GasToken.approve(address(l2Outbox), withdrawAmount);
        l2Outbox.requestWithdrawal(
            alice, // to (back to self on L1)
            withdrawAmount,
            bytes32(0), // memo
            0, // no callback
            alice, // fallback to self
            ""
        );
        vm.stopPrank();

        // Verify zone state - tokens burned
        assertEq(l2GasToken.balanceOf(alice), depositAmount - withdrawAmount);

        // === STEP 6: Sequencer observes withdrawal event ===
        _sequencerObserveWithdrawal(0, alice, alice, withdrawAmount, bytes32(0), 0, alice, "");

        // Update zone state root
        l2BlockHash = keccak256(abi.encode(l2BlockHash, "withdrawal", 0));

        // === STEP 7: Submit batch with withdrawal ===
        _sequencerSubmitBatch(newProcessedHash);

        // Verify L1 queue updated
        assertEq(l1Portal.withdrawalBatchIndex(), 2);
        Withdrawal memory w = Withdrawal({
            sender: alice,
            to: alice,
            amount: withdrawAmount,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 expectedQueueHash = keccak256(abi.encode(w, EMPTY_SENTINEL));
        // Withdrawal should be in slot 0 (first batch with withdrawals)
        assertEq(l1Portal.withdrawalQueueSlot(0), expectedQueueHash);
        assertEq(l1Portal.withdrawalQueueTail(), 1);

        // === STEP 8: Sequencer processes withdrawal on L1 ===
        uint256 aliceL1BalanceBefore = pathUSD.balanceOf(alice);
        l1Portal.processWithdrawal(w, bytes32(0)); // 0 = last item in slot

        // Verify Alice received funds on L1
        assertEq(pathUSD.balanceOf(alice), aliceL1BalanceBefore + withdrawAmount);

        // Verify slot cleared and head advanced
        assertEq(l1Portal.withdrawalQueueSlot(0), EMPTY_SENTINEL);
        assertEq(l1Portal.withdrawalQueueHead(), 1);
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
        l2Outbox.requestWithdrawal(alice, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        vm.startPrank(bob);
        l2GasToken.approve(address(l2Outbox), 1000e6);
        l2Outbox.requestWithdrawal(bob, 1000e6, bytes32(0), 0, bob, "");
        vm.stopPrank();

        // Sequencer observes withdrawals
        _sequencerObserveWithdrawal(0, alice, alice, 500e6, bytes32(0), 0, alice, "");
        _sequencerObserveWithdrawal(1, bob, bob, 1000e6, bytes32(0), 0, bob, "");

        // Submit batch with both withdrawals
        l2BlockHash = keccak256(abi.encode(l2BlockHash, "withdrawals"));
        _sequencerSubmitBatch(processedHash);

        // Build expected queue hash (oldest = outermost, innermost wraps EMPTY_SENTINEL)
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
            amount: 1000e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: bob,
            callbackData: ""
        });
        bytes32 innerHash = keccak256(abi.encode(w1, EMPTY_SENTINEL));
        bytes32 queueHash = keccak256(abi.encode(w0, innerHash));
        // Both withdrawals are in slot 0 (same batch)
        assertEq(l1Portal.withdrawalQueueSlot(0), queueHash);

        // Process withdrawals in order
        uint256 aliceBefore = pathUSD.balanceOf(alice);
        uint256 bobBefore = pathUSD.balanceOf(bob);

        l1Portal.processWithdrawal(w0, innerHash);
        assertEq(pathUSD.balanceOf(alice), aliceBefore + 500e6);

        l1Portal.processWithdrawal(w1, bytes32(0)); // 0 = last item in slot
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
            address(withdrawalReceiver), // to: receiver contract
            500e6,
            bytes32(0), // memo
            100_000, // gasLimit for callback
            alice, // fallbackRecipient on zone
            "callback_data"
        );
        vm.stopPrank();

        // Sequencer observes and submits
        _sequencerObserveWithdrawal(
            0,
            alice,
            address(withdrawalReceiver),
            500e6,
            bytes32(0),
            100_000,
            alice,
            "callback_data"
        );
        l2BlockHash = keccak256(abi.encode(l2BlockHash, "callback_withdrawal"));
        _sequencerSubmitBatch(processedHash);

        // Process withdrawal
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
        l1Portal.processWithdrawal(w, bytes32(0));

        // Verify callback was executed
        assertEq(pathUSD.balanceOf(address(withdrawalReceiver)), 500e6);
        assertEq(withdrawalReceiver.lastSender(), alice);
        assertEq(withdrawalReceiver.lastAmount(), 500e6);
        assertEq(withdrawalReceiver.lastCallbackData(), "callback_data");
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
        withdrawalReceiver.setShouldAccept(false);
        vm.startPrank(alice);
        l2GasToken.approve(address(l2Outbox), 500e6);
        l2Outbox.requestWithdrawal(
            address(withdrawalReceiver),
            500e6,
            bytes32(0), // memo
            100_000,
            alice, // fallback recipient
            ""
        );
        vm.stopPrank();

        // Sequencer observes and submits
        _sequencerObserveWithdrawal(
            0, alice, address(withdrawalReceiver), 500e6, bytes32(0), 100_000, alice, ""
        );
        l2BlockHash = keccak256(abi.encode(l2BlockHash, "failing_callback"));
        _sequencerSubmitBatch(processedHash);

        bytes32 depositHashBefore = l1Portal.currentDepositQueueHash();

        // Process withdrawal - callback will fail, triggering bounce-back
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
        l1Portal.processWithdrawal(w, bytes32(0));

        // Verify receiver did NOT get funds (transfer reverted)
        assertEq(pathUSD.balanceOf(address(withdrawalReceiver)), 0);

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
        l2Outbox.requestWithdrawal(bob, 300e6, bytes32(0), 0, bob, "");
        vm.stopPrank();

        // Verify Bob's zone balance debited
        assertEq(l2GasToken.balanceOf(bob), 0);
        assertEq(l2Outbox.nextWithdrawalIndex(), 1);
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
        l2Outbox.requestWithdrawal(alice, 2000e6, bytes32(0), 0, alice, "");
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

    function test_l2_invalidDepositHashReverts() public {
        // Deposit on L1
        vm.startPrank(alice);
        pathUSD.approve(address(l1Portal), 1000e6);
        l1Portal.deposit(alice, 1000e6, bytes32(""));
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));

        // Set up mock with wrong hash
        l2TempoState.setMockStorageValue(
            address(l1Portal), CURRENT_DEPOSIT_QUEUE_HASH_SLOT, bytes32("wrong hash")
        );

        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] = pendingDeposits[0].deposit;

        // Should revert because hash doesn't match
        vm.prank(admin);
        vm.expectRevert(IZoneInbox.InvalidDepositQueueHash.selector);
        l2Inbox.advanceTempo("", deposits);
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
            address(withdrawalReceiver),
            500e6,
            bytes32(0), // memo
            100_000, // gasLimit > 0
            address(0), // invalid fallback
            ""
        );
        vm.stopPrank();
    }

    function test_l2_onlySequencerCanAdvanceTempo() public {
        vm.startPrank(alice);
        pathUSD.approve(address(l1Portal), 1000e6);
        l1Portal.deposit(alice, 1000e6, bytes32(""));
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));

        bytes32 expectedHash = pendingDeposits[0].newCurrentDepositQueueHash;
        l2TempoState.setMockStorageValue(
            address(l1Portal), CURRENT_DEPOSIT_QUEUE_HASH_SLOT, expectedHash
        );

        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] = pendingDeposits[0].deposit;

        // Non-sequencer tries to advance
        vm.prank(alice);
        vm.expectRevert(IZoneInbox.OnlySequencer.selector);
        l2Inbox.advanceTempo("", deposits);
    }

}
