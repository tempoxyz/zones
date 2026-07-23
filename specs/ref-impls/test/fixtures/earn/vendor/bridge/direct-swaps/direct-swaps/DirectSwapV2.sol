// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

import { ITokenHandler } from "../token-handler/ITokenHandler.sol";
import {
    InvalidAmount,
    InvalidAuthPolicyId,
    InvalidFeeBps,
    InvalidSwapAmounts,
    InvalidTokenAddress,
    InvalidTokenDecimals,
    InvalidTokenHandler,
    UnauthorizedCaller,
    ZeroAddress
} from "../utils/DirectSwapErrorsAndEvents.sol";
import { Swap } from "../utils/DirectSwapErrorsAndEvents.sol";
import { DirectSwapTransientStorageLib } from "../utils/DirectSwapStorage.sol";
import { IDirectSwapV2 } from "../../../../src/demo/bridge/IDirectSwapV2.sol";
import { IAuthRegistry } from "auth-registry/src/IAuthRegistry.sol";
import { IERC20 } from "openzeppelin-contracts/token/ERC20/IERC20.sol";
import { IERC20Metadata } from "openzeppelin-contracts/token/ERC20/extensions/IERC20Metadata.sol";
import { SafeERC20 } from "openzeppelin-contracts/token/ERC20/utils/SafeERC20.sol";
import { ReentrancyGuardTransient } from "openzeppelin-contracts/utils/ReentrancyGuardTransient.sol";
import { Math } from "openzeppelin-contracts/utils/math/Math.sol";

import { ERC165Checker } from "openzeppelin-contracts/utils/introspection/ERC165Checker.sol";

/**
 * @title DirectSwapV2
 * @author Bridge
 * @notice Facilitates atomic swaps between backing asset tokens and stablecoins (e.g.,
 * xUSD).
 * @dev This contract acts as a router that coordinates token deposits and withdrawals through
 * specialized token handlers. It supports both exact-input and exact-output swap modes with
 * optional fee collection. Access is restricted to callers authorized via an external AuthRegistry.
 *
 * Key features:
 * - Atomic swaps between backing asset and stablecoin tokens
 * - Configurable fee collection in basis points
 * - Per-transaction rate limiting via transient storage
 * - Policy-based caller authorization
 */
