// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { BlockTransition, DepositQueueTransition, IVerifier } from "./IZone.sol";

/// @title Verifier
/// @notice Stub verifier for devnet/testing — always returns true.
///         For production, use NitroVerifier with TEE attestation-backed signatures.
///         Deployed by ZoneFactory — all zones share the same verifier instance.
contract Verifier is IVerifier {

    /// @inheritdoc IVerifier
    function verify(
        uint64,
        uint64,
        bytes32,
        uint64,
        address,
        BlockTransition calldata,
        DepositQueueTransition calldata,
        bytes32,
        bytes calldata,
        bytes calldata
    )
        external
        pure
        override
        returns (bool)
    {
        return true;
    }

}
