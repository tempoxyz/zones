// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { Deposit } from "./IZone.sol";

/// @title DepositQueueLib
/// @notice Library for managing the deposit queue hash chain
/// @dev The Tempo portal only tracks `currentDepositQueueHash` (where new deposits land).
///      The zone tracks its own `processedDepositQueueHash` in EVM state, and the proof
///      validates deposit processing by reading `currentDepositQueueHash` from Tempo state.
library DepositQueueLib {

    /// @notice Enqueue a new deposit into the queue (on-chain operation)
    /// @dev Hash chain: newHash = keccak256(abi.encode(deposit, prevHash))
    /// @param currentHash The current head of the deposit queue
    /// @param depositData The deposit to enqueue
    /// @return newHash The new head of the deposit queue
    function enqueue(bytes32 currentHash, Deposit memory depositData)
        internal
        pure
        returns (bytes32 newHash)
    {
        newHash = keccak256(abi.encode(depositData, currentHash));
    }

}
