// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    BlockTransition,
    DepositQueueTransition,
    IVerifier,
    TokenEnablementTransition
} from "../../src/runtime/interfaces/IZone.sol";

/// @title MockVerifier
/// @notice Mock verifier with configurable acceptance and optional proof-height binding.
contract MockVerifier is IVerifier {

    bool public shouldAccept = true;
    bool public checkProofHeight;

    function setShouldAccept(bool _shouldAccept) external {
        shouldAccept = _shouldAccept;
    }

    function setCheckProofHeight(bool _checkProofHeight) external {
        checkProofHeight = _checkProofHeight;
    }

    function verify(
        uint32, // zoneId
        uint64, // tempoBlockNumber
        uint64, // anchorBlockNumber
        bytes32, // anchorBlockHash
        uint64, // expectedWithdrawalBatchIndex
        uint256 nextZoneHeight,
        BlockTransition calldata,
        DepositQueueTransition calldata,
        TokenEnablementTransition calldata,
        bytes32, // withdrawalQueueHash
        bytes calldata, // verifierConfig
        bytes calldata proof
    )
        external
        view
        returns (bool)
    {
        return shouldAccept
            && (!checkProofHeight
                || (proof.length == 32 && nextZoneHeight == abi.decode(proof, (uint256))));
    }

}
