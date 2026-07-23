// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// @notice Optional engine capability for immediate, receiver-directed redemption.
interface IEarnEngineRedeem {
    function previewRedeem(uint256 engineShares) external view returns (uint256 assets);
    function redeem(uint256 engineShares, address receiver, uint256 minAssets) external returns (uint256 assets);
}
