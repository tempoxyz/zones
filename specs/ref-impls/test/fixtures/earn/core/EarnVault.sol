// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { Math } from "@openzeppelin/contracts/utils/math/Math.sol";

import { EarnFeesInit, FeeConfig, FeePreview } from "../interfaces/core/EarnFeeTypes.sol";
import { EarnVaultControls, EngineMigrationMode } from "../interfaces/core/EarnVaultTypes.sol";
import { IEarnFees } from "../interfaces/core/IEarnFees.sol";
import { IEarnShare } from "../interfaces/core/IEarnShare.sol";
import { IEarnVault } from "../interfaces/core/IEarnVault.sol";
import { IEarnEngine } from "../interfaces/engines/IEarnEngine.sol";
import { IEarnEngineAsyncRedeem } from "../interfaces/engines/IEarnEngineAsyncRedeem.sol";
import { IEarnEngineExactWithdraw } from "../interfaces/engines/IEarnEngineExactWithdraw.sol";
import { IEarnEngineInKindDeposit } from "../interfaces/engines/IEarnEngineInKindDeposit.sol";
import { IEarnEngineRedeem } from "../interfaces/engines/IEarnEngineRedeem.sol";
import { IERC20Like } from "../interfaces/external/IERC20Like.sol";
import { IAsyncRedeemReceiver } from "../interfaces/periphery/IAsyncRedeemReceiver.sol";
import { ERC165Checker } from "@openzeppelin/contracts/utils/introspection/ERC165Checker.sol";

