// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { IERC165 } from "@openzeppelin/contracts/utils/introspection/IERC165.sol";

/// @notice Required single-client custody and valuation surface for every Earn engine.
/// @dev This is intentionally not ERC-4626-shaped: an engine is not a user vault and always holds
///      venue shares for exactly one `VaultAdapter`. Exit and in-kind entry behaviors are separate
///      ERC-165 capabilities so future RWA engines expose only the operations they actually support.
interface IVaultEngine is IERC165 {
    function asset() external view returns (address);
    function deposit(uint256 assets) external returns (uint256 shares);
    function name() external view returns (string memory);
    function symbol() external view returns (string memory);
    function totalShares() external view returns (uint256 shares);
    function totalAssets() external view returns (uint256 assets);
    function valueOf(uint256 shares) external view returns (uint256 assets);
}
