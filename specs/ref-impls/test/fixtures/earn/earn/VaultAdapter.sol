// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { Math } from "../libraries/Math.sol";

import { IEarnShareToken } from "../interfaces/IEarnShareToken.sol";
import { IERC20Like } from "../interfaces/IERC20Like.sol";
import { IRedeemReceiver } from "../interfaces/IRedeemReceiver.sol";
import { IVaultEngine } from "../interfaces/IVaultEngine.sol";
import { IVaultEngineAsync } from "../interfaces/IVaultEngineAsync.sol";
import { IVaultEngineExactWithdraw } from "../interfaces/IVaultEngineExactWithdraw.sol";
import { IVaultEngineShares } from "../interfaces/IVaultEngineShares.sol";
import { IVaultEngineSync } from "../interfaces/IVaultEngineSync.sol";
import { IVaultAdapter } from "../interfaces/IVaultAdapter.sol";
import { ControlInit, EngineMigrationMode } from "../interfaces/IVaultControls.sol";
import { FeeConfig, FeeInit, FeePreview, MAX_FIXED_FEE_RECIPIENTS } from "../interfaces/IVaultFees.sol";
import { FeeMath } from "./FeeMath.sol";
import { ERC165Checker } from "@openzeppelin/contracts/utils/introspection/ERC165Checker.sol";

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
contract VaultAdapter is IVaultAdapter {
    uint256 internal constant MAX_FIXED_FEE_CAP = 0.25e18;
    uint256 internal constant MAX_EXCESS_FEE_CAP = 1e18;
    uint256 internal constant MAX_ANNUAL_TARGET_RATE = 1e18;
    uint256 internal constant CONVERSION_BPS = 10_000;

    /// @notice The current yield engine and sole custodian of the venue shares backing this adapter's
    ///         TIP20 supply. GOVERNED-MUTABLE: swapped only by {migrateEngine} (operator/governance).
    address public override engine;
    address public override asset;
    address public override shareToken;
    /// @notice Initialization-fixed governance seat. Production deployments should use a timelock.
    address public override operator;
    /// @notice Fast risk-reducing seat. It may pause new exposure but cannot resume it.
    address public override emergencyGuardian;
    /// @notice Narrow liveness seat that may cancel async requests only to their stored receivers.
    address public override asyncJanitor;
    address public feeAdministrator;
    address public feeGuardian;
    /// @notice Deployment-fixed whole-pool migration policy. There is intentionally no setter.
    EngineMigrationMode public override engineMigrationMode;
    uint96 public fixedFeeCap;
    uint96 public excessFeeCap;
    uint256 internal shareScale;

    /// @notice (venue-shares : TIP20) exchange anchor. `anchorEngineShares` venue shares are worth
    ///         `anchorSupply` TIP20. Initialised 1:1, explicitly re-anchored by {migrateEngine} or
    ///         shareless {contribute} funding, and restated after ordinary sync actions only when
    ///         needed to absorb conversion dust.
    uint256 public anchorEngineShares;
    uint256 public anchorSupply;

    uint256 public override pendingRedeemCount;

    uint64 public currentFeeConfigId;
    uint256 public highWaterMark;
    uint256 public targetBase;
    uint40 public targetStartedAt;
    bool public emergencyFeesDisabled;
    /// @notice Whether deposits, in-kind entry, and contributions are paused. Exit paths stay open.
    bool public override depositsPaused;

    mapping(uint64 configId => FeeConfig config) private _feeConfigs;
    mapping(uint64 configId => mapping(uint8 slot => uint256 remainder)) private _feeRemainders;
    mapping(uint64 configId => uint256 count) private _pendingRedeemsByFeeConfig;
    mapping(address recipient => uint256 shares) public override claimableFeeShares;
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

    event Contributed(
        address indexed caller, uint256 assets, uint256 venueShares, uint256 anchorEngineShares, uint256 anchorSupply
    );
    event RedeemRequested(
        bytes32 indexed requestId, address indexed requester, address indexed receiver, uint256 shares
    );
    event RedeemFinalized(
        bytes32 indexed requestId, address indexed receiver, uint256 shares, address asset, uint256 amount
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
    event FeeSharesAllocated(uint64 indexed configId, address indexed recipient, uint256 feeAssets, uint256 feeShares);
    event FeeSharesClaimed(address indexed recipient, address indexed to, uint256 shares);
    event FeeConfigurationSet(uint64 indexed configId, bytes32 indexed configHash, bool reactivated);
    event FeeDustWaived(uint64 indexed configId, uint8 indexed slot, uint256 remainder);
    event FeesDisabled(address indexed guardian);
    event FeeBaselinesInitialized(uint256 highWaterMark, uint256 targetBase, uint40 targetStartedAt);
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

    error DepositsPaused();
    error DuplicateRequest(bytes32 requestId);
    error EngineAssetMismatch();
    error EngineCapabilityUnsupported(bytes4 interfaceId);
    error ExcessiveConversionLoss(uint256 inputShares, uint256 representedShares);
    error ExceedsMaxShares();
    error InitialShareSupplyNotZero();
    error InvalidShareDecimals();
    error InvalidFeeClaimReceiver();
    error InsufficientOutput();
    error MinimumAssetsNotMet(uint256 minimumAssets, uint256 actualAssets);
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
    error NotEmergencyGuardianOrOperator();
    error NotOperator();
    error NotRequesterOrJanitor();
    error OperatorMigrationDisabled();
    error PendingRedeemsOpen();
    error ReentrantCall();
    error RequestNotOpen(bytes32 requestId);
    error ResidualBacking();
    error SameEngine();
    error TokenCallFailed();
    error TokenCallFalse();
    error ZeroAddress();
    error ZeroAmount();
    error ZeroMinimumAssets();
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
        ControlInit memory controlInit_,
        FeeInit memory feeInit_
    ) external {
        if (initialized) revert AlreadyInitialized();
        initialized = true;
        if (engine_ == address(0) || shareToken_ == address(0) || operator_ == address(0)) revert ZeroAddress();
        _requireCapability(engine_, type(IVaultEngine).interfaceId);

        address asset_ = IVaultEngine(engine_).asset();
        if (asset_ == address(0)) revert ZeroAddress();
        if (IEarnShareToken(shareToken_).totalSupply() != 0) revert InitialShareSupplyNotZero();

        engine = engine_;
        asset = asset_;
        shareToken = shareToken_;
        operator = operator_;
        emergencyGuardian = controlInit_.emergencyGuardian;
        asyncJanitor = controlInit_.asyncJanitor;
        engineMigrationMode = controlInit_.migrationMode;

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

    function feeConfig(uint64 configId) external view override returns (FeeConfig memory) {
        return _feeConfigs[configId];
    }

    function feeRemainder(uint64 configId, uint8 slot) external view returns (uint256) {
        return _feeRemainders[configId][slot];
    }

    function feesActive() external view override returns (bool) {
        return _feesActive();
    }

    function accrueFees() external override nonReentrant returns (uint256 feeAssets, uint256 feeShares) {
        FeePreview memory result = _accrueFees();
        return (result.totalFeeAssets, result.totalFeeShares);
    }

    function previewAccruedFees() public view override returns (FeePreview memory result) {
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

    function setFeeConfig(FeeConfig calldata config) external override nonReentrant returns (uint64 configId) {
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
                    targetBase = supply == 0 ? 0 : Math.mulDiv(IVaultEngine(engine).totalAssets(), shareScale, supply);
                    targetStartedAt = uint40(block.timestamp);
                }
            } else {
                targetBase = 0;
                targetStartedAt = 0;
            }
        }

        emit FeeConfigurationSet(configId, keccak256(abi.encode(config)), reactivated);
    }

    function disableFees() external override {
        if (msg.sender != feeGuardian) revert NotFeeGuardian();
        if (emergencyFeesDisabled) return;
        emergencyFeesDisabled = true;
        _waiveCurrentRemainders();
        emit FeesDisabled(msg.sender);
    }

    /// @notice Rotates the fast pause-only and bounded async cancellation seats together.
    /// @dev Governance may preserve either seat by passing its current address.
    function setEmergencyRoles(address newGuardian, address newJanitor) external override {
        if (msg.sender != operator) revert NotOperator();
        emergencyGuardian = newGuardian;
        asyncJanitor = newJanitor;
        emit EmergencyRolesChanged(newGuardian, newJanitor);
    }

    /// @notice Pauses or unpauses deposits, in-kind entry, and contributions without blocking exits.
    /// @dev The guardian may only pause. The operator may pause or unpause.
    function setDepositsPaused(bool paused) external override {
        if (msg.sender != operator) {
            if (msg.sender != emergencyGuardian) revert NotEmergencyGuardianOrOperator();
            if (!paused) revert NotOperator();
        }
        depositsPaused = paused;
        emit DepositPauseChanged(msg.sender, paused);
    }

    function claimFeeShares(address to, uint256 shares) external override nonReentrant {
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
    ///         the caller from quote drift, and the adapter rejects conversion floor loss above one
    ///         basis point even when the caller obtained a quote after the anchor was distorted.
    function deposit(uint256 assets, address receiver, uint256 minShares)
        external
        override
        nonReentrant
        returns (uint256 shares)
    {
        if (depositsPaused) revert DepositsPaused();
        if (assets == 0) revert ZeroAmount();
        if (receiver == address(0)) revert ZeroAddress();
        if (minShares == 0) revert ZeroMinimumShares();

        uint256 supplyBefore = IEarnShareToken(shareToken).totalSupply();
        _accrueFees();

        _safeTransferFrom(asset, msg.sender, address(this), assets);
        _safeApprove(asset, engine, 0);
        _safeApprove(asset, engine, assets);
        uint256 venueShares = IVaultEngine(engine).deposit(assets);
        if (venueShares == 0) revert InsufficientOutput();

        shares = _sharesToTokens(venueShares);
        if (shares == 0) revert InsufficientOutput();
        if (shares < minShares) revert MinimumSharesNotMet(minShares, shares);
        _requireAcceptableConversion(venueShares, shares);

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
    ///      the incoming principal from being treated as positive value accrual. The same one-basis-point
    ///      conversion-loss ceiling as {deposit} applies independently of `minEarnShares`.
    function depositShares(uint256 venueShares, address receiver, uint256 minEarnShares)
        external
        override
        nonReentrant
        returns (uint256 earnShares)
    {
        if (depositsPaused) revert DepositsPaused();
        if (venueShares == 0) revert ZeroAmount();
        if (receiver == address(0)) revert ZeroAddress();
        if (minEarnShares == 0) revert ZeroMinimumShares();

        uint256 supplyBefore = IEarnShareToken(shareToken).totalSupply();
        _accrueFees();

        _requireCapability(engine, type(IVaultEngineShares).interfaceId);
        uint256 engineSharesBefore = IVaultEngine(engine).totalShares();
        uint256 reportedVenueShares = IVaultEngineShares(engine).depositShares(venueShares, msg.sender);
        uint256 receivedVenueShares = IVaultEngine(engine).totalShares() - engineSharesBefore;
        if (receivedVenueShares == 0 || reportedVenueShares != receivedVenueShares) revert InsufficientOutput();

        earnShares = _sharesToTokens(receivedVenueShares);
        if (earnShares == 0) revert InsufficientOutput();
        if (earnShares < minEarnShares) revert MinimumSharesNotMet(minEarnShares, earnShares);
        _requireAcceptableConversion(receivedVenueShares, earnShares);

        IEarnShareToken(shareToken).mint(receiver, earnShares);
        _reanchorAtRest();
        if (supplyBefore == 0 && _feesActive()) _initializeFeeBaselines();
        emit VenueSharesDeposited(msg.sender, receiver, venueShares, receivedVenueShares, earnShares);
    }

    /// @notice Adds backing for current EarnToken holders without minting new EarnToken.
    /// @dev Permissionless and allowance-bound. The assets follow the normal engine deposit path,
    ///      then the conversion anchor is restated against the unchanged EarnToken
    ///      supply. With open async exits, only the active pool participates because queued claims
    ///      have already burned their EarnToken and left active NAV.
    function contribute(uint256 assets) external override nonReentrant returns (uint256 venueShares) {
        if (depositsPaused) revert DepositsPaused();
        if (assets == 0) revert ZeroAmount();

        uint256 supply = IEarnShareToken(shareToken).totalSupply();
        if (supply == 0) revert NoShareSupply();

        _accrueFees();

        _safeTransferFrom(asset, msg.sender, address(this), assets);
        _safeApprove(asset, engine, 0);
        _safeApprove(asset, engine, assets);
        venueShares = IVaultEngine(engine).deposit(assets);
        if (venueShares == 0) revert InsufficientOutput();

        anchorEngineShares = IVaultEngine(engine).totalShares();
        anchorSupply = IEarnShareToken(shareToken).totalSupply();

        _accrueFees();

        emit Contributed(msg.sender, assets, venueShares, anchorEngineShares, anchorSupply);
    }

    /// @notice Burns `shares` share tokens pulled from the caller and redeems the RATE-converted venue
    ///         shares from the engine. Proceeds go directly to `receiver`.
    function redeem(uint256 shares, address receiver, uint256 minAssets)
        external
        override
        nonReentrant
        returns (uint256 assets)
    {
        if (shares == 0) revert ZeroAmount();
        if (receiver == address(0)) revert ZeroAddress();
        if (minAssets == 0) revert ZeroMinimumAssets();
        _requireCapability(engine, type(IVaultEngineSync).interfaceId);

        _accrueFees();

        _safeTransferFrom(shareToken, msg.sender, address(this), shares);
        IEarnShareToken(shareToken).burn(shares);

        uint256 venueShares = _tokensToShares(shares);
        if (venueShares == 0) revert InsufficientOutput();

        assets = IVaultEngineSync(engine).redeem(venueShares, receiver, minAssets);
        if (assets == 0) revert InsufficientOutput();
        if (assets < minAssets) revert MinimumAssetsNotMet(minAssets, assets);

        _reanchorAtRest();
        emit Redeemed(msg.sender, receiver, shares, assets);
    }

    /// @notice Withdraws an exact `assets` amount directly to `receiver`, burning the share tokens
    ///         actually consumed (pulled from the caller). Reverts if more than `maxShares` (TIP20
    ///         units) would be burned.
    /// @dev Used when public requests are denominated in assets rather than shares. The caller keeps
    ///      any unused share tokens.
    function withdrawExact(uint256 assets, address receiver, uint256 maxShares)
        external
        override
        nonReentrant
        returns (uint256 sharesBurned)
    {
        if (assets == 0) revert ZeroAmount();
        if (receiver == address(0)) revert ZeroAddress();
        _requireCapability(engine, type(IVaultEngineExactWithdraw).interfaceId);

        _accrueFees();

        uint256 requiredVenue = IVaultEngineExactWithdraw(engine).previewWithdraw(assets);
        if (requiredVenue == 0) revert InsufficientOutput();
        uint256 requiredTokens = _sharesToTokensUp(requiredVenue);
        if (requiredTokens == 0) revert InsufficientOutput();
        if (requiredTokens > maxShares) revert ExceedsMaxShares();

        uint256 venueBurned = IVaultEngineExactWithdraw(engine).withdraw(assets, receiver);
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
    function requestRedeemAsync(uint256 shares, bytes calldata engineData, address receiver)
        external
        override
        nonReentrant
        returns (bytes32 requestId)
    {
        if (shares == 0) revert ZeroAmount();
        if (receiver == address(0)) revert ZeroAddress();
        _requireCapability(engine, type(IVaultEngineAsync).interfaceId);

        _accrueFees();

        uint256 venueShares = _tokensToShares(shares);
        if (venueShares == 0) revert InsufficientOutput();

        _safeTransferFrom(shareToken, msg.sender, address(this), shares);
        IEarnShareToken(shareToken).burn(shares);

        requestId = IVaultEngineAsync(engine).requestRedeem(venueShares, engineData);

        _recordPendingRedeem(requestId, msg.sender, receiver, shares, venueShares);

        pendingRedeemCount += 1;
        _reanchorAtRest();
        emit RedeemRequested(requestId, msg.sender, receiver, shares);
    }

    /// @notice ENGINE-ONLY. Forwards a solved queued payout to the stored receiver and closes the
    ///         pending claim. No EarnToken is burned here because it was burned at request time.
    function finalizeRedeem(bytes32 requestId, address asset_, uint256 amount) external nonReentrant {
        if (msg.sender != engine) revert NotEngine();
        PendingRedeem storage p = _pending[requestId];
        if (!p.open) revert RequestNotOpen(requestId);

        address receiver = p.receiver;
        uint256 shares = p.burnedEarnToken;
        uint64 pendingConfigId = p.feeConfigId;
        p.open = false;
        pendingRedeemCount -= 1;
        if (pendingConfigId != 0) _pendingRedeemsByFeeConfig[pendingConfigId] -= 1;

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
    function cancelRedeemAsync(bytes32 requestId) external override nonReentrant {
        PendingRedeem storage p = _pending[requestId];
        if (!p.open) revert RequestNotOpen(requestId);
        if (msg.sender != p.requester && msg.sender != asyncJanitor) revert NotRequesterOrJanitor();

        address receiver = p.receiver;
        uint256 burnedEarnToken = p.burnedEarnToken;

        _accrueFees();
        bool chargePending = !emergencyFeesDisabled && p.feeConfigId != 0 && _configHasFees(_feeConfigs[p.feeConfigId]);
        uint256 activeSupply = IEarnShareToken(shareToken).totalSupply();
        uint256 activeAssets = activeSupply == 0 || !chargePending ? 0 : IVaultEngine(engine).totalAssets();
        uint256 heldBefore = IVaultEngine(engine).totalShares();

        IVaultEngineAsync(engine).cancelRedeem(requestId);
        uint256 heldAfter = IVaultEngine(engine).totalShares();
        if (heldAfter <= heldBefore) revert InsufficientOutput();
        uint256 returnedVenueShares = heldAfter - heldBefore;

        p.open = false;
        pendingRedeemCount -= 1;
        if (p.feeConfigId != 0) _pendingRedeemsByFeeConfig[p.feeConfigId] -= 1;

        uint256 receiverShares;
        uint256 totalReentryShares;
        if (emergencyFeesDisabled || !chargePending) {
            totalReentryShares = activeSupply == 0 ? burnedEarnToken : _sharesToTokens(returnedVenueShares);
            receiverShares = totalReentryShares;
        } else {
            uint256 returnedValue = IVaultEngine(engine).valueOf(returnedVenueShares);
            if (returnedValue == 0) revert InsufficientOutput();
            (FeePreview memory pendingFees, uint256[5] memory nextRemainders) = _previewPendingFees(p, returnedValue);
            _storeRemainders(p.feeConfigId, nextRemainders);

            if (activeSupply == 0) {
                totalReentryShares = burnedEarnToken;
            } else {
                if (activeAssets == 0) revert InsufficientOutput();
                totalReentryShares = Math.mulDiv(returnedValue, activeSupply, activeAssets);
            }
            if (totalReentryShares == 0) revert InsufficientOutput();

            uint256 feeShares =
                returnedValue == 0 ? 0 : Math.mulDiv(totalReentryShares, pendingFees.totalFeeAssets, returnedValue);
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
            try IRedeemReceiver(receiver).onRedeemCancelled(requestId, shareToken, receiverShares) { } catch { }
        }

        _waiveClosedHistoricalRemainders(p.feeConfigId);
        emit RedeemCancelled(requestId, receiver, receiverShares);
    }

    /// @notice GOVERNANCE-ONLY (operator) when the deployment-fixed policy permits it. Atomically swaps
    ///         the yield engine in ONE tx: redeems ALL
    ///         venue shares from the current engine to stablecoins on the adapter, deposits them into
    ///         `newEngine` (which becomes the sole custodian of the new venue shares), repoints
    ///         `engine`, and RE-ANCHORS accounting so TIP20 totalSupply is UNCHANGED.
    /// @dev ATOMICITY: any failing leg (old-engine redeem, new-engine deposit) reverts the WHOLE tx, so
    ///      a failed migration leaves the pool entirely on the old engine. Requires no open async exits
    ///      (`pendingRedeemCount == 0`), because in-flight queued shares cannot be re-homed atomically, and
    ///      the new engine's asset must match this adapter's single asset.
    /// @dev RE-ANCHOR: TIP20 count is fixed; the venue-share count moves to `newEngine.totalShares()`
    ///      at the new rate. The (venue-shares : TIP20) anchor is restated to
    ///      `anchorEngineShares = newEngine.totalShares()`, `anchorSupply = totalSupply`, so the
    ///      restated invariant `engineShares == tokensToShares(totalSupply)` holds exactly right after
    ///      the swap. NAV per TIP20 is preserved; the share COUNT is not (that is the point).
    /// @dev TRUST: `operator` MUST be a timelock/multisig when migration is enabled because this seat can
    ///      move the entire pool's assets between venues. `UserOnly` permanently disables this path.
    /// @param newEngine Initialized replacement engine using this adapter and the same base asset.
    /// @param minNewShares Minimum replacement-engine shares accepted for a nonempty migration.
    /// @param minAssetsRetained Minimum base-asset value those replacement shares must represent.
    function migrateEngine(address newEngine, uint256 minNewShares, uint256 minAssetsRetained)
        external
        override
        nonReentrant
        returns (uint256 newShares)
    {
        if (engineMigrationMode != EngineMigrationMode.OperatorEnabled) revert OperatorMigrationDisabled();
        if (msg.sender != operator) revert NotOperator();
        if (newEngine == address(0)) revert ZeroAddress();
        address oldEngine = engine;
        if (newEngine == oldEngine) revert SameEngine();
        if (pendingRedeemCount != 0) revert PendingRedeemsOpen();
        _requireCapability(newEngine, type(IVaultEngine).interfaceId);
        if (IVaultEngine(newEngine).asset() != asset) revert EngineAssetMismatch();

        _accrueFees();

        uint256 supply = IEarnShareToken(shareToken).totalSupply();
        uint256 oldShares = IVaultEngine(oldEngine).totalShares();
        uint256 assetsMoved = 0;
        if (oldShares != 0) {
            if (minNewShares == 0) revert ZeroMinimumShares();
            if (minAssetsRetained == 0) revert ZeroMinimumAssets();
            _requireCapability(oldEngine, type(IVaultEngineSync).interfaceId);
            // Leg 1 — pull ALL value out of the old engine to the adapter. A dry/queue-only venue that
            // cannot honor a full sync redemption reverts here and the whole migration rolls back.
            assetsMoved = IVaultEngineSync(oldEngine).redeem(oldShares, address(this), 0);
            if (assetsMoved == 0) revert InsufficientOutput();

            // Leg 2 — deposit everything into the new engine, which mints and holds the new venue shares.
            _safeApprove(asset, newEngine, 0);
            _safeApprove(asset, newEngine, assetsMoved);
            newShares = IVaultEngine(newEngine).deposit(assetsMoved);
            if (newShares == 0) revert InsufficientOutput();
            if (newShares < minNewShares) revert MinimumSharesNotMet(minNewShares, newShares);

            uint256 assetsRetained = IVaultEngine(newEngine).valueOf(newShares);
            if (assetsRetained < minAssetsRetained) {
                revert MinimumAssetsNotMet(minAssetsRetained, assetsRetained);
            }
        }

        engine = newEngine;

        // Re-anchor from the ACTUAL held count so `engineShares == tokensToShares(supply)` holds exactly.
        uint256 heldNow = IVaultEngine(newEngine).totalShares();
        if (supply == 0 || heldNow == 0) {
            anchorEngineShares = 1;
            anchorSupply = 1;
        } else {
            anchorEngineShares = heldNow;
            anchorSupply = supply;
        }

        emit EngineMigrated(
            oldEngine, newEngine, oldShares, assetsMoved, newShares, supply, anchorEngineShares, anchorSupply
        );
    }

    /// @notice Estimates the base assets returned by redeeming `shares` of EarnToken through the
    ///         current engine after crystallizing any currently representable Earn-level fees.
    /// @dev Mirrors the fee-share mint and conditional re-anchor performed at the start of `redeem`,
    ///      then delegates venue-share valuation to the current engine so applicable venue exit fees
    ///      are reflected. This is a quote only; engine valuation may revert or change before execution.
    function previewRedeem(uint256 shares) public view override returns (uint256 assets) {
        if (shares == 0) return 0;
        _requireCapability(engine, type(IVaultEngineSync).interfaceId);
        (uint256 engineShareAnchor, uint256 supplyAnchor) = _previewConversionAnchor();
        uint256 venueShares = Math.mulDiv(shares, engineShareAnchor, supplyAnchor);
        if (venueShares == 0) return 0;
        return IVaultEngineSync(engine).previewRedeem(venueShares);
    }

    function previewWithdraw(uint256 assets) external view override returns (uint256 shares) {
        _requireCapability(engine, type(IVaultEngineExactWithdraw).interfaceId);
        uint256 venueShares = IVaultEngineExactWithdraw(engine).previewWithdraw(assets);
        (uint256 engineShareAnchor, uint256 supplyAnchor) = _previewConversionAnchor();
        return Math.mulDiv(venueShares, supplyAnchor, engineShareAnchor, Math.Rounding.Ceil);
    }

    function engineShares() public view override returns (uint256) {
        return IVaultEngine(engine).totalShares();
    }

    function shareSupply() public view override returns (uint256) {
        return IEarnShareToken(shareToken).totalSupply();
    }

    /// @notice Checks that live EarnToken supply converts exactly to active engine shares at the
    ///         current anchor. Pending async claims are excluded from both sides because their
    ///         EarnToken was burned when their venue shares entered the queue.
    function isSynced() external view override returns (bool) {
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
    function tokensToShares(uint256 tokens) external view override returns (uint256) {
        return _tokensToShares(tokens);
    }

    /// @notice Converts a venue-share amount to TIP20 share tokens at the current re-anchored rate.
    function sharesToTokens(uint256 shares) external view override returns (uint256) {
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

    /// @dev Rejects deposits whose floor-rounded EarnToken output represents materially less than
    ///      the venue shares that entered. The one-basis-point protocol ceiling protects callers even
    ///      when a tiny active supply makes an already-quoted conversion pathologically coarse.
    function _requireAcceptableConversion(uint256 inputShares, uint256 outputTokens) internal view {
        uint256 representedShares = _tokensToShares(outputTokens);
        uint256 maximumLoss = inputShares / CONVERSION_BPS;
        if (inputShares - representedShares > maximumLoss) {
            revert ExcessiveConversionLoss(inputShares, representedShares);
        }
    }

    /// @dev Projects the conversion anchor that `_accrueFees` and `_reanchorAtRest` would leave in
    ///      this block. If pending fee shares are too small to disturb the current total-supply
    ///      conversion, `_reanchorAtRest` keeps the stored anchor and so does this view.
    function _previewConversionAnchor() internal view returns (uint256 engineShareAnchor, uint256 supplyAnchor) {
        engineShareAnchor = anchorEngineShares;
        supplyAnchor = anchorSupply;

        uint256 feeShares = previewAccruedFees().totalFeeShares;
        if (feeShares == 0) return (engineShareAnchor, supplyAnchor);

        uint256 projectedSupply = IEarnShareToken(shareToken).totalSupply() + feeShares;
        uint256 held = IVaultEngine(engine).totalShares();
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
        uint256 held = IVaultEngine(engine).totalShares();
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

        if (result.preFeeValuePerShare > highWaterMark) highWaterMark = result.postFeeValuePerShare;

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

    function _previewPendingFees(PendingRedeem storage pending, uint256 returnedValue)
        internal
        view
        returns (FeePreview memory result, uint256[5] memory nextRemainders)
    {
        FeeConfig memory config = _feeConfigs[pending.feeConfigId];
        uint256 pendingHighWater =
            pending.burnedEarnToken == 0 ? 0 : Math.mulDiv(pending.highWaterValue, shareScale, pending.burnedEarnToken);
        uint256 pendingTarget =
            pending.burnedEarnToken == 0 ? 0 : Math.mulDiv(pending.targetValue, shareScale, pending.burnedEarnToken);
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
    ) internal {
        PendingRedeem storage pending = _pending[requestId];
        if (pending.open) revert DuplicateRequest(requestId);
        pending.receiver = receiver;
        pending.requester = requester;
        pending.burnedEarnToken = shares;
        pending.venueShares = venueShares;
        pending.requestedAt = uint40(block.timestamp);
        pending.open = true;

        if (_feesActive()) {
            pending.requestValue = IVaultEngine(engine).valueOf(venueShares);
            pending.highWaterValue = Math.mulDiv(highWaterMark, shares, shareScale);
            FeeConfig memory config = _feeConfigs[currentFeeConfigId];
            if (config.excess.enabled) {
                uint256 currentTarget =
                    FeeMath.targetAt(targetBase, config.excess.annualTargetRate, targetStartedAt, block.timestamp);
                pending.targetValue = Math.mulDiv(currentTarget, shares, shareScale);
            }
            pending.feeConfigId = currentFeeConfigId;
            _pendingRedeemsByFeeConfig[currentFeeConfigId] += 1;
        }
    }

    function _mintAndCreditFeeShares(uint64 configId, FeePreview memory fees, uint256 feeShares) internal {
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
                _creditFeeShares(configId, fees.allocations[i].account, fees.allocations[i].feeAssets, shares);
            }
        }
    }

    function _creditCalculatedAllocations(uint64 configId, FeePreview memory fees) internal {
        for (uint256 i = 0; i < fees.allocationCount; i++) {
            uint256 shares = fees.allocations[i].feeShares;
            if (shares != 0) {
                _creditFeeShares(configId, fees.allocations[i].account, fees.allocations[i].feeAssets, shares);
            }
        }
    }

    function _creditFeeShares(uint64 configId, address account, uint256 feeAssets, uint256 shares) internal {
        claimableFeeShares[account] += shares;
        totalClaimableFeeShares += shares;
        emit FeeSharesAllocated(configId, account, feeAssets, shares);
    }

    function _restoreReopenedBaselines(PendingRedeem storage pending, uint256 normalization) internal {
        if (normalization == 0) return;
        uint256 currentValue = Math.mulDiv(IVaultEngine(engine).totalAssets(), shareScale, normalization);
        uint256 pendingHighWater = Math.mulDiv(pending.highWaterValue, shareScale, pending.burnedEarnToken);
        highWaterMark = currentValue > pendingHighWater ? currentValue : pendingHighWater;

        FeeConfig memory pendingConfig = _feeConfigs[pending.feeConfigId];
        FeeConfig memory activeConfig = _feeConfigs[currentFeeConfigId];
        if (activeConfig.excess.enabled) {
            uint256 pendingTarget = Math.mulDiv(pending.targetValue, shareScale, pending.burnedEarnToken);
            uint256 grownTarget = FeeMath.targetAt(
                pendingTarget, pendingConfig.excess.annualTargetRate, pending.requestedAt, block.timestamp
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
            if (account == address(0) || account == address(this) || rate == 0) revert InvalidFeeConfiguration();
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
                    || config.excess.excessFeeRate == 0 || config.excess.excessFeeRate > excessFeeCap
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
        if (!emergencyFeesDisabled && _pendingRedeemsByFeeConfig[currentFeeConfigId] != 0) return;
        _waiveRemainders(currentFeeConfigId);
    }

    function _waiveClosedHistoricalRemainders(uint64 configId) internal {
        if (configId == 0 || configId == currentFeeConfigId || _pendingRedeemsByFeeConfig[configId] != 0) return;
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

    function _requireCapability(address candidate, bytes4 interfaceId) internal view {
        if (!ERC165Checker.supportsInterface(candidate, interfaceId)) {
            revert EngineCapabilityUnsupported(interfaceId);
        }
    }

    function _safeApprove(address token, address spender, uint256 value) internal {
        _callOptionalReturn(token, abi.encodeWithSelector(IERC20Like.approve.selector, spender, value));
    }

    function _safeTransfer(address token, address to, uint256 value) internal {
        _callOptionalReturn(token, abi.encodeWithSelector(IERC20Like.transfer.selector, to, value));
    }

    function _safeTransferFrom(address token, address from, address to, uint256 value) internal {
        _callOptionalReturn(token, abi.encodeWithSelector(IERC20Like.transferFrom.selector, from, to, value));
    }

    function _callOptionalReturn(address token, bytes memory data) internal {
        assembly ("memory-safe") {
            let ok := call(gas(), token, 0, add(data, 0x20), mload(data), 0, 0x20)
            if iszero(ok) {
                // TokenCallFailed()
                mstore(0, shl(224, 0x3f409f9a))
                revert(0, 4)
            }
            let returnSize := returndatasize()
            if returnSize {
                if or(lt(returnSize, 32), iszero(eq(mload(0), 1))) {
                    // TokenCallFalse()
                    mstore(0, shl(224, 0xfdcab105))
                    revert(0, 4)
                }
            }
        }
    }
}
