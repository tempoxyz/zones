// SPDX-License-Identifier: MIT
pragma solidity ^0.8.35;

/// @notice Minimal, local fixtures for the private-zone benchmark.
/// @dev These contracts intentionally implement the canonical synchronous gateway callback
///      surface. They are test fixtures, not a deployable product stack.

interface IFixtureToken {
    function approve(address spender, uint256 amount) external returns (bool);
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function mint(address to, uint256 amount) external;
    function burn(uint256 amount) external;
}

interface IFixturePortal {
    struct EncryptedDepositPayload {
        bytes32 ephemeralPubkeyX;
        uint8 ephemeralPubkeyYParity;
        bytes ciphertext;
        bytes12 nonce;
        bytes16 tag;
    }

    function depositEncrypted(
        address token,
        uint128 amount,
        uint256 keyIndex,
        EncryptedDepositPayload calldata encrypted,
        address bouncebackRecipient
    ) external returns (bytes32);

    function zoneId() external view returns (uint32);
    function messenger() external view returns (address);
}

interface IFixtureStableSwap {
    function swapExactIn(address tokenIn, address tokenOut, uint256 amountIn, address receiver, uint256 minAmountOut)
        external
        returns (uint256 amountOut);
}

/// @notice Exact synchronous subset consumed by the canonical gateway.
interface IFixtureVaultAdapter {
    function asset() external view returns (address);
    function shareToken() external view returns (address);
    function deposit(uint256 assets, address receiver, uint256 minShares) external returns (uint256 shares);
    function redeem(uint256 shares, address receiver, uint256 minAssets) external returns (uint256 assets);
}

/// @notice 1:1 reserve-backed swap fixture using the canonical swap signature.
contract DirectSwapFixture is IFixtureStableSwap {
    function swapExactIn(address tokenIn, address tokenOut, uint256 amountIn, address receiver, uint256 minAmountOut)
        external
        returns (uint256 amountOut)
    {
        amountOut = amountIn;
        require(amountOut >= minAmountOut, "minimum output");
        require(IFixtureToken(tokenIn).transferFrom(msg.sender, address(this), amountIn), "input transfer");
        require(IFixtureToken(tokenOut).transfer(receiver, amountOut), "output transfer");
    }
}

/// @notice 1:1 share fixture using the canonical adapter methods used by the gateway.
contract VaultAdapterFixture is IFixtureVaultAdapter {
    address public immutable override asset;
    address public immutable override shareToken;

    constructor(address asset_, address shareToken_) {
        asset = asset_;
        shareToken = shareToken_;
    }

    function deposit(uint256 assets, address receiver, uint256 minShares) external returns (uint256 shares) {
        shares = assets;
        require(shares >= minShares, "minimum shares");
        require(IFixtureToken(asset).transferFrom(msg.sender, address(this), assets), "asset transfer");
        IFixtureToken(shareToken).mint(receiver, shares);
    }

    function redeem(uint256 shares, address receiver, uint256 minAssets) external returns (uint256 assets) {
        assets = shares;
        require(assets >= minAssets, "minimum assets");
        require(IFixtureToken(shareToken).transferFrom(msg.sender, address(this), shares), "share transfer");
        IFixtureToken(shareToken).burn(shares);
        require(IFixtureToken(asset).transfer(receiver, assets), "asset transfer");
    }
}

