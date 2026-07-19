// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// @notice ERC4626-shaped synchronous vault engine surface used by `VaultAdapter`.
/// @dev Every engine (sync-only or async-capable) implements this. Async request/claim exits (for
///      example queue-based engines) are an optional extension exposed through `IVaultEngineAsync`;
///      `supportsAsync()` lets `VaultAdapter` gate the async paths without a hard dependency on the
///      async interface.
interface IVaultEngine {

    function asset() external view returns (address);
    function balanceOf(address account) external view returns (uint256);
    function decimals() external view returns (uint8);
    function deposit(uint256 assets, address receiver) external returns (uint256 shares);
    function name() external view returns (string memory);
    function previewRedeem(uint256 shares) external view returns (uint256 assets);
    function previewWithdraw(uint256 assets) external view returns (uint256 shares);
    function redeem(
        uint256 shares,
        address receiver,
        address owner
    )
        external
        returns (uint256 assets);
    function symbol() external view returns (string memory);
    function totalAssets() external view returns (uint256 assets);
    function withdraw(
        uint256 assets,
        address receiver,
        address owner
    )
        external
        returns (uint256 shares);

}
