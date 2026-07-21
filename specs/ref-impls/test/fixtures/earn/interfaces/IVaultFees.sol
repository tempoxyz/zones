// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

uint256 constant MAX_FIXED_FEE_RECIPIENTS = 4;

struct FixedFeeRecipient {
    address account;
    uint96 rate;
}

struct ExcessReturnFee {
    bool enabled;
    address account;
    uint96 annualTargetRate;
    uint96 excessFeeRate;
}

struct FeeConfig {
    uint8 fixedFeeCount;
    FixedFeeRecipient[4] fixedFees;
    ExcessReturnFee excess;
}

struct FeeInit {
    address administrator;
    address guardian;
    uint96 fixedFeeCap;
    uint96 excessFeeCap;
    FeeConfig initialConfig;
}

struct FeeAllocation {
    address account;
    uint256 feeAssets;
    uint256 feeShares;
}

struct FeePreview {
    uint256 activeAssets;
    uint256 positiveAccrualAssets;
    uint256 fixedFeeAssets;
    uint256 excessFeeAssets;
    uint256 totalFeeAssets;
    uint256 totalFeeShares;
    uint256 preFeeValuePerShare;
    uint256 postFeeValuePerShare;
    uint256 targetValuePerShare;
    uint8 allocationCount;
    FeeAllocation[5] allocations;
}

struct ContributionPreview {
    uint256 assumedAssetsCredited;
    uint256 netHolderAssets;
    FeePreview fees;
}
