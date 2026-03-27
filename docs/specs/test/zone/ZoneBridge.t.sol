// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { TIP20 } from "../../src/TIP20.sol";
import { EncryptedDepositLib } from "../../src/zone/EncryptedDeposit.sol";
import {
    AES_GCM_DECRYPT,
    BlockTransition,
    CHAUM_PEDERSEN_VERIFY,
    ChaumPedersenProof,
    DecryptionData,
    Deposit,
    DepositQueueTransition,
    DepositType,
    EnabledToken,
    EncryptedDeposit,
    EncryptedDepositPayload,
    IAesGcmDecrypt,
    IChaumPedersenVerify,
    IWithdrawalReceiver,
    IZoneFactory,
    IZoneInbox,
    IZonePortal,
    PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
    PORTAL_ENCRYPTION_KEYS_SLOT,
    QueuedDeposit,
    Withdrawal,
    ZoneParams
} from "../../src/zone/IZone.sol";
import { EMPTY_SENTINEL } from "../../src/zone/WithdrawalQueueLib.sol";
import { ZoneConfig } from "../../src/zone/ZoneConfig.sol";
import { ZoneFactory } from "../../src/zone/ZoneFactory.sol";
import { ZoneInbox } from "../../src/zone/ZoneInbox.sol";
import { ZoneMessenger } from "../../src/zone/ZoneMessenger.sol";
import { ZoneOutbox } from "../../src/zone/ZoneOutbox.sol";
import { ZonePortal } from "../../src/zone/ZonePortal.sol";
import { BaseTest } from "../BaseTest.t.sol";
import { MockTempoState } from "./mocks/MockTempoState.sol";
import { MockZoneToken } from "./mocks/MockZoneToken.sol";
import { Vm } from "forge-std/Vm.sol";

