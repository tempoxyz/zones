// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { BLOCKHASH_HISTORY_WINDOW } from "../../src/libraries/BlockHashHistory.sol";

/// @notice Mock EIP-2935 block hash history contract for tests.
/// @dev EIP-2935 expects raw 32-byte calldata (block number, no function selector)
///      and returns the block hash. This mock returns the EVM blockhash when available
///      (eg running in CI), deterministic non-zero hashes for older served blocks,
///      and bytes32(0) otherwise.
contract MockEIP2935 {

    fallback(bytes calldata data) external returns (bytes memory) {
        if (data.length != 32) return abi.encode(bytes32(0));
        uint256 blockNumber = abi.decode(data, (uint256));
        if (blockNumber == 0 || blockNumber >= block.number) {
            return abi.encode(bytes32(0));
        }
        // EIP-2935 keeps a 8192-slot ring buffer, but the oldest served block is
        // block.number - 8191. A gap of 8192 has already rotated out.
        // See https://eips.ethereum.org/EIPS/eip-2935#specification
        // HISTORY_SERVE_WINDOW = 8191
        if (block.number - blockNumber >= BLOCKHASH_HISTORY_WINDOW) {
            return abi.encode(bytes32(0));
        }
        bytes32 hash = blockhash(blockNumber);
        if (hash == bytes32(0)) hash = keccak256(abi.encode(blockNumber));
        return abi.encode(hash);
    }

}
