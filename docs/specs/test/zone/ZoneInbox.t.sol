// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { Test } from "forge-std/Test.sol";
import { ZoneInbox } from "../../src/zone/ZoneInbox.sol";
import { MockZoneGasToken } from "./mocks/MockZoneGasToken.sol";
import { MockTempoState } from "./mocks/MockTempoState.sol";
import { Deposit } from "../../src/zone/IZone.sol";

/// @title ZoneInboxTest
/// @notice Tests for ZoneInbox covering edge cases
contract ZoneInboxTest is Test {
    ZoneInbox public inbox;
    MockZoneGasToken public gasToken;
    MockTempoState public tempoState;

    address public sequencer = address(0x1);
    address public alice = address(0x200);
    address public bob = address(0x300);
    address public mockPortal = address(0x400);

    bytes32 constant GENESIS_TEMPO_BLOCK_HASH = keccak256("tempoGenesis");
    uint64 constant GENESIS_TEMPO_BLOCK_NUMBER = 1;

    /// @notice Storage slot for currentDepositQueueHash in ZonePortal
    /// @dev Layout: sequencerPubkey(0), batchIndex(1), blockHash(2), currentDepositQueueHash(3)
    bytes32 internal constant CURRENT_DEPOSIT_QUEUE_HASH_SLOT = bytes32(uint256(3));

    function setUp() public {
        gasToken = new MockZoneGasToken("Zone USD", "zUSD");
        tempoState = new MockTempoState(sequencer, GENESIS_TEMPO_BLOCK_HASH, GENESIS_TEMPO_BLOCK_NUMBER);
        inbox = new ZoneInbox(mockPortal, address(tempoState), address(gasToken), sequencer);

        gasToken.setMinter(address(inbox), true);
    }

    /*//////////////////////////////////////////////////////////////
                          EMPTY DEPOSITS TESTS
    //////////////////////////////////////////////////////////////*/

    function test_advanceTempo_emptyDepositsArray() public {
        // Set mock to return bytes32(0) for currentDepositQueueHash (empty queue)
        tempoState.setMockStorageValue(
            mockPortal,
            CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
            bytes32(0)
        );

        Deposit[] memory deposits = new Deposit[](0);

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits);

        // State should remain at bytes32(0)
        assertEq(inbox.processedDepositQueueHash(), bytes32(0));
    }

    function test_advanceTempo_singleDeposit() public {
        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] = Deposit({
            sender: alice,
            to: bob,
            amount: 1000e6,
            memo: bytes32("payment")
        });

        // Calculate expected hash
        bytes32 expectedHash = keccak256(abi.encode(deposits[0], bytes32(0)));

        tempoState.setMockStorageValue(
            mockPortal,
            CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
            expectedHash
        );

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits);

        assertEq(inbox.processedDepositQueueHash(), expectedHash);
        assertEq(gasToken.balanceOf(bob), 1000e6);
    }

    function test_advanceTempo_multipleDeposits() public {
        Deposit[] memory deposits = new Deposit[](3);
        deposits[0] = Deposit({
            sender: alice,
            to: alice,
            amount: 100e6,
            memo: bytes32("d1")
        });
        deposits[1] = Deposit({
            sender: bob,
            to: bob,
            amount: 200e6,
            memo: bytes32("d2")
        });
        deposits[2] = Deposit({
            sender: alice,
            to: bob,
            amount: 300e6,
            memo: bytes32("d3")
        });

        // Calculate expected hash chain
        bytes32 h0 = bytes32(0);
        bytes32 h1 = keccak256(abi.encode(deposits[0], h0));
        bytes32 h2 = keccak256(abi.encode(deposits[1], h1));
        bytes32 h3 = keccak256(abi.encode(deposits[2], h2));

        tempoState.setMockStorageValue(
            mockPortal,
            CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
            h3
        );

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits);

        assertEq(inbox.processedDepositQueueHash(), h3);
        assertEq(gasToken.balanceOf(alice), 100e6);
        assertEq(gasToken.balanceOf(bob), 200e6 + 300e6);
    }

    /*//////////////////////////////////////////////////////////////
                    HASH CHAIN VALIDATION TESTS
    //////////////////////////////////////////////////////////////*/

    function test_advanceTempo_revertsOnHashMismatch() public {
        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] = Deposit({
            sender: alice,
            to: bob,
            amount: 1000e6,
            memo: bytes32("payment")
        });

        // Set wrong hash
        tempoState.setMockStorageValue(
            mockPortal,
            CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
            keccak256("wrongHash")
        );

        vm.prank(sequencer);
        vm.expectRevert(ZoneInbox.InvalidDepositQueueHash.selector);
        inbox.advanceTempo("", deposits);
    }

    function test_advanceTempo_revertsOnPartialMismatch() public {
        // This tests that you can't claim a subset of deposits if the hash doesn't match
        Deposit[] memory allDeposits = new Deposit[](2);
        allDeposits[0] = Deposit({
            sender: alice,
            to: alice,
            amount: 100e6,
            memo: bytes32("d1")
        });
        allDeposits[1] = Deposit({
            sender: bob,
            to: bob,
            amount: 200e6,
            memo: bytes32("d2")
        });

        // Set hash to be for both deposits
        bytes32 h0 = bytes32(0);
        bytes32 h1 = keccak256(abi.encode(allDeposits[0], h0));
        bytes32 h2 = keccak256(abi.encode(allDeposits[1], h1));

        tempoState.setMockStorageValue(
            mockPortal,
            CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
            h2
        );

        // Try to process only one deposit - should fail
        Deposit[] memory oneDeposit = new Deposit[](1);
        oneDeposit[0] = allDeposits[0];

        vm.prank(sequencer);
        vm.expectRevert(ZoneInbox.InvalidDepositQueueHash.selector);
        inbox.advanceTempo("", oneDeposit);
    }

    function test_advanceTempo_revertsOnWrongOrder() public {
        // Deposits must be processed in the correct order
        Deposit[] memory deposits = new Deposit[](2);
        deposits[0] = Deposit({
            sender: bob,
            to: bob,
            amount: 200e6,
            memo: bytes32("d2")
        });
        deposits[1] = Deposit({
            sender: alice,
            to: alice,
            amount: 100e6,
            memo: bytes32("d1")
        });

        // Set hash for correct order (alice first, then bob)
        Deposit memory d1 = Deposit({
            sender: alice,
            to: alice,
            amount: 100e6,
            memo: bytes32("d1")
        });
        Deposit memory d2 = Deposit({
            sender: bob,
            to: bob,
            amount: 200e6,
            memo: bytes32("d2")
        });

        bytes32 h0 = bytes32(0);
        bytes32 h1 = keccak256(abi.encode(d1, h0));
        bytes32 h2 = keccak256(abi.encode(d2, h1));

        tempoState.setMockStorageValue(
            mockPortal,
            CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
            h2
        );

        // Processing in wrong order should fail
        vm.prank(sequencer);
        vm.expectRevert(ZoneInbox.InvalidDepositQueueHash.selector);
        inbox.advanceTempo("", deposits);
    }

    /*//////////////////////////////////////////////////////////////
                         ACCESS CONTROL TESTS
    //////////////////////////////////////////////////////////////*/

    function test_advanceTempo_onlySequencer() public {
        tempoState.setMockStorageValue(
            mockPortal,
            CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
            bytes32(0)
        );

        Deposit[] memory deposits = new Deposit[](0);

        // Random user should fail
        vm.prank(alice);
        vm.expectRevert(ZoneInbox.OnlySequencer.selector);
        inbox.advanceTempo("", deposits);

        // Sequencer should succeed
        vm.prank(sequencer);
        inbox.advanceTempo("", deposits);
    }

    /*//////////////////////////////////////////////////////////////
                        INCREMENTAL PROCESSING TESTS
    //////////////////////////////////////////////////////////////*/

    function test_advanceTempo_incrementalProcessing() public {
        // First batch of deposits
        Deposit[] memory batch1 = new Deposit[](2);
        batch1[0] = Deposit({
            sender: alice,
            to: alice,
            amount: 100e6,
            memo: bytes32("d1")
        });
        batch1[1] = Deposit({
            sender: bob,
            to: bob,
            amount: 200e6,
            memo: bytes32("d2")
        });

        bytes32 h0 = bytes32(0);
        bytes32 h1 = keccak256(abi.encode(batch1[0], h0));
        bytes32 h2 = keccak256(abi.encode(batch1[1], h1));

        tempoState.setMockStorageValue(mockPortal, CURRENT_DEPOSIT_QUEUE_HASH_SLOT, h2);

        vm.prank(sequencer);
        inbox.advanceTempo("", batch1);

        assertEq(inbox.processedDepositQueueHash(), h2);

        // Second batch of deposits
        Deposit[] memory batch2 = new Deposit[](1);
        batch2[0] = Deposit({
            sender: alice,
            to: bob,
            amount: 500e6,
            memo: bytes32("d3")
        });

        bytes32 h3 = keccak256(abi.encode(batch2[0], h2));

        tempoState.setMockStorageValue(mockPortal, CURRENT_DEPOSIT_QUEUE_HASH_SLOT, h3);

        vm.prank(sequencer);
        inbox.advanceTempo("", batch2);

        assertEq(inbox.processedDepositQueueHash(), h3);
        assertEq(gasToken.balanceOf(alice), 100e6);
        assertEq(gasToken.balanceOf(bob), 200e6 + 500e6);
    }

    /*//////////////////////////////////////////////////////////////
                          EVENT EMISSION TESTS
    //////////////////////////////////////////////////////////////*/

    function test_advanceTempo_emitsTempoAdvancedEvent() public {
        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] = Deposit({
            sender: alice,
            to: bob,
            amount: 1000e6,
            memo: bytes32("payment")
        });

        bytes32 expectedHash = keccak256(abi.encode(deposits[0], bytes32(0)));

        tempoState.setMockStorageValue(mockPortal, CURRENT_DEPOSIT_QUEUE_HASH_SLOT, expectedHash);

        vm.prank(sequencer);
        vm.expectEmit(true, true, false, true);
        // After finalizeTempo, block number will be GENESIS + 1
        emit ZoneInbox.TempoAdvanced(
            keccak256(abi.encode(GENESIS_TEMPO_BLOCK_HASH, GENESIS_TEMPO_BLOCK_NUMBER + 1)),
            GENESIS_TEMPO_BLOCK_NUMBER + 1,
            1,
            expectedHash
        );
        inbox.advanceTempo("", deposits);
    }

    function test_advanceTempo_emitsDepositProcessedEvent() public {
        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] = Deposit({
            sender: alice,
            to: bob,
            amount: 1000e6,
            memo: bytes32("payment")
        });

        bytes32 expectedHash = keccak256(abi.encode(deposits[0], bytes32(0)));

        tempoState.setMockStorageValue(mockPortal, CURRENT_DEPOSIT_QUEUE_HASH_SLOT, expectedHash);

        vm.prank(sequencer);
        vm.expectEmit(true, true, true, true);
        emit ZoneInbox.DepositProcessed(expectedHash, alice, bob, 1000e6, bytes32("payment"));
        inbox.advanceTempo("", deposits);
    }

    /*//////////////////////////////////////////////////////////////
                         ZERO AMOUNT TESTS
    //////////////////////////////////////////////////////////////*/

    function test_advanceTempo_zeroAmountDeposit() public {
        Deposit[] memory deposits = new Deposit[](1);
        deposits[0] = Deposit({
            sender: alice,
            to: bob,
            amount: 0,
            memo: bytes32("empty")
        });

        bytes32 expectedHash = keccak256(abi.encode(deposits[0], bytes32(0)));

        tempoState.setMockStorageValue(mockPortal, CURRENT_DEPOSIT_QUEUE_HASH_SLOT, expectedHash);

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits);

        assertEq(inbox.processedDepositQueueHash(), expectedHash);
        assertEq(gasToken.balanceOf(bob), 0);
    }

    /*//////////////////////////////////////////////////////////////
                        IMMUTABLE GETTERS TESTS
    //////////////////////////////////////////////////////////////*/

    function test_immutableGetters() public view {
        assertEq(inbox.tempoPortal(), mockPortal);
        assertEq(address(inbox.tempoState()), address(tempoState));
        assertEq(address(inbox.gasToken()), address(gasToken));
        assertEq(inbox.sequencer(), sequencer);
    }

    /*//////////////////////////////////////////////////////////////
                      LARGE DEPOSIT BATCH TESTS
    //////////////////////////////////////////////////////////////*/

    function test_advanceTempo_manyDeposits() public {
        uint256 numDeposits = 50;
        Deposit[] memory deposits = new Deposit[](numDeposits);

        bytes32 currentHash = bytes32(0);
        for (uint256 i = 0; i < numDeposits; i++) {
            deposits[i] = Deposit({
                sender: alice,
                to: bob,
                amount: uint128(i + 1) * 1e6,
                memo: bytes32(i)
            });
            currentHash = keccak256(abi.encode(deposits[i], currentHash));
        }

        tempoState.setMockStorageValue(mockPortal, CURRENT_DEPOSIT_QUEUE_HASH_SLOT, currentHash);

        vm.prank(sequencer);
        inbox.advanceTempo("", deposits);

        assertEq(inbox.processedDepositQueueHash(), currentHash);

        // Calculate expected balance: sum of 1 + 2 + ... + 50 = 50 * 51 / 2 = 1275
        uint256 expectedBalance = (numDeposits * (numDeposits + 1) / 2) * 1e6;
        assertEq(gasToken.balanceOf(bob), expectedBalance);
    }
}
