// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// @notice Minimal ERC-4626 surface used to redeem an external position into Earn's base asset.
interface IERC4626Source {

    function asset() external view returns (address);
    function redeem(
        uint256 sourceVaultShares,
        address receiver,
        address owner
    )
        external
        returns (uint256 assets);

}
