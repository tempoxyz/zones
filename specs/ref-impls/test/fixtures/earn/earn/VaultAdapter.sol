// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { Math } from "../libraries/Math.sol";

import { IERC20Like } from "../interfaces/IERC20Like.sol";
import { IEarnShareToken } from "../interfaces/IEarnShareToken.sol";
import { IRedeemReceiver } from "../interfaces/IRedeemReceiver.sol";
import { IVaultEngine } from "../interfaces/IVaultEngine.sol";
import { IVaultEngineAsync } from "../interfaces/IVaultEngineAsync.sol";
import { IVaultEngineShares } from "../interfaces/IVaultEngineShares.sol";
import {
    ContributionPreview,
    FeeConfig,
    FeeInit,
    FeePreview,
    MAX_FIXED_FEE_RECIPIENTS
} from "../interfaces/IVaultFees.sol";
import { FeeMath } from "./FeeMath.sol";

/// @notice Custody, issuance, fee accounting, and async-claim adapter for one Earn deployment.
/// @dev The adapter is the exclusive EarnToken issuer and the single client of its current engine.
///      Engines hold venue shares; the adapter holds only in-flight assets and unclaimed fee
///      EarnToken. Fees apply only to positive asset value per EarnToken above a post-fee high-water
///      mark. They mint claimable EarnToken without transferring backing or calling recipients.
///
///      Sync exits pay receivers directly. An async request burns EarnToken immediately and removes
///      the queued venue shares from active NAV. Finalization forwards the fixed venue payout.
///      Cancellation measures returned backing and mints a fee-aware current-value re-entry to the
///      stored receiver. Contributions and active-pool fees may continue while claims are queued.
///
///      Each deployed adapter is a non-upgradeable ERC-1967 proxy pointing to the factory's fixed
///      implementation. All economic state, roles, caps, engine custody, and fee ledgers are isolated
///      per proxy. The implementation is locked against direct initialization.
contract VaultAdapter {

    uint256 public constant MAX_FIXED_FEE_CAP = 0.25e18;
    uint256 public constant MAX_EXCESS_FEE_CAP = 1e18;
    uint256 public constant MAX_ANNUAL_TARGET_RATE = 1e18;

    /// @notice The current yield engine and sole custodian of the venue shares backing this adapter's
    ///         TIP20 supply. GOVERNED-MUTABLE: swapped only by {migrateEngine} (operator/governance).
    address public engine;
    address public asset;
    address public shareToken;
    address public operator;
    address public feeAdministrator;
    address public feeGuardian;
    uint96 public fixedFeeCap;
    uint96 public excessFeeCap;
    uint256 public shareScale;

    /// @notice (venue-shares : TIP20) exchange anchor. `anchorEngineShares` venue shares are worth
    ///         `anchorSupply` TIP20. Initialised 1:1, explicitly re-anchored by {migrateEngine} or
    ///         permissionless {contribute} funding, and restated after ordinary sync actions only when
    ///         needed to absorb conversion dust.
    uint256 public anchorEngineShares;
    uint256 public anchorSupply;

    uint256 public pendingRedeemCount;

    uint64 public currentFeeConfigId;
    uint256 public highWaterMark;
    uint256 public targetBase;
    uint40 public targetStartedAt;
    bool public emergencyFeesDisabled;

    mapping(uint64 configId => FeeConfig config) private _feeConfigs;
    mapping(uint64 configId => mapping(uint8 slot => uint256 remainder)) private _feeRemainders;
    mapping(uint64 configId => uint256 count) public pendingRedeemsByFeeConfig;
    mapping(address recipient => uint256 shares) public claimableFeeShares;
    uint256 public totalClaimableFeeShares;

    struct PendingRedeem {
        address receiver;
        address requester;
        uint256 burnedEarnToken;
        uint256 venueShares;
        uint256 requestValue;
        uint256 highWaterValue;
        uint256 targetValue;
        uint64 feeConfigId;
        uint40 requestedAt;
        bool open;
    }

    mapping(bytes32 requestId => PendingRedeem) private _pending;

    uint256 private locked;
    bool private initialized;

    event Deposited(
        address indexed caller, address indexed receiver, uint256 assets, uint256 shares
    );
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
    event Contributed(
        address indexed caller,
        uint256 assets,
        uint256 venueShares,
        uint256 anchorEngineShares,
        uint256 anchorSupply
    );
    event RedeemRequested(
        bytes32 indexed requestId,
        address indexed requester,
        address indexed receiver,
        uint256 shares
    );
    event RedeemFinalized(
        bytes32 indexed requestId,
        address indexed receiver,
        uint256 shares,
        address asset,
        uint256 amount
    );
    event RedeemCancelled(bytes32 indexed requestId, address indexed receiver, uint256 shares);
    event FeesAccrued(
        uint64 indexed configId,
        uint256 activeAssets,
        uint256 positiveAccrualAssets,
        uint256 feeAssets,
        uint256 feeShares,
        uint256 highWaterMark,
        uint256 targetValuePerShare
    );
    event FeeSharesAllocated(
        uint64 indexed configId, address indexed recipient, uint256 feeAssets, uint256 feeShares
    );
    event FeeSharesClaimed(address indexed recipient, address indexed to, uint256 shares);
    event FeeConfigurationSet(
        uint64 indexed configId, bytes32 indexed configHash, bool reactivated
    );
    event FeeDustWaived(uint64 indexed configId, uint8 indexed slot, uint256 remainder);
    event FeesDisabled(address indexed guardian);
    event FeeBaselinesInitialized(
        uint256 highWaterMark, uint256 targetBase, uint40 targetStartedAt
    );
    /// @notice Emitted when {migrateEngine} atomically re-homes custody and re-anchors accounting.
    ///         `shareSupply` is unchanged across the swap; `oldShares`/`newShares` are the venue-share
    ///         counts before/after (they differ at the new engine's rate).
    event EngineMigrated(
        address indexed oldEngine,
        address indexed newEngine,
        uint256 oldShares,
        uint256 assetsMoved,
        uint256 newShares,
        uint256 shareSupply,
        uint256 anchorEngineShares,
        uint256 anchorSupply
    );

    error AsyncNotSupported();
    error DuplicateRequest(bytes32 requestId);
    error EngineAssetMismatch();
    error ExceedsMaxShares();
    error InitialShareSupplyNotZero();
    error InvalidShareDecimals();
    error InvalidFeeClaimReceiver();
    error InsufficientOutput();
    error MinimumSharesNotMet(uint256 minimumShares, uint256 actualShares);
    error AlreadyInitialized();
    error NoShareSupply();
    error FeesPermanentlyDisabled();
    error InvalidFeeConfiguration();
    error FeeCapTooHigh();
    error InsufficientClaimableFeeShares();
    error NotFeeAdministrator();
    error NotFeeGuardian();
    error NotEngine();
    error NotOperator();
    error NotRequesterOrOperator();
    error PendingRedeemsOpen();
    error ReentrantCall();
    error RequestNotOpen(bytes32 requestId);
    error ResidualBacking();
    error SameEngine();
    error SharesTooLarge(uint256 shares);
    error TokenCallFailed();
    error TokenCallFalse();
    error ZeroAddress();
    error ZeroAmount();
    error ZeroMinimumShares();

    /// @dev Locks the standalone implementation. Each adapter is an ERC-1967 proxy with no upgrade
    ///      entrypoint, initialized atomically by {EarnFactory} against this fixed implementation.
    constructor() {
        initialized = true;
    }

    function initialize(
        address engine_,
        address shareToken_,
        address operator_,
        FeeInit memory feeInit_
    )
        external
    {
        if (initialized) revert AlreadyInitialized();
        initialized = true;
        if (engine_ == address(0) || shareToken_ == address(0) || operator_ == address(0)) {
            revert ZeroAddress();
        }

        address asset_ = IVaultEngine(engine_).asset();
        if (asset_ == address(0)) revert ZeroAddress();
        if (IEarnShareToken(shareToken_).totalSupply() != 0) revert InitialShareSupplyNotZero();

        engine = engine_;
        asset = asset_;
        shareToken = shareToken_;
        operator = operator_;

        uint256 fixedCap = feeInit_.fixedFeeCap;
        uint256 excessCap = feeInit_.excessFeeCap;
        if (fixedCap > MAX_FIXED_FEE_CAP || excessCap > MAX_EXCESS_FEE_CAP) revert FeeCapTooHigh();
        if (
            (fixedCap != 0 || excessCap != 0)
                && (feeInit_.administrator == address(0) || feeInit_.guardian == address(0))
        ) {
            revert ZeroAddress();
        }

        feeAdministrator = feeInit_.administrator;
        feeGuardian = feeInit_.guardian;
        fixedFeeCap = feeInit_.fixedFeeCap;
        excessFeeCap = feeInit_.excessFeeCap;
        uint8 shareDecimals = IEarnShareToken(shareToken_).decimals();
        if (shareDecimals > 77) revert InvalidShareDecimals();
        shareScale = 10 ** shareDecimals;
        anchorEngineShares = 1;
        anchorSupply = 1;
        locked = 1;

        _validateFeeConfig(feeInit_.initialConfig);
        currentFeeConfigId = 1;
        _storeFeeConfig(1, feeInit_.initialConfig);
        emit FeeConfigurationSet(1, keccak256(abi.encode(feeInit_.initialConfig)), false);
    }

    modifier nonReentrant() {
        if (locked != 1) revert ReentrantCall();
        locked = 2;
        _;
        locked = 1;
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

    function accrueFees() external nonReentrant returns (uint256 feeAssets, uint256 feeShares) {
        FeePreview memory result = _accrueFees();
        return (result.totalFeeAssets, result.totalFeeShares);
    }

    function previewAccruedFees() public view returns (FeePreview memory result) {
        if (!_feesActive()) return result;
        uint256 supply = IEarnShareToken(shareToken).totalSupply();
        if (supply == 0) return result;

        (result,) = FeeMath.preview(
            FeeMath.Input({
                activeAssets: IVaultEngine(engine).totalAssets(),
                supply: supply,
                shareScale: shareScale,
                highWaterMark: highWaterMark,
                targetBase: targetBase,
                targetStartedAt: targetStartedAt,
                timestamp: block.timestamp
            }),
            _feeConfigs[currentFeeConfigId],
            _currentRemainders()
        );
    }

    function previewContributionOutcome(uint256 assets)
        external
        view
        returns (ContributionPreview memory result)
    {
        result.assumedAssetsCredited = assets;
        uint256 supply = IEarnShareToken(shareToken).totalSupply();
        if (supply == 0 || !_feesActive()) {
            result.netHolderAssets = assets;
            return result;
        }

        FeeConfig memory config = _feeConfigs[currentFeeConfigId];
        uint256[5] memory remainders = _currentRemainders();
        (FeePreview memory existing, uint256[5] memory afterExisting) = FeeMath.preview(
            FeeMath.Input({
                activeAssets: IVaultEngine(engine).totalAssets(),
                supply: supply,
                shareScale: shareScale,
                highWaterMark: highWaterMark,
                targetBase: targetBase,
                targetStartedAt: targetStartedAt,
                timestamp: block.timestamp
            }),
            config,
            remainders
        );

        uint256 nextSupply = supply;
        uint256 nextHighWater = highWaterMark;
        uint256 nextTargetBase = targetBase;
        uint256 nextTargetStartedAt = targetStartedAt;

        // Match `_accrueFees`: if an asset-denominated fee is too small to mint even one
        // EarnToken unit, no state checkpoint occurs. Preview the contribution together with that
        // still-unsettled growth and the original precision remainders.
        bool existingSettles = existing.totalFeeAssets == 0 || existing.totalFeeShares != 0;
        if (existingSettles) {
            nextSupply += existing.totalFeeShares;
            if (existing.preFeeValuePerShare > highWaterMark) {
                nextHighWater = existing.postFeeValuePerShare;
            }
            remainders = afterExisting;
            if (
                config.excess.enabled
                    && existing.postFeeValuePerShare > existing.targetValuePerShare
            ) {
                nextTargetBase = existing.postFeeValuePerShare;
                nextTargetStartedAt = block.timestamp;
            }
        }

        (result.fees,) = FeeMath.preview(
            FeeMath.Input({
                activeAssets: existing.activeAssets + assets,
                supply: nextSupply,
                shareScale: shareScale,
                highWaterMark: nextHighWater,
                targetBase: nextTargetBase,
                targetStartedAt: nextTargetStartedAt,
                timestamp: block.timestamp
            }),
            config,
            remainders
        );
        result.netHolderAssets =
            assets > result.fees.totalFeeAssets ? assets - result.fees.totalFeeAssets : 0;
    }

    function setFeeConfig(FeeConfig calldata config)
        external
        nonReentrant
        returns (uint64 configId)
    {
        if (msg.sender != feeAdministrator) revert NotFeeAdministrator();
        if (fixedFeeCap == 0 && excessFeeCap == 0) revert FeesPermanentlyDisabled();
        _validateFeeConfig(config);

        bool reactivated = emergencyFeesDisabled;
        bool wasActive = _feesActive();
        FeeConfig memory oldConfig = _feeConfigs[currentFeeConfigId];
        uint256 oldTargetNow = 0;
        if (wasActive) {
            FeePreview memory accrued = _accrueFees();
            oldTargetNow = accrued.targetValuePerShare;
        }
        _waiveCurrentRemainders();

        configId = currentFeeConfigId + 1;
        currentFeeConfigId = configId;
        _storeFeeConfig(configId, config);
        emergencyFeesDisabled = false;

        if (_configHasFees(config)) {
            if (!wasActive || reactivated) {
                _initializeFeeBaselines();
            } else if (config.excess.enabled) {
                if (oldConfig.excess.enabled) {
                    targetBase = oldTargetNow;
                    targetStartedAt = uint40(block.timestamp);
                } else {
                    uint256 supply = IEarnShareToken(shareToken).totalSupply();
                    targetBase = supply == 0
                        ? 0
                        : Math.mulDiv(IVaultEngine(engine).totalAssets(), shareScale, supply);
                    targetStartedAt = uint40(block.timestamp);
                }
            } else {
                targetBase = 0;
                targetStartedAt = 0;
            }
        }

        emit FeeConfigurationSet(configId, keccak256(abi.encode(config)), reactivated);
    }

    function disableFees() external {
        if (msg.sender != feeGuardian) revert NotFeeGuardian();
        if (emergencyFeesDisabled) return;
        emergencyFeesDisabled = true;
        _waiveCurrentRemainders();
        emit FeesDisabled(msg.sender);
    }

    function claimFeeShares(address to, uint256 shares) external nonReentrant {
        if (to == address(0) || to == address(this)) revert InvalidFeeClaimReceiver();
        if (shares == 0) revert ZeroAmount();
        uint256 claimable = claimableFeeShares[msg.sender];
        if (shares > claimable) revert InsufficientClaimableFeeShares();

        claimableFeeShares[msg.sender] = claimable - shares;
        totalClaimableFeeShares -= shares;
        _safeTransfer(shareToken, to, shares);
        emit FeeSharesClaimed(msg.sender, to, shares);
    }

    /// @notice Deposits `assets` of the engine asset pulled from the caller into the engine (which
    ///         holds the resulting venue shares) and mints the RATE-converted share-token amount to
    ///         `receiver`. Before any contribution or migration the rate is 1:1; afterwards the
    ///         measured venue-share delta is converted at the active anchor. `minShares` protects
    ///         the caller from intervening contributions, venue-rate movement, and floor rounding.
    function deposit(
        uint256 assets,
        address receiver,
        uint256 minShares
    )
        external
        nonReentrant
        returns (uint256 shares)
    {
        if (assets == 0) revert ZeroAmount();
        if (receiver == address(0)) revert ZeroAddress();
        if (minShares == 0) revert ZeroMinimumShares();

        uint256 supplyBefore = IEarnShareToken(shareToken).totalSupply();
        _accrueFees();

        _safeTransferFrom(asset, msg.sender, address(this), assets);
        _safeApprove(asset, engine, 0);
        _safeApprove(asset, engine, assets);
        uint256 venueShares = IVaultEngine(engine).deposit(assets, address(this));
        if (venueShares == 0) revert InsufficientOutput();

        shares = _sharesToTokens(venueShares);
        if (shares == 0) revert InsufficientOutput();
        if (shares < minShares) revert MinimumSharesNotMet(minShares, shares);

        IEarnShareToken(shareToken).mint(receiver, shares);
        _reanchorAtRest();
        if (supplyBefore == 0 && _feesActive()) _initializeFeeBaselines();
        emit Deposited(msg.sender, receiver, assets, shares);
    }

    /// @notice Deposits shares of the current engine's venue directly and mints rate-converted
    ///         EarnToken to `receiver`. The caller approves the engine—not this adapter—to pull the
    ///         venue shares. Only engines implementing {IVaultEngineShares} support this path.
    /// @dev This is a principal-preserving entry path for existing venue shareholders. Fees are
    ///      crystallized before the new backing enters, and the proportional EarnToken mint prevents
    ///      the incoming principal from being treated as positive value accrual.
    function depositShares(
        uint256 venueShares,
        address receiver,
        uint256 minEarnShares
    )
        external
        nonReentrant
        returns (uint256 earnShares)
    {
        if (venueShares == 0) revert ZeroAmount();
        if (receiver == address(0)) revert ZeroAddress();
        if (minEarnShares == 0) revert ZeroMinimumShares();

        uint256 supplyBefore = IEarnShareToken(shareToken).totalSupply();
        _accrueFees();

        uint256 engineSharesBefore = IVaultEngine(engine).balanceOf(address(this));
        uint256 reportedVenueShares =
            IVaultEngineShares(engine).depositShares(venueShares, msg.sender);
        uint256 receivedVenueShares =
            IVaultEngine(engine).balanceOf(address(this)) - engineSharesBefore;
        if (receivedVenueShares == 0 || reportedVenueShares != receivedVenueShares) {
            revert InsufficientOutput();
        }

        earnShares = _sharesToTokens(receivedVenueShares);
        if (earnShares == 0) revert InsufficientOutput();
        if (earnShares < minEarnShares) revert MinimumSharesNotMet(minEarnShares, earnShares);

        IEarnShareToken(shareToken).mint(receiver, earnShares);
        _reanchorAtRest();
        if (supplyBefore == 0 && _feesActive()) _initializeFeeBaselines();
        emit VenueSharesDeposited(
            msg.sender, receiver, venueShares, receivedVenueShares, earnShares
        );
    }

    /// @notice Adds backing for current EarnToken holders without minting new EarnToken.
    /// @dev Permissionless and allowance-bound: any caller may contribute the base asset, while the
    ///      adapter stores no rewarder list or reward policy. The assets follow the normal engine
    ///      deposit path, then the conversion anchor is restated against the unchanged EarnToken
    ///      supply. With open async exits, only the active pool participates because queued claims
    ///      have already burned their EarnToken and left active NAV.
    function contribute(uint256 assets) external nonReentrant returns (uint256 venueShares) {
        if (assets == 0) revert ZeroAmount();

        uint256 supply = IEarnShareToken(shareToken).totalSupply();
        if (supply == 0) revert NoShareSupply();

        _accrueFees();

        _safeTransferFrom(asset, msg.sender, address(this), assets);
        _safeApprove(asset, engine, 0);
        _safeApprove(asset, engine, assets);
        venueShares = IVaultEngine(engine).deposit(assets, address(this));
        if (venueShares == 0) revert InsufficientOutput();

        anchorEngineShares = IVaultEngine(engine).balanceOf(address(this));
        anchorSupply = IEarnShareToken(shareToken).totalSupply();

        _accrueFees();

        emit Contributed(msg.sender, assets, venueShares, anchorEngineShares, anchorSupply);
    }

    /// @notice Burns `shares` share tokens pulled from the caller and redeems the RATE-converted venue
    ///         shares from the engine. Proceeds go directly to `receiver`.
    function redeem(
        uint256 shares,
        address receiver
    )
        external
        nonReentrant
        returns (uint256 assets)
    {
        if (shares == 0) revert ZeroAmount();
        if (receiver == address(0)) revert ZeroAddress();

        _accrueFees();

        _safeTransferFrom(shareToken, msg.sender, address(this), shares);
        IEarnShareToken(shareToken).burn(shares);

        uint256 venueShares = _tokensToShares(shares);
        if (venueShares == 0) revert InsufficientOutput();

        assets = IVaultEngine(engine).redeem(venueShares, receiver, address(this));
        if (assets == 0) revert InsufficientOutput();

        _reanchorAtRest();
        emit Redeemed(msg.sender, receiver, shares, assets);
    }

    /// @notice Withdraws an exact `assets` amount directly to `receiver`, burning the share tokens
    ///         actually consumed (pulled from the caller). Reverts if more than `maxShares` (TIP20
    ///         units) would be burned.
    /// @dev Used when public requests are denominated in assets rather than shares. The caller keeps
    ///      any unused share tokens.
    function withdrawExact(
        uint256 assets,
        address receiver,
        uint256 maxShares
    )
        external
        nonReentrant
        returns (uint256 sharesBurned)
    {
        if (assets == 0) revert ZeroAmount();
        if (receiver == address(0)) revert ZeroAddress();

        _accrueFees();

        uint256 requiredVenue = IVaultEngine(engine).previewWithdraw(assets);
        if (requiredVenue == 0) revert InsufficientOutput();
        uint256 requiredTokens = _sharesToTokensUp(requiredVenue);
        if (requiredTokens == 0) revert InsufficientOutput();
        if (requiredTokens > maxShares) revert ExceedsMaxShares();

        uint256 venueBurned = IVaultEngine(engine).withdraw(assets, receiver, address(this));
        if (venueBurned == 0) revert InsufficientOutput();
        sharesBurned = _sharesToTokensUp(venueBurned);
        if (sharesBurned == 0) revert InsufficientOutput();
        if (sharesBurned > maxShares) revert ExceedsMaxShares();
        if (sharesBurned == IEarnShareToken(shareToken).totalSupply() && engineShares() != 0) {
            revert ResidualBacking();
        }

        _safeTransferFrom(shareToken, msg.sender, address(this), sharesBurned);
        IEarnShareToken(shareToken).burn(sharesBurned);

        _reanchorAtRest();
        emit WithdrewExact(msg.sender, receiver, assets, sharesBurned);
    }

    /// @notice Files an async redemption, burns `shares` immediately, and records a separate pending
    ///         claim containing the queued venue shares, request value, fee baselines, configuration
    ///         version, requester, and stored receiver.
    /// @dev The queued backing and burned EarnToken leave active accounting atomically. If the engine
    ///      request fails, the entire transaction—including the burn—reverts.
    function requestRedeemAsync(
        uint256 shares,
        address assetOut,
        uint16 discountBps,
        uint24 secondsToDeadline,
        address receiver
    )
        external
        nonReentrant
        returns (bytes32 requestId)
    {
        if (shares == 0) revert ZeroAmount();
        if (receiver == address(0) || assetOut == address(0)) revert ZeroAddress();
        if (!IVaultEngineAsync(engine).supportsAsync()) revert AsyncNotSupported();

        _accrueFees();

        uint256 venueShares = _tokensToShares(shares);
        if (venueShares == 0 || venueShares > type(uint128).max) {
            revert SharesTooLarge(venueShares);
        }
        // Bounded just above, so the downcast cannot truncate.
        // forge-lint: disable-next-line(unsafe-typecast)
        uint128 venueShares128 = uint128(venueShares);

        _safeTransferFrom(shareToken, msg.sender, address(this), shares);
        IEarnShareToken(shareToken).burn(shares);

        requestId = IVaultEngineAsync(engine)
            .requestRedeem(venueShares128, assetOut, discountBps, secondsToDeadline);

        _recordPendingRedeem(requestId, msg.sender, receiver, shares, venueShares);

        pendingRedeemCount += 1;
        _reanchorAtRest();
        emit RedeemRequested(requestId, msg.sender, receiver, shares);
    }

    /// @notice ENGINE-ONLY. Forwards a solved queued payout to the stored receiver and closes the
    ///         pending claim. No EarnToken is burned here because it was burned at request time.
    function finalizeRedeem(
        bytes32 requestId,
        address asset_,
        uint256 amount
    )
        external
        nonReentrant
    {
        if (msg.sender != engine) revert NotEngine();
        PendingRedeem storage p = _pending[requestId];
        if (!p.open) revert RequestNotOpen(requestId);

        address receiver = p.receiver;
        uint256 shares = p.burnedEarnToken;
        uint64 pendingConfigId = p.feeConfigId;
        p.open = false;
        pendingRedeemCount -= 1;
        if (pendingConfigId != 0) pendingRedeemsByFeeConfig[pendingConfigId] -= 1;

        // The engine already sent `amount` of `asset_` here; forward it to the stored receiver.
        if (amount != 0) _safeTransfer(asset_, receiver, amount);

        if (receiver.code.length != 0) {
            // forge-lint: disable-next-line(unused-return)
            try IRedeemReceiver(receiver).onRedeemFinalized(requestId, asset_, amount) { } catch { }
        }

        _waiveClosedHistoricalRemainders(pendingConfigId);
        emit RedeemFinalized(requestId, receiver, shares, asset_, amount);
    }

    /// @notice Cancels a still-open async request. The returned venue shares re-enter at the active
    ///         pool's current price, snapshotted fee terms apply to queued growth, and the resulting
    ///         EarnToken is minted to the same stored receiver used by finalization.
    function cancelRedeemAsync(bytes32 requestId) external nonReentrant {
        PendingRedeem storage p = _pending[requestId];
        if (!p.open) revert RequestNotOpen(requestId);
        if (msg.sender != p.requester && msg.sender != operator) revert NotRequesterOrOperator();

        address receiver = p.receiver;
        uint256 burnedEarnToken = p.burnedEarnToken;

        _accrueFees();
        bool chargePending = !emergencyFeesDisabled && p.feeConfigId != 0
            && _configHasFees(_feeConfigs[p.feeConfigId]);
        uint256 activeSupply = IEarnShareToken(shareToken).totalSupply();
        uint256 activeAssets =
            activeSupply == 0 || !chargePending ? 0 : IVaultEngine(engine).totalAssets();
        uint256 heldBefore = IVaultEngine(engine).balanceOf(address(this));

        IVaultEngineAsync(engine).cancelRedeem(requestId);
        uint256 heldAfter = IVaultEngine(engine).balanceOf(address(this));
        if (heldAfter <= heldBefore) revert InsufficientOutput();
        uint256 returnedVenueShares = heldAfter - heldBefore;

        p.open = false;
        pendingRedeemCount -= 1;
        if (p.feeConfigId != 0) pendingRedeemsByFeeConfig[p.feeConfigId] -= 1;

        uint256 receiverShares;
        uint256 totalReentryShares;
        if (emergencyFeesDisabled || !chargePending) {
            totalReentryShares =
                activeSupply == 0 ? burnedEarnToken : _sharesToTokens(returnedVenueShares);
            receiverShares = totalReentryShares;
        } else {
            uint256 returnedValue = IVaultEngine(engine).previewRedeem(returnedVenueShares);
            if (returnedValue == 0) revert InsufficientOutput();
            (FeePreview memory pendingFees, uint256[5] memory nextRemainders) =
                _previewPendingFees(p, returnedValue);
            _storeRemainders(p.feeConfigId, nextRemainders);

            if (activeSupply == 0) {
                totalReentryShares = burnedEarnToken;
            } else {
                if (activeAssets == 0) revert InsufficientOutput();
                totalReentryShares = Math.mulDiv(returnedValue, activeSupply, activeAssets);
            }
            if (totalReentryShares == 0) revert InsufficientOutput();

            uint256 feeShares = returnedValue == 0
                ? 0
                : Math.mulDiv(totalReentryShares, pendingFees.totalFeeAssets, returnedValue);
            if (feeShares > totalReentryShares) feeShares = totalReentryShares;
            _mintAndCreditFeeShares(p.feeConfigId, pendingFees, feeShares);
            receiverShares = totalReentryShares - feeShares;
        }

        if (receiverShares != 0) IEarnShareToken(shareToken).mint(receiver, receiverShares);
        _reanchorAtRest();

        if (activeSupply == 0 && !emergencyFeesDisabled && _feesActive()) {
            _restoreReopenedBaselines(p, totalReentryShares);
        }

        if (receiver.code.length != 0) {
            // forge-lint: disable-next-line(unused-return)
            try IRedeemReceiver(receiver)
                .onRedeemCancelled(requestId, shareToken, receiverShares) { }
                catch { }
        }

        _waiveClosedHistoricalRemainders(p.feeConfigId);
        emit RedeemCancelled(requestId, receiver, receiverShares);
    }

    /// @notice GOVERNANCE-ONLY (operator). Atomically swaps the yield engine in ONE tx: redeems ALL
    ///         venue shares from the current engine to stablecoins on the adapter, deposits them into
    ///         `newEngine` (which becomes the sole custodian of the new venue shares), repoints
    ///         `engine`, and RE-ANCHORS accounting so TIP20 totalSupply is UNCHANGED.
    /// @dev ATOMICITY: any failing leg (old-engine redeem, new-engine deposit) reverts the WHOLE tx, so
    ///      a failed migration leaves the pool entirely on the old engine. Requires no open async exits
    ///      (`pendingRedeemCount == 0`), because in-flight queued shares cannot be re-homed atomically, and
    ///      the new engine's asset must match this adapter's single asset.
    /// @dev RE-ANCHOR: TIP20 count is fixed; the venue-share count moves to `newEngine.balanceOf(this)`
    ///      at the new rate. The (venue-shares : TIP20) anchor is restated to
    ///      `anchorEngineShares = newEngine.balanceOf(this)`, `anchorSupply = totalSupply`, so the
    ///      restated invariant `engineShares == tokensToShares(totalSupply)` holds exactly right after
    ///      the swap. NAV per TIP20 is preserved; the share COUNT is not (that is the point).
    /// @dev TRUST: `operator` MUST be a timelock/multisig — this seat can move the entire pool's assets
    ///      between venues. Keep it, the async cancel seat, the gateway owner, the engine owner, and the
    ///      solver owner under the SAME governed key set.
    function migrateEngine(address newEngine) external nonReentrant returns (uint256 newShares) {
        if (msg.sender != operator) revert NotOperator();
        if (newEngine == address(0)) revert ZeroAddress();
        address oldEngine = engine;
        if (newEngine == oldEngine) revert SameEngine();
        if (pendingRedeemCount != 0) revert PendingRedeemsOpen();
        if (IVaultEngine(newEngine).asset() != asset) revert EngineAssetMismatch();

        _accrueFees();

        uint256 supply = IEarnShareToken(shareToken).totalSupply();
        uint256 oldShares = IVaultEngine(oldEngine).balanceOf(address(this));
        uint256 assetsMoved;
        if (oldShares != 0) {
            // Leg 1 — pull ALL value out of the old engine to the adapter. A dry/queue-only venue that
            // cannot honor a full sync redemption reverts here and the whole migration rolls back.
            assetsMoved = IVaultEngine(oldEngine).redeem(oldShares, address(this), address(this));
            if (assetsMoved == 0) revert InsufficientOutput();

            // Leg 2 — deposit everything into the new engine, which mints and holds the new venue shares.
            _safeApprove(asset, newEngine, 0);
            _safeApprove(asset, newEngine, assetsMoved);
            newShares = IVaultEngine(newEngine).deposit(assetsMoved, address(this));
            if (newShares == 0) revert InsufficientOutput();
        }

        engine = newEngine;

        // Re-anchor from the ACTUAL held count so `engineShares == tokensToShares(supply)` holds exactly.
        uint256 heldNow = IVaultEngine(newEngine).balanceOf(address(this));
        if (supply == 0 || heldNow == 0) {
            anchorEngineShares = 1;
            anchorSupply = 1;
        } else {
            anchorEngineShares = heldNow;
            anchorSupply = supply;
        }

        emit EngineMigrated(
            oldEngine,
            newEngine,
            oldShares,
            assetsMoved,
            newShares,
            supply,
            anchorEngineShares,
            anchorSupply
        );
    }

    /// @notice Estimates the base assets returned by redeeming `shares` of EarnToken through the
    ///         current engine after crystallizing any currently representable Earn-level fees.
    /// @dev Mirrors the fee-share mint and conditional re-anchor performed at the start of `redeem`,
    ///      then delegates venue-share valuation to the current engine so applicable venue exit fees
    ///      are reflected. This is a quote only; engine valuation may revert or change before execution.
    function previewRedeem(uint256 shares) public view returns (uint256 assets) {
        if (shares == 0) return 0;
        (uint256 engineShareAnchor, uint256 supplyAnchor) = _previewConversionAnchor();
        uint256 venueShares = Math.mulDiv(shares, engineShareAnchor, supplyAnchor);
        if (venueShares == 0) return 0;
        return IVaultEngine(engine).previewRedeem(venueShares);
    }

    /// @notice ERC-4626-shaped compatibility alias for the fee-aware EarnToken redemption quote.
    /// @dev `VaultAdapter` is not itself an ERC-4626 vault: EarnToken is a separate TIP-20, and this
    ///      view intentionally follows `previewRedeem` semantics including current engine exit fees.
    function convertToAssets(uint256 shares) external view returns (uint256 assets) {
        return previewRedeem(shares);
    }

    function previewWithdraw(uint256 assets) external view returns (uint256 shares) {
        uint256 venueShares = IVaultEngine(engine).previewWithdraw(assets);
        (uint256 engineShareAnchor, uint256 supplyAnchor) = _previewConversionAnchor();
        return Math.mulDiv(venueShares, supplyAnchor, engineShareAnchor, Math.Rounding.Ceil);
    }

    function engineShares() public view returns (uint256) {
        return IVaultEngine(engine).balanceOf(address(this));
    }

    function shareSupply() public view returns (uint256) {
        return IEarnShareToken(shareToken).totalSupply();
    }

    /// @notice Checks that live EarnToken supply converts exactly to active engine shares at the
    ///         current anchor. Pending async claims are excluded from both sides because their
    ///         EarnToken was burned when their venue shares entered the queue.
    function isSynced() external view returns (bool) {
        uint256 supply = shareSupply();
        uint256 held = engineShares();
        if (supply == 0 || held == 0) return supply == 0 && held == 0;
        return _tokensToShares(supply) == held;
    }

    /// @notice Reads the stored fields of the pending async request `requestId`.
    function pendingRedeem(bytes32 requestId) external view returns (PendingRedeem memory) {
        return _pending[requestId];
    }

    /// @notice Converts a TIP20 share-token amount to venue shares at the current re-anchored rate.
    function tokensToShares(uint256 tokens) external view returns (uint256) {
        return _tokensToShares(tokens);
    }

    /// @notice Converts a venue-share amount to TIP20 share tokens at the current re-anchored rate.
    function sharesToTokens(uint256 shares) external view returns (uint256) {
        return _sharesToTokens(shares);
    }

    function _tokensToShares(uint256 tokens) internal view returns (uint256) {
        return Math.mulDiv(tokens, anchorEngineShares, anchorSupply);
    }

    function _sharesToTokens(uint256 shares) internal view returns (uint256) {
        return Math.mulDiv(shares, anchorSupply, anchorEngineShares);
    }

    function _sharesToTokensUp(uint256 shares) internal view returns (uint256) {
        return Math.mulDiv(shares, anchorSupply, anchorEngineShares, Math.Rounding.Ceil);
    }

    /// @dev Projects the conversion anchor that `_accrueFees` and `_reanchorAtRest` would leave in
    ///      this block. If pending fee shares are too small to disturb the current total-supply
    ///      conversion, `_reanchorAtRest` keeps the stored anchor and so does this view.
    function _previewConversionAnchor()
        internal
        view
        returns (uint256 engineShareAnchor, uint256 supplyAnchor)
    {
        engineShareAnchor = anchorEngineShares;
        supplyAnchor = anchorSupply;

        uint256 feeShares = previewAccruedFees().totalFeeShares;
        if (feeShares == 0) return (engineShareAnchor, supplyAnchor);

        uint256 projectedSupply = IEarnShareToken(shareToken).totalSupply() + feeShares;
        uint256 held = IVaultEngine(engine).balanceOf(address(this));
        if (projectedSupply == 0 || held == 0) return (1, 1);
        if (Math.mulDiv(projectedSupply, engineShareAnchor, supplyAnchor) == held) {
            return (engineShareAnchor, supplyAnchor);
        }
        return (held, projectedSupply);
    }

    /// @dev Restates the anchor from actual backing whenever no async claim is in flight. This keeps
    ///      rounding dust in the exchange rate instead of letting it make accounting appear unsynced.
    function _reanchorAtRest() internal {
        uint256 supply = IEarnShareToken(shareToken).totalSupply();
        uint256 held = IVaultEngine(engine).balanceOf(address(this));
        if (supply == 0 || held == 0) {
            anchorEngineShares = 1;
            anchorSupply = 1;
            return;
        }
        if (_tokensToShares(supply) == held) return;

        anchorEngineShares = held;
        anchorSupply = supply;
    }

    function _accrueFees() internal returns (FeePreview memory result) {
        if (!_feesActive()) return result;
        uint256 supply = IEarnShareToken(shareToken).totalSupply();
        if (supply == 0) return result;

        uint256[5] memory nextRemainders;
        (result, nextRemainders) = FeeMath.preview(
            FeeMath.Input({
                activeAssets: IVaultEngine(engine).totalAssets(),
                supply: supply,
                shareScale: shareScale,
                highWaterMark: highWaterMark,
                targetBase: targetBase,
                targetStartedAt: targetStartedAt,
                timestamp: block.timestamp
            }),
            _feeConfigs[currentFeeConfigId],
            _currentRemainders()
        );

        // Leave the checkpoint untouched until an asset-denominated fee can be represented by at
        // least one EarnToken unit. The next call will include the same growth again.
        if (result.totalFeeAssets != 0 && result.totalFeeShares == 0) return result;

        for (uint8 i = 0; i < 5; i++) {
            _feeRemainders[currentFeeConfigId][i] = nextRemainders[i];
        }

        if (result.totalFeeShares != 0) {
            IEarnShareToken(shareToken).mint(address(this), result.totalFeeShares);
            _creditCalculatedAllocations(currentFeeConfigId, result);
            _reanchorAtRest();
        }

        if (result.preFeeValuePerShare > highWaterMark) {
            highWaterMark = result.postFeeValuePerShare;
        }

        FeeConfig memory config = _feeConfigs[currentFeeConfigId];
        if (config.excess.enabled && result.postFeeValuePerShare > result.targetValuePerShare) {
            targetBase = result.postFeeValuePerShare;
            targetStartedAt = uint40(block.timestamp);
        }

        emit FeesAccrued(
            currentFeeConfigId,
            result.activeAssets,
            result.positiveAccrualAssets,
            result.totalFeeAssets,
            result.totalFeeShares,
            highWaterMark,
            result.targetValuePerShare
        );
    }

    function _previewPendingFees(
        PendingRedeem storage pending,
        uint256 returnedValue
    )
        internal
        view
        returns (FeePreview memory result, uint256[5] memory nextRemainders)
    {
        FeeConfig memory config = _feeConfigs[pending.feeConfigId];
        uint256 pendingHighWater = pending.burnedEarnToken == 0
            ? 0
            : Math.mulDiv(pending.highWaterValue, shareScale, pending.burnedEarnToken);
        uint256 pendingTarget = pending.burnedEarnToken == 0
            ? 0
            : Math.mulDiv(pending.targetValue, shareScale, pending.burnedEarnToken);
        (result, nextRemainders) = FeeMath.preview(
            FeeMath.Input({
                activeAssets: returnedValue,
                supply: pending.burnedEarnToken,
                shareScale: shareScale,
                highWaterMark: pendingHighWater,
                targetBase: pendingTarget,
                targetStartedAt: pending.requestedAt,
                timestamp: block.timestamp
            }),
            config,
            _remainders(pending.feeConfigId)
        );
    }

    function _recordPendingRedeem(
        bytes32 requestId,
        address requester,
        address receiver,
        uint256 shares,
        uint256 venueShares
    )
        internal
    {
        PendingRedeem storage pending = _pending[requestId];
        if (pending.open) revert DuplicateRequest(requestId);
        pending.receiver = receiver;
        pending.requester = requester;
        pending.burnedEarnToken = shares;
        pending.venueShares = venueShares;
        pending.requestedAt = uint40(block.timestamp);
        pending.open = true;

        if (_feesActive()) {
            pending.requestValue = IVaultEngine(engine).previewRedeem(venueShares);
            pending.highWaterValue = Math.mulDiv(highWaterMark, shares, shareScale);
            FeeConfig memory config = _feeConfigs[currentFeeConfigId];
            if (config.excess.enabled) {
                uint256 currentTarget = FeeMath.targetAt(
                    targetBase, config.excess.annualTargetRate, targetStartedAt, block.timestamp
                );
                pending.targetValue = Math.mulDiv(currentTarget, shares, shareScale);
            }
            pending.feeConfigId = currentFeeConfigId;
            pendingRedeemsByFeeConfig[currentFeeConfigId] += 1;
        }
    }

    function _mintAndCreditFeeShares(
        uint64 configId,
        FeePreview memory fees,
        uint256 feeShares
    )
        internal
    {
        if (feeShares == 0 || fees.totalFeeAssets == 0) return;
        IEarnShareToken(shareToken).mint(address(this), feeShares);

        uint256 assigned = 0;
        uint256 last = type(uint256).max;
        for (uint256 i = 0; i < fees.allocationCount; i++) {
            if (fees.allocations[i].feeAssets != 0) last = i;
        }
        for (uint256 i = 0; i < fees.allocationCount; i++) {
            uint256 shares = 0;
            if (i == last) {
                shares = feeShares - assigned;
            } else if (fees.allocations[i].feeAssets != 0) {
                shares = Math.mulDiv(feeShares, fees.allocations[i].feeAssets, fees.totalFeeAssets);
                assigned += shares;
            }
            if (shares != 0) {
                _creditFeeShares(
                    configId, fees.allocations[i].account, fees.allocations[i].feeAssets, shares
                );
            }
        }
    }

    function _creditCalculatedAllocations(uint64 configId, FeePreview memory fees) internal {
        for (uint256 i = 0; i < fees.allocationCount; i++) {
            uint256 shares = fees.allocations[i].feeShares;
            if (shares != 0) {
                _creditFeeShares(
                    configId, fees.allocations[i].account, fees.allocations[i].feeAssets, shares
                );
            }
        }
    }

    function _creditFeeShares(
        uint64 configId,
        address account,
        uint256 feeAssets,
        uint256 shares
    )
        internal
    {
        claimableFeeShares[account] += shares;
        totalClaimableFeeShares += shares;
        emit FeeSharesAllocated(configId, account, feeAssets, shares);
    }

    function _restoreReopenedBaselines(
        PendingRedeem storage pending,
        uint256 normalization
    )
        internal
    {
        if (normalization == 0) return;
        uint256 currentValue =
            Math.mulDiv(IVaultEngine(engine).totalAssets(), shareScale, normalization);
        uint256 pendingHighWater =
            Math.mulDiv(pending.highWaterValue, shareScale, pending.burnedEarnToken);
        highWaterMark = currentValue > pendingHighWater ? currentValue : pendingHighWater;

        FeeConfig memory pendingConfig = _feeConfigs[pending.feeConfigId];
        FeeConfig memory activeConfig = _feeConfigs[currentFeeConfigId];
        if (activeConfig.excess.enabled) {
            uint256 pendingTarget =
                Math.mulDiv(pending.targetValue, shareScale, pending.burnedEarnToken);
            uint256 grownTarget = FeeMath.targetAt(
                pendingTarget,
                pendingConfig.excess.annualTargetRate,
                pending.requestedAt,
                block.timestamp
            );
            targetBase = currentValue > grownTarget ? currentValue : grownTarget;
            targetStartedAt = uint40(block.timestamp);
        }
    }

    function _initializeFeeBaselines() internal {
        uint256 supply = IEarnShareToken(shareToken).totalSupply();
        if (supply == 0) {
            highWaterMark = 0;
            targetBase = 0;
            targetStartedAt = 0;
            return;
        }
        uint256 valuePerShare = Math.mulDiv(IVaultEngine(engine).totalAssets(), shareScale, supply);
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
        return !emergencyFeesDisabled && _configHasFees(_feeConfigs[currentFeeConfigId]);
    }

    function _configHasFees(FeeConfig memory config) internal pure returns (bool) {
        return config.fixedFeeCount != 0 || config.excess.enabled;
    }

    function _validateFeeConfig(FeeConfig memory config) internal view {
        if (config.fixedFeeCount > MAX_FIXED_FEE_RECIPIENTS) revert InvalidFeeConfiguration();
        uint256 totalFixed = 0;
        for (uint256 i = 0; i < config.fixedFeeCount; i++) {
            address account = config.fixedFees[i].account;
            uint256 rate = config.fixedFees[i].rate;
            if (account == address(0) || account == address(this) || rate == 0) {
                revert InvalidFeeConfiguration();
            }
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
                config.excess.account == address(0) || config.excess.account == address(this)
                    || config.excess.excessFeeRate == 0
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

    function _storeFeeConfig(uint64 configId, FeeConfig memory config) internal {
        FeeConfig storage stored = _feeConfigs[configId];
        stored.fixedFeeCount = config.fixedFeeCount;
        for (uint256 i = 0; i < MAX_FIXED_FEE_RECIPIENTS; i++) {
            stored.fixedFees[i] = config.fixedFees[i];
        }
        stored.excess = config.excess;
    }

    function _currentRemainders() internal view returns (uint256[5] memory remainders) {
        return _remainders(currentFeeConfigId);
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
        if (!emergencyFeesDisabled && pendingRedeemsByFeeConfig[currentFeeConfigId] != 0) return;
        _waiveRemainders(currentFeeConfigId);
    }

    function _waiveClosedHistoricalRemainders(uint64 configId) internal {
        if (
            configId == 0 || configId == currentFeeConfigId
                || pendingRedeemsByFeeConfig[configId] != 0
        ) return;
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

    function _safeApprove(address token, address spender, uint256 value) internal {
        _callOptionalReturn(
            token, abi.encodeWithSelector(IERC20Like.approve.selector, spender, value)
        );
    }

    function _safeTransfer(address token, address to, uint256 value) internal {
        _callOptionalReturn(token, abi.encodeWithSelector(IERC20Like.transfer.selector, to, value));
    }

    function _safeTransferFrom(address token, address from, address to, uint256 value) internal {
        _callOptionalReturn(
            token, abi.encodeWithSelector(IERC20Like.transferFrom.selector, from, to, value)
        );
    }

    function _callOptionalReturn(address token, bytes memory data) internal {
        (bool ok, bytes memory returnData) = token.call(data);
        if (!ok) revert TokenCallFailed();
        if (returnData.length != 0 && !abi.decode(returnData, (bool))) revert TokenCallFalse();
    }

}
