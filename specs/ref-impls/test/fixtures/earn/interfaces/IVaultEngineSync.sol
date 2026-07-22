// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// @notice Optional engine capability for immediate, receiver-directed redemption.
interface IVaultEngineSync {
    function previewRedeem(uint256 shares) external view returns (uint256 assets);
    function redeem(uint256 shares, address receiver, uint256 minAssets) external returns (uint256 assets);
}