/// @notice Narrow synchronous gateway fixture matching the canonical callback payload and flow.
contract ZoneGatewayFixture {
    enum Flow { Deposit, Redeem }

    struct CallbackData {
        Flow flow;
        address outputToken;
        uint256 keyIndex;
        IFixturePortal.EncryptedDepositPayload encrypted;
        uint128 minVaultAssets;
        uint128 minVaultShares;
        uint128 minOutputAmount;
        bytes32 actionId;
        address refundRecipient;
    }

    address public immutable vaultAdapter;
    address public immutable vaultAsset;
    address public immutable shareToken;
    address public immutable defaultSwapper;
    address public immutable zonePortal;
    address public immutable zoneMessenger;
    uint32 public immutable zoneId;

    event EarnDeposit(
        bytes32 indexed actionId,
        address indexed inputToken,
        uint256 inputAmount,
        uint256 vaultAssets,
        uint256 shares,
        bytes32 zoneDepositHash
    );
    event EarnRedeem(
        bytes32 indexed actionId,
        address indexed outputToken,
        uint256 shares,
        uint256 vaultAssets,
        uint256 outputAmount,
        bytes32 zoneDepositHash
    );

    constructor(address vaultAdapter_, address defaultSwapper_, address zonePortal_, address zoneMessenger_) {
        vaultAdapter = vaultAdapter_;
        vaultAsset = IFixtureVaultAdapter(vaultAdapter_).asset();
        shareToken = IFixtureVaultAdapter(vaultAdapter_).shareToken();
        defaultSwapper = defaultSwapper_;
        zonePortal = zonePortal_;
        zoneMessenger = zoneMessenger_;
        zoneId = IFixturePortal(zonePortal_).zoneId();
        require(IFixturePortal(zonePortal_).messenger() == zoneMessenger_, "messenger mismatch");
    }

    function onWithdrawalReceived(
        uint32 sourceZoneId,
        address sourcePortal,
        bytes32,
        address token,
        uint128 amount,
        bytes calldata callbackData
    ) external returns (bytes4) {
        require(msg.sender == zoneMessenger, "not messenger");
        require(sourceZoneId == zoneId && sourcePortal == zonePortal && amount > 0, "invalid source");
        CallbackData memory data = abi.decode(callbackData, (CallbackData));
        require(data.refundRecipient != address(0), "refund recipient");
        if (data.flow == Flow.Deposit) _deposit(token, amount, data);
        else _redeem(token, amount, data);
        return this.onWithdrawalReceived.selector;
    }

    function _deposit(address token, uint128 amount, CallbackData memory data) private {
        require(data.outputToken == shareToken, "wrong output");
        uint256 assets = token == vaultAsset ? amount : _swap(token, vaultAsset, amount, data.minVaultAssets);
        _approve(vaultAsset, vaultAdapter, assets);
        uint256 shares = IFixtureVaultAdapter(vaultAdapter).deposit(assets, address(this), data.minVaultShares);
        bytes32 depositHash = _returnToZone(shareToken, shares, data);
        emit EarnDeposit(data.actionId, token, amount, assets, shares, depositHash);
    }

    function _redeem(address token, uint128 amount, CallbackData memory data) private {
        require(token == shareToken && data.outputToken != address(0) && data.outputToken != shareToken, "wrong token");
        _approve(shareToken, vaultAdapter, amount);
        uint256 assets = IFixtureVaultAdapter(vaultAdapter).redeem(amount, address(this), data.minVaultAssets);
        uint256 output = data.outputToken == vaultAsset ? assets : _swap(vaultAsset, data.outputToken, assets, data.minOutputAmount);
        bytes32 depositHash = _returnToZone(data.outputToken, output, data);
        emit EarnRedeem(data.actionId, data.outputToken, amount, assets, output, depositHash);
    }

    function _swap(address tokenIn, address tokenOut, uint256 amount, uint256 minimum) private returns (uint256) {
        _approve(tokenIn, defaultSwapper, amount);
        return IFixtureStableSwap(defaultSwapper).swapExactIn(tokenIn, tokenOut, amount, address(this), minimum);
    }

    function _returnToZone(address token, uint256 amount, CallbackData memory data) private returns (bytes32) {
        require(amount <= type(uint128).max, "amount overflow");
        _approve(token, zonePortal, amount);
        return IFixturePortal(zonePortal).depositEncrypted(token, uint128(amount), data.keyIndex, data.encrypted, data.refundRecipient);
    }

    function _approve(address token, address spender, uint256 amount) private {
        require(IFixtureToken(token).approve(spender, 0), "approval reset");
        require(IFixtureToken(token).approve(spender, amount), "approval");
    }
}

/// @notice Token-only terminal recipient for the off-ramp benchmark leg.
contract BridgeWalletFixture { }