/// @notice Mock withdrawal receiver for callback tests
contract MockWithdrawalReceiver is IWithdrawalReceiver {

    bool public shouldAccept = true;
    bytes32 public lastSenderTag;
    address public lastToken;
    uint128 public lastAmount;
    bytes public lastCallbackData;

    function setShouldAccept(bool _accept) external {
        shouldAccept = _accept;
    }

    function onWithdrawalReceived(
        bytes32 senderTag,
        address token,
        uint128 amount,
        bytes calldata callbackData
    )
        external
        returns (bytes4)
    {
        lastSenderTag = senderTag;
        lastToken = token;
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

    /*//////////////////////////////////////////////////////////////
                             ZONE CONTRACTS
    //////////////////////////////////////////////////////////////*/

    MockZoneToken public l2ZoneToken;
    MockTempoState public l2TempoState;
    ZoneConfig public l2Config;
    ZoneInbox public l2Inbox;
    ZoneOutbox public l2Outbox;

    /*//////////////////////////////////////////////////////////////
                             TEST HELPERS
    //////////////////////////////////////////////////////////////*/

    MockWithdrawalReceiver public withdrawalReceiver;
    uint32 public zoneId;

    bytes32 constant GENESIS_BLOCK_HASH = keccak256("genesis");
    bytes32 constant GENESIS_TEMPO_BLOCK_HASH = keccak256("tempoGenesis");
    uint64 public genesisTempoBlockNumber;

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
        l1Factory = new ZoneFactory(); // Keep factory for verifier only
        withdrawalReceiver = new MockWithdrawalReceiver();

        // Deploy zone token FIRST (used for both L1 escrow and zone-side operations).
        // In production, L1 and zone-side tokens are at the same address, so we use
        // a single MockZoneToken for both roles to avoid ISSUER_ROLE issues with pathUSD.
        l2ZoneToken = new MockZoneToken("Zone USD", "zUSD");

        // Fund test accounts with zone token (for L1 deposits)
        l2ZoneToken.setMinter(address(this), true);
        l2ZoneToken.mint(alice, 100_000e6);
        l2ZoneToken.mint(bob, 100_000e6);
        l2ZoneToken.setMinter(address(this), false);

        // Record genesis block number for Tempo
        genesisTempoBlockNumber = uint64(block.number);

        // Deploy messenger and portal directly (bypass factory to avoid TIP20 prefix check).
        // Predict portal address so messenger can reference it in its constructor.
        uint256 currentNonce = vm.getNonce(address(this));
        address predictedPortal = vm.computeCreateAddress(address(this), currentNonce + 1);
        ZoneMessenger messengerContract = new ZoneMessenger(predictedPortal);
        l1Portal = new ZonePortal(
            1, // zoneId
            address(l2ZoneToken), // initialToken = MockZoneToken (NOT pathUSD)
            address(messengerContract),
            admin, // sequencer
            l1Factory.verifier(),
            GENESIS_BLOCK_HASH,
            genesisTempoBlockNumber
        );
        zoneId = 1;

        // === Deploy zone contracts ===
        // TempoState mock for testing
        l2TempoState = new MockTempoState(admin, GENESIS_TEMPO_BLOCK_HASH, genesisTempoBlockNumber);

        // Zone config (reads sequencer from L1 portal via Tempo state)
        l2Config = new ZoneConfig(address(l1Portal), address(l2TempoState));
        l2TempoState.setMockStorageValue(
            address(l1Portal), bytes32(uint256(0)), bytes32(uint256(uint160(admin)))
        );

        // Zone inbox (advances Tempo state and processes deposits)
        l2Inbox = new ZoneInbox(address(l2Config), address(l1Portal), address(l2TempoState));
        l2ZoneToken.setMinter(address(l2Inbox), true);

        // Zone outbox (handles withdrawals)
        l2Outbox = new ZoneOutbox(address(l2Config));
        l2ZoneToken.setBurner(address(l2Outbox), true);

        // Initialize zone block hash
        l2BlockHash = GENESIS_BLOCK_HASH;
    }

    function _senderTag(address sender, uint256 txSequence) internal view returns (bytes32) {
        return keccak256(abi.encodePacked(sender, zoneTxContext.txHashFor(txSequence)));
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
            token: address(l2ZoneToken),
            senderTag: _senderTag(sender, txSequence),
            to: to,
            amount: amount,
            fee: 0,
            memo: memo,
            gasLimit: gasLimit,
            fallbackRecipient: fallbackRecipient,
            callbackData: callbackData,
            encryptedSender: ""
        });
    }

    function _emptyEncryptedSenders(uint256 count)
        internal
        view
        returns (bytes[] memory encryptedSenders)
    {
        uint256 pending = l2Outbox.pendingWithdrawalsCount();
        if (count > pending) {
            count = pending;
        }
        encryptedSenders = new bytes[](count);
    }

    function _finalizeWithdrawalBatch(uint256 count) internal returns (bytes32) {
        vm.startPrank(admin);
        bytes32 hash = l2Outbox.finalizeWithdrawalBatch(
            count, uint64(block.number), _emptyEncryptedSenders(count)
        );
        vm.stopPrank();
        return hash;
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
        Deposit memory d = Deposit({
            token: address(l2ZoneToken), sender: sender, to: to, amount: amount, memo: memo
        });

        // Calculate the new hash (matches what Tempo portal computes)
        bytes32 prevHash = pendingDeposits.length > 0
            ? pendingDeposits[pendingDeposits.length - 1].newCurrentDepositQueueHash
            : l2Inbox.processedDepositQueueHash();

        newHash = keccak256(abi.encode(DepositType.Regular, d, prevHash));

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
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, newProcessedHash
        );

        // Process on zone via advanceTempo (sequencer-only call)
        // Empty header since MockTempoState just advances block number
        vm.prank(admin);
        l2Inbox.advanceTempo(
            "", _wrapDeposits(deposits), new DecryptionData[](0), new EnabledToken[](0)
        );

        // Clear pending
        delete pendingDeposits;

        // Update zone block hash (simulated)
        l2BlockHash = keccak256(abi.encode(l2BlockHash, "deposits", newProcessedHash));
    }

    function _wrapDeposits(Deposit[] memory deposits)
        internal
        pure
        returns (QueuedDeposit[] memory queued)
    {
        queued = new QueuedDeposit[](deposits.length);
        for (uint256 i = 0; i < deposits.length; i++) {
            queued[i] = QueuedDeposit({
                depositType: DepositType.Regular, depositData: abi.encode(deposits[i])
            });
        }
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
                withdrawal: _withdrawal(
                    uint256(index) + 1, sender, to, amount, memo, gasLimit, fallbackRecipient, data
                )
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
        bytes32 withdrawalQueueHash = _finalizeWithdrawalBatch(type(uint256).max);

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
            withdrawalQueueHash,
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
        l2ZoneToken.approve(address(l1Portal), depositAmount);
        bytes32 l1DepositHash =
            l1Portal.deposit(address(l2ZoneToken), alice, depositAmount, bytes32("hello zone"));
        vm.stopPrank();

        // Verify L1 state
        assertEq(l1Portal.currentDepositQueueHash(), l1DepositHash);
        assertEq(l2ZoneToken.balanceOf(address(l1Portal)), depositAmount);

        // === STEP 2: Sequencer observes deposit (simulated event watching) ===
        _sequencerObserveDeposit(alice, alice, depositAmount, bytes32("hello zone"));

        // === STEP 3: Sequencer relays deposit to zone (sequencer-only call) ===
        bytes32 newProcessedHash = _sequencerRelayDepositsToL2();

        // Verify zone state (alice's net balance is unchanged: -deposit on L1, +mint on zone)
        assertEq(l2ZoneToken.balanceOf(alice), 100_000e6);
        assertEq(l2Inbox.processedDepositQueueHash(), newProcessedHash);
        assertEq(l2ZoneToken.totalSupply(), 200_000e6 + depositAmount);

        // === STEP 4: Submit batch to L1 (no withdrawals yet) ===
        _sequencerSubmitBatch(newProcessedHash);

        // Verify L1 batch state updated
        assertEq(l1Portal.withdrawalBatchIndex(), 1);
        assertEq(l1Portal.blockHash(), l2BlockHash);

        // === STEP 5: Alice requests withdrawal on zone ===
        uint128 withdrawAmount = 400e6;
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), withdrawAmount);
        l2Outbox.requestWithdrawal(
            address(l2ZoneToken),
            alice, // to (back to self on L1)
            withdrawAmount,
            bytes32(0), // memo
            0, // no callback
            alice, // fallback to self
            ""
        );
        vm.stopPrank();

        // Verify zone state - tokens burned (from alice's net balance of 100_000e6)
        assertEq(l2ZoneToken.balanceOf(alice), 100_000e6 - withdrawAmount);

        // === STEP 6: Sequencer observes withdrawal event ===
        _sequencerObserveWithdrawal(0, alice, alice, withdrawAmount, bytes32(0), 0, alice, "");

        // Update zone state root
        l2BlockHash = keccak256(abi.encode(l2BlockHash, "withdrawal", 0));

        // === STEP 7: Submit batch with withdrawal ===
        _sequencerSubmitBatch(newProcessedHash);

        // Verify L1 queue updated
        assertEq(l1Portal.withdrawalBatchIndex(), 2);
        Withdrawal memory w = _withdrawal(1, alice, alice, withdrawAmount, bytes32(0), 0, alice, "");
        bytes32 expectedQueueHash = keccak256(abi.encode(w, EMPTY_SENTINEL));
        // Withdrawal should be in slot 0 (first batch with withdrawals)
        assertEq(l1Portal.withdrawalQueueSlot(0), expectedQueueHash);
        assertEq(l1Portal.withdrawalQueueTail(), 1);

        // === STEP 8: Sequencer processes withdrawal on L1 ===
        uint256 aliceL1BalanceBefore = l2ZoneToken.balanceOf(alice);
        l1Portal.processWithdrawal(w, bytes32(0)); // 0 = last item in slot

        // Verify Alice received funds on L1
        assertEq(l2ZoneToken.balanceOf(alice), aliceL1BalanceBefore + withdrawAmount);

        // Verify slot cleared and head advanced
        assertEq(l1Portal.withdrawalQueueSlot(0), EMPTY_SENTINEL);
        assertEq(l1Portal.withdrawalQueueHead(), 1);
    }

    function test_fullFlow_multipleDepositsAndWithdrawals() public {
        // === Alice and Bob both deposit ===
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 5000e6);
        l1Portal.deposit(address(l2ZoneToken), alice, 2000e6, bytes32("alice1"));
        vm.stopPrank();

        vm.startPrank(bob);
        l2ZoneToken.approve(address(l1Portal), 5000e6);
        l1Portal.deposit(address(l2ZoneToken), bob, 3000e6, bytes32("bob1"));
        vm.stopPrank();

        // Sequencer observes and relays
        _sequencerObserveDeposit(alice, alice, 2000e6, bytes32("alice1"));
        _sequencerObserveDeposit(bob, bob, 3000e6, bytes32("bob1"));
        bytes32 processedHash = _sequencerRelayDepositsToL2();

        // Verify zone balances (net: -deposit on L1, +mint on zone = initial funding)
        assertEq(l2ZoneToken.balanceOf(alice), 100_000e6);
        assertEq(l2ZoneToken.balanceOf(bob), 100_000e6);

        // Submit batch
        _sequencerSubmitBatch(processedHash);

        // === Both request withdrawals ===
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), 500e6);
        l2Outbox.requestWithdrawal(address(l2ZoneToken), alice, 500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        vm.startPrank(bob);
        l2ZoneToken.approve(address(l2Outbox), 1000e6);
        l2Outbox.requestWithdrawal(address(l2ZoneToken), bob, 1000e6, bytes32(0), 0, bob, "");
        vm.stopPrank();

        // Sequencer observes withdrawals
        _sequencerObserveWithdrawal(0, alice, alice, 500e6, bytes32(0), 0, alice, "");
        _sequencerObserveWithdrawal(1, bob, bob, 1000e6, bytes32(0), 0, bob, "");

        // Submit batch with both withdrawals
        l2BlockHash = keccak256(abi.encode(l2BlockHash, "withdrawals"));
        _sequencerSubmitBatch(processedHash);

        // Build expected queue hash (oldest = outermost, innermost wraps EMPTY_SENTINEL)
        Withdrawal memory w0 = _withdrawal(1, alice, alice, 500e6, bytes32(0), 0, alice, "");
        Withdrawal memory w1 = _withdrawal(2, bob, bob, 1000e6, bytes32(0), 0, bob, "");
        bytes32 innerHash = keccak256(abi.encode(w1, EMPTY_SENTINEL));
        bytes32 queueHash = keccak256(abi.encode(w0, innerHash));
        // Both withdrawals are in slot 0 (same batch)
        assertEq(l1Portal.withdrawalQueueSlot(0), queueHash);

        // Process withdrawals in order
        uint256 aliceBefore = l2ZoneToken.balanceOf(alice);
        uint256 bobBefore = l2ZoneToken.balanceOf(bob);

        l1Portal.processWithdrawal(w0, innerHash);
        assertEq(l2ZoneToken.balanceOf(alice), aliceBefore + 500e6);

        l1Portal.processWithdrawal(w1, bytes32(0)); // 0 = last item in slot
        assertEq(l2ZoneToken.balanceOf(bob), bobBefore + 1000e6);
    }

    function test_fullFlow_withdrawalWithCallback() public {
        // Setup: deposit to zone
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 1000e6);
        l1Portal.deposit(address(l2ZoneToken), alice, 1000e6, bytes32(""));
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));
        bytes32 processedHash = _sequencerRelayDepositsToL2();
        _sequencerSubmitBatch(processedHash);

        // Request withdrawal with callback
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), 500e6);
        l2Outbox.requestWithdrawal(
            address(l2ZoneToken),
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
        Withdrawal memory w = _withdrawal(
            1,
            alice,
            address(withdrawalReceiver),
            500e6,
            bytes32(0),
            100_000,
            alice,
            "callback_data"
        );
        l1Portal.processWithdrawal(w, bytes32(0));

        // Verify callback was executed
        assertEq(l2ZoneToken.balanceOf(address(withdrawalReceiver)), 500e6);
        assertEq(withdrawalReceiver.lastSenderTag(), _senderTag(alice, 1));
        assertEq(withdrawalReceiver.lastAmount(), 500e6);
        assertEq(withdrawalReceiver.lastCallbackData(), "callback_data");
    }

    function test_fullFlow_bounceBackOnCallbackFailure() public {
        // Setup: deposit to zone
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 1000e6);
        l1Portal.deposit(address(l2ZoneToken), alice, 1000e6, bytes32(""));
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));
        bytes32 processedHash = _sequencerRelayDepositsToL2();
        _sequencerSubmitBatch(processedHash);

        // Request withdrawal with callback that will fail
        withdrawalReceiver.setShouldAccept(false);
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), 500e6);
        l2Outbox.requestWithdrawal(
            address(l2ZoneToken),
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
        Withdrawal memory w = _withdrawal(
            1, alice, address(withdrawalReceiver), 500e6, bytes32(0), 100_000, alice, ""
        );
        l1Portal.processWithdrawal(w, bytes32(0));

        // Verify receiver did NOT get funds (transfer reverted)
        assertEq(l2ZoneToken.balanceOf(address(withdrawalReceiver)), 0);

        // Verify bounce-back deposit was created
        assertTrue(l1Portal.currentDepositQueueHash() != depositHashBefore);
    }

    function test_fullFlow_transferOnL2() public {
        // Deposit to Alice
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 1000e6);
        l1Portal.deposit(address(l2ZoneToken), alice, 1000e6, bytes32(""));
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));
        _sequencerRelayDepositsToL2();

        // Alice transfers to Bob on zone
        vm.prank(alice);
        l2ZoneToken.transfer(bob, 300e6);

        // Verify zone balances (alice net = 100K, then -300e6 transfer; bob = 100K + 300e6)
        assertEq(l2ZoneToken.balanceOf(alice), 100_000e6 - 300e6);
        assertEq(l2ZoneToken.balanceOf(bob), 100_000e6 + 300e6);

        // Bob withdraws on zone
        vm.startPrank(bob);
        l2ZoneToken.approve(address(l2Outbox), 300e6);
        l2Outbox.requestWithdrawal(address(l2ZoneToken), bob, 300e6, bytes32(0), 0, bob, "");
        vm.stopPrank();

        // Verify Bob's zone balance debited (100K + 300e6 received - 300e6 withdrawn)
        assertEq(l2ZoneToken.balanceOf(bob), 100_000e6);
        assertEq(l2Outbox.nextWithdrawalIndex(), 1);
    }

    function test_l2_insufficientBalanceReverts() public {
        // Deposit to Alice
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 1000e6);
        l1Portal.deposit(address(l2ZoneToken), alice, 1000e6, bytes32(""));
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));
        _sequencerRelayDepositsToL2();

        // Alice tries to withdraw more than balance (net balance is 100_000e6 after deposit+mint)
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), type(uint256).max);
        vm.expectRevert(MockZoneToken.InsufficientBalance.selector);
        l2Outbox.requestWithdrawal(
            address(l2ZoneToken), alice, uint128(100_001e6), bytes32(0), 0, alice, ""
        );
        vm.stopPrank();
    }

    function test_l2_transferInsufficientBalance() public {
        // Deposit to Alice
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 1000e6);
        l1Portal.deposit(address(l2ZoneToken), alice, 1000e6, bytes32(""));
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));
        _sequencerRelayDepositsToL2();

        // Alice tries to transfer more than balance (net balance is 100_000e6 after deposit+mint)
        vm.prank(alice);
        vm.expectRevert(MockZoneToken.InsufficientBalance.selector);
        l2ZoneToken.transfer(bob, 100_001e6);
    }

    function test_l2_depositHashMismatchAllowed() public {
        // Deposit on L1
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 1000e6);
        l1Portal.deposit(address(l2ZoneToken), alice, 1000e6, bytes32(""));
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));

        // Set up mock with different hash (simulating more deposits pending)
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, bytes32("different hash")
        );

        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] = pendingDeposits[0].deposit;

        // Should succeed — proof validates ancestor contiguity, not exact match
        vm.prank(admin);
        l2Inbox.advanceTempo(
            "", _wrapDeposits(deposits), new DecryptionData[](0), new EnabledToken[](0)
        );
    }

    function test_l2_callbackRequiresFallbackRecipient() public {
        // Deposit to Alice
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 1000e6);
        l1Portal.deposit(address(l2ZoneToken), alice, 1000e6, bytes32(""));
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));
        _sequencerRelayDepositsToL2();

        // Try callback without fallback recipient
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), 500e6);
        vm.expectRevert(ZoneOutbox.InvalidFallbackRecipient.selector);
        l2Outbox.requestWithdrawal(
            address(l2ZoneToken),
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
        l2ZoneToken.approve(address(l1Portal), 1000e6);
        l1Portal.deposit(address(l2ZoneToken), alice, 1000e6, bytes32(""));
        vm.stopPrank();

        _sequencerObserveDeposit(alice, alice, 1000e6, bytes32(""));

        bytes32 expectedHash = pendingDeposits[0].newCurrentDepositQueueHash;
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, expectedHash
        );

        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] = pendingDeposits[0].deposit;

        // Non-sequencer tries to advance
        vm.prank(alice);
        vm.expectRevert(IZoneInbox.OnlySequencer.selector);
        l2Inbox.advanceTempo(
            "", _wrapDeposits(deposits), new DecryptionData[](0), new EnabledToken[](0)
        );
    }

    /*//////////////////////////////////////////////////////////////
                    STORAGE LAYOUT VERIFICATION TESTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Verify PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT matches the actual ZonePortal storage layout.
    /// @dev This is a critical regression test. If ZonePortal's storage layout changes,
    ///      this test will fail, preventing silent slot mismatches.
    function test_storageLayout_currentDepositQueueHashSlot() public {
        // Make a deposit to get a non-zero currentDepositQueueHash
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 1000e6);
        l1Portal.deposit(address(l2ZoneToken), alice, 1000e6, bytes32("layout-test"));
        vm.stopPrank();

        // Read via vm.load using our constant
        bytes32 fromSlot = vm.load(address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT);

        // Compare against the public getter
        assertEq(
            fromSlot,
            l1Portal.currentDepositQueueHash(),
            "PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT does not match actual storage position"
        );

        // Sanity: value should be non-zero after deposit
        assertTrue(fromSlot != bytes32(0), "deposit queue hash should be non-zero after deposit");
    }

    /*//////////////////////////////////////////////////////////////
            ENCRYPTED DEPOSIT INTEGRATION TESTS — HELPERS
    //////////////////////////////////////////////////////////////*/

    // secp256k1 generator point X (known valid point on curve)
    bytes32 internal constant VALID_SECP256K1_X =
        0x79BE667EF9DCBBAC55A06295CE870B07029BFCDB2DCE28D959F2815B16F81798;

    // Test private keys for encryption key PoP
    uint256 internal constant ENC_KEY_1 = 1;
    uint256 internal constant ENC_KEY_2 = 2;

    /// @notice Observed encrypted deposit from L1 (simulating sequencer watching events)
    struct ObservedEncryptedDeposit {
        EncryptedDeposit encDeposit;
        bytes32 newCurrentDepositQueueHash;
    }

    /// @notice Pending encrypted deposit observations
    ObservedEncryptedDeposit[] internal pendingEncryptedDeposits;

    /// @notice Helper: set encryption key on L1 portal with proof of possession
    function _setEncKeyOnL1(uint256 privateKey) internal returns (bytes32 x, uint8 yParity) {
        Vm.Wallet memory w = vm.createWallet(privateKey);
        x = bytes32(w.publicKeyX);
        yParity = w.publicKeyY % 2 == 0 ? 0x02 : 0x03;
        bytes32 message = keccak256(abi.encode(address(l1Portal), x, yParity));
        (uint8 v, bytes32 r, bytes32 s) = vm.sign(w.privateKey, message);
        l1Portal.setSequencerEncryptionKey(x, yParity, v, r, s);
    }

    /// @notice Helper: create an encrypted deposit payload
    function _makeEncryptedPayload() internal pure returns (EncryptedDepositPayload memory) {
        return EncryptedDepositPayload({
            ephemeralPubkeyX: VALID_SECP256K1_X,
            ephemeralPubkeyYParity: 0x02,
            ciphertext: new bytes(64),
            nonce: bytes12(0),
            tag: bytes16(0)
        });
    }

    /// @notice Simulate sequencer observing an encrypted deposit event on L1
    function _sequencerObserveEncryptedDeposit(
        address sender,
        uint128 netAmount,
        uint256 keyIndex,
        EncryptedDepositPayload memory encrypted
    )
        internal
        returns (bytes32 newHash)
    {
        EncryptedDeposit memory ed = EncryptedDeposit({
            token: address(l2ZoneToken),
            sender: sender,
            amount: netAmount,
            keyIndex: keyIndex,
            encrypted: encrypted
        });

        // Calculate the new hash (matches what portal computes via DepositQueueLib)
        bytes32 prevHash;
        if (pendingDeposits.length > 0) {
            prevHash = pendingDeposits[pendingDeposits.length - 1].newCurrentDepositQueueHash;
        } else if (pendingEncryptedDeposits.length > 0) {
            prevHash =
            pendingEncryptedDeposits[pendingEncryptedDeposits.length - 1].newCurrentDepositQueueHash;
        } else {
            prevHash = l2Inbox.processedDepositQueueHash();
        }

        newHash = keccak256(abi.encode(DepositType.Encrypted, ed, prevHash));
        pendingEncryptedDeposits.push(
            ObservedEncryptedDeposit({ encDeposit: ed, newCurrentDepositQueueHash: newHash })
        );
    }

    /// @notice Set up encryption key mock storage on zone side so ZoneInbox._readEncryptionKey works
    function _setupEncryptionKeyMockOnZone(
        uint256 keyIndex,
        bytes32 keyX,
        uint8 keyYParity
    )
        internal
    {
        uint256 base = uint256(keccak256(abi.encode(uint256(PORTAL_ENCRYPTION_KEYS_SLOT))));
        uint256 slotX = base + (keyIndex * 2);
        uint256 slotMeta = slotX + 1;
        l2TempoState.setMockStorageValue(address(l1Portal), bytes32(slotX), keyX);
        l2TempoState.setMockStorageValue(
            address(l1Portal), bytes32(slotMeta), bytes32(uint256(keyYParity))
        );
    }

    /// @notice Set up precompile mocks for successful encrypted deposit decryption
    function _setupPrecompileMocksSuccess(address recipient, bytes32 memo) internal {
        vm.etch(CHAUM_PEDERSEN_VERIFY, hex"00");
        vm.etch(AES_GCM_DECRYPT, hex"00");

        // Mock Chaum-Pedersen to return valid
        vm.mockCall(
            CHAUM_PEDERSEN_VERIFY,
            abi.encodeWithSelector(IChaumPedersenVerify.verifyProof.selector),
            abi.encode(true)
        );

        // Mock AES-GCM to return expected plaintext
        bytes memory plaintext = EncryptedDepositLib.encodePlaintext(recipient, memo);
        vm.mockCall(
            AES_GCM_DECRYPT,
            abi.encodeWithSelector(IAesGcmDecrypt.decrypt.selector),
            abi.encode(plaintext, true)
        );
    }

    /// @notice Set up precompile mocks for failed AES-GCM decryption (bounce)
    function _setupPrecompileMocksFail() internal {
        vm.etch(CHAUM_PEDERSEN_VERIFY, hex"00");
        vm.etch(AES_GCM_DECRYPT, hex"00");

        // Mock Chaum-Pedersen to return valid (proof is fine, decryption fails)
        vm.mockCall(
            CHAUM_PEDERSEN_VERIFY,
            abi.encodeWithSelector(IChaumPedersenVerify.verifyProof.selector),
            abi.encode(true)
        );

        // Mock AES-GCM to return failure
        vm.mockCall(
            AES_GCM_DECRYPT,
            abi.encodeWithSelector(IAesGcmDecrypt.decrypt.selector),
            abi.encode(new bytes(0), false)
        );
    }

    /// @notice Simulate sequencer relaying a single encrypted deposit to the zone
    /// @dev Handles all the mock setup, builds the unified queue entries, and calls advanceTempo
    function _sequencerRelayEncryptedDepositsToL2(
        address decryptedTo,
        bytes32 decryptedMemo,
        bool shouldSucceed
    )
        internal
        returns (bytes32 newProcessedHash)
    {
        require(pendingEncryptedDeposits.length > 0, "no encrypted deposits to relay");
        require(
            pendingDeposits.length == 0, "use _sequencerRelayMixedDepositsToL2 for mixed queues"
        );

        // Build queued deposits array
        QueuedDeposit[] memory queued = new QueuedDeposit[](pendingEncryptedDeposits.length);
        DecryptionData[] memory decs = new DecryptionData[](pendingEncryptedDeposits.length);

        for (uint256 i = 0; i < pendingEncryptedDeposits.length; i++) {
            queued[i] = QueuedDeposit({
                depositType: DepositType.Encrypted,
                depositData: abi.encode(pendingEncryptedDeposits[i].encDeposit)
            });
            decs[i] = DecryptionData({
                sharedSecret: bytes32(uint256(0xDEAD)),
                sharedSecretYParity: 0x02,
                to: decryptedTo,
                memo: decryptedMemo,
                cpProof: ChaumPedersenProof({ s: bytes32(uint256(1)), c: bytes32(uint256(2)) })
            });
        }

        // Get expected final hash
        newProcessedHash =
        pendingEncryptedDeposits[pendingEncryptedDeposits.length - 1].newCurrentDepositQueueHash;

        // Set up mock: TempoState will return this hash when reading from portal
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, newProcessedHash
        );

        // Mock precompiles
        if (shouldSucceed) {
            _setupPrecompileMocksSuccess(decryptedTo, decryptedMemo);
        } else {
            _setupPrecompileMocksFail();
        }

        // Process on zone via advanceTempo
        vm.prank(admin);
        l2Inbox.advanceTempo("", queued, decs, new EnabledToken[](0));

        // Clear pending
        delete pendingEncryptedDeposits;

        // Update zone block hash (simulated)
        l2BlockHash = keccak256(abi.encode(l2BlockHash, "enc-deposits", newProcessedHash));
    }

    /*//////////////////////////////////////////////////////////////
            ENCRYPTED DEPOSIT INTEGRATION TESTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Full lifecycle: encrypted deposit on L1 → relay to zone → mint to decrypted recipient
    function test_fullFlow_encryptedDepositAndMint() public {
        // === STEP 1: Sequencer sets encryption key on L1 ===
        (bytes32 encKeyX, uint8 encKeyYParity) = _setEncKeyOnL1(ENC_KEY_1);

        // === STEP 2: Alice makes encrypted deposit on L1 ===
        uint128 depositAmount = 1000e6;
        uint128 fee = l1Portal.calculateDepositFee();
        uint128 netAmount = depositAmount - fee;
        EncryptedDepositPayload memory payload = _makeEncryptedPayload();

        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), depositAmount);
        bytes32 l1DepositHash =
            l1Portal.depositEncrypted(address(l2ZoneToken), depositAmount, 0, payload);
        vm.stopPrank();

        // Verify L1 state
        assertEq(l1Portal.currentDepositQueueHash(), l1DepositHash, "L1 queue hash mismatch");
        assertEq(
            l2ZoneToken.balanceOf(address(l1Portal)),
            depositAmount - fee,
            "Portal should hold net amount"
        );

        // === STEP 3: Sequencer observes encrypted deposit event ===
        _sequencerObserveEncryptedDeposit(alice, netAmount, 0, payload);

        // Verify our local hash matches L1
        assertEq(
            pendingEncryptedDeposits[0].newCurrentDepositQueueHash,
            l1DepositHash,
            "Observed hash must match L1 hash"
        );

        // === STEP 4: Set up zone-side encryption key mock and relay ===
        _setupEncryptionKeyMockOnZone(0, encKeyX, encKeyYParity);

        address decryptedRecipient = bob;
        bytes32 decryptedMemo = bytes32("secret memo");
        bytes32 newProcessedHash =
            _sequencerRelayEncryptedDepositsToL2(decryptedRecipient, decryptedMemo, true);

        // Verify zone state — tokens minted to decrypted recipient (bob starts with 100K)
        assertEq(
            l2ZoneToken.balanceOf(decryptedRecipient),
            100_000e6 + netAmount,
            "Recipient should receive tokens"
        );
        assertEq(
            l2ZoneToken.balanceOf(alice),
            100_000e6 - depositAmount,
            "Sender keeps remaining balance"
        );
        assertEq(
            l2Inbox.processedDepositQueueHash(), newProcessedHash, "Zone processed hash mismatch"
        );

        // === STEP 5: Submit batch to L1 ===
        _sequencerSubmitBatch(newProcessedHash);

        // Verify L1 batch state updated
        assertEq(l1Portal.withdrawalBatchIndex(), 1, "Batch index should advance");
        assertEq(l1Portal.blockHash(), l2BlockHash, "Block hash should update");
    }

    /// @notice Full lifecycle: encrypted deposit → decryption failure → funds returned to sender
    function test_fullFlow_encryptedDepositBounce() public {
        // === STEP 1: Sequencer sets encryption key on L1 ===
        (bytes32 encKeyX, uint8 encKeyYParity) = _setEncKeyOnL1(ENC_KEY_1);

        // === STEP 2: Alice makes encrypted deposit on L1 ===
        uint128 depositAmount = 1000e6;
        uint128 fee = l1Portal.calculateDepositFee();
        uint128 netAmount = depositAmount - fee;
        EncryptedDepositPayload memory payload = _makeEncryptedPayload();

        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), depositAmount);
        l1Portal.depositEncrypted(address(l2ZoneToken), depositAmount, 0, payload);
        vm.stopPrank();

        // === STEP 3: Sequencer observes and relays with FAILED decryption ===
        _sequencerObserveEncryptedDeposit(alice, netAmount, 0, payload);
        _setupEncryptionKeyMockOnZone(0, encKeyX, encKeyYParity);

        // Even with shouldSucceed=false, we need a to/memo for DecryptionData (values don't matter)
        bytes32 newProcessedHash =
            _sequencerRelayEncryptedDepositsToL2(address(0xBEEF), bytes32("wrong"), false);

        // Verify zone state — tokens bounced back to sender (alice = 100K - deposit + bounce)
        assertEq(
            l2ZoneToken.balanceOf(alice),
            100_000e6 - depositAmount + netAmount,
            "Sender should get bounced tokens"
        );
        assertEq(l2ZoneToken.balanceOf(address(0xBEEF)), 0, "Failed recipient should get nothing");
        assertEq(
            l2Inbox.processedDepositQueueHash(), newProcessedHash, "Zone processed hash mismatch"
        );

        // === STEP 4: Submit batch to L1 ===
        _sequencerSubmitBatch(newProcessedHash);
        assertEq(l1Portal.withdrawalBatchIndex(), 1, "Batch index should advance");
    }

    /// @notice Mixed queue: regular deposit + encrypted deposit in single advanceTempo
    function test_fullFlow_mixedRegularAndEncryptedDeposits() public {
        // === STEP 1: Set up encryption key ===
        (bytes32 encKeyX, uint8 encKeyYParity) = _setEncKeyOnL1(ENC_KEY_1);

        uint128 depositAmount = 1000e6;
        uint128 fee = l1Portal.calculateDepositFee();
        uint128 netAmount = depositAmount - fee;

        // === STEP 2: Alice makes a regular deposit ===
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), depositAmount * 2);
        bytes32 h1 =
            l1Portal.deposit(address(l2ZoneToken), alice, depositAmount, bytes32("regular"));
        vm.stopPrank();

        // === STEP 3: Bob makes an encrypted deposit ===
        EncryptedDepositPayload memory payload = _makeEncryptedPayload();
        vm.startPrank(bob);
        l2ZoneToken.approve(address(l1Portal), depositAmount);
        bytes32 h2 = l1Portal.depositEncrypted(address(l2ZoneToken), depositAmount, 0, payload);
        vm.stopPrank();

        // === STEP 4: Carol makes another regular deposit ===
        address carol = address(0x600);
        l2ZoneToken.setMinter(address(this), true);
        l2ZoneToken.mint(carol, 100_000e6);
        l2ZoneToken.setMinter(address(this), false);
        vm.startPrank(carol);
        l2ZoneToken.approve(address(l1Portal), depositAmount);
        bytes32 h3 = l1Portal.deposit(address(l2ZoneToken), carol, depositAmount, bytes32("carol"));
        vm.stopPrank();

        assertEq(l1Portal.currentDepositQueueHash(), h3, "L1 hash should be after 3rd deposit");

        // === STEP 5: Sequencer observes all deposits and manually builds the unified queue ===
        // We need to compute hashes in the same order the portal did

        // Regular deposit from alice
        Deposit memory d1 = Deposit({
            token: address(l2ZoneToken),
            sender: alice,
            to: alice,
            amount: depositAmount,
            memo: bytes32("regular")
        });
        bytes32 prevHash = l2Inbox.processedDepositQueueHash();
        bytes32 hash1 = keccak256(abi.encode(DepositType.Regular, d1, prevHash));
        assertEq(hash1, h1, "hash1 must match L1");

        // Encrypted deposit from bob
        EncryptedDeposit memory ed = EncryptedDeposit({
            token: address(l2ZoneToken),
            sender: bob,
            amount: netAmount,
            keyIndex: 0,
            encrypted: payload
        });
        bytes32 hash2 = keccak256(abi.encode(DepositType.Encrypted, ed, hash1));
        assertEq(hash2, h2, "hash2 must match L1");

        // Regular deposit from carol
        Deposit memory d3 = Deposit({
            token: address(l2ZoneToken),
            sender: carol,
            to: carol,
            amount: depositAmount,
            memo: bytes32("carol")
        });
        bytes32 hash3 = keccak256(abi.encode(DepositType.Regular, d3, hash2));
        assertEq(hash3, h3, "hash3 must match L1");

        // === STEP 6: Build the mixed queue and relay to zone ===
        QueuedDeposit[] memory queued = new QueuedDeposit[](3);
        queued[0] = QueuedDeposit({ depositType: DepositType.Regular, depositData: abi.encode(d1) });
        queued[1] =
            QueuedDeposit({ depositType: DepositType.Encrypted, depositData: abi.encode(ed) });
        queued[2] = QueuedDeposit({ depositType: DepositType.Regular, depositData: abi.encode(d3) });

        // Decryption data (only 1 encrypted deposit)
        address decryptedTo = address(0x700);
        bytes32 decryptedMemo = bytes32("bob-secret");
        DecryptionData[] memory decs = new DecryptionData[](1);
        decs[0] = DecryptionData({
            sharedSecret: bytes32(uint256(0xDEAD)),
            sharedSecretYParity: 0x02,
            to: decryptedTo,
            memo: decryptedMemo,
            cpProof: ChaumPedersenProof({ s: bytes32(uint256(1)), c: bytes32(uint256(2)) })
        });

        // Set up zone-side state
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, hash3
        );
        _setupEncryptionKeyMockOnZone(0, encKeyX, encKeyYParity);
        _setupPrecompileMocksSuccess(decryptedTo, decryptedMemo);

        vm.prank(admin);
        l2Inbox.advanceTempo("", queued, decs, new EnabledToken[](0));

        // === STEP 7: Verify all balances ===
        // alice: 100K - deposit + zone mint = 100K
        assertEq(l2ZoneToken.balanceOf(alice), 100_000e6, "Alice gets regular deposit");
        // decryptedTo (0x700) has no initial balance, receives only zone mint
        assertEq(
            l2ZoneToken.balanceOf(decryptedTo), netAmount, "Bob's encrypted recipient gets tokens"
        );
        // bob: 100K - deposit, no zone mint to bob (encrypted goes to decryptedTo)
        assertEq(
            l2ZoneToken.balanceOf(bob),
            100_000e6 - depositAmount,
            "Bob (sender) keeps remaining balance"
        );
        // carol: 100K - deposit + zone mint = 100K
        assertEq(l2ZoneToken.balanceOf(carol), 100_000e6, "Carol gets regular deposit");
        assertEq(l2Inbox.processedDepositQueueHash(), hash3, "Zone processed hash matches L1");

        // Total supply = initial (alice 100K + bob 100K + carol 100K) + zone mints
        assertEq(
            l2ZoneToken.totalSupply(),
            300_000e6 + depositAmount + netAmount + depositAmount,
            "Total supply should equal initial funding plus all zone mints"
        );

        // === STEP 8: Submit batch to L1 ===
        l2BlockHash = keccak256(abi.encode(l2BlockHash, "mixed-deposits", hash3));
        _sequencerSubmitBatch(hash3);
        assertEq(l1Portal.withdrawalBatchIndex(), 1, "Batch index should advance");
    }

    /// @notice Key rotation: two encrypted deposits using different encryption keys
    function test_fullFlow_keyRotationWithPendingDeposits() public {
        // === STEP 1: Sequencer sets first encryption key ===
        (bytes32 keyX1, uint8 keyYParity1) = _setEncKeyOnL1(ENC_KEY_1);

        // === STEP 2: Alice deposits with keyIndex=0 ===
        uint128 depositAmount = 1000e6;
        uint128 fee = l1Portal.calculateDepositFee();
        uint128 netAmount = depositAmount - fee;
        EncryptedDepositPayload memory payload1 = _makeEncryptedPayload();

        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), depositAmount);
        bytes32 h1 = l1Portal.depositEncrypted(address(l2ZoneToken), depositAmount, 0, payload1);
        vm.stopPrank();

        // === STEP 3: Sequencer rotates to second encryption key ===
        vm.roll(block.number + 100);
        (bytes32 keyX2, uint8 keyYParity2) = _setEncKeyOnL1(ENC_KEY_2);

        // === STEP 4: Bob deposits with keyIndex=1 ===
        EncryptedDepositPayload memory payload2 = _makeEncryptedPayload();

        vm.startPrank(bob);
        l2ZoneToken.approve(address(l1Portal), depositAmount);
        bytes32 h2 = l1Portal.depositEncrypted(address(l2ZoneToken), depositAmount, 1, payload2);
        vm.stopPrank();

        assertEq(l1Portal.currentDepositQueueHash(), h2, "L1 hash after both deposits");

        // === STEP 5: Compute expected hashes ===
        bytes32 prevHash = l2Inbox.processedDepositQueueHash();
        EncryptedDeposit memory ed1 = EncryptedDeposit({
            token: address(l2ZoneToken),
            sender: alice,
            amount: netAmount,
            keyIndex: 0,
            encrypted: payload1
        });
        bytes32 hash1 = keccak256(abi.encode(DepositType.Encrypted, ed1, prevHash));
        assertEq(hash1, h1, "hash1 must match L1");

        EncryptedDeposit memory ed2 = EncryptedDeposit({
            token: address(l2ZoneToken),
            sender: bob,
            amount: netAmount,
            keyIndex: 1,
            encrypted: payload2
        });
        bytes32 hash2 = keccak256(abi.encode(DepositType.Encrypted, ed2, hash1));
        assertEq(hash2, h2, "hash2 must match L1");

        // === STEP 6: Build queue and relay ===
        QueuedDeposit[] memory queued = new QueuedDeposit[](2);
        queued[0] =
            QueuedDeposit({ depositType: DepositType.Encrypted, depositData: abi.encode(ed1) });
        queued[1] =
            QueuedDeposit({ depositType: DepositType.Encrypted, depositData: abi.encode(ed2) });

        address aliceRecipient = address(0x700);
        bytes32 aliceMemo = bytes32("alice-secret");
        address bobRecipient = address(0x800);
        bytes32 bobMemo = bytes32("bob-secret");

        DecryptionData[] memory decs = new DecryptionData[](2);
        decs[0] = DecryptionData({
            sharedSecret: bytes32(uint256(0xDEAD)),
            sharedSecretYParity: 0x02,
            to: aliceRecipient,
            memo: aliceMemo,
            cpProof: ChaumPedersenProof({ s: bytes32(uint256(1)), c: bytes32(uint256(2)) })
        });
        decs[1] = DecryptionData({
            sharedSecret: bytes32(uint256(0xBEEF)),
            sharedSecretYParity: 0x02,
            to: bobRecipient,
            memo: bobMemo,
            cpProof: ChaumPedersenProof({ s: bytes32(uint256(3)), c: bytes32(uint256(4)) })
        });

        // Set up zone-side mocks: both keys in storage
        _setupEncryptionKeyMockOnZone(0, keyX1, keyYParity1);
        _setupEncryptionKeyMockOnZone(1, keyX2, keyYParity2);

        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, hash2
        );

        // Mock precompiles — we use broad mocks since vm.mockCall matches any input
        // For the success path, we need AES-GCM to return the correct plaintext.
        // Since vm.mockCall with just the selector matches ALL calls, we mock for the LAST
        // decryption (bobRecipient). For aliceRecipient we set up mock before advanceTempo,
        // but vm.mockCall replaces: we need a workaround.
        //
        // Since Foundry's vm.mockCall uses last-registered-wins for the same address+selector,
        // and encrypted deposits are processed sequentially, we can't differentiate two calls
        // to the same precompile with different expected outputs using selector-only mocking.
        //
        // Workaround: mock both precompiles to return bobRecipient's plaintext.
        // Alice's deposit will fail the plaintext check (dec.to != decryptedTo), causing a bounce.
        // We test a simpler scenario: mock for aliceRecipient so BOTH succeed with the same plaintext.
        //
        // Actually, the cleanest approach: make both deposits decrypt to the same recipient/memo.
        // This tests key rotation without needing differentiated mocks.

        // Use same recipient/memo for both decryptions
        address sharedRecipient = address(0x700);
        bytes32 sharedMemo = bytes32("shared-secret");
        decs[0].to = sharedRecipient;
        decs[0].memo = sharedMemo;
        decs[1].to = sharedRecipient;
        decs[1].memo = sharedMemo;

        vm.etch(CHAUM_PEDERSEN_VERIFY, hex"00");
        vm.etch(AES_GCM_DECRYPT, hex"00");
        vm.mockCall(
            CHAUM_PEDERSEN_VERIFY,
            abi.encodeWithSelector(IChaumPedersenVerify.verifyProof.selector),
            abi.encode(true)
        );
        bytes memory plaintext = EncryptedDepositLib.encodePlaintext(sharedRecipient, sharedMemo);
        vm.mockCall(
            AES_GCM_DECRYPT,
            abi.encodeWithSelector(IAesGcmDecrypt.decrypt.selector),
            abi.encode(plaintext, true)
        );

        vm.prank(admin);
        l2Inbox.advanceTempo("", queued, decs, new EnabledToken[](0));

        // === STEP 7: Verify ===
        // Both deposits go to sharedRecipient (no prior balance)
        assertEq(
            l2ZoneToken.balanceOf(sharedRecipient),
            netAmount * 2,
            "Recipient should receive both deposits"
        );
        // alice/bob: 100K - deposit, no zone mint to them (encrypted goes to sharedRecipient)
        assertEq(
            l2ZoneToken.balanceOf(alice), 100_000e6 - depositAmount, "Alice keeps remaining balance"
        );
        assertEq(
            l2ZoneToken.balanceOf(bob), 100_000e6 - depositAmount, "Bob keeps remaining balance"
        );
        assertEq(l2Inbox.processedDepositQueueHash(), hash2, "Zone processed hash matches L1");

        // === STEP 8: Submit batch ===
        l2BlockHash = keccak256(abi.encode(l2BlockHash, "key-rotation", hash2));
        _sequencerSubmitBatch(hash2);
        assertEq(l1Portal.withdrawalBatchIndex(), 1, "Batch index should advance");
    }

}
