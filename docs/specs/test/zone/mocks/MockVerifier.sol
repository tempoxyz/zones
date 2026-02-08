// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { BlockTransition, DepositQueueTransition, IVerifier } from "../../../src/zone/IZone.sol";

/// @title MockVerifier
/// @notice Mock verifier for testing that always accepts proofs (configurable)
contract MockVerifier is IVerifier {

    bool public shouldAccept = true;

    function setShouldAccept(bool _shouldAccept) external {
        shouldAccept = _shouldAccept;
    }

    function verify(
        uint64, // tempoBlockNumber
        uint64, // anchorBlockNumber
        bytes32, // anchorBlockHash
        uint64, // expectedWithdrawalBatchIndex
        address, // sequencer
        BlockTransition calldata,
        DepositQueueTransition calldata,
        bytes32, // withdrawalQueueHash
        bytes calldata, // verifierConfig
        bytes calldata // proof
    )
        external
        view
        returns (bool)
    {
        return shouldAccept;
    }

}
