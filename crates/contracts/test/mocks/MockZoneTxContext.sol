// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/// @notice Mock tx context precompile for Solidity-only tests.
/// @dev Returns deterministic pseudo tx hashes in increasing sequence order so tests
///      can reconstruct sender tags exactly.
contract MockZoneTxContext {

    uint256 public sequence;

    function currentTxHash() external returns (bytes32) {
        sequence++;
        return txHashFor(sequence);
    }

    function txHashFor(uint256 seq) public pure returns (bytes32) {
        return keccak256(abi.encodePacked("mock-zone-tx-hash", seq));
    }

}
