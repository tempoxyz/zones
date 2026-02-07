// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { DepositQueueLib } from "../../src/zone/DepositQueueLib.sol";
import { Deposit, DepositType } from "../../src/zone/IZone.sol";
import { Test } from "forge-std/Test.sol";

/// @title DepositQueueLibTest
/// @notice Direct tests for DepositQueueLib functionality
contract DepositQueueLibTest is Test {

    address public alice = address(0x200);
    address public bob = address(0x300);

    /*//////////////////////////////////////////////////////////////
                            ENQUEUE TESTS
    //////////////////////////////////////////////////////////////*/

    function test_enqueue_singleDeposit() public pure {
        Deposit memory d = Deposit({
            sender: address(0x200), to: address(0x300), amount: 100e6, memo: bytes32("memo")
        });

        bytes32 newHash = DepositQueueLib.enqueue(bytes32(0), d);

        bytes32 expectedHash = keccak256(abi.encode(DepositType.Regular, d, bytes32(0)));
        assertEq(newHash, expectedHash);
    }

    function test_enqueue_multipleDeposits() public pure {
        Deposit memory d1 = Deposit({
            sender: address(0x200), to: address(0x300), amount: 100e6, memo: bytes32("d1")
        });
        Deposit memory d2 = Deposit({
            sender: address(0x300), to: address(0x200), amount: 200e6, memo: bytes32("d2")
        });
        Deposit memory d3 = Deposit({
            sender: address(0x200), to: address(0x200), amount: 300e6, memo: bytes32("d3")
        });

        bytes32 h1 = DepositQueueLib.enqueue(bytes32(0), d1);
        bytes32 h2 = DepositQueueLib.enqueue(h1, d2);
        bytes32 h3 = DepositQueueLib.enqueue(h2, d3);

        // Verify hash chain structure
        bytes32 expected1 = keccak256(abi.encode(DepositType.Regular, d1, bytes32(0)));
        bytes32 expected2 = keccak256(abi.encode(DepositType.Regular, d2, expected1));
        bytes32 expected3 = keccak256(abi.encode(DepositType.Regular, d3, expected2));

        assertEq(h1, expected1);
        assertEq(h2, expected2);
        assertEq(h3, expected3);
    }

    function test_enqueue_hashChainStructure() public pure {
        // Verify that newer deposits wrap older ones
        Deposit memory d1 = Deposit({
            sender: address(0x200), to: address(0x300), amount: 100e6, memo: bytes32("first")
        });
        Deposit memory d2 = Deposit({
            sender: address(0x300), to: address(0x200), amount: 200e6, memo: bytes32("second")
        });

        bytes32 h1 = DepositQueueLib.enqueue(bytes32(0), d1);
        bytes32 h2 = DepositQueueLib.enqueue(h1, d2);

        // h2 should wrap h1
        assertEq(h2, keccak256(abi.encode(DepositType.Regular, d2, h1)));
    }

    function test_enqueue_emptyToEmpty() public pure {
        // An empty deposit struct should still produce a valid hash
        Deposit memory d =
            Deposit({ sender: address(0), to: address(0), amount: 0, memo: bytes32(0) });

        bytes32 h = DepositQueueLib.enqueue(bytes32(0), d);
        bytes32 expected = keccak256(abi.encode(DepositType.Regular, d, bytes32(0)));
        assertEq(h, expected);
        assertTrue(h != bytes32(0)); // Hash of something is non-zero
    }

    function test_enqueue_differentInputsProduceDifferentHashes() public pure {
        Deposit memory d1 = Deposit({
            sender: address(0x200), to: address(0x300), amount: 100e6, memo: bytes32("memo1")
        });
        Deposit memory d2 = Deposit({
            sender: address(0x200),
            to: address(0x300),
            amount: 100e6,
            memo: bytes32("memo2") // Only memo differs
        });

        bytes32 h1 = DepositQueueLib.enqueue(bytes32(0), d1);
        bytes32 h2 = DepositQueueLib.enqueue(bytes32(0), d2);

        assertTrue(h1 != h2);
    }

    function test_enqueue_sameDepositDifferentPrevHashProducesDifferentResult() public pure {
        Deposit memory d = Deposit({
            sender: address(0x200), to: address(0x300), amount: 100e6, memo: bytes32("memo")
        });

        bytes32 h1 = DepositQueueLib.enqueue(bytes32(0), d);
        bytes32 h2 = DepositQueueLib.enqueue(bytes32(uint256(1)), d);

        assertTrue(h1 != h2);
    }

}
