// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { IERC20Like } from "../interfaces/IERC20Like.sol";
import { IVaultEngine } from "../interfaces/IVaultEngine.sol";
import { IVaultEngineExactWithdraw } from "../interfaces/IVaultEngineExactWithdraw.sol";
import { IVaultEngineShares } from "../interfaces/IVaultEngineShares.sol";
import { IVaultEngineSync } from "../interfaces/IVaultEngineSync.sol";
import { Ownable } from "@openzeppelin/contracts/access/Ownable.sol";
import { Ownable2Step } from "@openzeppelin/contracts/access/Ownable2Step.sol";
import { IERC165 } from "@openzeppelin/contracts/utils/introspection/IERC165.sol";

/// @dev Minimal ERC-4626 surface of the wrapped venue vault (Morpho vault, {Simple4626Vault} demo,
///      or any conformant 4626). Only the methods the engine drives are declared.
interface IERC4626Vault {
    function asset() external view returns (address);
    function decimals() external view returns (uint8);
    function name() external view returns (string memory);
    function symbol() external view returns (string memory);
    function balanceOf(address account) external view returns (uint256);
    function previewWithdraw(uint256 assets) external view returns (uint256 shares);
    function previewRedeem(uint256 shares) external view returns (uint256 assets);
    function deposit(uint256 assets, address receiver) external returns (uint256 shares);
    function redeem(uint256 shares, address receiver, address owner) external returns (uint256 assets);
    function withdraw(uint256 assets, address receiver, address owner) external returns (uint256 shares);
}

