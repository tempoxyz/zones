// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { TIP20 } from "../../src/TIP20.sol";
import {
    BlockTransition,
    DecryptionData,
    Deposit,
    DepositQueueTransition,
    DepositType,
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
import { ZoneOutbox } from "../../src/zone/ZoneOutbox.sol";
import { ZonePortal } from "../../src/zone/ZonePortal.sol";
import { BaseTest } from "../BaseTest.t.sol";

import { MockTempoState } from "./mocks/MockTempoState.sol";
import { MockVerifier } from "./mocks/MockVerifier.sol";
import { MockZoneToken } from "./mocks/MockZoneToken.sol";

/// @notice Mock receiver that tracks received amounts
contract TrackingReceiver is IWithdrawalReceiver {

    uint256 public totalReceived;
    uint256 public callCount;

    function onWithdrawalReceived(
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
    MockVerifier public l1Verifier;

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

        // L1 setup
        l1Factory = new ZoneFactory();
        l1Verifier = new MockVerifier();
        receiver = new TrackingReceiver();

        vm.startPrank(pathUSDAdmin);
        pathUSD.grantRole(_ISSUER_ROLE, pathUSDAdmin);
        pathUSD.mint(alice, 1_000_000e6);
        pathUSD.mint(bob, 1_000_000e6);
        pathUSD.mint(charlie, 1_000_000e6);
        vm.stopPrank();

        genesisTempoBlockNumber = uint64(block.number);

        // Create zone
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

        // L2 setup
        l2ZoneToken = new MockZoneToken("Zone USD", "zUSD");
        l2TempoState = new MockTempoState(admin, GENESIS_TEMPO_BLOCK_HASH, genesisTempoBlockNumber);
        l2Config = new ZoneConfig(address(l2ZoneToken), portalAddr, address(l2TempoState));
        l2TempoState.setMockStorageValue(
            portalAddr, bytes32(uint256(0)), bytes32(uint256(uint160(admin)))
        );
        l2Inbox = new ZoneInbox(
            address(l2Config), portalAddr, address(l2TempoState), address(l2ZoneToken)
        );
        l2Outbox = new ZoneOutbox(address(l2Config), address(l2ZoneToken));

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
        l2Inbox.advanceTempo("", _wrapDeposits(deposits), new DecryptionData[](0));
    }

    /*//////////////////////////////////////////////////////////////
                   MULTI-USER DEPOSIT FLOW TESTS
    //////////////////////////////////////////////////////////////*/

    function test_multiUserDeposit_processedCorrectly() public {
        // Alice, Bob, Charlie all deposit
        vm.startPrank(alice);
        pathUSD.approve(address(l1Portal), 10_000e6);
        bytes32 h1 = l1Portal.deposit(alice, 1000e6, bytes32("alice1"));
        bytes32 h2 = l1Portal.deposit(alice, 2000e6, bytes32("alice2"));
        vm.stopPrank();

        vm.startPrank(bob);
        pathUSD.approve(address(l1Portal), 5000e6);
        bytes32 h3 = l1Portal.deposit(bob, 3000e6, bytes32("bob1"));
        vm.stopPrank();

        vm.startPrank(charlie);
        pathUSD.approve(address(l1Portal), 2000e6);
        bytes32 h4 = l1Portal.deposit(charlie, 500e6, bytes32("charlie1"));
        vm.stopPrank();

        // Build deposit array
        Deposit[] memory deposits = new Deposit[](4);
        deposits[0] = Deposit({ sender: alice, to: alice, amount: 1000e6, memo: bytes32("alice1") });
        deposits[1] = Deposit({ sender: alice, to: alice, amount: 2000e6, memo: bytes32("alice2") });
        deposits[2] = Deposit({ sender: bob, to: bob, amount: 3000e6, memo: bytes32("bob1") });
        deposits[3] =
            Deposit({ sender: charlie, to: charlie, amount: 500e6, memo: bytes32("charlie1") });

        // Set up L2 mock
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, h4
        );

        // Process on L2
        vm.prank(admin);
        _advanceTempo(deposits);

        // Verify L2 balances
        assertEq(l2ZoneToken.balanceOf(alice), 3000e6);
        assertEq(l2ZoneToken.balanceOf(bob), 3000e6);
        assertEq(l2ZoneToken.balanceOf(charlie), 500e6);
        assertEq(l2ZoneToken.totalSupply(), 6500e6);
    }

    /*//////////////////////////////////////////////////////////////
               INCREMENTAL BATCH PROCESSING TESTS
    //////////////////////////////////////////////////////////////*/

    function test_incrementalBatchProcessing() public {
        // Batch 1: Two deposits
        vm.startPrank(alice);
        pathUSD.approve(address(l1Portal), 10_000e6);
        bytes32 d1 = l1Portal.deposit(alice, 1000e6, bytes32("d1"));
        bytes32 d2 = l1Portal.deposit(alice, 2000e6, bytes32("d2"));
        vm.stopPrank();

        // Process only first deposit
        Deposit[] memory batch1 = new Deposit[](1);
        batch1[0] = Deposit({ sender: alice, to: alice, amount: 1000e6, memo: bytes32("d1") });

        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, d1
        );
        vm.prank(admin);
        _advanceTempo(batch1);

        assertEq(l2ZoneToken.balanceOf(alice), 1000e6);
        assertEq(l2Inbox.processedDepositQueueHash(), d1);

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
        bytes32 d3 = l1Portal.deposit(alice, 3000e6, bytes32("d3"));

        // Process remaining deposits
        Deposit[] memory batch2 = new Deposit[](2);
        batch2[0] = Deposit({ sender: alice, to: alice, amount: 2000e6, memo: bytes32("d2") });
        batch2[1] = Deposit({ sender: alice, to: alice, amount: 3000e6, memo: bytes32("d3") });

        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, d3
        );
        vm.prank(admin);
        _advanceTempo(batch2);

        assertEq(l2ZoneToken.balanceOf(alice), 6000e6);
    }

    /*//////////////////////////////////////////////////////////////
              WITHDRAWAL WITH CALLBACK SUCCESS FLOW
    //////////////////////////////////////////////////////////////*/

    function test_withdrawalWithCallback_fullFlow() public {
        // Setup: Alice deposits
        vm.startPrank(alice);
        pathUSD.approve(address(l1Portal), 10_000e6);
        bytes32 depositHash = l1Portal.deposit(alice, 5000e6, bytes32("deposit"));
        vm.stopPrank();

        // Process deposit on L2
        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] =
            Deposit({ sender: alice, to: alice, amount: 5000e6, memo: bytes32("deposit") });
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, depositHash
        );
        vm.prank(admin);
        _advanceTempo(deposits);

        // Alice requests withdrawal with callback
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), 2000e6);
        l2Outbox.requestWithdrawal(
            address(receiver), 2000e6, bytes32("payment"), 100_000, alice, "callback"
        );
        vm.stopPrank();

        // Finalize L2 batch
        vm.prank(admin);
        bytes32 withdrawalHash = l2Outbox.finalizeWithdrawalBatch(type(uint256).max);

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
        assertEq(pathUSD.balanceOf(address(receiver)), 2000e6);
    }

    /*//////////////////////////////////////////////////////////////
                MULTIPLE BATCHES WITH WITHDRAWALS
    //////////////////////////////////////////////////////////////*/

    function test_multipleBatches_withdrawalsInDifferentSlots() public {
        // Initial deposit
        vm.startPrank(alice);
        pathUSD.approve(address(l1Portal), 100_000e6);
        bytes32 depositHash = l1Portal.deposit(alice, 50_000e6, bytes32("big deposit"));
        vm.stopPrank();

        // Process on L2
        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] =
            Deposit({ sender: alice, to: alice, amount: 50_000e6, memo: bytes32("big deposit") });
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, depositHash
        );
        vm.prank(admin);
        _advanceTempo(deposits);

        // First batch: Alice withdraws to Bob
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), 50_000e6);
        l2Outbox.requestWithdrawal(bob, 1000e6, bytes32("to bob"), 0, alice, "");
        vm.stopPrank();

        vm.prank(admin);
        bytes32 wHash1 = l2Outbox.finalizeWithdrawalBatch(type(uint256).max);

        vm.roll(block.number + 1);
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
        l2Outbox.requestWithdrawal(charlie, 2000e6, bytes32("to charlie"), 0, alice, "");
        vm.stopPrank();

        vm.prank(admin);
        bytes32 wHash2 = l2Outbox.finalizeWithdrawalBatch(type(uint256).max);

        vm.roll(block.number + 1);
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
        l2Outbox.requestWithdrawal(alice, 3000e6, bytes32("to self"), 0, alice, "");
        vm.stopPrank();

        vm.prank(admin);
        bytes32 wHash3 = l2Outbox.finalizeWithdrawalBatch(type(uint256).max);

        vm.roll(block.number + 1);
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
        // Process in order
        uint256 bobBefore = pathUSD.balanceOf(bob);
        uint256 charlieBefore = pathUSD.balanceOf(charlie);
        uint256 aliceBefore = pathUSD.balanceOf(alice);

        Withdrawal memory w1 = Withdrawal({
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
        assertEq(pathUSD.balanceOf(bob), bobBefore + 1000e6);

        Withdrawal memory w2 = Withdrawal({
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
        assertEq(pathUSD.balanceOf(charlie), charlieBefore + 2000e6);

        Withdrawal memory w3 = Withdrawal({
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
        assertEq(pathUSD.balanceOf(alice), aliceBefore + 3000e6);

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
        pathUSD.approve(address(l1Portal), 100_000e6);
        l1Portal.deposit(alice, 10_000e6, bytes32("d1"));
        vm.stopPrank();

        vm.startPrank(bob);
        pathUSD.approve(address(l1Portal), 100_000e6);
        bytes32 d2 = l1Portal.deposit(bob, 5000e6, bytes32("d2"));
        vm.stopPrank();

        // Process both deposits
        Deposit[] memory deposits1 = new Deposit[](2);
        deposits1[0] = Deposit({ sender: alice, to: alice, amount: 10_000e6, memo: bytes32("d1") });
        deposits1[1] = Deposit({ sender: bob, to: bob, amount: 5000e6, memo: bytes32("d2") });

        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, d2
        );
        vm.prank(admin);
        _advanceTempo(deposits1);

        // Phase 2: Withdrawals
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), 5000e6);
        l2Outbox.requestWithdrawal(charlie, 2000e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        vm.startPrank(bob);
        l2ZoneToken.approve(address(l2Outbox), 3000e6);
        l2Outbox.requestWithdrawal(charlie, 1500e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        vm.prank(admin);
        bytes32 wHash = l2Outbox.finalizeWithdrawalBatch(type(uint256).max);

        // Phase 3: More deposits arrive while withdrawals are pending
        vm.startPrank(charlie);
        pathUSD.approve(address(l1Portal), 20_000e6);
        bytes32 d3 = l1Portal.deposit(charlie, 7500e6, bytes32("d3"));
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
        deposits2[0] =
            Deposit({ sender: charlie, to: charlie, amount: 7500e6, memo: bytes32("d3") });

        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, d3
        );
        vm.prank(admin);
        _advanceTempo(deposits2);

        // Verify all L2 balances
        assertEq(l2ZoneToken.balanceOf(alice), 10_000e6 - 2000e6);
        assertEq(l2ZoneToken.balanceOf(bob), 5000e6 - 1500e6);
        assertEq(l2ZoneToken.balanceOf(charlie), 7500e6);

        // Process withdrawals
        Withdrawal memory w1 = Withdrawal({
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
        uint256 charlieBefore = pathUSD.balanceOf(charlie);

        l1Portal.processWithdrawal(w1, innerHash);
        l1Portal.processWithdrawal(w2, bytes32(0));

        assertEq(pathUSD.balanceOf(charlie), charlieBefore + 3500e6);
    }

    /*//////////////////////////////////////////////////////////////
                       INVARIANT CHECKS
    //////////////////////////////////////////////////////////////*/

    function test_invariant_totalSupplyMatchesNetDeposits() public {
        // Deposit 10000
        vm.startPrank(alice);
        pathUSD.approve(address(l1Portal), 10_000e6);
        bytes32 d1 = l1Portal.deposit(alice, 10_000e6, bytes32("d1"));
        vm.stopPrank();

        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] = Deposit({ sender: alice, to: alice, amount: 10_000e6, memo: bytes32("d1") });
        l2TempoState.setMockStorageValue(
            address(l1Portal), PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, d1
        );
        vm.prank(admin);
        _advanceTempo(deposits);

        assertEq(l2ZoneToken.totalSupply(), 10_000e6);

        // Withdraw 3000
        vm.startPrank(alice);
        l2ZoneToken.approve(address(l2Outbox), 3000e6);
        l2Outbox.requestWithdrawal(bob, 3000e6, bytes32(0), 0, alice, "");
        vm.stopPrank();

        assertEq(l2ZoneToken.totalSupply(), 7000e6); // Tokens burned on withdrawal request

        // Transfer on L2 shouldn't change supply
        vm.prank(alice);
        l2ZoneToken.transfer(bob, 2000e6);

        assertEq(l2ZoneToken.totalSupply(), 7000e6);
    }

    /*//////////////////////////////////////////////////////////////
                    STORAGE LAYOUT VERIFICATION TESTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Verify PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT matches the actual ZonePortal storage layout.
    /// @dev If ZonePortal's storage layout changes, this test will fail.
    function test_storageLayout_currentDepositQueueHashSlot() public {
        // Make a deposit to get a non-zero currentDepositQueueHash
        vm.startPrank(alice);
        pathUSD.approve(address(l1Portal), 1000e6);
        l1Portal.deposit(alice, 1000e6, bytes32("layout-test"));
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
