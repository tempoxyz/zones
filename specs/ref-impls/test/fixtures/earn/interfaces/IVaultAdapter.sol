// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { EngineMigrationMode } from "./IVaultControls.sol";
import { FeeConfig, FeePreview } from "./IVaultFees.sol";

/// @notice Custody-adapter surface consumed by gateways and other periphery.
/// @dev Extends the canonical synchronous surface with burn-at-request async claims, permissionless
///      backing contributions, configurable positive-accrual fees, and the governed engine seam.
interface IVaultAdapter {

    event Deposited(
        address indexed caller, address indexed receiver, uint256 assets, uint256 shares
    );
    event DepositPauseChanged(address indexed caller, bool paused);
    event EmergencyRolesChanged(address indexed emergencyGuardian, address indexed asyncJanitor);
    event VenueSharesDeposited(
        address indexed caller,
        address indexed receiver,
        uint256 requestedVenueShares,
        uint256 receivedVenueShares,
        uint256 earnShares
    );
    event Redeemed(
        address indexed caller, address indexed receiver, uint256 shares, uint256 assets
    );
    event WithdrewExact(
        address indexed caller, address indexed receiver, uint256 assets, uint256 sharesBurned
    );

    function accrueFees() external returns (uint256 feeAssets, uint256 feeShares);
    function asset() external view returns (address);
    function claimableFeeShares(address recipient) external view returns (uint256 shares);
    function claimFeeShares(address to, uint256 shares) external;
    function contribute(uint256 assets) external returns (uint256 venueShares);
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
    function emergencyGuardian() external view returns (address);
    function engine() external view returns (address);
    function engineMigrationMode() external view returns (EngineMigrationMode);
    function engineShares() external view returns (uint256);
    function feeConfig(uint64 configId) external view returns (FeeConfig memory);
    function feesActive() external view returns (bool);
    function isSynced() external view returns (bool);
    function operator() external view returns (address);
    function asyncJanitor() external view returns (address);
    function depositsPaused() external view returns (bool);
    function pendingRedeemCount() external view returns (uint256);
    function previewAccruedFees() external view returns (FeePreview memory);
    function previewRedeem(uint256 shares) external view returns (uint256 assets);
    function previewWithdraw(uint256 assets) external view returns (uint256 shares);
    function redeem(
        uint256 shares,
        address receiver,
        uint256 minAssets
    )
        external
        returns (uint256 assets);
    function sharesToTokens(uint256 shares) external view returns (uint256 tokens);
    function shareSupply() external view returns (uint256);
    function shareToken() external view returns (address);
    function setFeeConfig(FeeConfig calldata config) external returns (uint64 configId);
    function tokensToShares(uint256 tokens) external view returns (uint256 shares);
    function setEmergencyRoles(address newGuardian, address newJanitor) external;
    function setDepositsPaused(bool paused) external;
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
        bytes calldata engineData,
        address receiver
    )
        external
        returns (bytes32 requestId);
    function cancelRedeemAsync(bytes32 requestId) external;

    // Governed engine seam. Operator-only when the deployment-fixed migration mode enables it.
    // A nonempty migration requires both a replacement-share floor and a retained-base-asset floor.
    function migrateEngine(
        address newEngine,
        uint256 minNewShares,
        uint256 minAssetsRetained
    )
        external
        returns (uint256 newShares);

}
