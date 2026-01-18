// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    IVerifier,
    StateTransition,
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
        bytes32, // tempoBlockHash
        StateTransition calldata,
        DepositQueueTransition calldata,
        WithdrawalQueueTransition calldata,
        bytes calldata, // verifierData
        bytes calldata  // proof
    ) external view returns (bool) {
        return shouldAccept;
    }
}
