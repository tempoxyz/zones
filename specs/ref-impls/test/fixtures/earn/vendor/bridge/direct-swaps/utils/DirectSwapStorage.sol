// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/**
 * @title DirectSwapTransientStorageLib
 * @author Bridge
 * @notice Library for accessing DirectSwap transient storage used for per-transaction rate
 * limiting (EIP-1153).
 * @dev Operational parameters (fee, recipient, transaction limit, policy id) live as immutables on
 * DirectSwapV2 itself, so only transient storage is namespaced here.
 */
library DirectSwapTransientStorageLib {
    /// @custom:storage-location erc7201:DirectSwap.transientMintLimit
    /// @dev Transient storage slot for per-transaction rate limiting
    bytes32 constant TRANSIENT_MINT_LIMIT_SLOT = 0xd21a8481dbdfecff978f311939ec8b63cac43b11c4553304925477428350ed00;

    /**
     * @notice Returns the current transient mint limit usage for this transaction
     * @return limit The amount already used in this transaction
     */
    function getTransientMintLimit() internal view returns (uint96 limit) {
        assembly {
            limit := tload(TRANSIENT_MINT_LIMIT_SLOT)
        }
    }

    /**
     * @notice Increases the transient mint limit usage and validates against the transaction limit
     * @dev Reverts with TransientMintLimitExceeded if the new total would exceed the limit
     * @param _amount Amount to add to the current usage
     * @param _transactionLimit Maximum allowed per transaction
     */
    function increaseTransientMintLimitUsed(uint96 _amount, uint96 _transactionLimit) internal {
        assembly {
            let limit := tload(TRANSIENT_MINT_LIMIT_SLOT)
            let newLimit := add(limit, _amount)
            if or(iszero(gt(newLimit, limit)), gt(newLimit, _transactionLimit)) {
                // error TransientMintLimitExceeded()
                mstore(0, 0x4e3e3343)
                revert(0x1c, 4)
            }

            tstore(TRANSIENT_MINT_LIMIT_SLOT, newLimit)
        }
    }
}
