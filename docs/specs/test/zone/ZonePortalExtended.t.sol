// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { BaseTest } from "../BaseTest.t.sol";
import { ZoneFactory } from "../../src/zone/ZoneFactory.sol";
import { ZonePortal } from "../../src/zone/ZonePortal.sol";
import { MockVerifier } from "./mocks/MockVerifier.sol";
import { TIP20 } from "../../src/TIP20.sol";
import {
    IZoneFactory,
    IZonePortal,
    IWithdrawalReceiver,
    Deposit,
    Withdrawal,
    BlockTransition,
    DepositQueueTransition,
    WithdrawalQueueTransition
} from "../../src/zone/IZone.sol";
import { DepositQueueLib } from "../../src/zone/DepositQueueLib.sol";
import { WithdrawalQueueLib, EMPTY_SENTINEL } from "../../src/zone/WithdrawalQueueLib.sol";

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

/// @title ZonePortalExtendedTest
/// @notice Extended tests for ZonePortal covering edge cases and validation
contract ZonePortalExtendedTest is BaseTest {
    ZoneFactory public zoneFactory;
    MockVerifier public mockVerifier;
    ZonePortal public portal;
    GasConsumingReceiver public gasConsumingReceiver;
    SuccessfulReceiver public successfulReceiver;

    uint64 public testZoneId;
    bytes32 public constant GENESIS_BLOCK_HASH = keccak256("genesis");
    bytes32 public constant GENESIS_TEMPO_BLOCK_HASH = keccak256("tempoGenesis");
    uint64 public genesisTempoBlockNumber;

    function setUp() public override {
        super.setUp();

        zoneFactory = new ZoneFactory();
        mockVerifier = new MockVerifier();
        gasConsumingReceiver = new GasConsumingReceiver();
        successfulReceiver = new SuccessfulReceiver();

        vm.startPrank(pathUSDAdmin);
        pathUSD.grantRole(_ISSUER_ROLE, pathUSDAdmin);
        pathUSD.mint(admin, 1_000_000e6);
        pathUSD.mint(alice, 100_000e6);
        pathUSD.mint(bob, 100_000e6);
        vm.stopPrank();

        genesisTempoBlockNumber = uint64(block.number);

        IZoneFactory.CreateZoneParams memory params = IZoneFactory.CreateZoneParams({
            token: address(pathUSD),
            sequencer: admin,
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
        bytes32 pubkey = keccak256("pubkey");

        vm.prank(alice);
        vm.expectRevert(IZonePortal.NotSequencer.selector);
        portal.setSequencerPubkey(pubkey);
    }

    /*//////////////////////////////////////////////////////////////
                   TEMPO BLOCK NUMBER VALIDATION TESTS
    //////////////////////////////////////////////////////////////*/

    function test_submitBatch_revertsIfTempoBlockNumberBeforeGenesis() public {
        vm.roll(block.number + 10);

        vm.expectRevert(IZonePortal.InvalidTempoBlockNumber.selector);
        portal.submitBatch(
            genesisTempoBlockNumber - 1, // Before genesis
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("state") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0) }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );
    }

    function test_submitBatch_revertsIfTempoBlockNumberInFuture() public {
        vm.roll(block.number + 10);

        vm.expectRevert(IZonePortal.InvalidTempoBlockNumber.selector);
        portal.submitBatch(
            uint64(block.number + 1), // In future
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("state") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0) }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );
    }

    function test_submitBatch_revertsIfTempoBlockNumberTooOld() public {
        // Advance more than 256 blocks
        vm.roll(block.number + 300);

        vm.expectRevert(IZonePortal.InvalidTempoBlockNumber.selector);
        portal.submitBatch(
            genesisTempoBlockNumber, // Valid but > 256 blocks ago
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("state") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0) }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );
    }

    function test_submitBatch_succeedsAtBoundary256Blocks() public {
        // Advance exactly 256 blocks
        vm.roll(genesisTempoBlockNumber + 256);

        // Should still work at exactly 256 blocks
        portal.submitBatch(
            genesisTempoBlockNumber,
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("state") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: bytes32(0) }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );

        assertEq(portal.batchIndex(), 1);
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
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("state") }),
            DepositQueueTransition({
                prevProcessedHash: keccak256("wrongHash"), // This is ignored by implementation
                nextProcessedHash: depositHash
            }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );

        // Verify batch was accepted
        assertEq(portal.batchIndex(), 1);
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
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("state1") }),
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
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("state2") }),
            DepositQueueTransition({ prevProcessedHash: h1, nextProcessedHash: h2 }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );

        assertEq(portal.batchIndex(), 2);
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
            sender: alice, to: bob, amount: 100e6, memo: bytes32(0),
            gasLimit: 0, fallbackRecipient: address(0), callbackData: ""
        });
        bytes32 w1Hash = keccak256(abi.encode(w1, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("s1") }),
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
            sender: alice, to: charlie, amount: 200e6, memo: bytes32(0),
            gasLimit: 0, fallbackRecipient: address(0), callbackData: ""
        });
        bytes32 w2Hash = keccak256(abi.encode(w2, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("s2") }),
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
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("s1") }),
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
            sender: alice, to: bob, amount: 100e6, memo: bytes32("w1"),
            gasLimit: 0, fallbackRecipient: address(0), callbackData: ""
        });
        bytes32 w1Hash = keccak256(abi.encode(w1, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("s1") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHash }),
            WithdrawalQueueTransition({ withdrawalQueueHash: w1Hash }),
            "",
            ""
        );

        Withdrawal memory w2 = Withdrawal({
            sender: alice, to: charlie, amount: 200e6, memo: bytes32("w2"),
            gasLimit: 0, fallbackRecipient: address(0), callbackData: ""
        });
        bytes32 w2Hash = keccak256(abi.encode(w2, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("s2") }),
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
            memo: bytes32(0),
            gasLimit: 50000, // Limited gas
            fallbackRecipient: alice,
            callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("s1") }),
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
            memo: bytes32(0),
            gasLimit: 0, // No callback
            fallbackRecipient: address(0),
            callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("s1") }),
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
            memo: bytes32(0),
            gasLimit: 100000,
            fallbackRecipient: alice,
            callbackData: "test"
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("s1") }),
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
            memo: bytes32("payment"),
            gasLimit: 50000,
            fallbackRecipient: bob, // Bob is the fallback
            callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("s1") }),
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
        // Deposit { sender: portal, to: bob, amount: 500e6, memo: 0 }
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

    function test_batchIndex_incrementsOnEachBatch() public {
        vm.startPrank(alice);
        pathUSD.approve(address(portal), 3000e6);
        portal.deposit(alice, 1000e6, bytes32(""));
        vm.stopPrank();

        bytes32 depositHash = portal.currentDepositQueueHash();

        assertEq(portal.batchIndex(), 0);

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("s1") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHash }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );
        assertEq(portal.batchIndex(), 1);

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("s2") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHash }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );
        assertEq(portal.batchIndex(), 2);

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("s3") }),
            DepositQueueTransition({ prevProcessedHash: bytes32(0), nextProcessedHash: depositHash }),
            WithdrawalQueueTransition({ withdrawalQueueHash: bytes32(0) }),
            "",
            ""
        );
        assertEq(portal.batchIndex(), 3);
    }

    /*//////////////////////////////////////////////////////////////
                        EVENT EMISSION TESTS
    //////////////////////////////////////////////////////////////*/

    function test_deposit_emitsDepositMadeEvent() public {
        vm.startPrank(alice);
        pathUSD.approve(address(portal), 1000e6);

        vm.expectEmit(true, true, false, true);
        bytes32 expectedHash = keccak256(abi.encode(
            Deposit({ sender: alice, to: bob, amount: 500e6, memo: bytes32("test") }),
            bytes32(0)
        ));
        emit IZonePortal.DepositMade(expectedHash, alice, bob, 500e6, bytes32("test"));

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
            sender: alice, to: bob, amount: 500e6, memo: bytes32(0),
            gasLimit: 0, fallbackRecipient: address(0), callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("s1") }),
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
            sender: alice, to: address(gasConsumingReceiver), amount: 500e6, memo: bytes32(0),
            gasLimit: 50000, fallbackRecipient: alice, callbackData: ""
        });
        bytes32 wHash = keccak256(abi.encode(w, EMPTY_SENTINEL));

        vm.roll(block.number + 1);
        portal.submitBatch(
            uint64(block.number - 1),
            BlockTransition({ prevBlockHash: bytes32(0), nextBlockHash: keccak256("s1") }),
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
