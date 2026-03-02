// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { TIP20 } from "../../src/TIP20.sol";
import {
    BlockTransition,
    DecryptionData,
    Deposit,
    DepositQueueTransition,
    DepositType,
    EnabledToken,
    IWithdrawalReceiver,
    IZoneFactory,
    IZonePortal,
    PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
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

/// @notice Mock receiver that tracks received amounts
contract TrackingReceiver is IWithdrawalReceiver {

    uint256 public totalReceived;
    uint256 public callCount;

    function onWithdrawalReceived(
        address,
        address,
        uint128 amount,
        bytes calldata
    )
        external
        returns (bytes4)
    {
        totalReceived += amount;
        callCount++;
        return IWithdrawalReceiver.onWithdrawalReceived.selector;
    }

}

/// @title ZoneIntegrationTest
/// @notice Comprehensive integration tests for the full zone lifecycle
contract ZoneIntegrationTest is BaseTest {

    // L1 contracts
    ZoneFactory public l1Factory;
    ZonePortal public l1Portal;

    // L2 contracts
    MockZoneToken public l2ZoneToken;
    MockTempoState public l2TempoState;
    ZoneConfig public l2Config;
    ZoneInbox public l2Inbox;
    ZoneOutbox public l2Outbox;

    // Helpers
    TrackingReceiver public receiver;
    uint64 public zoneId;

    bytes32 constant GENESIS_BLOCK_HASH = keccak256("genesis");
    bytes32 constant GENESIS_TEMPO_BLOCK_HASH = keccak256("tempoGenesis");
    uint64 public genesisTempoBlockNumber;

    function setUp() public override {
        super.setUp();

        l1Factory = new ZoneFactory(); // Keep for verifier only
        receiver = new TrackingReceiver();

        // Deploy zone token FIRST
        l2ZoneToken = new MockZoneToken("Zone USD", "zUSD");

        // Fund test accounts with zone token (for L1 deposits)
        l2ZoneToken.setMinter(address(this), true);
        l2ZoneToken.mint(alice, 1_000_000e6);
        l2ZoneToken.mint(bob, 1_000_000e6);
        l2ZoneToken.mint(charlie, 1_000_000e6);
        l2ZoneToken.setMinter(address(this), false);

        genesisTempoBlockNumber = uint64(block.number);

        // Deploy messenger and portal directly (bypass factory TIP20 prefix check)
        uint256 currentNonce = vm.getNonce(address(this));
        address predictedPortal = vm.computeCreateAddress(address(this), currentNonce + 1);
        ZoneMessenger messengerContract = new ZoneMessenger(predictedPortal);
        l1Portal = new ZonePortal(
            1,
            address(l2ZoneToken),
            address(messengerContract),
            admin,
            l1Factory.verifier(),
            GENESIS_BLOCK_HASH,
            genesisTempoBlockNumber
        );
        zoneId = 1;

        // L2 setup
        l2TempoState = new MockTempoState(admin, GENESIS_TEMPO_BLOCK_HASH, genesisTempoBlockNumber);
        l2Config = new ZoneConfig(address(l1Portal), address(l2TempoState));
        l2TempoState.setMockStorageValue(
            address(l1Portal), bytes32(uint256(0)), bytes32(uint256(uint160(admin)))
        );
        l2Inbox = new ZoneInbox(address(l2Config), address(l1Portal), address(l2TempoState));
        l2Outbox = new ZoneOutbox(address(l2Config));

        l2ZoneToken.setMinter(address(l2Inbox), true);
        l2ZoneToken.setBurner(address(l2Outbox), true);
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

    function _advanceTempo(Deposit[] memory deposits) internal {
        l2Inbox.advanceTempo(
            "", _wrapDeposits(deposits), new DecryptionData[](0), new EnabledToken[](0)
        );
    }

    /*//////////////////////////////////////////////////////////////
                   MULTI-USER DEPOSIT FLOW TESTS
    //////////////////////////////////////////////////////////////*/

    function test_multiUserDeposit_processedCorrectly() public {
        // Alice, Bob, Charlie all deposit
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 10_000e6);
        l1Portal.deposit(address(l2ZoneToken), alice, 1000e6, bytes32("alice1"));
        l1Portal.deposit(address(l2ZoneToken), alice, 2000e6, bytes32("alice2"));
        vm.stopPrank();

        vm.startPrank(bob);
        l2ZoneToken.approve(address(l1Portal), 5000e6);
        l1Portal.deposit(address(l2ZoneToken), bob, 3000e6, bytes32("bob1"));
        vm.stopPrank();

        vm.startPrank(charlie);
        l2ZoneToken.approve(address(l1Portal), 2000e6);
        l1Portal.deposit(address(l2ZoneToken), charlie, 500e6, bytes32("charlie1"));
        vm.stopPrank();

        // Build deposit array
        Deposit[] memory deposits = new Deposit[](4);
        deposits[0] = Deposit({
            token: address(l2ZoneToken),
            sender: alice,
            to: alice,
            amount: 1000e6,
            memo: bytes32("alice1")
        });
        deposits[1] = Deposit({
            token: address(l2ZoneToken),
            sender: alice,
            to: alice,
            amount: 2000e6,
            memo: bytes32("alice2")
        });
        deposits[2] = Deposit({
            token: address(l2ZoneToken), sender: bob, to: bob, amount: 3000e6, memo: bytes32("bob1")
        });
        deposits[3] = Deposit({
            token: address(l2ZoneToken),
            sender: charlie,
            to: charlie,
            amount: 500e6,
            memo: bytes32("charlie1")
        });

        // Set up L2 mock — hash chain uses l2ZoneToken consistently
        bytes32 l2h0 = bytes32(0);
        bytes32 l2h1 = keccak256(abi.encode(DepositType.Regular, deposits[0], l2h0));
        bytes32 l2h2 = keccak256(abi.encode(DepositType.Regular, deposits[1], l2h1));
        bytes32 l2h3 = keccak256(abi.encode(DepositType.Regular, deposits[2], l2h2));
        bytes32 l2h4 = keccak256(abi.encode(DepositType.Regular, deposits[3], l2h3));
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, l2h4
        );

        // Capture balances after L1 deposits but before L2 minting
        uint256 alicePre = l2ZoneToken.balanceOf(alice);
        uint256 bobPre = l2ZoneToken.balanceOf(bob);
        uint256 charliePre = l2ZoneToken.balanceOf(charlie);
        uint256 supplyPre = l2ZoneToken.totalSupply();

        // Process on L2
        vm.prank(admin);
        _advanceTempo(deposits);

        // Verify L2 minting: each user receives their deposited amounts
        assertEq(l2ZoneToken.balanceOf(alice), alicePre + 3000e6);
        assertEq(l2ZoneToken.balanceOf(bob), bobPre + 3000e6);
        assertEq(l2ZoneToken.balanceOf(charlie), charliePre + 500e6);
        assertEq(l2ZoneToken.totalSupply(), supplyPre + 6500e6);
    }

    /*//////////////////////////////////////////////////////////////
               INCREMENTAL BATCH PROCESSING TESTS
    //////////////////////////////////////////////////////////////*/

    function test_incrementalBatchProcessing() public {
        // Batch 1: Two deposits
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 10_000e6);
        bytes32 d1 = l1Portal.deposit(address(l2ZoneToken), alice, 1000e6, bytes32("d1"));
        bytes32 d2 = l1Portal.deposit(address(l2ZoneToken), alice, 2000e6, bytes32("d2"));
        vm.stopPrank();

        // Process only first deposit
        Deposit[] memory batch1 = new Deposit[](1);
        batch1[0] = Deposit({
            token: address(l2ZoneToken),
            sender: alice,
            to: alice,
            amount: 1000e6,
            memo: bytes32("d1")
        });

        // Deposit hash uses l2ZoneToken consistently
        bytes32 l2Hash1 = keccak256(abi.encode(DepositType.Regular, batch1[0], bytes32(0)));
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, l2Hash1
        );
        uint256 alicePreBatch1 = l2ZoneToken.balanceOf(alice);
        vm.prank(admin);
        _advanceTempo(batch1);

        assertEq(l2ZoneToken.balanceOf(alice), alicePreBatch1 + 1000e6);
        assertEq(l2Inbox.processedDepositQueueHash(), l2Hash1);

        // Submit L1 batch for first deposit
        vm.roll(block.number + 1);
        l1Portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({
                prevBlockHash: l1Portal.blockHash(), nextBlockHash: keccak256("s1")
            }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: d1 }),
            bytes32(0),
            "",
            ""
        );

        // Portal no longer tracks processed hash - that's on the zone
        assertEq(l1Portal.currentDepositQueueHash(), d2);

        // More deposits arrive
        vm.prank(alice);
        l1Portal.deposit(address(l2ZoneToken), alice, 3000e6, bytes32("d3"));

        // Process remaining deposits
        Deposit[] memory batch2 = new Deposit[](2);
        batch2[0] = Deposit({
            token: address(l2ZoneToken),
            sender: alice,
            to: alice,
            amount: 2000e6,
            memo: bytes32("d2")
        });
        batch2[1] = Deposit({
            token: address(l2ZoneToken),
            sender: alice,
            to: alice,
            amount: 3000e6,
            memo: bytes32("d3")
        });

        // Compute L2 hash chain continuing from l2Hash1
        bytes32 l2Hash2 = keccak256(abi.encode(DepositType.Regular, batch2[0], l2Hash1));
        bytes32 l2Hash3 = keccak256(abi.encode(DepositType.Regular, batch2[1], l2Hash2));
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, l2Hash3
        );
        uint256 alicePreBatch2 = l2ZoneToken.balanceOf(alice);
        vm.prank(admin);
        _advanceTempo(batch2);

        assertEq(l2ZoneToken.balanceOf(alice), alicePreBatch2 + 5000e6);
    }

    /*//////////////////////////////////////////////////////////////
              WITHDRAWAL WITH CALLBACK SUCCESS FLOW
    //////////////////////////////////////////////////////////////*/

    function test_withdrawalWithCallback_fullFlow() public {
        // Setup: Alice deposits
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 10_000e6);
        bytes32 depositHash =
            l1Portal.deposit(address(l2ZoneToken), alice, 5000e6, bytes32("deposit"));
        vm.stopPrank();

        // Process deposit on L2
        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] = Deposit({
            token: address(l2ZoneToken),
            sender: alice,
            to: alice,
            amount: 5000e6,
            memo: bytes32("deposit")
        });
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, depositHash
        );
        vm.prank(admin);
        _advanceTempo(deposits);

        // Alice requests withdrawal with callback
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), 2000e6);
        l2Outbox.requestWithdrawal(
            address(l2ZoneToken),
            address(receiver),
            2000e6,
            bytes32("payment"),
            100_000,
            alice,
            "callback"
        );
        vm.stopPrank();

        // Finalize L2 batch
        vm.prank(admin);
        bytes32 withdrawalHash =
            l2Outbox.finalizeWithdrawalBatch(type(uint256).max, uint64(block.number));

        // Submit L1 batch
        vm.roll(block.number + 1);
        l1Portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({
                prevBlockHash: l1Portal.blockHash(), nextBlockHash: keccak256("state")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0), nextProcessedHash: depositHash
                }),
            withdrawalHash,
            "",
            ""
        );

        // Process withdrawal
        Withdrawal memory w = Withdrawal({
            token: address(l2ZoneToken),
            sender: alice,
            to: address(receiver),
            amount: 2000e6,
            fee: 0,
            memo: bytes32("payment"),
            gasLimit: 100_000,
            fallbackRecipient: alice,
            callbackData: "callback"
        });
        l1Portal.processWithdrawal(w, bytes32(0));

        // Verify callback was executed
        assertEq(receiver.callCount(), 1);
        assertEq(receiver.totalReceived(), 2000e6);
        assertEq(l2ZoneToken.balanceOf(address(receiver)), 2000e6);
    }

    /*//////////////////////////////////////////////////////////////
                MULTIPLE BATCHES WITH WITHDRAWALS
    //////////////////////////////////////////////////////////////*/

    function test_multipleBatches_withdrawalsInDifferentSlots() public {
        // Initial deposit
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 100_000e6);
        bytes32 depositHash =
            l1Portal.deposit(address(l2ZoneToken), alice, 50_000e6, bytes32("big deposit"));
        vm.stopPrank();

        // Process on L2
        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] = Deposit({
            token: address(l2ZoneToken),
            sender: alice,
            to: alice,
            amount: 50_000e6,
            memo: bytes32("big deposit")
        });
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, depositHash
        );
        vm.prank(admin);
        _advanceTempo(deposits);

        // First batch: Alice withdraws to Bob
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), 50_000e6);
        l2Outbox.requestWithdrawal(
            address(l2ZoneToken), bob, 1000e6, bytes32("to bob"), 0, alice, ""
        );
        vm.stopPrank();

        // Each finalizeWithdrawalBatch requires blockNumber == block.number, and each
        // batch needs a distinct block.number, so we advance before each finalize+submit pair.
        vm.roll(block.number + 1);

        vm.prank(admin);
        bytes32 wHash1 = l2Outbox.finalizeWithdrawalBatch(type(uint256).max, uint64(block.number));

        l1Portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({
                prevBlockHash: l1Portal.blockHash(), nextBlockHash: keccak256("s1")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0), nextProcessedHash: depositHash
                }),
            wHash1,
            "",
            ""
        );

        // Second batch: Alice withdraws to Charlie
        vm.startPrank(alice);
        l2Outbox.requestWithdrawal(
            address(l2ZoneToken), charlie, 2000e6, bytes32("to charlie"), 0, alice, ""
        );
        vm.stopPrank();

        vm.roll(block.number + 1);

        vm.prank(admin);
        bytes32 wHash2 = l2Outbox.finalizeWithdrawalBatch(type(uint256).max, uint64(block.number));

        l1Portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({
                prevBlockHash: l1Portal.blockHash(), nextBlockHash: keccak256("s2")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0), nextProcessedHash: depositHash
                }),
            wHash2,
            "",
            ""
        );

        // Third batch: Alice withdraws to herself
        vm.startPrank(alice);
        l2Outbox.requestWithdrawal(
            address(l2ZoneToken), alice, 3000e6, bytes32("to self"), 0, alice, ""
        );
        vm.stopPrank();

        vm.roll(block.number + 1);

        vm.prank(admin);
        bytes32 wHash3 = l2Outbox.finalizeWithdrawalBatch(type(uint256).max, uint64(block.number));

        l1Portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({
                prevBlockHash: l1Portal.blockHash(), nextBlockHash: keccak256("s3")
            }),
            DepositQueueTransition({
                    prevProcessedHash: bytes32(0), nextProcessedHash: depositHash
                }),
            wHash3,
            "",
            ""
        );

        // Verify queue state
        assertEq(l1Portal.withdrawalQueueHead(), 0);
        assertEq(l1Portal.withdrawalQueueTail(), 3);
        // Process in order (portal transfers l2ZoneToken from its balance)
        uint256 bobBefore = l2ZoneToken.balanceOf(bob);
        uint256 charlieBefore = l2ZoneToken.balanceOf(charlie);
        uint256 aliceBefore = l2ZoneToken.balanceOf(alice);

        Withdrawal memory w1 = Withdrawal({
            token: address(l2ZoneToken),
            sender: alice,
            to: bob,
            amount: 1000e6,
            fee: 0,
            memo: bytes32("to bob"),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        l1Portal.processWithdrawal(w1, bytes32(0));
        assertEq(l2ZoneToken.balanceOf(bob), bobBefore + 1000e6);

        Withdrawal memory w2 = Withdrawal({
            token: address(l2ZoneToken),
            sender: alice,
            to: charlie,
            amount: 2000e6,
            fee: 0,
            memo: bytes32("to charlie"),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        l1Portal.processWithdrawal(w2, bytes32(0));
        assertEq(l2ZoneToken.balanceOf(charlie), charlieBefore + 2000e6);

        Withdrawal memory w3 = Withdrawal({
            token: address(l2ZoneToken),
            sender: alice,
            to: alice,
            amount: 3000e6,
            fee: 0,
            memo: bytes32("to self"),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        l1Portal.processWithdrawal(w3, bytes32(0));
        assertEq(l2ZoneToken.balanceOf(alice), aliceBefore + 3000e6);

        // All processed
        assertEq(l1Portal.withdrawalQueueHead(), 3);
        assertFalse(l1Portal.withdrawalQueueHead() < l1Portal.withdrawalQueueTail());
    }

    /*//////////////////////////////////////////////////////////////
                    MIXED OPERATIONS FLOW
    //////////////////////////////////////////////////////////////*/

    function test_mixedFlow_depositsAndWithdrawalsInterleaved() public {
        // Phase 1: Initial deposits
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 100_000e6);
        l1Portal.deposit(address(l2ZoneToken), alice, 10_000e6, bytes32("d1"));
        vm.stopPrank();

        vm.startPrank(bob);
        l2ZoneToken.approve(address(l1Portal), 100_000e6);
        bytes32 d2 = l1Portal.deposit(address(l2ZoneToken), bob, 5000e6, bytes32("d2"));
        vm.stopPrank();

        // Process both deposits
        Deposit[] memory deposits1 = new Deposit[](2);
        deposits1[0] = Deposit({
            token: address(l2ZoneToken),
            sender: alice,
            to: alice,
            amount: 10_000e6,
            memo: bytes32("d1")
        });
        deposits1[1] = Deposit({
            token: address(l2ZoneToken), sender: bob, to: bob, amount: 5000e6, memo: bytes32("d2")
        });

        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, d2
        );
        vm.prank(admin);
        _advanceTempo(deposits1);

        // Phase 2: Withdrawals
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), 5000e6);
        l2Outbox.requestWithdrawal(address(l2ZoneToken), charlie, 2000e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        vm.startPrank(bob);
        l2ZoneToken.approve(address(l2Outbox), 3000e6);
        l2Outbox.requestWithdrawal(address(l2ZoneToken), charlie, 1500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        vm.prank(admin);
        bytes32 wHash = l2Outbox.finalizeWithdrawalBatch(type(uint256).max, uint64(block.number));

        // Phase 3: More deposits arrive while withdrawals are pending
        vm.startPrank(charlie);
        l2ZoneToken.approve(address(l1Portal), 20_000e6);
        bytes32 d3 = l1Portal.deposit(address(l2ZoneToken), charlie, 7500e6, bytes32("d3"));
        vm.stopPrank();

        // Submit batch with withdrawals
        vm.roll(block.number + 1);
        l1Portal.submitBatch(
            uint64(block.number - 1),
            0,
            BlockTransition({
                prevBlockHash: l1Portal.blockHash(), nextBlockHash: keccak256("s1")
            }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: d2 }),
            wHash,
            "",
            ""
        );

        // Process new deposit
        Deposit[] memory deposits2 = new Deposit[](1);
        deposits2[0] = Deposit({
            token: address(l2ZoneToken),
            sender: charlie,
            to: charlie,
            amount: 7500e6,
            memo: bytes32("d3")
        });

        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, d3
        );
        vm.prank(admin);
        _advanceTempo(deposits2);

        // Verify all L2 balances (initial 1M - deposited + minted - burned)
        // alice: 1M - 10k + 10k - 2k = 998k; bob: 1M - 5k + 5k - 1.5k = 998.5k
        // charlie: 1M - 7.5k + 7.5k = 1M
        assertEq(l2ZoneToken.balanceOf(alice), 1_000_000e6 - 2000e6);
        assertEq(l2ZoneToken.balanceOf(bob), 1_000_000e6 - 1500e6);
        assertEq(l2ZoneToken.balanceOf(charlie), 1_000_000e6);

        // Process withdrawals
        Withdrawal memory w1 = Withdrawal({
            token: address(l2ZoneToken),
            sender: alice,
            to: charlie,
            amount: 2000e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });
        Withdrawal memory w2 = Withdrawal({
            token: address(l2ZoneToken),
            sender: bob,
            to: charlie,
            amount: 1500e6,
            fee: 0,
            memo: bytes32(0),
            gasLimit: 0,
            fallbackRecipient: alice,
            callbackData: ""
        });

        bytes32 innerHash = keccak256(abi.encode(w2, EMPTY_SENTINEL));
        uint256 charlieBefore = l2ZoneToken.balanceOf(charlie);

        l1Portal.processWithdrawal(w1, innerHash);
        l1Portal.processWithdrawal(w2, bytes32(0));

        assertEq(l2ZoneToken.balanceOf(charlie), charlieBefore + 3500e6);
    }

    /*//////////////////////////////////////////////////////////////
                       INVARIANT CHECKS
    //////////////////////////////////////////////////////////////*/

    function test_invariant_totalSupplyMatchesNetDeposits() public {
        // Initial supply: 3 users × 1M = 3M (from setUp)
        uint256 initialSupply = l2ZoneToken.totalSupply();

        // Deposit 10000
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l1Portal), 10_000e6);
        bytes32 d1 = l1Portal.deposit(address(l2ZoneToken), alice, 10_000e6, bytes32("d1"));
        vm.stopPrank();

        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] = Deposit({
            token: address(l2ZoneToken),
            sender: alice,
            to: alice,
            amount: 10_000e6,
            memo: bytes32("d1")
        });
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, d1
        );
        vm.prank(admin);
        _advanceTempo(deposits);

        assertEq(l2ZoneToken.totalSupply(), initialSupply + 10_000e6);

        // Withdraw 3000
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), 3000e6);
        l2Outbox.requestWithdrawal(address(l2ZoneToken), bob, 3000e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(l2ZoneToken.totalSupply(), initialSupply + 10_000e6 - 3000e6); // Tokens burned on withdrawal request

        // Transfer on L2 shouldn't change supply
        vm.prank(alice);
        l2ZoneToken.transfer(bob, 2000e6);

        assertEq(l2ZoneToken.totalSupply(), initialSupply + 10_000e6 - 3000e6);
    }

    /*//////////////////////////////////////////////////////////////
                    STORAGE LAYOUT VERIFICATION TESTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Verify PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT matches the actual ZonePortal storage layout.
    /// @dev If ZonePortal's storage layout changes, this test will fail.
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

}