/// @title ERC4626Engine
/// @notice The generic SYNC engine behind the `IVaultEngine` seam. It wraps ANY conformant ERC-4626
///         vault (a Morpho vault, the {Simple4626Vault} demo, a Sentora venue, etc.) and, under the
///         UNIFIED-CUSTODY model, is itself the SOLE holder of the wrapped 4626 shares attributed to
///         its single client (the `VaultAdapter`). The adapter never holds venue shares; it only ever
///         talks to an engine, and this engine is the sync/4626 case of that seam (the async/queue
///         case is {VedaEngine}).
///
/// @dev SINGLE-CLIENT by construction: every mutating `IVaultEngine` call is `onlyCore`, so the
///      engine's entire 4626 balance backs exactly one adapter and `totalShares()` is the whole
///      backing. `deposit` returns the MEASURED share delta (the adapter mints that at its rate);
///      `redeem`/`withdraw` are receiver-directed (the 4626 pays the adapter's `receiver` straight from
///      the venue, so proceeds never rest here). `asset` is derived from the wrapped vault;
///      `name`/`symbol` use immutable constructor overrides when supplied and otherwise derive from
///      the vault.
contract ERC4626Engine is IVaultEngine, IVaultEngineSync, IVaultEngineExactWithdraw, IVaultEngineShares, Ownable2Step {
    /// @notice The wrapped ERC-4626 venue vault. The engine holds this vault's shares.
    IERC4626Vault public immutable vault;
    address public immutable baseAsset;
    string internal _name;
    string internal _symbol;

    /// @notice The sole `VaultAdapter` client. Set once, post-deploy (the factory creates the adapter
    ///         after the engine exists), then immutable.
    address public core;

    uint256 private locked = 1;

    event CoreInitialized(address indexed core);
    event Deposited(address indexed receiver, uint256 assets, uint256 shares);
    event VenueSharesDeposited(address indexed from, uint256 requestedShares, uint256 receivedShares);
    event Redeemed(address indexed receiver, uint256 shares, uint256 assets);
    event WithdrewExact(address indexed receiver, uint256 assets, uint256 sharesBurned);

    error AlreadyInitialized();
    error CoreNotSet();
    error EmptyMetadata();
    error InsufficientAssetsReceived(uint256 minimum, uint256 actual);
    error NoSharesReceived();
    error NotCore(address caller);
    error ReentrantCall();
    error TransferFailed();
    error ZeroAddress();

    modifier onlyCore() {
        if (core == address(0)) revert CoreNotSet();
        if (msg.sender != core) revert NotCore(msg.sender);
        _;
    }

    modifier nonReentrant() {
        if (locked != 1) revert ReentrantCall();
        locked = 2;
        _;
        locked = 1;
    }

    /// @param nameOverride_ Optional immutable engine name used when the venue omits or should not
    ///        supply metadata. An empty value derives the name from `vault_`.
    /// @param symbolOverride_ Optional immutable engine symbol. An empty value derives it from
    ///        `vault_`. The effective name and symbol must both be nonempty.
    constructor(IERC4626Vault vault_, address owner_, string memory nameOverride_, string memory symbolOverride_)
        Ownable(owner_ == address(0) ? msg.sender : owner_)
    {
        if (address(vault_) == address(0)) revert ZeroAddress();
        address asset_ = vault_.asset();
        if (asset_ == address(0)) revert ZeroAddress();
        vault = vault_;
        baseAsset = asset_;
        _name = bytes(nameOverride_).length == 0 ? vault_.name() : nameOverride_;
        _symbol = bytes(symbolOverride_).length == 0 ? vault_.symbol() : symbolOverride_;
        if (bytes(_name).length == 0 || bytes(_symbol).length == 0) revert EmptyMetadata();
    }

    /// @notice One-time binding of the sole client adapter (the factory deploys the adapter after the
    ///         engine). Owner-gated, set-once.
    function initializeCore(address core_) external onlyOwner {
        if (core != address(0)) revert AlreadyInitialized();
        if (core_ == address(0)) revert ZeroAddress();
        core = core_;
        emit CoreInitialized(core_);
    }

    // ------------------------------------------------------------------
    // IVaultEngine (sync) — onlyCore for mutating calls
    // ------------------------------------------------------------------

    function asset() external view returns (address) {
        return baseAsset;
    }

    function name() external view returns (string memory) {
        return _name;
    }

    function symbol() external view returns (string memory) {
        return _symbol;
    }

    /// @notice Wrapped 4626 shares held by the engine for the adapter (its entire backing).
    function totalShares() public view returns (uint256) {
        return vault.balanceOf(address(this));
    }

    /// @notice Pull `assets` from the adapter, deposit into the 4626 so shares mint to THIS engine, and
    ///         return the MEASURED share delta (the adapter mints its rate-converted share tokens).
    function deposit(uint256 assets) external onlyCore nonReentrant returns (uint256 shares) {
        _pull(baseAsset, msg.sender, assets);
        _forceApprove(baseAsset, address(vault), assets);

        uint256 before = vault.balanceOf(address(this));
        // forge-lint: disable-next-line(unused-return)
        vault.deposit(assets, address(this));
        shares = vault.balanceOf(address(this)) - before;

        _forceApprove(baseAsset, address(vault), 0);
        if (shares == 0) revert NoSharesReceived();
        emit Deposited(msg.sender, assets, shares);
    }

    /// @notice Pull already-issued shares of this engine's immutable ERC-4626 vault directly into
    ///         engine custody. This lets an existing vault shareholder enter Earn without first
    ///         redeeming to the base asset. The adapter independently converts the measured share
    ///         delta into EarnToken at its current anchor.
    function depositShares(uint256 shares, address from)
        external
        onlyCore
        nonReentrant
        returns (uint256 sharesReceived)
    {
        if (from == address(0)) revert ZeroAddress();
        uint256 before = vault.balanceOf(address(this));
        _pull(address(vault), from, shares);
        sharesReceived = vault.balanceOf(address(this)) - before;
        if (sharesReceived == 0) revert NoSharesReceived();
        emit VenueSharesDeposited(from, shares, sharesReceived);
    }

    /// @notice Redeem `shares` of the engine's held 4626 shares straight to `receiver`.
    function redeem(uint256 shares, address receiver, uint256 minAssets)
        external
        onlyCore
        nonReentrant
        returns (uint256 assets)
    {
        if (receiver == address(0)) revert ZeroAddress();
        assets = vault.redeem(shares, receiver, address(this));
        if (assets < minAssets) revert InsufficientAssetsReceived(minAssets, assets);
        emit Redeemed(receiver, shares, assets);
    }

    /// @notice EXACT-asset exit: withdraw exactly `assets` from the 4626 straight to `receiver`,
    ///         returning the 4626 shares the venue burned.
    function withdraw(uint256 assets, address receiver) external onlyCore nonReentrant returns (uint256 shares) {
        if (receiver == address(0)) revert ZeroAddress();
        shares = vault.withdraw(assets, receiver, address(this));
        emit WithdrewExact(receiver, assets, shares);
    }

    function previewWithdraw(uint256 assets) external view returns (uint256 shares) {
        return vault.previewWithdraw(assets);
    }

    function previewRedeem(uint256 shares) external view returns (uint256 assets) {
        return vault.previewRedeem(shares);
    }

    function valueOf(uint256 shares) external view returns (uint256 assets) {
        return vault.previewRedeem(shares);
    }

    function totalAssets() external view returns (uint256 assets) {
        return vault.previewRedeem(totalShares());
    }

    function supportsInterface(bytes4 interfaceId) external pure returns (bool) {
        return interfaceId == type(IERC165).interfaceId || interfaceId == type(IVaultEngine).interfaceId
            || interfaceId == type(IVaultEngineSync).interfaceId
            || interfaceId == type(IVaultEngineExactWithdraw).interfaceId
            || interfaceId == type(IVaultEngineShares).interfaceId;
    }

    // ------------------------------------------------------------------
    // Internal
    // ------------------------------------------------------------------

    function _pull(address token, address from, uint256 amount) private {
        if (!IERC20Like(token).transferFrom(from, address(this), amount)) revert TransferFailed();
    }

    /// @dev Zero-then-set safe approve with bool checks (USDT-style).
    function _forceApprove(address token, address spender, uint256 amount) private {
        if (!IERC20Like(token).approve(spender, 0)) revert TransferFailed();
        if (amount != 0) {
            if (!IERC20Like(token).approve(spender, amount)) revert TransferFailed();
        }
    }
}
