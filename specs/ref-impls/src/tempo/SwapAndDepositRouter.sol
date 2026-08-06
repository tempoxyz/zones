// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    DepositPayload,
    IWithdrawalReceiver,
    IZoneFactory,
    IZonePortal,
    ZONE_MESSENGER_ADDRESS,
    ZoneInfo
} from "../interfaces/IZone.sol";
import { IStablecoinDEX } from "tempo-std/interfaces/IStablecoinDEX.sol";
import { ITIP20 } from "tempo-std/interfaces/ITIP20.sol";

/// @title SwapAndDepositRouter
/// @notice Router contract for cross-zone transfers with optional token swap
/// @dev Receives withdrawal callbacks, swaps tokens if needed via StablecoinDEX, and deposits to target zone.
///      Handles both same-token (no swap) and different-token (swap) cross-zone transfers.
///      On any failure (swap or deposit), the entire callback reverts, causing the withdrawal
///      to bounce back to the zoneFallbackRecipient on the source zone.
contract SwapAndDepositRouter is IWithdrawalReceiver {

    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    IStablecoinDEX public immutable stablecoinDEX;
    IZoneFactory public immutable zoneFactory;

    /// @notice Encrypted deposit payloads already forwarded by this router.
    /// @dev The canonical payload hash acts as a nullifier. A successful callback may consume a
    ///      payload exactly once, preventing replay through this shared sender.
    mapping(bytes32 nullifier => bool consumed) public payloads;

    /*//////////////////////////////////////////////////////////////
                                ERRORS
    //////////////////////////////////////////////////////////////*/

    error UnauthorizedMessenger();
    error InvalidSourcePortal();
    error InvalidTargetPortal();
    error InvalidToken();
    error EncryptedPayloadAlreadyConsumed(bytes32 nullifier);

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
    /// @dev Implements IWithdrawalReceiver. Only callable by the shared zone messenger.
    ///      The messenger has already transferred tokens to this router.
    ///      On failure, the entire callback reverts, triggering bounce-back to source zone.
    /// @param tokenIn The TIP-20 token received from the source zone withdrawal
    /// @param amount The amount of tokens transferred
    /// @param data ABI-encoded callbackData (see format below)
    /// @return selector The function selector to confirm successful handling
    ///
    /// Format: (address tokenOut, address targetPortal, uint256 keyIndex, DepositPayload encrypted, address tempoRefundRecipient, uint128 minAmountOut)
    ///
    /// Note: minAmountOut is ignored for same-token transfers (no swap)
    function onWithdrawalReceived(
        uint32 sourceZoneId,
        address sourcePortal,
        bytes32, /* senderTag */
        address tokenIn,
        uint128 amount,
        bytes calldata data
    )
        external
        returns (bytes4)
    {
        if (msg.sender != ZONE_MESSENGER_ADDRESS) {
            revert UnauthorizedMessenger();
        }

        ZoneInfo memory sourceZone = zoneFactory.zones(sourceZoneId);
        if (sourceZone.portal != sourcePortal) {
            revert InvalidSourcePortal();
        }

        (
            address tokenOut,
            address targetPortal,
            uint256 keyIndex,
            DepositPayload memory encrypted,
            address tempoRefundRecipient,
            uint128 minAmountOut
        ) = abi.decode(data, (address, address, uint256, DepositPayload, address, uint128));

        // Zone portals treat this shared router as the depositor, so sender binding alone can't
        // distinguish withdrawals. Consume each encrypted payload once to prevent replay.
        bytes32 nullifier = _payloadNullifier(encrypted);
        if (payloads[nullifier]) {
            revert EncryptedPayloadAlreadyConsumed(nullifier);
        }
        payloads[nullifier] = true;

        _validateTarget(targetPortal, tokenOut);

        uint128 amountOut = _swapIfNeeded(tokenIn, tokenOut, amount, minAmountOut);

        ITIP20(tokenOut).approve(targetPortal, amountOut);
        IZonePortal(targetPortal)
            .deposit(tokenOut, amountOut, keyIndex, encrypted, tempoRefundRecipient);

        return IWithdrawalReceiver.onWithdrawalReceived.selector;
    }

    /*//////////////////////////////////////////////////////////////
                           INTERNAL HELPERS
    //////////////////////////////////////////////////////////////*/

    /// @dev Excludes Y parity because P and -P have the same ECDH x-coordinate and AES key,
    ///      making parity a malleable encoding of the same effective encrypted payload.
    function _payloadNullifier(DepositPayload memory encrypted) internal pure returns (bytes32) {
        return keccak256(
            abi.encode(
                encrypted.ephemeralPubkeyX, encrypted.ciphertext, encrypted.nonce, encrypted.tag
            )
        );
    }

    /// @notice Validate the target portal is registered and the token is enabled on it
    function _validateTarget(address targetPortal, address tokenOut) internal view {
        if (!zoneFactory.isZonePortal(targetPortal)) {
            revert InvalidTargetPortal();
        }
        if (!IZonePortal(targetPortal).isTokenEnabled(tokenOut)) {
            revert InvalidToken();
        }
    }

    function _swapIfNeeded(
        address tokenIn,
        address tokenOut,
        uint128 amountIn,
        uint128 minAmountOut
    )
        internal
        returns (uint128 amountOut)
    {
        if (tokenIn == tokenOut) {
            return amountIn;
        }

        ITIP20(tokenIn).approve(address(stablecoinDEX), amountIn);
        amountOut = stablecoinDEX.swapExactAmountIn(tokenIn, tokenOut, amountIn, minAmountOut);
    }

}
