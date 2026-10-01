// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    BlockTransition,
    DepositQueueTransition,
    IVerifier,
    TokenEnablementTransition
} from "../../src/runtime/interfaces/IZone.sol";

/// @title MockVerifier
/// @notice Mock verifier for testing that always accepts proofs (configurable)
contract MockVerifier is IVerifier {

    event Verified();

    bool public shouldAccept = true;

    function setShouldAccept(bool _shouldAccept) external {
        shouldAccept = _shouldAccept;
    }

    function verify(
        uint32, // zoneId
        uint64, // tempoBlockNumber
        uint64, // anchorBlockNumber
        bytes32, // anchorBlockHash
        uint64, // expectedWithdrawalBatchIndex
        uint256, // nextZoneHeight
        BlockTransition calldata,
        DepositQueueTransition calldata,
        TokenEnablementTransition calldata,
        bytes32, // withdrawalQueueHash
        bytes calldata, // verifierConfig
        bytes calldata // proof
    )
        external
        returns (bool)
    {
        emit Verified();
        return shouldAccept;
    }

}
