// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { IERC20Like } from "../interfaces/external/IERC20Like.sol";

/// @notice Minimal ERC4626-like vault used only by the Moderato live setup.
/// @dev This support contract is intentionally small and is not a production vault implementation.
contract Simple4626Vault {

    string public name;
    string public symbol;
    uint8 public immutable decimals;
    address public immutable asset;
    uint256 public totalSupply;

    mapping(address account => uint256 shares) public balanceOf;
    mapping(address owner => mapping(address spender => uint256 amount)) public allowance;

    event Approval(address indexed owner, address indexed spender, uint256 amount);
    event Deposit(address indexed caller, address indexed owner, uint256 assets, uint256 shares);
    event Donated(address indexed caller, uint256 assets);
    event Transfer(address indexed from, address indexed to, uint256 amount);
    event Withdraw(
        address indexed caller,
        address indexed receiver,
        address indexed owner,
        uint256 assets,
        uint256 shares
    );

    error AllowanceExceeded();
    error InsufficientOutput();
    error InsufficientShares();
    error TokenCallFailed();
    error TokenCallFalse();
    error ZeroAddress();
    error ZeroAmount();

    constructor(address asset_, string memory name_, string memory symbol_, uint8 decimals_) {
        if (asset_ == address(0)) revert ZeroAddress();
        asset = asset_;
        name = name_;
        symbol = symbol_;
        decimals = decimals_;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        emit Approval(msg.sender, spender, amount);
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        _transfer(msg.sender, to, amount);
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        uint256 allowed = allowance[from][msg.sender];
        if (allowed != type(uint256).max) {
            if (allowed < amount) revert AllowanceExceeded();
            allowance[from][msg.sender] = allowed - amount;
            emit Approval(from, msg.sender, allowance[from][msg.sender]);
        }
        _transfer(from, to, amount);
        return true;
    }

    function totalAssets() public view returns (uint256) {
        return IERC20Like(asset).balanceOf(address(this));
    }

    function convertToShares(uint256 assets) public view returns (uint256) {
        uint256 supply = totalSupply;
        uint256 managedAssets = totalAssets();
        if (supply == 0 || managedAssets == 0) return _scaleInitialAssetsToShares(assets);
        return assets * supply / managedAssets;
    }

    function convertToAssets(uint256 shares) public view returns (uint256) {
        uint256 supply = totalSupply;
        if (supply == 0) return _scaleInitialSharesToAssets(shares);
        return shares * totalAssets() / supply;
    }

    function previewWithdraw(uint256 assets) public view returns (uint256) {
        uint256 supply = totalSupply;
        uint256 managedAssets = totalAssets();
        if (supply == 0 || managedAssets == 0) return _scaleInitialAssetsToShares(assets);
        return _ceilDiv(assets * supply, managedAssets);
    }

    function previewRedeem(uint256 shares) public view returns (uint256) {
        return convertToAssets(shares);
    }

    function deposit(uint256 assets, address receiver) external returns (uint256 shares) {
        if (assets == 0) revert ZeroAmount();
        if (receiver == address(0)) revert ZeroAddress();

        shares = convertToShares(assets);
        if (shares == 0) revert InsufficientOutput();

        _safeTransferFrom(asset, msg.sender, address(this), assets);
        _mint(receiver, shares);
        emit Deposit(msg.sender, receiver, assets, shares);
    }

    /// @notice Demo-only yield injection that adds assets without minting venue shares.
    function donate(uint256 assets) external {
        if (assets == 0) revert ZeroAmount();
        _safeTransferFrom(asset, msg.sender, address(this), assets);
        emit Donated(msg.sender, assets);
    }

    function redeem(
        uint256 shares,
        address receiver,
        address owner
    )
        external
        returns (uint256 assets)
    {
        if (shares == 0) revert ZeroAmount();
        if (receiver == address(0) || owner == address(0)) revert ZeroAddress();

        assets = convertToAssets(shares);
        if (assets == 0) revert InsufficientOutput();

        if (msg.sender != owner) {
            uint256 allowed = allowance[owner][msg.sender];
            if (allowed != type(uint256).max) {
                if (allowed < shares) revert AllowanceExceeded();
                allowance[owner][msg.sender] = allowed - shares;
                emit Approval(owner, msg.sender, allowance[owner][msg.sender]);
            }
        }

        _burn(owner, shares);
        _safeTransfer(asset, receiver, assets);
        emit Withdraw(msg.sender, receiver, owner, assets, shares);
    }

    function withdraw(
        uint256 assets,
        address receiver,
        address owner
    )
        external
        returns (uint256 shares)
    {
        if (assets == 0) revert ZeroAmount();
        if (receiver == address(0) || owner == address(0)) revert ZeroAddress();

        shares = previewWithdraw(assets);
        if (shares == 0) revert InsufficientOutput();

        if (msg.sender != owner) {
            uint256 allowed = allowance[owner][msg.sender];
            if (allowed != type(uint256).max) {
                if (allowed < shares) revert AllowanceExceeded();
                allowance[owner][msg.sender] = allowed - shares;
                emit Approval(owner, msg.sender, allowance[owner][msg.sender]);
            }
        }

        _burn(owner, shares);
        _safeTransfer(asset, receiver, assets);
        emit Withdraw(msg.sender, receiver, owner, assets, shares);
    }

    function _scaleInitialAssetsToShares(uint256 assets) internal view returns (uint256) {
        uint8 assetDecimals = _assetDecimals();
        if (decimals == assetDecimals) return assets;
        if (decimals > assetDecimals) return assets * (10 ** uint256(decimals - assetDecimals));
        return assets / (10 ** uint256(assetDecimals - decimals));
    }

    function _scaleInitialSharesToAssets(uint256 shares) internal view returns (uint256) {
        uint8 assetDecimals = _assetDecimals();
        if (decimals == assetDecimals) return shares;
        if (decimals > assetDecimals) return shares / (10 ** uint256(decimals - assetDecimals));
        return shares * (10 ** uint256(assetDecimals - decimals));
    }

    function _assetDecimals() internal view returns (uint8 assetDecimals) {
        (bool ok, bytes memory result) = asset.staticcall(abi.encodeWithSignature("decimals()"));
        if (!ok || result.length == 0) return 18;
        return abi.decode(result, (uint8));
    }

    function _ceilDiv(uint256 numerator, uint256 denominator) internal pure returns (uint256) {
        if (numerator == 0) return 0;
        return (numerator - 1) / denominator + 1;
    }

    function _mint(address to, uint256 amount) internal {
        totalSupply += amount;
        balanceOf[to] += amount;
        emit Transfer(address(0), to, amount);
    }

    function _burn(address from, uint256 amount) internal {
        if (balanceOf[from] < amount) revert InsufficientShares();
        balanceOf[from] -= amount;
        totalSupply -= amount;
        emit Transfer(from, address(0), amount);
    }

    function _transfer(address from, address to, uint256 amount) internal {
        if (to == address(0)) revert ZeroAddress();
        if (balanceOf[from] < amount) revert InsufficientShares();
        balanceOf[from] -= amount;
        balanceOf[to] += amount;
        emit Transfer(from, to, amount);
    }

    function _safeTransfer(address token, address to, uint256 amount) internal {
        _callOptionalReturn(token, abi.encodeWithSelector(IERC20Like.transfer.selector, to, amount));
    }

    function _safeTransferFrom(address token, address from, address to, uint256 amount) internal {
        _callOptionalReturn(
            token, abi.encodeWithSelector(IERC20Like.transferFrom.selector, from, to, amount)
        );
    }

    function _callOptionalReturn(address token, bytes memory data) internal {
        (bool ok, bytes memory returnData) = token.call(data);
        if (!ok) revert TokenCallFailed();
        if (returnData.length != 0 && !abi.decode(returnData, (bool))) revert TokenCallFalse();
    }

}
