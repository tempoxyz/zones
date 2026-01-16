// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {IVerifier} from "./interfaces/IVerifier.sol";

/// @title MockVerifier
/// @notice A mock verifier that always returns true (for testing only)
contract MockVerifier is IVerifier {
    /// @inheritdoc IVerifier
    function verify(bytes32, bytes calldata) external pure returns (bool) {
        return true;
    }
}
