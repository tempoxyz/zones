// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    IVerifier,
    BlockTransition,
    DepositQueueTransition,
    WithdrawalQueueTransition
} from "../../../src/zone/IZone.sol";

/// @title MockVerifier
/// @notice Mock verifier for testing that always accepts proofs (configurable)
contract MockVerifier is IVerifier {
    bool public shouldAccept = true;

    function setShouldAccept(bool _shouldAccept) external {
        shouldAccept = _shouldAccept;
    }

    function verify(
        uint64, // tempoBlockNumber
        bytes32, // tempoBlockHash
        BlockTransition calldata,
        DepositQueueTransition calldata,
        WithdrawalQueueTransition calldata,
        bytes calldata, // verifierData
        bytes calldata  // proof
    ) external view returns (bool) {
        return shouldAccept;
    }
}
