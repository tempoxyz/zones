// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    BlockTransition,
    DepositQueueTransition,
    TokenEnablementTransition
} from "../../src/runtime/interfaces/IZone.sol";
import { MockVerifier } from "../mocks/MockVerifier.sol";
import { Test } from "forge-std/Test.sol";

contract MockVerifierTest is Test {

    /// @notice Verifies the mock verifier accepts arbitrary transition inputs by default.
    function test_verify_returnsTrue() public {
        MockVerifier verifier = new MockVerifier();

        bool ok = verifier.verify(
            1,
            1,
            1,
            bytes32("anchor"),
            1,
            BlockTransition({ prevBlockHash: bytes32("prev"), nextBlockHash: bytes32("next") }),
            DepositQueueTransition({
                prevProcessedHash: bytes32(0),
                nextProcessedHash: bytes32("deposits"),
                prevDepositNumber: 0,
                nextDepositNumber: 0
            }),
            TokenEnablementTransition({ prevProcessedTokenCount: 0, nextProcessedTokenCount: 0 }),
            bytes32("withdrawals"),
            "config",
            "proof"
        );

        assertTrue(ok);
    }

}
