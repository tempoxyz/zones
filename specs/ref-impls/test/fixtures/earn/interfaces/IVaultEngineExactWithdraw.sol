// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// @notice Optional engine capability for immediate exact-asset withdrawal.
interface IVaultEngineExactWithdraw {
    function previewWithdraw(uint256 assets) external view returns (uint256 shares);
    function withdraw(uint256 assets, address receiver) external returns (uint256 shares);
}