/// @notice Persistent issuance, accounting, migration, and async-claim vault for one Earn deployment.
/// @dev The EarnVault is the exclusive EarnShare issuer and the single client of its current engine.
///      Engines hold venue shares; the EarnVault holds only in-flight assets. Its immutable `EarnFees`
///      module owns all fee state and fee EarnShare custody without receiving issuer authority.
///
///      Immediate exits pay receivers directly. An async request burns EarnShare immediately and removes
///      the queued venue shares from active NAV. Finalization forwards the fixed venue payout.
///      Cancellation measures returned backing and mints a fee-aware current-value re-entry to the
///      stored receiver. Contributions and active-pool fees may continue while claims are queued.
///
///      Each deployed EarnVault is a non-upgradeable ERC-1967 proxy pointing to the factory's fixed
///      implementation. The implementation is locked against direct initialization.
contract EarnVault is IEarnVault {

    uint256 internal constant CONVERSION_BPS = 10_000;

    /// @notice The current yield engine and sole custodian of the venue shares backing this EarnVault's
    ///         EarnShare supply. MUTABLE only through operator-authorized {migrateEngine}.
    address public override engine;
    address public override asset;
    address public override earnShare;
    address public override earnFees;
    /// @notice Initialization-fixed operator seat.
    address public override operator;
    /// @notice Fast risk-reducing seat. It may pause new exposure but cannot resume it.
    address public override emergencyGuardian;
    /// @notice Narrow liveness seat that may cancel async requests only to their stored receivers.
    address public override asyncJanitor;
    address public feeAdministrator;
    address public feeGuardian;
    /// @notice Deployment-fixed whole-pool migration policy. There is intentionally no setter.
    EngineMigrationMode public override engineMigrationMode;
    uint256 internal earnShareScale;

    /// @notice (engine-shares : EarnShare) exchange anchor. `anchorEngineShares` engine shares are
    ///         represented by `anchorEarnShares` EarnShare. Empty pools normalize the engine and
    ///         EarnShare whole-unit scales;
    ///         live pools are explicitly re-anchored by {migrateEngine}, permissionless {contribute}
    ///         funding, fee mints, and ordinary immediate actions that change backing or supply.
    uint256 public anchorEngineShares;
    uint256 public anchorEarnShares;

    uint256 public override openRedeemRequestCount;
    /// @notice Whether deposits, in-kind entry, and contributions are paused. Exit paths stay open.
    bool public override depositsPaused;

    mapping(address inputToken => address swapAdapter) public override depositSwapOverride;
    mapping(address outputToken => address swapAdapter) public override redeemSwapOverride;

    struct PendingRedeem {
        address receiver;
        address requester;
        uint256 burnedEarnShares;
        uint256 venueShares;
        bool open;
    }

    mapping(bytes32 requestId => PendingRedeem) private _pending;

    uint256 private locked;
    bool private initialized;

    event Contributed(
        address indexed caller,
        uint256 assets,
        uint256 venueShares,
        uint256 anchorEngineShares,
        uint256 anchorEarnShares
    );
    event RedeemRequested(
        bytes32 indexed requestId,
        address indexed requester,
        address indexed receiver,
        uint256 earnShares
    );
    event RedeemFinalized(
        bytes32 indexed requestId,
        address indexed receiver,
        uint256 earnShares,
        address asset,
        uint256 assets
    );
    event RedeemCancelled(bytes32 indexed requestId, address indexed receiver, uint256 earnShares);
    event DepositSwapOverrideSet(address indexed inputToken, address indexed swapAdapter);
    event RedeemSwapOverrideSet(address indexed outputToken, address indexed swapAdapter);
    /// @notice Emitted when {migrateEngine} atomically re-homes custody and re-anchors accounting.
    ///         `totalEarnShares` is unchanged across the swap; `oldEngineShares`/`newEngineShares` are the venue-share
    ///         counts before/after (they differ at the new engine's rate).
    event EngineMigrated(
        address indexed oldEngine,
        address indexed newEngine,
        uint256 oldEngineShares,
        uint256 assetsMoved,
        uint256 newEngineShares,
        uint256 totalEarnShares,
        uint256 anchorEngineShares,
        uint256 anchorEarnShares
    );

    error DepositsPaused();
    error DuplicateRequest(bytes32 requestId);
    error EngineAssetMismatch();
    error EngineCapabilityUnsupported(bytes4 interfaceId);
    error ExcessiveConversionLoss(uint256 inputEngineShares, uint256 representedEngineShares);
    error ExceedsMaxEarnShares();
    error InitialEarnShareSupplyNotZero();
    error InvalidEngineShareScale();
    error InvalidEarnDecimals();
    error InvalidSwapOverride();
    error InsufficientOutput();
    error MinimumAssetsNotMet(uint256 minimumAssets, uint256 actualAssets);
    error MinimumEarnSharesNotMet(uint256 minimumEarnShares, uint256 actualEarnShares);
    error MinimumEngineSharesNotMet(uint256 minimumEngineShares, uint256 actualEngineShares);
    error AlreadyInitialized();
    error NoEarnShares();
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
    error ZeroMinimumEarnShares();
    error ZeroMinimumEngineShares();

    /// @dev Locks the standalone implementation. Each EarnVault is an ERC-1967 proxy with no upgrade
    ///      entrypoint, initialized atomically by {EarnFactory} against this fixed implementation.
    constructor() {
        initialized = true;
    }

    function initialize(
        address engine_,
        address earnShare_,
        address earnFees_,
        address operator_,
        EarnVaultControls memory controlInit_,
        EarnFeesInit memory feeInit_
    )
        external
    {
        if (initialized) revert AlreadyInitialized();
        initialized = true;
        if (
            engine_ == address(0) || earnShare_ == address(0) || earnFees_ == address(0)
                || operator_ == address(0)
        ) {
            revert ZeroAddress();
        }
        _requireCapability(engine_, type(IEarnEngine).interfaceId);

        address asset_ = IEarnEngine(engine_).asset();
        if (asset_ == address(0)) revert ZeroAddress();
        if (IEarnShare(earnShare_).totalSupply() != 0) revert InitialEarnShareSupplyNotZero();
        if (
            IEarnFees(earnFees_).earnVault() != address(this)
                || IEarnFees(earnFees_).earnShare() != earnShare_
        ) {
            revert ZeroAddress();
        }

        engine = engine_;
        asset = asset_;
        earnShare = earnShare_;
        earnFees = earnFees_;
        operator = operator_;
        emergencyGuardian = controlInit_.emergencyGuardian;
        asyncJanitor = controlInit_.asyncJanitor;
        engineMigrationMode = controlInit_.migrationMode;

        if (
            (feeInit_.fixedFeeCap != 0 || feeInit_.excessFeeCap != 0)
                && (feeInit_.administrator == address(0) || feeInit_.guardian == address(0))
        ) {
            revert ZeroAddress();
        }

        feeAdministrator = feeInit_.administrator;
        feeGuardian = feeInit_.guardian;
        uint8 earnDecimals = IEarnShare(earnShare_).decimals();
        if (earnDecimals > 77) revert InvalidEarnDecimals();
        earnShareScale = 10 ** earnDecimals;
        _resetEmptyAnchor(engine_);
        locked = 1;
    }

    modifier nonReentrant() {
        if (locked != 1) revert ReentrantCall();
        locked = 2;
        _;
        locked = 1;
    }

    function accrueFees()
        external
        override
        nonReentrant
        returns (uint256 feeAssets, uint256 feeEarnShares)
    {
        FeePreview memory result = _accrueFees();
        return (result.totalFeeAssets, result.totalFeeEarnShares);
    }

    function setFeeConfig(FeeConfig calldata config)
        external
        override
        nonReentrant
        returns (uint64 configId)
    {
        if (msg.sender != feeAdministrator) revert NotFeeAdministrator();
        _accrueFees();
        configId = IEarnFees(earnFees).setFeeConfig(config);
    }

    function disableFees() external override {
        if (msg.sender != feeGuardian) revert NotFeeGuardian();
        IEarnFees(earnFees).disableFees(msg.sender);
    }

    /// @notice Rotates the fast pause-only and bounded async cancellation seats together.
    /// @dev The operator may preserve either seat by passing its current address.
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

    function setDepositSwapOverride(address inputToken, address swapAdapter) external override {
        if (msg.sender != operator) revert NotOperator();
        if (inputToken == address(0) || inputToken == asset || inputToken == earnShare) {
            revert InvalidSwapOverride();
        }
        if (swapAdapter != address(0) && swapAdapter.code.length == 0) {
            revert InvalidSwapOverride();
        }
        depositSwapOverride[inputToken] = swapAdapter;
        emit DepositSwapOverrideSet(inputToken, swapAdapter);
    }

    function setRedeemSwapOverride(address outputToken, address swapAdapter) external override {
        if (msg.sender != operator) revert NotOperator();
        if (outputToken == address(0) || outputToken == asset || outputToken == earnShare) {
            revert InvalidSwapOverride();
        }
        if (swapAdapter != address(0) && swapAdapter.code.length == 0) {
            revert InvalidSwapOverride();
        }
        redeemSwapOverride[outputToken] = swapAdapter;
        emit RedeemSwapOverrideSet(outputToken, swapAdapter);
    }

    /// @notice Deposits `assets` of the engine asset pulled from the caller into the engine (which
    ///         holds the resulting venue shares) and mints the rate-converted EarnShare amount to
    ///         `receiver`. The measured venue-share delta is converted at the active anchor, whose
    ///         empty-pool rate normalizes engine and EarnShare precision. `minEarnShares` protects
    ///         the caller from quote drift, and the EarnVault rejects conversion floor loss above one
    ///         basis point even when the caller obtained a quote after the anchor was distorted.
    function deposit(
        uint256 assets,
        address receiver,
        uint256 minEarnShares
    )
        external
        override
        nonReentrant
        returns (uint256 earnShares)
    {
        if (depositsPaused) revert DepositsPaused();
        if (assets == 0) revert ZeroAmount();
        if (receiver == address(0)) revert ZeroAddress();
        if (minEarnShares == 0) revert ZeroMinimumEarnShares();

        uint256 supplyBefore = IEarnShare(earnShare).totalSupply();
        _accrueFees();

        _safeTransferFrom(asset, msg.sender, address(this), assets);
        _safeApprove(asset, engine, 0);
        _safeApprove(asset, engine, assets);
        uint256 venueShares = IEarnEngine(engine).deposit(assets);
        if (venueShares == 0) revert InsufficientOutput();

        earnShares = _convertEngineSharesToEarnShares(venueShares);
        if (earnShares == 0) revert InsufficientOutput();
        if (earnShares < minEarnShares) revert MinimumEarnSharesNotMet(minEarnShares, earnShares);
        _requireAcceptableConversion(venueShares, earnShares);

        IEarnShare(earnShare).mint(receiver, earnShares);
        _reanchorAtRest();
        if (supplyBefore == 0) IEarnFees(earnFees).initializeBaselines();
        emit Deposited(msg.sender, receiver, assets, earnShares);
    }

    /// @notice Deposits shares of the current engine's venue directly and mints rate-converted
    ///         EarnShare to `receiver`. The caller approves the engine—not this EarnVault—to pull the
    ///         venue shares. Only engines implementing {IEarnEngineInKindDeposit} support this path.
    /// @dev This is a principal-preserving entry path for existing venue shareholders. Fees are
    ///      crystallized before the new backing enters, and the proportional EarnShare mint prevents
    ///      the incoming principal from being treated as positive value accrual. The same one-basis-point
    ///      conversion-loss ceiling as {deposit} applies independently of `minEarnShares`.
    function depositVenueShares(
        uint256 venueShares,
        address receiver,
        uint256 minEarnShares
    )
        external
        override
        nonReentrant
        returns (uint256 earnShares)
    {
        if (depositsPaused) revert DepositsPaused();
        if (venueShares == 0) revert ZeroAmount();
        if (receiver == address(0)) revert ZeroAddress();
        if (minEarnShares == 0) revert ZeroMinimumEarnShares();

        uint256 supplyBefore = IEarnShare(earnShare).totalSupply();
        _accrueFees();

        _requireCapability(engine, type(IEarnEngineInKindDeposit).interfaceId);
        uint256 engineSharesBefore = IEarnEngine(engine).totalShares();
        uint256 reportedVenueShares =
            IEarnEngineInKindDeposit(engine).depositInKind(venueShares, msg.sender);
        uint256 receivedVenueShares = IEarnEngine(engine).totalShares() - engineSharesBefore;
        if (receivedVenueShares == 0 || reportedVenueShares != receivedVenueShares) {
            revert InsufficientOutput();
        }

        earnShares = _convertEngineSharesToEarnShares(receivedVenueShares);
        if (earnShares == 0) revert InsufficientOutput();
        if (earnShares < minEarnShares) revert MinimumEarnSharesNotMet(minEarnShares, earnShares);
        _requireAcceptableConversion(receivedVenueShares, earnShares);

        IEarnShare(earnShare).mint(receiver, earnShares);
        _reanchorAtRest();
        if (supplyBefore == 0) IEarnFees(earnFees).initializeBaselines();
        emit VenueSharesDeposited(
            msg.sender, receiver, venueShares, receivedVenueShares, earnShares
        );
    }

    /// @notice Adds backing for current EarnShare holders without minting new EarnShare.
    /// @dev Permissionless and allowance-bound. The assets follow the normal engine deposit path,
    ///      then the conversion anchor is restated against the unchanged EarnShare
    ///      supply. With open async exits, only the active pool participates because queued claims
    ///      have already burned their EarnShare and left active NAV.
    function contribute(uint256 assets)
        external
        override
        nonReentrant
        returns (uint256 venueShares)
    {
        if (depositsPaused) revert DepositsPaused();
        if (assets == 0) revert ZeroAmount();

        uint256 supply = IEarnShare(earnShare).totalSupply();
        if (supply == 0) revert NoEarnShares();

        _accrueFees();

        _safeTransferFrom(asset, msg.sender, address(this), assets);
        _safeApprove(asset, engine, 0);
        _safeApprove(asset, engine, assets);
        venueShares = IEarnEngine(engine).deposit(assets);
        if (venueShares == 0) revert InsufficientOutput();

        anchorEngineShares = IEarnEngine(engine).totalShares();
        anchorEarnShares = IEarnShare(earnShare).totalSupply();

        _accrueFees();

        emit Contributed(msg.sender, assets, venueShares, anchorEngineShares, anchorEarnShares);
    }

    /// @notice Burns `earnShares` of EarnShare pulled from the caller and redeems the rate-converted
    ///         engine shares. Proceeds go directly to `receiver`.
    function redeem(
        uint256 earnShares,
        address receiver,
        uint256 minAssets
    )
        external
        override
        nonReentrant
        returns (uint256 assets)
    {
        if (earnShares == 0) revert ZeroAmount();
        if (receiver == address(0)) revert ZeroAddress();
        if (minAssets == 0) revert ZeroMinimumAssets();
        _requireCapability(engine, type(IEarnEngineRedeem).interfaceId);

        _accrueFees();

        _safeTransferFrom(earnShare, msg.sender, address(this), earnShares);
        IEarnShare(earnShare).burn(earnShares);

        uint256 venueShares = _convertToEngineShares(earnShares);
        if (venueShares == 0) revert InsufficientOutput();

        assets = IEarnEngineRedeem(engine).redeem(venueShares, receiver, minAssets);
        if (assets == 0) revert InsufficientOutput();
        if (assets < minAssets) revert MinimumAssetsNotMet(minAssets, assets);

        _reanchorAtRest();
        emit Redeemed(msg.sender, receiver, earnShares, assets);
    }

    /// @notice Withdraws an exact `assets` amount directly to `receiver`, burning the EarnShare amount
    ///         actually consumed (pulled from the caller). Reverts if more than `maxEarnShares`
    ///         would be burned.
    /// @dev Used when public requests are denominated in assets. The caller keeps any unused EarnShare.
    function withdrawExact(
        uint256 assets,
        address receiver,
        uint256 maxEarnShares
    )
        external
        override
        nonReentrant
        returns (uint256 earnSharesBurned)
    {
        if (assets == 0) revert ZeroAmount();
        if (receiver == address(0)) revert ZeroAddress();
        _requireCapability(engine, type(IEarnEngineExactWithdraw).interfaceId);

        _accrueFees();

        uint256 requiredVenue = IEarnEngineExactWithdraw(engine).previewWithdraw(assets);
        if (requiredVenue == 0) revert InsufficientOutput();
        uint256 requiredEarnShares = _convertEngineSharesToEarnSharesUp(requiredVenue);
        if (requiredEarnShares == 0) revert InsufficientOutput();
        if (requiredEarnShares > maxEarnShares) revert ExceedsMaxEarnShares();

        uint256 venueBurned = IEarnEngineExactWithdraw(engine).withdraw(assets, receiver);
        if (venueBurned == 0) revert InsufficientOutput();
        earnSharesBurned = _convertEngineSharesToEarnSharesUp(venueBurned);
        if (earnSharesBurned == 0) revert InsufficientOutput();
        if (earnSharesBurned > maxEarnShares) revert ExceedsMaxEarnShares();
        if (earnSharesBurned == IEarnShare(earnShare).totalSupply() && engineShares() != 0) {
            revert ResidualBacking();
        }

        _safeTransferFrom(earnShare, msg.sender, address(this), earnSharesBurned);
        IEarnShare(earnShare).burn(earnSharesBurned);

        _reanchorAtRest();
        emit WithdrewExact(msg.sender, receiver, assets, earnSharesBurned);
    }

    /// @notice Files an async redemption, burns `earnShares` immediately, and records a separate
    ///         entitlement while `EarnFees` snapshots any applicable queued-growth fee state.
    /// @dev The queued backing and burned EarnShare leave active accounting atomically. If the engine
    ///      request fails, the entire transaction—including the burn—reverts.
    function requestRedeem(
        uint256 earnShares,
        bytes calldata engineData,
        address receiver
    )
        external
        override
        nonReentrant
        returns (bytes32 requestId)
    {
        if (earnShares == 0) revert ZeroAmount();
        if (receiver == address(0)) revert ZeroAddress();
        _requireCapability(engine, type(IEarnEngineAsyncRedeem).interfaceId);

        _accrueFees();

        uint256 venueShares = _convertToEngineShares(earnShares);
        if (venueShares == 0) revert InsufficientOutput();

        _safeTransferFrom(earnShare, msg.sender, address(this), earnShares);
        IEarnShare(earnShare).burn(earnShares);

        requestId = IEarnEngineAsyncRedeem(engine).requestRedeem(venueShares, engineData);

        _recordPendingRedeem(requestId, msg.sender, receiver, earnShares, venueShares);

        openRedeemRequestCount += 1;
        _reanchorAtRest();
        emit RedeemRequested(requestId, msg.sender, receiver, earnShares);
    }

    /// @notice ENGINE-ONLY. Forwards a solved queued payout to the stored receiver and closes the
    ///         pending claim. No EarnShare is burned here because it was burned at request time.
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
        uint256 earnShares = p.burnedEarnShares;
        delete _pending[requestId];
        openRedeemRequestCount -= 1;
        IEarnFees(earnFees).closeRedeemRequest(requestId);

        // The engine already sent `amount` of `asset_` here; forward it to the stored receiver.
        if (amount != 0) _safeTransfer(asset_, receiver, amount);

        if (receiver.code.length != 0) {
            try IAsyncRedeemReceiver(receiver).onRedeemFinalized(requestId, asset_, amount) { }
                catch { }
        }

        emit RedeemFinalized(requestId, receiver, earnShares, asset_, amount);
    }

    /// @notice Cancels a still-open async request. The returned venue shares re-enter at the active
    ///         pool's current price, snapshotted fee terms apply to queued growth, and the resulting
    ///         EarnShare is minted to the same stored receiver used by finalization.
    /// @param minReceiverEarnShares Nonzero minimum EarnShare that the stored receiver must receive.
    function cancelRedeem(
        bytes32 requestId,
        uint256 minReceiverEarnShares
    )
        external
        override
        nonReentrant
    {
        if (minReceiverEarnShares == 0) revert ZeroMinimumEarnShares();
        PendingRedeem storage p = _pending[requestId];
        if (!p.open) revert RequestNotOpen(requestId);
        if (msg.sender != p.requester && msg.sender != asyncJanitor) {
            revert NotRequesterOrJanitor();
        }

        address receiver = p.receiver;
        uint256 burnedEarnShares = p.burnedEarnShares;

        _accrueFees();
        bool chargePending = IEarnFees(earnFees).shouldChargeRedeem(requestId);
        uint256 activeSupply = IEarnShare(earnShare).totalSupply();
        uint256 activeAssets =
            activeSupply == 0 || !chargePending ? 0 : IEarnEngine(engine).totalAssets();
        uint256 heldBefore = IEarnEngine(engine).totalShares();

        IEarnEngineAsyncRedeem(engine).cancelRedeem(requestId);
        uint256 heldAfter = IEarnEngine(engine).totalShares();
        if (heldAfter <= heldBefore) revert InsufficientOutput();
        uint256 returnedVenueShares = heldAfter - heldBefore;

        delete _pending[requestId];
        openRedeemRequestCount -= 1;

        uint256 totalReentryEarnShares;
        uint256 returnedValue;
        if (chargePending) {
            returnedValue = IEarnEngine(engine).valueOf(returnedVenueShares);
            if (returnedValue == 0) revert InsufficientOutput();
        }
        if (activeSupply == 0) {
            totalReentryEarnShares = burnedEarnShares;
        } else if (chargePending) {
            if (activeAssets == 0) revert InsufficientOutput();
            totalReentryEarnShares = Math.mulDiv(returnedValue, activeSupply, activeAssets);
        } else {
            totalReentryEarnShares = _convertEngineSharesToEarnShares(returnedVenueShares);
        }
        if (totalReentryEarnShares == 0) revert InsufficientOutput();

        uint256 feeEarnShares = IEarnFees(earnFees)
            .settleCancelledRedeem(
                requestId, returnedValue, activeSupply, activeAssets, totalReentryEarnShares
            );
        if (feeEarnShares != 0) IEarnShare(earnShare).mint(earnFees, feeEarnShares);
        uint256 receiverEarnShares = totalReentryEarnShares - feeEarnShares;

        if (activeSupply != 0) {
            _requireAcceptableConversion(returnedVenueShares, totalReentryEarnShares);
        }
        if (receiverEarnShares == 0) revert InsufficientOutput();
        if (receiverEarnShares < minReceiverEarnShares) {
            revert MinimumEarnSharesNotMet(minReceiverEarnShares, receiverEarnShares);
        }
        IEarnShare(earnShare).mint(receiver, receiverEarnShares);
        _reanchorAtRest();

        if (receiver.code.length != 0) {
            try IAsyncRedeemReceiver(receiver)
                .onRedeemCancelled(requestId, earnShare, receiverEarnShares) { }
                catch { }
        }

        emit RedeemCancelled(requestId, receiver, receiverEarnShares);
    }

    /// @notice OPERATOR-ONLY when the deployment-fixed policy permits it. Atomically swaps
    ///         the yield engine in ONE tx: redeems ALL
    ///         venue shares from the current engine to stablecoins on the EarnVault, deposits them into
    ///         `newEngine` (which becomes the sole custodian of the new venue shares), repoints
    ///         `engine`, and RE-ANCHORS accounting so TIP20 totalSupply is UNCHANGED.
    /// @dev ATOMICITY: any failing leg (old-engine redeem, new-engine deposit) reverts the WHOLE tx, so
    ///      a failed migration leaves the pool entirely on the old engine. Requires no open async exits
    ///      (`openRedeemRequestCount == 0`), because in-flight queued shares cannot be re-homed atomically, and
    ///      the new engine's asset must match this EarnVault's single asset.
    /// @dev RE-ANCHOR: TIP20 count is fixed; the venue-share count moves to `newEngine.totalShares()`
    ///      at the new rate. The (venue-shares : TIP20) anchor is restated to
    ///      `anchorEngineShares = newEngine.totalShares()`, `anchorEarnShares = totalSupply`, so the
    ///      restated invariant `engineShares == convertToEngineShares(totalSupply)` holds exactly right after
    ///      the swap. NAV per TIP20 is preserved; the share COUNT is not (that is the point).
    /// @dev TRUST: `operator` MUST be a timelock/multisig when migration is enabled because this seat can
    ///      move the entire pool's assets between venues. `UserOnly` permanently disables this path.
    /// @param newEngine Initialized replacement engine using this EarnVault and the same base asset.
    /// @param minNewEngineShares Minimum replacement engine shares accepted for a nonempty migration.
    /// @param minAssetsRetained Minimum base-asset value those replacement shares must represent.
    function migrateEngine(
        address newEngine,
        uint256 minNewEngineShares,
        uint256 minAssetsRetained
    )
        external
        override
        nonReentrant
        returns (uint256 newEngineShares)
    {
        if (engineMigrationMode != EngineMigrationMode.OperatorEnabled) {
            revert OperatorMigrationDisabled();
        }
        if (msg.sender != operator) revert NotOperator();
        if (newEngine == address(0)) revert ZeroAddress();
        address oldEngine = engine;
        if (newEngine == oldEngine) revert SameEngine();
        if (openRedeemRequestCount != 0) revert PendingRedeemsOpen();
        _requireCapability(newEngine, type(IEarnEngine).interfaceId);
        if (IEarnEngine(newEngine).asset() != asset) revert EngineAssetMismatch();

        _accrueFees();

        uint256 supply = IEarnShare(earnShare).totalSupply();
        uint256 oldEngineShares = IEarnEngine(oldEngine).totalShares();
        uint256 assetsMoved = 0;
        if (oldEngineShares != 0) {
            if (minNewEngineShares == 0) revert ZeroMinimumEngineShares();
            if (minAssetsRetained == 0) revert ZeroMinimumAssets();
            _requireCapability(oldEngine, type(IEarnEngineRedeem).interfaceId);
            // Leg 1 — pull ALL value out of the old engine to the EarnVault. A dry/queue-only venue that
            // cannot honor a full immediate redemption reverts here and the whole migration rolls back.
            assetsMoved = IEarnEngineRedeem(oldEngine).redeem(oldEngineShares, address(this), 0);
            if (assetsMoved == 0) revert InsufficientOutput();

            // Leg 2 — deposit everything into the new engine, which mints and holds the new venue shares.
            _safeApprove(asset, newEngine, 0);
            _safeApprove(asset, newEngine, assetsMoved);
            newEngineShares = IEarnEngine(newEngine).deposit(assetsMoved);
            if (newEngineShares == 0) revert InsufficientOutput();
            if (newEngineShares < minNewEngineShares) {
                revert MinimumEngineSharesNotMet(minNewEngineShares, newEngineShares);
            }

            uint256 assetsRetained = IEarnEngine(newEngine).valueOf(newEngineShares);
            if (assetsRetained < minAssetsRetained) {
                revert MinimumAssetsNotMet(minAssetsRetained, assetsRetained);
            }
        }

        engine = newEngine;

        // Re-anchor from the ACTUAL held count so `engineShares == convertToEngineShares(supply)` holds exactly.
        uint256 heldNow = IEarnEngine(newEngine).totalShares();
        if (supply == 0 || heldNow == 0) {
            _resetEmptyAnchor(newEngine);
        } else {
            anchorEngineShares = heldNow;
            anchorEarnShares = supply;
        }

        emit EngineMigrated(
            oldEngine,
            newEngine,
            oldEngineShares,
            assetsMoved,
            newEngineShares,
            supply,
            anchorEngineShares,
            anchorEarnShares
        );
    }

    /// @notice Estimates the base assets returned by redeeming `earnShares` of EarnShare through the
    ///         current engine after crystallizing any currently representable Earn-level fees.
    /// @dev Mirrors the fee-share mint and conditional re-anchor performed at the start of `redeem`,
    ///      then delegates venue-share valuation to the current engine so applicable venue exit fees
    ///      are reflected. This is a quote only; engine valuation may revert or change before execution.
    function previewRedeem(uint256 earnShares) public view override returns (uint256 assets) {
        if (earnShares == 0) return 0;
        _requireCapability(engine, type(IEarnEngineRedeem).interfaceId);
        (uint256 engineShareAnchor, uint256 supplyAnchor) = _previewConversionAnchor();
        uint256 venueShares = Math.mulDiv(earnShares, engineShareAnchor, supplyAnchor);
        if (venueShares == 0) return 0;
        return IEarnEngineRedeem(engine).previewRedeem(venueShares);
    }

    function previewWithdraw(uint256 assets) external view override returns (uint256 earnShares) {
        _requireCapability(engine, type(IEarnEngineExactWithdraw).interfaceId);
        uint256 venueShares = IEarnEngineExactWithdraw(engine).previewWithdraw(assets);
        (uint256 engineShareAnchor, uint256 supplyAnchor) = _previewConversionAnchor();
        return Math.mulDiv(venueShares, supplyAnchor, engineShareAnchor, Math.Rounding.Ceil);
    }

    function engineShares() public view override returns (uint256) {
        return IEarnEngine(engine).totalShares();
    }

    function totalEarnShares() public view override returns (uint256) {
        return IEarnShare(earnShare).totalSupply();
    }

    /// @notice Checks that live EarnShare supply converts exactly to active engine shares at the
    ///         current anchor. Pending async claims are excluded from both sides because their
    ///         EarnShare was burned when their venue shares entered the queue.
    function isAccountingAligned() external view override returns (bool) {
        uint256 supply = totalEarnShares();
        uint256 held = engineShares();
        if (supply == 0 || held == 0) return supply == 0 && held == 0;
        return _convertToEngineShares(supply) == held;
    }

    /// @notice Reads the stored fields of the pending async request `requestId`.
    function pendingRedeem(bytes32 requestId) external view returns (PendingRedeem memory) {
        return _pending[requestId];
    }

    /// @notice Converts an EarnShare amount to engine shares at the current re-anchored rate.
    function convertToEngineShares(uint256 earnShares)
        external
        view
        override
        returns (uint256 engineShares_)
    {
        return _convertToEngineShares(earnShares);
    }

    /// @notice Converts an engine-share amount to EarnShare at the current re-anchored rate.
    function convertEngineSharesToEarnShares(uint256 engineShares_)
        external
        view
        override
        returns (uint256 earnShares)
    {
        return _convertEngineSharesToEarnShares(engineShares_);
    }

    function _convertToEngineShares(uint256 earnShares) internal view returns (uint256) {
        return Math.mulDiv(earnShares, anchorEngineShares, anchorEarnShares);
    }

    function _convertEngineSharesToEarnShares(uint256 engineShares_)
        internal
        view
        returns (uint256)
    {
        return Math.mulDiv(engineShares_, anchorEarnShares, anchorEngineShares);
    }

    function _convertEngineSharesToEarnSharesUp(uint256 engineShares_)
        internal
        view
        returns (uint256)
    {
        return Math.mulDiv(engineShares_, anchorEarnShares, anchorEngineShares, Math.Rounding.Ceil);
    }

    /// @dev Rejects entries whose floor-rounded EarnShare output represents materially less than
    ///      the venue shares that entered. The one-basis-point protocol ceiling protects callers even
    ///      when a tiny active supply makes an already-quoted conversion pathologically coarse.
    function _requireAcceptableConversion(uint256 inputShares, uint256 outputTokens) internal view {
        uint256 representedShares = _convertToEngineShares(outputTokens);
        uint256 maximumLoss = inputShares / CONVERSION_BPS;
        if (inputShares - representedShares > maximumLoss) {
            revert ExcessiveConversionLoss(inputShares, representedShares);
        }
    }

    /// @dev Projects the exact conversion anchor that `_accrueFees` and `_reanchorAtRest` would leave
    ///      in this block. A fee-share mint always changes supply and therefore always restates the
    ///      anchor, even when converting the new supply at the old anchor floors to the held count.
    function _previewConversionAnchor()
        internal
        view
        returns (uint256 engineShareAnchor, uint256 supplyAnchor)
    {
        engineShareAnchor = anchorEngineShares;
        supplyAnchor = anchorEarnShares;

        uint256 feeEarnShares = IEarnFees(earnFees).previewAccruedFees().totalFeeEarnShares;
        if (feeEarnShares == 0) return (engineShareAnchor, supplyAnchor);

        uint256 projectedSupply = IEarnShare(earnShare).totalSupply() + feeEarnShares;
        uint256 held = IEarnEngine(engine).totalShares();
        if (projectedSupply == supplyAnchor && held == engineShareAnchor) {
            return (engineShareAnchor, supplyAnchor);
        }
        return (held, projectedSupply);
    }

    /// @dev Restates the anchor from exact active backing and supply. Exact pair equality is required:
    ///      a floor conversion can map a changed supply to the same held count while still preserving
    ///      a stale pre-fee rate that lets holders consume backing allocated to fee EarnShare.
    function _reanchorAtRest() internal {
        uint256 supply = IEarnShare(earnShare).totalSupply();
        uint256 held = IEarnEngine(engine).totalShares();
        if (supply == 0 || held == 0) {
            _resetEmptyAnchor(engine);
            return;
        }
        if (anchorEarnShares == supply && anchorEngineShares == held) return;

        anchorEngineShares = held;
        anchorEarnShares = supply;
    }

    /// @dev Empty pools use whole-share units instead of a raw-unit 1:1 rate. This keeps the first
    ///      EarnShare mint normalized even when the engine and EarnShare use different decimals.
    function _resetEmptyAnchor(address engine_) internal {
        anchorEngineShares = IEarnEngine(engine_).shareScale();
        if (anchorEngineShares == 0) revert InvalidEngineShareScale();
        anchorEarnShares = earnShareScale;
    }

    function _accrueFees() internal returns (FeePreview memory result) {
        result = IEarnFees(earnFees).accrueFees();
        if (result.totalFeeEarnShares != 0) {
            IEarnShare(earnShare).mint(earnFees, result.totalFeeEarnShares);
            _reanchorAtRest();
        }
    }

    function _recordPendingRedeem(
        bytes32 requestId,
        address requester,
        address receiver,
        uint256 earnShares,
        uint256 venueShares
    )
        internal
    {
        PendingRedeem storage pending = _pending[requestId];
        if (pending.open) revert DuplicateRequest(requestId);
        pending.receiver = receiver;
        pending.requester = requester;
        pending.burnedEarnShares = earnShares;
        pending.venueShares = venueShares;
        pending.open = true;
        uint256 requestValue = IEarnEngine(engine).valueOf(venueShares);
        IEarnFees(earnFees).recordRedeemRequest(requestId, earnShares, requestValue);
    }

    function _requireCapability(address candidate, bytes4 interfaceId) internal view {
        if (!ERC165Checker.supportsInterface(candidate, interfaceId)) {
            revert EngineCapabilityUnsupported(interfaceId);
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