contract DirectSwapV2 is IDirectSwapV2, ReentrancyGuardTransient {
    using SafeERC20 for IERC20;

    /// @dev Denominator for basis point calculations (100% = 10,000 bps)
    uint256 private constant BPS_DENOMINATOR = 10_000;

    /// @dev Limit for the fee in basis points (10%)
    uint256 private constant FEE_LIMIT_BPS = 1000;

    /// @notice The AuthRegistry contract used for caller authorization
    IAuthRegistry public immutable AUTH_REGISTRY;

    /// @notice The address of the reserve ledger token
    address public immutable RESERVE_LEDGER_TOKEN_ADDRESS;

    /// @notice The address of the stablecoin handler
    address public immutable STABLECOIN_HANDLER_ADDRESS;

    /// @dev Fee amount in basis points (1 bps = 0.01%)
    uint256 private immutable FEE_BPS;

    /// @dev Maximum amount allowed per transaction
    uint96 private immutable TRANSACTION_LIMIT;

    /// @dev Address that receives collected fees
    address private immutable FEE_RECIPIENT;

    /// @dev Policy ID for caller authorization via AuthRegistry
    uint64 private immutable ALLOWED_CALLER_POLICY_ID;

    /// @dev Validates the token pair for swapping
    modifier onlyValidTokens(address token0, address token1) {
        _validateTokens(token0, token1);
        _;
    }

    /// @dev Restricts access to callers authorized by the AuthRegistry policy
    modifier onlyAllowedCaller() {
        _checkAllowedCaller();
        _;
    }

    /**
     * @notice Constructs a new DirectSwapV2 contract
     * @param _reserveLedgerTokenAddress Address of the reserve ledger token
     * @param _stablecoinHandlerAddress Address of the handler for stablecoin operations
     * @param _transactionLimit Maximum amount allowed per transaction
     * @param _feeRecipient Address that receives swap fees
     * @param _feeBps Fee amount in basis points (must be < 1,000 i.e. 10%)
     * @param _authRegistryAddress Address of the AuthRegistry for caller authorization
     * @param _allowedCallerPolicyId Policy ID used to check caller authorization
     */
    constructor(
        address _reserveLedgerTokenAddress,
        address _stablecoinHandlerAddress,
        uint96 _transactionLimit,
        address _feeRecipient,
        uint256 _feeBps,
        address _authRegistryAddress,
        uint64 _allowedCallerPolicyId
    ) {
        require(_reserveLedgerTokenAddress != address(0), ZeroAddress());
        require(_stablecoinHandlerAddress != address(0), ZeroAddress());
        require(_feeRecipient != address(0), ZeroAddress());
        require(_feeBps <= FEE_LIMIT_BPS, InvalidFeeBps());
        require(_authRegistryAddress != address(0), ZeroAddress());
        require(_allowedCallerPolicyId != 0, InvalidAuthPolicyId());

        AUTH_REGISTRY = IAuthRegistry(_authRegistryAddress);
        RESERVE_LEDGER_TOKEN_ADDRESS = _reserveLedgerTokenAddress;
        STABLECOIN_HANDLER_ADDRESS = _stablecoinHandlerAddress;

        require(
            ERC165Checker.supportsInterface(_stablecoinHandlerAddress, type(ITokenHandler).interfaceId),
            InvalidTokenHandler()
        );

        FEE_BPS = _feeBps;
        FEE_RECIPIENT = _feeRecipient;
        TRANSACTION_LIMIT = _transactionLimit;
        ALLOWED_CALLER_POLICY_ID = _allowedCallerPolicyId;
    }

    /**
     * @notice Swaps tokens with an exact output amount
     * @dev The caller specifies the exact amount they want to receive. The input amount is
     * calculated based on the fee, rounding up to ensure the protocol doesn't lose on rounding.
     * Output tokens are always sent to `msg.sender`.
     * @param _tokenIn Address of the token being sold
     * @param _tokenOut Address of the token being bought
     * @param _amountOut Exact amount of output tokens to receive
     */
    function swapExactOut(address _tokenIn, address _tokenOut, uint256 _amountOut)
        external
        onlyValidTokens(_tokenIn, _tokenOut)
        onlyAllowedCaller
        nonReentrant
    {
        require(_amountOut > 0, InvalidAmount());

        uint256 amountIn = _quoteExactOut(_amountOut);

        _swap(_tokenIn, _tokenOut, amountIn, _amountOut);
    }

    /**
     * @notice Swaps tokens with an exact input amount
     * @dev The caller specifies the exact amount they want to sell. The output amount is
     * calculated after deducting the fee from the input. Output tokens are always sent to
     * `msg.sender`.
     * @param _tokenIn Address of the token being sold
     * @param _tokenOut Address of the token being bought
     * @param _amountIn Exact amount of input tokens to sell
     */
    function swapExactIn(address _tokenIn, address _tokenOut, uint256 _amountIn)
        external
        onlyValidTokens(_tokenIn, _tokenOut)
        onlyAllowedCaller
        nonReentrant
    {
        require(_amountIn > 0, InvalidAmount());

        uint256 amountOut = _quoteExactIn(_amountIn);

        _swap(_tokenIn, _tokenOut, _amountIn, amountOut);
    }

    /**
     * @notice Returns the input amount required to receive `_amountOut` output tokens via
     * `swapExactOut`.
     * @dev Mirrors the rounding used by `swapExactOut` (rounds up so the protocol does not
     * lose on rounding). Reverts on the same input validation as the swap (zero amount).
     * @param _amountOut Exact amount of output tokens that would be received
     * @return amountIn Amount of input tokens that would be pulled from the caller
     */
    function quoteExactOut(uint256 _amountOut) external view returns (uint256 amountIn) {
        require(_amountOut > 0, InvalidAmount());
        return _quoteExactOut(_amountOut);
    }

    /**
     * @notice Returns the output amount that would be received for `_amountIn` input tokens via
     * `swapExactIn`.
     * @dev Mirrors the rounding used by `swapExactIn` (rounds down to favor the protocol).
     * Reverts on the same input validation as the swap (zero amount).
     * @param _amountIn Exact amount of input tokens that would be sold
     * @return amountOut Amount of output tokens that would be sent to the caller
     */
    function quoteExactIn(uint256 _amountIn) external view returns (uint256 amountOut) {
        require(_amountIn > 0, InvalidAmount());
        return _quoteExactIn(_amountIn);
    }

    /// @notice Returns the current fee in basis points
    /// @return The fee amount in basis points (1 bps = 0.01%)
    function getFeeBps() external view returns (uint256) {
        return FEE_BPS;
    }

    /// @notice Returns the per-transaction limit
    /// @return The maximum amount allowed per transaction
    function getTransactionLimit() external view returns (uint256) {
        return TRANSACTION_LIMIT;
    }

    /// @notice Returns the address that receives fees
    /// @return The fee recipient address
    function getFeeRecipient() external view returns (address) {
        return FEE_RECIPIENT;
    }

    /// @notice Returns the policy ID used for caller authorization
    /// @return The AuthRegistry policy ID
    function getAllowedCallerPolicyId() external view returns (uint64) {
        return ALLOWED_CALLER_POLICY_ID;
    }

    /**
     * @dev Executes the swap by coordinating deposits and withdrawals through handlers.
     * Output tokens are sent to `msg.sender`.
     * @param _tokenIn Address of the input token
     * @param _tokenOut Address of the output token
     * @param _amountIn Amount of input tokens
     * @param _amountOut Amount of output tokens
     */
    function _swap(address _tokenIn, address _tokenOut, uint256 _amountIn, uint256 _amountOut) internal {
        require(_amountIn > 0, InvalidAmount());
        require(_amountOut > 0, InvalidAmount());
        require(_amountIn >= _amountOut, InvalidAmount());

        uint256 feeAmount = _amountIn - _amountOut;

        _increaseTransientLimit(_amountOut);

        IERC20(_tokenIn).safeTransferFrom(msg.sender, address(this), _amountIn);

        // Unwrap input stablecoin -> RL
        IERC20(_tokenIn).forceApprove(STABLECOIN_HANDLER_ADDRESS, _amountOut);
        ITokenHandler(STABLECOIN_HANDLER_ADDRESS).deposit(_tokenIn, _amountOut);

        // Wrap RL -> output stablecoin
        IERC20(RESERVE_LEDGER_TOKEN_ADDRESS).forceApprove(STABLECOIN_HANDLER_ADDRESS, _amountOut);
        ITokenHandler(STABLECOIN_HANDLER_ADDRESS).withdraw(_tokenOut, _amountOut);

        IERC20(_tokenOut).safeTransfer(msg.sender, _amountOut);

        if (feeAmount > 0) {
            IERC20(_tokenIn).safeTransfer(FEE_RECIPIENT, feeAmount);
        }

        emit Swap(msg.sender, _tokenIn, _tokenOut, _amountIn, _amountOut, feeAmount, msg.sender);
    }

    /**
     * @dev Updates the transient rate limit for the current transaction
     * @param amount The amount to add to the current transaction's usage
     */
    function _increaseTransientLimit(uint256 amount) internal {
        require(amount <= TRANSACTION_LIMIT);
        require(amount <= type(uint96).max);
        // casting to 'uint96' is safe because we check above that the amount is less than the max
        // uint96
        // forge-lint: disable-next-line(unsafe-typecast)
        uint96 amountCast = uint96(amount);
        DirectSwapTransientStorageLib.increaseTransientMintLimitUsed(amountCast, TRANSACTION_LIMIT);
    }

    /**
     * @dev Validates that the token pair is valid for swapping
     * @param token0 First token address
     * @param token1 Second token address
     */
    function _validateTokens(address token0, address token1) internal view {
        require(token0 != token1, InvalidTokenAddress());
        require(IERC20Metadata(token0).decimals() == IERC20Metadata(token1).decimals(), InvalidTokenDecimals());
    }

    /**
     * @dev Computes the input amount required to receive `_amountOut` output tokens.
     * Rounds up so the protocol does not lose on rounding.
     * amountIn = ceil(amountOut * BPS_DENOMINATOR / (BPS_DENOMINATOR - FEE_BPS))
     */
    function _quoteExactOut(uint256 _amountOut) internal view returns (uint256) {
        return Math.mulDiv(_amountOut, BPS_DENOMINATOR, BPS_DENOMINATOR - FEE_BPS, Math.Rounding.Ceil);
    }

    /**
     * @dev Computes the output amount produced by selling `_amountIn` input tokens.
     * Rounds down so any rounding dust accrues to the protocol via the fee.
     * amountOut = floor(amountIn * (BPS_DENOMINATOR - FEE_BPS) / BPS_DENOMINATOR)
     */
    function _quoteExactIn(uint256 _amountIn) internal view returns (uint256) {
        return Math.mulDiv(_amountIn, BPS_DENOMINATOR - FEE_BPS, BPS_DENOMINATOR, Math.Rounding.Floor);
    }

    /// @dev Verifies the caller is authorized via the AuthRegistry policy
    function _checkAllowedCaller() internal view {
        require(AUTH_REGISTRY.isAuthorized(ALLOWED_CALLER_POLICY_ID, msg.sender), UnauthorizedCaller());
    }
}
