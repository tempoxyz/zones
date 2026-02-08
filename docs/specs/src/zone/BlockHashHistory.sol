// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

// EIP-2935 block hash history precompile address (chain-specific).
address constant BLOCKHASH_HISTORY = 0x0000000000000000000000000000000000000100;

// EIP-2935 history window size (8192 blocks).
uint256 constant BLOCKHASH_HISTORY_WINDOW = 8192;

/// @notice Interface for EIP-2935 block hash history precompile.
interface IBlockHashHistory {

    function getBlockHash(uint256 blockNumber) external view returns (bytes32);

}

/// @notice Mock block hash history contract for spec tests.
/// @dev Tempo uses a precompile for EIP-2935; this mock returns a deterministic hash
///      for blocks within the history window.
contract BlockHashHistory is IBlockHashHistory {

    function getBlockHash(uint256 blockNumber) external view returns (bytes32) {
        if (blockNumber >= block.number) {
            return bytes32(0);
        }
        if (block.number - blockNumber > BLOCKHASH_HISTORY_WINDOW) {
            return bytes32(0);
        }
        return keccak256(abi.encode(blockNumber));
    }

}
