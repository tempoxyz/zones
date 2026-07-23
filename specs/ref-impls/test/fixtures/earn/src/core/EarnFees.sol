// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { Math } from "@openzeppelin/contracts/utils/math/Math.sol";

import { EarnFeeMath } from "./EarnFeeMath.sol";
import { EarnFeesInit, FeeConfig, FeePreview, MAX_FIXED_FEE_RECIPIENTS } from "../interfaces/core/EarnFeeTypes.sol";
import { IEarnFees } from "../interfaces/core/IEarnFees.sol";
import { IEarnShare } from "../interfaces/core/IEarnShare.sol";
import { IEarnEngine } from "../interfaces/engines/IEarnEngine.sol";
import { IERC20Like } from "../interfaces/external/IERC20Like.sol";

interface IEarnVaultFeeContext {
    function engine() external view returns (address);
}

/// @title EarnFees
/// @notice Ownerless fee state machine and fee-EarnShare custodian for one immutable EarnVault.
/// @dev Only the paired EarnVault mutates accounting. The EarnVault remains the sole EarnShare issuer and
///      atomically mints exactly the amount returned by a checkpoint to this contract.
contract EarnFees is IEarnFees {
    uint256 internal constant MAX_FIXED_FEE_CAP = 0.25e18;
    uint256 internal constant MAX_EXCESS_FEE_CAP = 1e18;
    uint256 internal constant MAX_ANNUAL_TARGET_RATE = 1e18;

    address public earnVault;
    address public earnShare;
    uint96 public fixedFeeCap;
    uint96 public excessFeeCap;
    uint256 public earnShareScale;

    uint64 public currentFeeConfigId;
    uint256 public highWaterMark;
    uint256 public targetBase;
    uint40 public targetStartedAt;
    bool public feesDisabled;

    mapping(uint64 configId => FeeConfig config) private _feeConfigs;
    mapping(uint64 configId => mapping(uint8 slot => uint256 remainder)) private _feeRemainders;
    mapping(uint64 configId => uint256 count) private _openRequestsByFeeConfig;
    mapping(bytes32 requestId => PendingFeeSnapshot snapshot) private _pendingFeeSnapshots;
    mapping(address recipient => uint256 earnShares) public claimableEarnShares;
    uint256 public totalClaimableEarnShares;

    uint256 private locked;
    bool private initialized;

    event FeesAccrued(
        uint64 indexed configId,
        uint256 activeAssets,
        uint256 positiveAccrualAssets,
        uint256 feeAssets,
        uint256 feeEarnShares,
        uint256 highWaterMark,
        uint256 targetValuePerEarnShare
    );
    event FeeEarnSharesAllocated(
        uint64 indexed configId, address indexed recipient, uint256 feeAssets, uint256 feeEarnShares
    );
    event FeeEarnSharesClaimed(address indexed recipient, address indexed to, uint256 earnShares);
    event FeeConfigurationSet(uint64 indexed configId, bytes32 indexed configHash, bool reactivated);
    event FeeDustWaived(uint64 indexed configId, uint8 indexed slot, uint256 remainder);
    event FeesDisabled(address indexed guardian);
    event FeeBaselinesInitialized(uint256 highWaterMark, uint256 targetBase, uint40 targetStartedAt);

    error AlreadyInitialized();
    error FeeCapTooHigh();
    error FeesPermanentlyDisabled();
    error InsufficientClaimableEarnShares();
    error InvalidFeeClaimReceiver();
    error InvalidFeeConfiguration();
    error NotEarnVault();
    error ReentrantCall();
    error ZeroAddress();
    error ZeroAmount();

    constructor() {
        initialized = true;
    }

    modifier onlyEarnVault() {
        if (msg.sender != earnVault) revert NotEarnVault();
        _;
    }

    modifier nonReentrant() {
        if (locked != 1) revert ReentrantCall();
        locked = 2;
        _;
        locked = 1;
    }

    function initialize(address earnVault_, address earnShare_, EarnFeesInit calldata init) external {
        if (initialized) revert AlreadyInitialized();
        initialized = true;
        if (earnVault_ == address(0) || earnShare_ == address(0)) revert ZeroAddress();
        if (init.fixedFeeCap > MAX_FIXED_FEE_CAP || init.excessFeeCap > MAX_EXCESS_FEE_CAP) {
            revert FeeCapTooHigh();
        }

        earnVault = earnVault_;
        earnShare = earnShare_;
        fixedFeeCap = init.fixedFeeCap;
        excessFeeCap = init.excessFeeCap;

        uint8 decimals = IEarnShare(earnShare_).decimals();
        if (decimals > 77) revert InvalidFeeConfiguration();
        earnShareScale = 10 ** uint256(decimals);

        _validateFeeConfig(init.initialConfig);
        currentFeeConfigId = 1;
        _storeFeeConfig(1, init.initialConfig);
        locked = 1;
        emit FeeConfigurationSet(1, keccak256(abi.encode(init.initialConfig)), false);
    }

    function feeConfig(uint64 configId) external view returns (FeeConfig memory) {
        return _feeConfigs[configId];
    }

    function feeRemainder(uint64 configId, uint8 slot) external view returns (uint256) {
        return _feeRemainders[configId][slot];
    }

    function feesActive() external view returns (bool) {
        return _feesActive();
    }

    function pendingFeeSnapshot(bytes32 requestId) external view returns (PendingFeeSnapshot memory) {
        return _pendingFeeSnapshots[requestId];
    }

    function shouldChargeRedeem(bytes32 requestId) external view returns (bool) {
        PendingFeeSnapshot storage pending = _pendingFeeSnapshots[requestId];
        return pending.open && !feesDisabled && _configHasFees(_feeConfigs[pending.feeConfigId]);
    }

    function previewAccruedFees() public view returns (FeePreview memory result) {
        if (!_feesActive()) return result;
        uint256 earnShareSupply = IEarnShare(earnShare).totalSupply();
        if (earnShareSupply == 0) return result;

        (result,) = EarnFeeMath.preview(
            EarnFeeMath.Input({
                activeAssets: IEarnEngine(_engine()).totalAssets(),
                earnShareSupply: earnShareSupply,
                earnShareScale: earnShareScale,
                highWaterMark: highWaterMark,
                targetBase: targetBase,
                targetStartedAt: targetStartedAt,
                timestamp: block.timestamp
            }),
            _feeConfigs[currentFeeConfigId],
            _remainders(currentFeeConfigId)
        );
    }

    function accrueFees() external onlyEarnVault returns (FeePreview memory result) {
        result = _accrueFees();
    }

    function setFeeConfig(FeeConfig calldata config) external onlyEarnVault returns (uint64 configId) {
        if (fixedFeeCap == 0 && excessFeeCap == 0) revert FeesPermanentlyDisabled();
        _validateFeeConfig(config);

        bool reactivated = feesDisabled;
        bool wasActive = _feesActive();
        FeeConfig memory oldConfig = _feeConfigs[currentFeeConfigId];
        uint256 oldTargetNow = oldConfig.excess.enabled
            ? EarnFeeMath.targetAt(targetBase, oldConfig.excess.annualTargetRate, targetStartedAt, block.timestamp)
            : 0;
        _waiveCurrentRemainders();

        configId = currentFeeConfigId + 1;
        currentFeeConfigId = configId;
        _storeFeeConfig(configId, config);
        feesDisabled = false;

        if (_configHasFees(config)) {
            if (!wasActive || reactivated) {
                _initializeFeeBaselines();
            } else if (config.excess.enabled) {
                if (oldConfig.excess.enabled) {
                    targetBase = oldTargetNow;
                    targetStartedAt = uint40(block.timestamp);
                } else {
                    uint256 earnShareSupply = IEarnShare(earnShare).totalSupply();
                    targetBase = earnShareSupply == 0
                        ? 0
                        : Math.mulDiv(IEarnEngine(_engine()).totalAssets(), earnShareScale, earnShareSupply);
                    targetStartedAt = uint40(block.timestamp);
                }
            } else {
                targetBase = 0;
                targetStartedAt = 0;
            }
        }

        emit FeeConfigurationSet(configId, keccak256(abi.encode(config)), reactivated);
    }

    function disableFees(address guardian) external onlyEarnVault {
        if (feesDisabled) return;
        feesDisabled = true;
        _waiveCurrentRemainders();
        emit FeesDisabled(guardian);
    }

    function initializeBaselines() external onlyEarnVault {
        if (_feesActive()) _initializeFeeBaselines();
    }

    function recordRedeemRequest(bytes32 requestId, uint256 burnedEarnShares, uint256 requestValue)
        external
        onlyEarnVault
    {
        if (!_feesActive()) return;
        PendingFeeSnapshot storage pending = _pendingFeeSnapshots[requestId];
        if (pending.open) revert InvalidFeeConfiguration();

        pending.burnedEarnShares = burnedEarnShares;
        pending.requestValue = requestValue;
        pending.highWaterValue = Math.mulDiv(highWaterMark, burnedEarnShares, earnShareScale);
        FeeConfig memory config = _feeConfigs[currentFeeConfigId];
        if (config.excess.enabled) {
            uint256 currentTarget =
                EarnFeeMath.targetAt(targetBase, config.excess.annualTargetRate, targetStartedAt, block.timestamp);
            pending.targetValue = Math.mulDiv(currentTarget, burnedEarnShares, earnShareScale);
        }
        pending.feeConfigId = currentFeeConfigId;
        pending.requestedAt = uint40(block.timestamp);
        pending.open = true;
        _openRequestsByFeeConfig[currentFeeConfigId] += 1;
    }

    function closeRedeemRequest(bytes32 requestId) external onlyEarnVault {
        PendingFeeSnapshot storage pending = _pendingFeeSnapshots[requestId];
        if (!pending.open) return;
        uint64 configId = pending.feeConfigId;
        delete _pendingFeeSnapshots[requestId];
        _openRequestsByFeeConfig[configId] -= 1;
        _waiveClosedHistoricalRemainders(configId);
    }

    function settleCancelledRedeem(
        bytes32 requestId,
        uint256 returnedValue,
        uint256 activeSupply,
        uint256 activeAssets,
        uint256 totalReentryEarnShares
    ) external onlyEarnVault returns (uint256 feeEarnShares) {
        PendingFeeSnapshot storage pending = _pendingFeeSnapshots[requestId];
        if (!pending.open) return 0;
        uint64 configId = pending.feeConfigId;

        bool chargePending = !feesDisabled && _configHasFees(_feeConfigs[configId]);
        if (chargePending) {
            (FeePreview memory pendingFees, uint256[5] memory nextRemainders) =
                _previewPendingFees(pending, returnedValue);
            _storeRemainders(configId, nextRemainders);

            feeEarnShares =
                returnedValue == 0 ? 0 : Math.mulDiv(totalReentryEarnShares, pendingFees.totalFeeAssets, returnedValue);
            if (feeEarnShares > totalReentryEarnShares) feeEarnShares = totalReentryEarnShares;
            _creditAllocatedFeeEarnShares(configId, pendingFees, feeEarnShares);

            if (activeSupply == 0 && _feesActive()) {
                _restoreReopenedBaselines(pending, totalReentryEarnShares);
            } else if (activeSupply != 0 && activeAssets == 0) {
                revert InvalidFeeConfiguration();
            }
        }

        delete _pendingFeeSnapshots[requestId];
        _openRequestsByFeeConfig[configId] -= 1;
        _waiveClosedHistoricalRemainders(configId);
    }

    function claim(address to, uint256 earnShares) external nonReentrant {
        if (to == address(0) || to == address(this) || to == earnVault) revert InvalidFeeClaimReceiver();
        if (earnShares == 0) revert ZeroAmount();
        uint256 claimable = claimableEarnShares[msg.sender];
        if (earnShares > claimable) revert InsufficientClaimableEarnShares();

        claimableEarnShares[msg.sender] = claimable - earnShares;
        totalClaimableEarnShares -= earnShares;
        _safeTransfer(earnShare, to, earnShares);
        emit FeeEarnSharesClaimed(msg.sender, to, earnShares);
    }

    function _accrueFees() internal returns (FeePreview memory result) {
        if (!_feesActive()) return result;
        uint256 earnShareSupply = IEarnShare(earnShare).totalSupply();
        if (earnShareSupply == 0) return result;

        uint256[5] memory nextRemainders;
        (result, nextRemainders) = EarnFeeMath.preview(
            EarnFeeMath.Input({
                activeAssets: IEarnEngine(_engine()).totalAssets(),
                earnShareSupply: earnShareSupply,
                earnShareScale: earnShareScale,
                highWaterMark: highWaterMark,
                targetBase: targetBase,
                targetStartedAt: targetStartedAt,
                timestamp: block.timestamp
            }),
            _feeConfigs[currentFeeConfigId],
            _remainders(currentFeeConfigId)
        );
        _storeRemainders(currentFeeConfigId, nextRemainders);

        if (result.totalFeeEarnShares != 0) {
            _creditCalculatedAllocations(currentFeeConfigId, result);
        }
        if (result.preFeeValuePerEarnShare > highWaterMark) highWaterMark = result.postFeeValuePerEarnShare;

        FeeConfig memory config = _feeConfigs[currentFeeConfigId];
        if (config.excess.enabled && result.postFeeValuePerEarnShare > result.targetValuePerEarnShare) {
            targetBase = result.postFeeValuePerEarnShare;
            targetStartedAt = uint40(block.timestamp);
        }

        emit FeesAccrued(
            currentFeeConfigId,
            result.activeAssets,
            result.positiveAccrualAssets,
            result.totalFeeAssets,
            result.totalFeeEarnShares,
            highWaterMark,
            result.targetValuePerEarnShare
        );
    }

    function _previewPendingFees(PendingFeeSnapshot storage pending, uint256 returnedValue)
        internal
        view
        returns (FeePreview memory result, uint256[5] memory nextRemainders)
    {
        uint256 pendingHighWater = pending.burnedEarnShares == 0
            ? 0
            : Math.mulDiv(pending.highWaterValue, earnShareScale, pending.burnedEarnShares);
        uint256 pendingTarget = pending.burnedEarnShares == 0
            ? 0
            : Math.mulDiv(pending.targetValue, earnShareScale, pending.burnedEarnShares);
        return EarnFeeMath.preview(
            EarnFeeMath.Input({
                activeAssets: returnedValue,
                earnShareSupply: pending.burnedEarnShares,
                earnShareScale: earnShareScale,
                highWaterMark: pendingHighWater,
                targetBase: pendingTarget,
                targetStartedAt: pending.requestedAt,
                timestamp: block.timestamp
            }),
            _feeConfigs[pending.feeConfigId],
            _remainders(pending.feeConfigId)
        );
    }

    function _creditCalculatedAllocations(uint64 configId, FeePreview memory fees) internal {
        for (uint256 i = 0; i < fees.allocationCount; i++) {
            uint256 earnShares = fees.allocations[i].feeEarnShares;
            if (earnShares != 0) {
                _creditFeeEarnShares(configId, fees.allocations[i].account, fees.allocations[i].feeAssets, earnShares);
            }
        }
    }

    function _creditAllocatedFeeEarnShares(uint64 configId, FeePreview memory fees, uint256 feeEarnShares) internal {
        if (feeEarnShares == 0 || fees.totalFeeAssets == 0) return;
        uint256 assigned;
        uint256 last = type(uint256).max;
        for (uint256 i = 0; i < fees.allocationCount; i++) {
            if (fees.allocations[i].feeAssets != 0) last = i;
        }
        for (uint256 i = 0; i < fees.allocationCount; i++) {
            uint256 earnShares;
            if (i == last) {
                earnShares = feeEarnShares - assigned;
            } else if (fees.allocations[i].feeAssets != 0) {
                earnShares = Math.mulDiv(feeEarnShares, fees.allocations[i].feeAssets, fees.totalFeeAssets);
                assigned += earnShares;
            }
            if (earnShares != 0) {
                _creditFeeEarnShares(configId, fees.allocations[i].account, fees.allocations[i].feeAssets, earnShares);
            }
        }
    }

    function _creditFeeEarnShares(uint64 configId, address account, uint256 feeAssets, uint256 earnShares) internal {
        claimableEarnShares[account] += earnShares;
        totalClaimableEarnShares += earnShares;
        emit FeeEarnSharesAllocated(configId, account, feeAssets, earnShares);
    }

    function _restoreReopenedBaselines(PendingFeeSnapshot storage pending, uint256 normalization) internal {
        if (normalization == 0) return;
        uint256 currentValue = Math.mulDiv(IEarnEngine(_engine()).totalAssets(), earnShareScale, normalization);
        uint256 pendingHighWater = Math.mulDiv(pending.highWaterValue, earnShareScale, pending.burnedEarnShares);
        highWaterMark = currentValue > pendingHighWater ? currentValue : pendingHighWater;

        FeeConfig memory pendingConfig = _feeConfigs[pending.feeConfigId];
        FeeConfig memory activeConfig = _feeConfigs[currentFeeConfigId];
        if (activeConfig.excess.enabled) {
            uint256 pendingTarget = Math.mulDiv(pending.targetValue, earnShareScale, pending.burnedEarnShares);
            uint256 grownTarget = EarnFeeMath.targetAt(
                pendingTarget, pendingConfig.excess.annualTargetRate, pending.requestedAt, block.timestamp
            );
            targetBase = currentValue > grownTarget ? currentValue : grownTarget;
            targetStartedAt = uint40(block.timestamp);
        }
    }

    function _initializeFeeBaselines() internal {
        uint256 earnShareSupply = IEarnShare(earnShare).totalSupply();
        if (earnShareSupply == 0) {
            highWaterMark = 0;
            targetBase = 0;
            targetStartedAt = 0;
            return;
        }
        uint256 valuePerShare = Math.mulDiv(IEarnEngine(_engine()).totalAssets(), earnShareScale, earnShareSupply);
        highWaterMark = valuePerShare;
        if (_feeConfigs[currentFeeConfigId].excess.enabled) {
            targetBase = valuePerShare;
            targetStartedAt = uint40(block.timestamp);
        } else {
            targetBase = 0;
            targetStartedAt = 0;
        }
        emit FeeBaselinesInitialized(highWaterMark, targetBase, targetStartedAt);
    }

    function _feesActive() internal view returns (bool) {
        return !feesDisabled && _configHasFees(_feeConfigs[currentFeeConfigId]);
    }

    function _configHasFees(FeeConfig memory config) internal pure returns (bool) {
        return config.fixedFeeCount != 0 || config.excess.enabled;
    }

    function _validateFeeConfig(FeeConfig memory config) internal view {
        if (config.fixedFeeCount > MAX_FIXED_FEE_RECIPIENTS) revert InvalidFeeConfiguration();
        uint256 totalFixed;
        for (uint256 i = 0; i < config.fixedFeeCount; i++) {
            address account = config.fixedFees[i].account;
            uint256 rate = config.fixedFees[i].rate;
            if (_invalidRecipient(account) || rate == 0) revert InvalidFeeConfiguration();
            for (uint256 j = 0; j < i; j++) {
                if (config.fixedFees[j].account == account) revert InvalidFeeConfiguration();
            }
            totalFixed += rate;
        }
        for (uint256 i = config.fixedFeeCount; i < MAX_FIXED_FEE_RECIPIENTS; i++) {
            if (config.fixedFees[i].account != address(0) || config.fixedFees[i].rate != 0) {
                revert InvalidFeeConfiguration();
            }
        }
        if (totalFixed > fixedFeeCap) revert InvalidFeeConfiguration();

        if (config.excess.enabled) {
            if (
                _invalidRecipient(config.excess.account) || config.excess.excessFeeRate == 0
                    || config.excess.excessFeeRate > excessFeeCap
                    || config.excess.annualTargetRate > MAX_ANNUAL_TARGET_RATE
            ) revert InvalidFeeConfiguration();
        } else if (
            config.excess.account != address(0) || config.excess.annualTargetRate != 0
                || config.excess.excessFeeRate != 0
        ) {
            revert InvalidFeeConfiguration();
        }
    }

    function _invalidRecipient(address account) internal view returns (bool) {
        return account == address(0) || account == address(this) || account == earnVault;
    }

    function _storeFeeConfig(uint64 configId, FeeConfig memory config) internal {
        FeeConfig storage stored = _feeConfigs[configId];
        stored.fixedFeeCount = config.fixedFeeCount;
        for (uint256 i = 0; i < MAX_FIXED_FEE_RECIPIENTS; i++) {
            stored.fixedFees[i] = config.fixedFees[i];
        }
        stored.excess = config.excess;
    }

    function _remainders(uint64 configId) internal view returns (uint256[5] memory remainders) {
        for (uint8 i = 0; i < 5; i++) {
            remainders[i] = _feeRemainders[configId][i];
        }
    }

    function _storeRemainders(uint64 configId, uint256[5] memory remainders) internal {
        for (uint8 i = 0; i < 5; i++) {
            _feeRemainders[configId][i] = remainders[i];
        }
    }

    function _waiveCurrentRemainders() internal {
        if (!feesDisabled && _openRequestsByFeeConfig[currentFeeConfigId] != 0) return;
        _waiveRemainders(currentFeeConfigId);
    }

    function _waiveClosedHistoricalRemainders(uint64 configId) internal {
        if (configId == 0 || configId == currentFeeConfigId || _openRequestsByFeeConfig[configId] != 0) return;
        _waiveRemainders(configId);
    }

    function _waiveRemainders(uint64 configId) internal {
        for (uint8 i = 0; i < 5; i++) {
            uint256 remainder = _feeRemainders[configId][i];
            if (remainder == 0) continue;
            delete _feeRemainders[configId][i];
            emit FeeDustWaived(configId, i, remainder);
        }
    }

    function _engine() internal view returns (address) {
        return IEarnVaultFeeContext(earnVault).engine();
    }

    function _safeTransfer(address token, address to, uint256 value) internal {
        (bool ok, bytes memory result) = token.call(abi.encodeCall(IERC20Like.transfer, (to, value)));
        if (!ok || (result.length != 0 && !abi.decode(result, (bool)))) revert InvalidFeeClaimReceiver();
    }
}
