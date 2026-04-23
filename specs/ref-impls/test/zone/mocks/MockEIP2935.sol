// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { BLOCKHASH_HISTORY_WINDOW } from "../../../src/zone/BlockHashHistory.sol";

/// @notice Mock EIP-2935 block hash history contract for tests.
/// @dev EIP-2935 expects raw 32-byte calldata (block number, no function selector)
///      and returns the block hash. This mock returns keccak256(abi.encode(blockNumber))
///      for blocks within the history window, and bytes32(0) otherwise.
contract MockEIP2935 {

    fallback(bytes calldata data) external returns (bytes memory) {
        if (data.length != 32) return abi.encode(bytes32(0));
        uint256 blockNumber = abi.decode(data, (uint256));
        if (blockNumber == 0 || blockNumber >= block.number) {
            return abi.encode(bytes32(0));
        }
        // Respect EIP-2935 history window
        if (block.number - blockNumber > BLOCKHASH_HISTORY_WINDOW) {
            return abi.encode(bytes32(0));
        }
        return abi.encode(keccak256(abi.encode(blockNumber)));
    }

}
