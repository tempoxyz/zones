// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { ContributionPreview, FeeConfig, FeePreview } from "./IVaultFees.sol";

/// @notice Custody-adapter surface consumed by gateways and other periphery.
/// @dev Extends the canonical synchronous surface with burn-at-request async claims, permissionless
///      backing contributions, configurable positive-accrual fees, and the governed engine seam.
interface IVaultAdapter {

    function accrueFees() external returns (uint256 feeAssets, uint256 feeShares);
    function asset() external view returns (address);
    function claimFeeShares(address to, uint256 shares) external;
    function contribute(uint256 assets) external returns (uint256 venueShares);
    function convertToAssets(uint256 shares) external view returns (uint256 assets);
    function deposit(
        uint256 assets,
        address receiver,
        uint256 minShares
    )
        external
        returns (uint256 shares);
    function depositShares(
        uint256 venueShares,
        address receiver,
        uint256 minEarnShares
    )
        external
        returns (uint256 earnShares);
    function disableFees() external;
    function engine() external view returns (address);
    function engineShares() external view returns (uint256);
    function feeConfig(uint64 configId) external view returns (FeeConfig memory);
    function feesActive() external view returns (bool);
    function operator() external view returns (address);
    function pendingRedeemCount() external view returns (uint256);
    function previewAccruedFees() external view returns (FeePreview memory);
    function previewContributionOutcome(uint256 assets)
        external
        view
        returns (ContributionPreview memory);
    function previewRedeem(uint256 shares) external view returns (uint256 assets);
    function previewWithdraw(uint256 assets) external view returns (uint256 shares);
    function redeem(uint256 shares, address receiver) external returns (uint256 assets);
    function shareSupply() external view returns (uint256);
    function shareToken() external view returns (address);
    function setFeeConfig(FeeConfig calldata config) external returns (uint64 configId);
    function withdrawExact(
        uint256 assets,
        address receiver,
        uint256 maxShares
    )
        external
        returns (uint256 sharesBurned);

    // Async extension (burn-at-request pending-claim model).
    function requestRedeemAsync(
        uint256 shares,
        address assetOut,
        uint16 discountBps,
        uint24 secondsToDeadline,
        address receiver
    )
        external
        returns (bytes32 requestId);
    function cancelRedeemAsync(bytes32 requestId) external;

    // Governed engine seam. Swaps the yield engine in one atomic, NAV-preserving tx (operator-only).
    function migrateEngine(address newEngine) external returns (uint256 newShares);

}
