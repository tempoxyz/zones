// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { IERC20 } from "../interfaces/IERC20.sol";
import { IStablecoinDEX } from "../interfaces/IStablecoinDEX.sol";
import { IZoneFactory, IZonePortal, IZoneMessenger, IWithdrawalReceiver } from "./IZone.sol";

/// @title SwapAndDepositRouter
/// @notice Router contract for cross-zone transfers with token swap
/// @dev Receives withdrawal callbacks, swaps tokens via StablecoinDEX, and deposits to target zone
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

    /// @notice Receive a cross-zone withdrawal, swap tokens, and deposit to target zone
    /// @dev Implements IWithdrawalReceiver. Only callable by registered zone messengers.
    ///      The messenger has already transferred tokens to this router.
    ///      The callbackData encodes (tokenOut, targetPortal, recipient, memo, minAmountOut).
    /// @param sender The original sender on the source zone
    /// @param amount The amount of tokens transferred
    /// @param data ABI-encoded (address tokenOut, address targetPortal, address recipient, bytes32 memo, uint128 minAmountOut)
    /// @return selector The function selector to confirm successful handling
    function onWithdrawalReceived(
        address sender,
        uint128 amount,
        bytes calldata data
    ) external returns (bytes4) {
        // Verify caller is a registered zone messenger
        if (!zoneFactory.isZoneMessenger(msg.sender)) {
            revert UnauthorizedMessenger();
        }

        // Decode parameters
        (
            address tokenOut,
            address targetPortal,
            address recipient,
            bytes32 memo,
            uint128 minAmountOut
        ) = abi.decode(data, (address, address, address, bytes32, uint128));

        // Get input token from the messenger
        address tokenIn = IZoneMessenger(msg.sender).token();

        // Approve and swap via StablecoinDEX
        IERC20(tokenIn).approve(address(stablecoinDEX), amount);
        uint128 amountOut = stablecoinDEX.swapExactAmountIn(tokenIn, tokenOut, amount, minAmountOut);

        // Approve and deposit to target zone
        IERC20(tokenOut).approve(targetPortal, amountOut);
        IZonePortal(targetPortal).deposit(recipient, amountOut, memo);

        return IWithdrawalReceiver.onWithdrawalReceived.selector;
    }
}
