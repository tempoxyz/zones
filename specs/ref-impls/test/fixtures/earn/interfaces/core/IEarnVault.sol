// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { FeeConfig } from "./EarnFeeTypes.sol";
import { EngineMigrationMode } from "./EarnVaultTypes.sol";

/// @notice Persistent EarnVault surface consumed by the shared router and other periphery.
/// @dev Extends the canonical immediate-execution surface with burn-at-request async claims, permissionless
///      backing contributions, configurable positive-accrual fees, and the operator-controlled engine seam.
interface IEarnVault {

    event Deposited(
        address indexed caller, address indexed receiver, uint256 assets, uint256 earnShares
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
        address indexed caller, address indexed receiver, uint256 earnShares, uint256 assets
    );
    event WithdrewExact(
        address indexed caller, address indexed receiver, uint256 assets, uint256 earnSharesBurned
    );

    function accrueFees() external returns (uint256 feeAssets, uint256 feeEarnShares);
    function asset() external view returns (address);
    function earnFees() external view returns (address);
    function contribute(uint256 assets) external returns (uint256 venueShares);
    function deposit(
        uint256 assets,
        address receiver,
        uint256 minEarnShares
    )
        external
        returns (uint256 earnShares);
    function depositVenueShares(
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
    function isAccountingAligned() external view returns (bool);
    function operator() external view returns (address);
    function asyncJanitor() external view returns (address);
    function depositsPaused() external view returns (bool);
    function openRedeemRequestCount() external view returns (uint256);
    function previewRedeem(uint256 earnShares) external view returns (uint256 assets);
    function previewWithdraw(uint256 assets) external view returns (uint256 earnShares);
    function redeem(
        uint256 earnShares,
        address receiver,
        uint256 minAssets
    )
        external
        returns (uint256 assets);
    function convertEngineSharesToEarnShares(uint256 engineShares)
        external
        view
        returns (uint256 earnShares);
    function totalEarnShares() external view returns (uint256);
    function earnShare() external view returns (address);
    function setFeeConfig(FeeConfig calldata config) external returns (uint64 configId);
    function convertToEngineShares(uint256 earnShares) external view returns (uint256 engineShares);
    function depositSwapOverride(address inputToken) external view returns (address);
    function redeemSwapOverride(address outputToken) external view returns (address);
    function setDepositSwapOverride(address inputToken, address swapAdapter) external;
    function setRedeemSwapOverride(address outputToken, address swapAdapter) external;
    function setEmergencyRoles(address newGuardian, address newJanitor) external;
    function setDepositsPaused(bool paused) external;
    function withdrawExact(
        uint256 assets,
        address receiver,
        uint256 maxEarnShares
    )
        external
        returns (uint256 earnSharesBurned);

    // Async extension (burn-at-request pending-claim model).
    function requestRedeem(
        uint256 earnShares,
        bytes calldata engineData,
        address receiver
    )
        external
        returns (bytes32 requestId);
    function cancelRedeem(bytes32 requestId, uint256 minReceiverEarnShares) external;

    // Engine seam. Operator-only when the deployment-fixed migration mode enables it.
    // A nonempty migration requires both a replacement-engine-share floor and a retained-base-asset floor.
    function migrateEngine(
        address newEngine,
        uint256 minNewEngineShares,
        uint256 minAssetsRetained
    )
        external
        returns (uint256 newEngineShares);

}
