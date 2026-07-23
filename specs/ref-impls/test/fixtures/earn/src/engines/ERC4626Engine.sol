// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { IERC20Like } from "../interfaces/external/IERC20Like.sol";
import { IEarnEngine } from "../interfaces/engines/IEarnEngine.sol";
import { IEarnEngineExactWithdraw } from "../interfaces/engines/IEarnEngineExactWithdraw.sol";
import { IEarnEngineInKindDeposit } from "../interfaces/engines/IEarnEngineInKindDeposit.sol";
import { IEarnEngineRedeem } from "../interfaces/engines/IEarnEngineRedeem.sol";
import { Ownable } from "@openzeppelin/contracts/access/Ownable.sol";
import { Ownable2Step } from "@openzeppelin/contracts/access/Ownable2Step.sol";
import { IERC165 } from "@openzeppelin/contracts/utils/introspection/IERC165.sol";

interface IERC4626Vault {
    function asset() external view returns (address);
    function decimals() external view returns (uint8);
    function name() external view returns (string memory);
    function symbol() external view returns (string memory);
    function balanceOf(address account) external view returns (uint256);
    function previewWithdraw(uint256 assets) external view returns (uint256 venueShares);
    function previewRedeem(uint256 venueShares) external view returns (uint256 assets);
    function deposit(uint256 assets, address receiver) external returns (uint256 venueShares);
    function redeem(uint256 venueShares, address receiver, address owner) external returns (uint256 assets);
    function withdraw(uint256 assets, address receiver, address owner) external returns (uint256 venueShares);
}

