// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { IERC20 } from "../interfaces/IERC20.sol";
import { IStablecoinDEX } from "../interfaces/IStablecoinDEX.sol";
import { IZoneFactory, IZonePortal, IZoneMessenger, IWithdrawalReceiver, EncryptedDepositPayload } from "./IZone.sol";

/// @title SwapAndDepositRouter
/// @notice Router contract for cross-zone transfers with optional token swap
/// @dev Receives withdrawal callbacks, swaps tokens if needed via StablecoinDEX, and deposits to target zone.
///      Handles both same-token (no swap) and different-token (swap) cross-zone transfers.
///      Supports both plaintext and encrypted deposits.
contract SwapAndDepositRouter is IWithdrawalReceiver {
    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    IStablecoinDEX public immutable stablecoinDEX;
    IZoneFactory public immutable zoneFactory;

    /*//////////////////////////////////////////////////////////////
                                ERRORS
    //////////////////////////////////////////////////////////////*/

    error UnauthorizedMessenger();
    error InvalidToken();
    error SwapFailed();
    error DepositFailed();

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(address _stablecoinDEX, address _zoneFactory) {
        stablecoinDEX = IStablecoinDEX(_stablecoinDEX);
        zoneFactory = IZoneFactory(_zoneFactory);
    }

    /*//////////////////////////////////////////////////////////////
                         WITHDRAWAL CALLBACK
    //////////////////////////////////////////////////////////////*/

    /// @notice Receive a cross-zone withdrawal, optionally swap tokens, and deposit to target zone
    /// @dev Implements IWithdrawalReceiver. Only callable by registered zone messengers.
    ///      The messenger has already transferred tokens to this router.
    ///      Handles both same-token (no swap) and different-token (swap required) transfers.
    ///      Supports both plaintext and encrypted deposits.
    /// @param sender The original sender on the source zone
    /// @param amount The amount of tokens transferred
    /// @param data ABI-encoded callbackData (see format below)
    /// @return selector The function selector to confirm successful handling
    ///
    /// Plaintext format: (bool isEncrypted=false, address tokenOut, address targetPortal, address recipient, bytes32 memo, uint128 minAmountOut)
    /// Encrypted format: (bool isEncrypted=true, address tokenOut, address targetPortal, uint256 keyIndex, EncryptedDepositPayload encrypted, uint128 minAmountOut)
    ///
    /// Note: minAmountOut is ignored for same-token transfers (no swap)
    function onWithdrawalReceived(
        address sender,
        uint128 amount,
        bytes calldata data
    ) external returns (bytes4) {
        // Verify caller is a registered zone messenger
        if (!zoneFactory.isZoneMessenger(msg.sender)) {
            revert UnauthorizedMessenger();
        }

        // Get input token from the messenger
        address tokenIn = IZoneMessenger(msg.sender).token();

        // Decode isEncrypted flag
        bool isEncrypted = abi.decode(data, (bool));

        if (isEncrypted) {
            // Encrypted deposit: decode encrypted payload
            (
                , // skip isEncrypted
                address tokenOut,
                address targetPortal,
                uint256 keyIndex,
                EncryptedDepositPayload memory encrypted,
                uint128 minAmountOut
            ) = abi.decode(data, (bool, address, address, uint256, EncryptedDepositPayload, uint128));

            uint128 amountOut;
            if (tokenIn == tokenOut) {
                // Same token: no swap needed
                amountOut = amount;
            } else {
                // Different tokens: swap via StablecoinDEX
                IERC20(tokenIn).approve(address(stablecoinDEX), amount);
                amountOut = stablecoinDEX.swapExactAmountIn(tokenIn, tokenOut, amount, minAmountOut);
            }

            // Approve and deposit encrypted to target zone
            IERC20(tokenOut).approve(targetPortal, amountOut);
            IZonePortal(targetPortal).depositEncrypted(amountOut, keyIndex, encrypted);
        } else {
            // Plaintext deposit: decode recipient and memo
            (
                , // skip isEncrypted
                address tokenOut,
                address targetPortal,
                address recipient,
                bytes32 memo,
                uint128 minAmountOut
            ) = abi.decode(data, (bool, address, address, address, bytes32, uint128));

            uint128 amountOut;
            if (tokenIn == tokenOut) {
                // Same token: no swap needed
                amountOut = amount;
            } else {
                // Different tokens: swap via StablecoinDEX
                IERC20(tokenIn).approve(address(stablecoinDEX), amount);
                amountOut = stablecoinDEX.swapExactAmountIn(tokenIn, tokenOut, amount, minAmountOut);
            }

            // Approve and deposit to target zone
            IERC20(tokenOut).approve(targetPortal, amountOut);
            IZonePortal(targetPortal).deposit(recipient, amountOut, memo);
        }

        return IWithdrawalReceiver.onWithdrawalReceived.selector;
    }
}
