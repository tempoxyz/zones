// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { EarnFeesInit, FeeConfig, FeePreview } from "./EarnFeeTypes.sol";

interface IEarnFees {

    struct PendingFeeSnapshot {
        uint256 burnedEarnShares;
        uint256 requestValue;
        uint256 highWaterValue;
        uint256 targetValue;
        uint64 feeConfigId;
        uint40 requestedAt;
        bool open;
    }

    function initialize(address earnVault, address earnShare, EarnFeesInit calldata init) external;

    function accrueFees() external returns (FeePreview memory result);
    function setFeeConfig(FeeConfig calldata config) external returns (uint64 configId);
    function disableFees(address guardian) external;
    function initializeBaselines() external;

    function recordRedeemRequest(
        bytes32 requestId,
        uint256 burnedEarnShares,
        uint256 requestValue
    )
        external;
    function closeRedeemRequest(bytes32 requestId) external;
    function settleCancelledRedeem(
        bytes32 requestId,
        uint256 returnedValue,
        uint256 activeSupply,
        uint256 activeAssets,
        uint256 totalReentryEarnShares
    )
        external
        returns (uint256 feeEarnShares);

    function feeConfig(uint64 configId) external view returns (FeeConfig memory);
    function feeRemainder(uint64 configId, uint8 slot) external view returns (uint256);
    function fixedFeeCap() external view returns (uint96);
    function excessFeeCap() external view returns (uint96);
    function currentFeeConfigId() external view returns (uint64);
    function highWaterMark() external view returns (uint256);
    function targetBase() external view returns (uint256);
    function targetStartedAt() external view returns (uint40);
    function feesDisabled() external view returns (bool);
    function feesActive() external view returns (bool);
    function previewAccruedFees() external view returns (FeePreview memory result);
    function pendingFeeSnapshot(bytes32 requestId) external view returns (PendingFeeSnapshot memory);
    function shouldChargeRedeem(bytes32 requestId) external view returns (bool);
    function claimableEarnShares(address recipient) external view returns (uint256);
    function totalClaimableEarnShares() external view returns (uint256);
    function claim(address to, uint256 earnShares) external;
    function earnVault() external view returns (address);
    function earnShare() external view returns (address);

}