/// @title ERC4626Engine
/// @notice Single-EarnVault engine for an immediate-execution ERC-4626 venue.
/// @dev The engine is the sole holder of venue shares backing its paired `EarnVault`.
contract ERC4626Engine is
    IEarnEngine,
    IEarnEngineRedeem,
    IEarnEngineExactWithdraw,
    IEarnEngineInKindDeposit,
    Ownable2Step
{
    IERC4626Vault public immutable vault;
    address public immutable baseAsset;
    uint256 private immutable _shareScale;
    string internal _name;
    string internal _symbol;

    address public earnVault;
    uint256 private locked = 1;

    event EarnVaultInitialized(address indexed earnVault);
    event Deposited(address indexed earnVault, uint256 assets, uint256 engineShares);
    event VenueSharesDeposited(address indexed from, uint256 requestedVenueShares, uint256 receivedEngineShares);
    event Redeemed(address indexed receiver, uint256 engineShares, uint256 assets);
    event WithdrewExact(address indexed receiver, uint256 assets, uint256 engineSharesBurned);

    error AlreadyInitialized();
    error EarnVaultNotSet();
    error EmptyMetadata();
    error InvalidVenueShareDecimals();
    error InsufficientAssetsReceived(uint256 minimumAssets, uint256 actualAssets);
    error NoVenueSharesReceived();
    error NotEarnVault(address caller);
    error ReentrantCall();
    error TransferFailed();
    error ZeroAddress();

    modifier onlyEarnVault() {
        if (earnVault == address(0)) revert EarnVaultNotSet();
        if (msg.sender != earnVault) revert NotEarnVault(msg.sender);
        _;
    }

    modifier nonReentrant() {
        if (locked != 1) revert ReentrantCall();
        locked = 2;
        _;
        locked = 1;
    }

    constructor(IERC4626Vault vault_, address owner_, string memory nameOverride_, string memory symbolOverride_)
        Ownable(owner_ == address(0) ? msg.sender : owner_)
    {
        if (address(vault_) == address(0)) revert ZeroAddress();
        address asset_ = vault_.asset();
        if (asset_ == address(0)) revert ZeroAddress();
        uint8 shareDecimals = vault_.decimals();
        if (shareDecimals > 77) revert InvalidVenueShareDecimals();
        vault = vault_;
        baseAsset = asset_;
        _shareScale = 10 ** uint256(shareDecimals);
        _name = bytes(nameOverride_).length == 0 ? vault_.name() : nameOverride_;
        _symbol = bytes(symbolOverride_).length == 0 ? vault_.symbol() : symbolOverride_;
        if (bytes(_name).length == 0 || bytes(_symbol).length == 0) revert EmptyMetadata();
    }

    function initializeEarnVault(address earnVault_) external onlyOwner {
        if (earnVault != address(0)) revert AlreadyInitialized();
        if (earnVault_ == address(0)) revert ZeroAddress();
        earnVault = earnVault_;
        emit EarnVaultInitialized(earnVault_);
    }

    function asset() external view returns (address) {
        return baseAsset;
    }

    function name() external view returns (string memory) {
        return _name;
    }

    function symbol() external view returns (string memory) {
        return _symbol;
    }

    function shareScale() external view returns (uint256) {
        return _shareScale;
    }

    function totalShares() public view returns (uint256) {
        return vault.balanceOf(address(this));
    }

    function deposit(uint256 assets) external onlyEarnVault nonReentrant returns (uint256 engineShares) {
        _pull(baseAsset, msg.sender, assets);
        _forceApprove(baseAsset, address(vault), assets);

        uint256 beforeBalance = vault.balanceOf(address(this));
        vault.deposit(assets, address(this));
        engineShares = vault.balanceOf(address(this)) - beforeBalance;

        _forceApprove(baseAsset, address(vault), 0);
        if (engineShares == 0) revert NoVenueSharesReceived();
        emit Deposited(msg.sender, assets, engineShares);
    }

    function depositInKind(uint256 venueShares, address from)
        external
        onlyEarnVault
        nonReentrant
        returns (uint256 engineSharesReceived)
    {
        if (from == address(0)) revert ZeroAddress();
        uint256 beforeBalance = vault.balanceOf(address(this));
        _pull(address(vault), from, venueShares);
        engineSharesReceived = vault.balanceOf(address(this)) - beforeBalance;
        if (engineSharesReceived == 0) revert NoVenueSharesReceived();
        emit VenueSharesDeposited(from, venueShares, engineSharesReceived);
    }

    function redeem(uint256 engineShares, address receiver, uint256 minAssets)
        external
        onlyEarnVault
        nonReentrant
        returns (uint256 assets)
    {
        if (receiver == address(0)) revert ZeroAddress();
        assets = vault.redeem(engineShares, receiver, address(this));
        if (assets < minAssets) revert InsufficientAssetsReceived(minAssets, assets);
        emit Redeemed(receiver, engineShares, assets);
    }

    function withdraw(uint256 assets, address receiver)
        external
        onlyEarnVault
        nonReentrant
        returns (uint256 engineShares)
    {
        if (receiver == address(0)) revert ZeroAddress();
        engineShares = vault.withdraw(assets, receiver, address(this));
        emit WithdrewExact(receiver, assets, engineShares);
    }

    function previewWithdraw(uint256 assets) external view returns (uint256 engineShares) {
        return vault.previewWithdraw(assets);
    }

    function previewRedeem(uint256 engineShares) external view returns (uint256 assets) {
        return vault.previewRedeem(engineShares);
    }

    function valueOf(uint256 engineShares) external view returns (uint256 assets) {
        return vault.previewRedeem(engineShares);
    }

    function totalAssets() external view returns (uint256 assets) {
        return vault.previewRedeem(totalShares());
    }

    function supportsInterface(bytes4 interfaceId) external pure returns (bool) {
        return interfaceId == type(IERC165).interfaceId || interfaceId == type(IEarnEngine).interfaceId
            || interfaceId == type(IEarnEngineRedeem).interfaceId
            || interfaceId == type(IEarnEngineExactWithdraw).interfaceId
            || interfaceId == type(IEarnEngineInKindDeposit).interfaceId;
    }

    function _pull(address token, address from, uint256 amount) private {
        if (!IERC20Like(token).transferFrom(from, address(this), amount)) revert TransferFailed();
    }

    function _forceApprove(address token, address spender, uint256 amount) private {
        if (!IERC20Like(token).approve(spender, 0)) revert TransferFailed();
        if (amount != 0 && !IERC20Like(token).approve(spender, amount)) revert TransferFailed();
    }
}
