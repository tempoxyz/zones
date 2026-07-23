// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { IERC165 } from "@openzeppelin/contracts/utils/introspection/IERC165.sol";

/// @notice Required single-EarnVault custody and valuation surface for every Earn engine.
/// @dev This is intentionally not ERC-4626-shaped: an engine is not a user vault and always holds
///      venue shares for exactly one `EarnVault`. Exit and in-kind entry behaviors are separate
///      ERC-165 capabilities so future RWA engines expose only the operations they actually support.
interface IEarnEngine is IERC165 {
    function asset() external view returns (address);
    function deposit(uint256 assets) external returns (uint256 engineShares);
    function name() external view returns (string memory);
    /// @notice Nonzero unit for one whole raw engine share, in the units returned by {deposit}/{totalShares}.
    function shareScale() external view returns (uint256 scale);
    function symbol() external view returns (string memory);
    function totalShares() external view returns (uint256 engineShares);
    function totalAssets() external view returns (uint256 assets);
    function valueOf(uint256 engineShares) external view returns (uint256 assets);
}
