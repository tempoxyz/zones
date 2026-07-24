// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { Math } from "@openzeppelin/contracts/utils/math/Math.sol";

import {
    FeeAllocation,
    FeeConfig,
    FeePreview,
    MAX_FIXED_FEE_RECIPIENTS
} from "../interfaces/core/EarnFeeTypes.sol";

library EarnFeeMath {

    uint256 internal constant WAD = 1e18;
    uint256 internal constant YEAR = 365 days;

    struct Input {
        uint256 activeAssets;
        uint256 earnShareSupply;
        uint256 earnShareScale;
        uint256 highWaterMark;
        uint256 targetBase;
        uint256 targetStartedAt;
        uint256 timestamp;
    }

    function targetAt(
        uint256 base,
        uint256 annualRate,
        uint256 startedAt,
        uint256 timestamp
    )
        internal
        pure
        returns (uint256 target)
    {
        target = base;
        if (base == 0 || annualRate == 0 || timestamp <= startedAt) return target;

        uint256 annualGrowth = Math.mulDiv(base, annualRate, WAD);
        target += Math.mulDiv(annualGrowth, timestamp - startedAt, YEAR);
    }

    function preview(
        Input memory input,
        FeeConfig memory config,
        uint256[5] memory remainders
    )
        internal
        pure
        returns (FeePreview memory result, uint256[5] memory nextRemainders)
    {
        result.activeAssets = input.activeAssets;
        if (input.earnShareSupply == 0 || input.earnShareScale == 0) return (result, remainders);

        result.preFeeValuePerEarnShare =
            Math.mulDiv(input.activeAssets, input.earnShareScale, input.earnShareSupply);
        result.targetValuePerEarnShare = config.excess.enabled
            ? targetAt(
                input.targetBase,
                config.excess.annualTargetRate,
                input.targetStartedAt,
                input.timestamp
            )
            : 0;

        if (result.preFeeValuePerEarnShare > input.highWaterMark) {
            result.positiveAccrualAssets = Math.mulDiv(
                result.preFeeValuePerEarnShare - input.highWaterMark,
                input.earnShareSupply,
                input.earnShareScale
            );
        }

        uint256 fixedCount = config.fixedFeeCount;
        for (uint256 i = 0; i < fixedCount && i < MAX_FIXED_FEE_RECIPIENTS; i++) {
            (uint256 feeAssets, uint256 remainder) = _withRemainder(
                result.positiveAccrualAssets, config.fixedFees[i].rate, remainders[i]
            );
            nextRemainders[i] = remainder;
            result.fixedFeeAssets += feeAssets;
            result.allocations[result.allocationCount++] =
                FeeAllocation(config.fixedFees[i].account, feeAssets, 0);
        }

        if (config.excess.enabled) {
            uint256 targetAssets = Math.mulDiv(
                result.targetValuePerEarnShare, input.earnShareSupply, input.earnShareScale
            );
            uint256 afterFixed = input.activeAssets > result.fixedFeeAssets
                ? input.activeAssets - result.fixedFeeAssets
                : 0;
            uint256 excessBase = afterFixed > targetAssets ? afterFixed - targetAssets : 0;
            uint256 remainingAccrual = result.positiveAccrualAssets > result.fixedFeeAssets
                ? result.positiveAccrualAssets - result.fixedFeeAssets
                : 0;
            if (excessBase > remainingAccrual) excessBase = remainingAccrual;

            (result.excessFeeAssets, nextRemainders[4]) =
                _withRemainder(excessBase, config.excess.excessFeeRate, remainders[4]);
            result.allocations[result.allocationCount++] =
                FeeAllocation(config.excess.account, result.excessFeeAssets, 0);
        }

        result.totalFeeAssets = result.fixedFeeAssets + result.excessFeeAssets;
        if (result.totalFeeAssets > result.positiveAccrualAssets) {
            result.totalFeeAssets = result.positiveAccrualAssets;
        }

        if (result.totalFeeAssets != 0 && result.totalFeeAssets < input.activeAssets) {
            result.totalFeeEarnShares = Math.mulDiv(
                result.totalFeeAssets,
                input.earnShareSupply,
                input.activeAssets - result.totalFeeAssets
            );
            _allocateEarn(result);
        }

        uint256 postEarnShareSupply = input.earnShareSupply + result.totalFeeEarnShares;
        result.postFeeValuePerEarnShare =
            Math.mulDiv(input.activeAssets, input.earnShareScale, postEarnShareSupply);
    }

    function _withRemainder(
        uint256 value,
        uint256 rate,
        uint256 oldRemainder
    )
        private
        pure
        returns (uint256 quotient, uint256 remainder)
    {
        quotient = Math.mulDiv(value, rate, WAD);
        remainder = mulmod(value, rate, WAD) + oldRemainder;
        if (remainder >= WAD) {
            quotient += 1;
            remainder -= WAD;
        }
    }

    function _allocateEarn(FeePreview memory result) private pure {
        uint256 assigned = 0;
        uint256 lastNonzero = type(uint256).max;
        for (uint256 i = 0; i < result.allocationCount; i++) {
            if (result.allocations[i].feeAssets != 0) lastNonzero = i;
        }
        if (lastNonzero == type(uint256).max) return;

        for (uint256 i = 0; i < result.allocationCount; i++) {
            if (i == lastNonzero) {
                result.allocations[i].feeEarnShares = result.totalFeeEarnShares - assigned;
            } else if (result.allocations[i].feeAssets != 0) {
                uint256 feeEarnShares = Math.mulDiv(
                    result.totalFeeEarnShares,
                    result.allocations[i].feeAssets,
                    result.totalFeeAssets
                );
                result.allocations[i].feeEarnShares = feeEarnShares;
                assigned += feeEarnShares;
            }
        }
    }

}
