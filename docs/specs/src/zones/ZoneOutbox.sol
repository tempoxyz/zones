// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {IZoneOutbox} from "./interfaces/IZoneOutbox.sol";
import {IZoneTypes} from "./interfaces/IZoneTypes.sol";

/// @title ZoneOutbox
/// @notice Zone predeploy for exit intent management
/// @dev This contract is deployed as a predeploy on the zone. Its storage root
///      is committed to the zone block header's withdrawals_root field.
contract ZoneOutbox is IZoneOutbox {
    /// @notice The zone's gas token (TIP-20)
    address public immutable gasToken;

    /// @inheritdoc IZoneOutbox
    uint64 public nextExitIndex;

    /// @notice Exit intents by index
    mapping(uint64 => ExitIntent) private _exits;

    constructor(address gasToken_) {
        gasToken = gasToken_;
    }

    /// @inheritdoc IZoneOutbox
    function exitByIndex(uint64 exitIndex) external view returns (ExitIntent memory) {
        return _exits[exitIndex];
    }

    /// @inheritdoc IZoneOutbox
    function requestTransferExit(
        address recipient,
        uint128 amount,
        bytes32 memo
    ) external returns (bytes32 exitId) {
        if (amount == 0) {
            revert InvalidAmount();
        }

        _burnGasToken(msg.sender, amount);

        uint64 exitIndex = nextExitIndex++;

        ExitIntent memory intent = ExitIntent({
            kind: ExitKind.Transfer,
            exitIndex: exitIndex,
            sender: msg.sender,
            transfer: TransferExit({
                recipient: recipient,
                amount: amount
            }),
            swap: SwapExit({
                recipient: address(0),
                tokenOut: address(0),
                amountIn: 0,
                minAmountOut: 0
            }),
            swapAndDeposit: SwapAndDepositExit({
                tokenOut: address(0),
                amountIn: 0,
                minAmountOut: 0,
                destinationZoneId: 0,
                destinationRecipient: address(0)
            }),
            memo: memo
        });

        _exits[exitIndex] = intent;
        exitId = _computeExitId(exitIndex, intent);

        emit ExitRequested(exitId, exitIndex, ExitKind.Transfer);
    }

    /// @inheritdoc IZoneOutbox
    function requestSwapExit(
        address tokenOut,
        uint128 amountIn,
        uint128 minAmountOut,
        address recipient,
        bytes32 memo
    ) external returns (bytes32 exitId) {
        if (amountIn == 0) {
            revert InvalidAmount();
        }

        _burnGasToken(msg.sender, amountIn);

        uint64 exitIndex = nextExitIndex++;

        ExitIntent memory intent = ExitIntent({
            kind: ExitKind.Swap,
            exitIndex: exitIndex,
            sender: msg.sender,
            transfer: TransferExit({
                recipient: address(0),
                amount: 0
            }),
            swap: SwapExit({
                recipient: recipient,
                tokenOut: tokenOut,
                amountIn: amountIn,
                minAmountOut: minAmountOut
            }),
            swapAndDeposit: SwapAndDepositExit({
                tokenOut: address(0),
                amountIn: 0,
                minAmountOut: 0,
                destinationZoneId: 0,
                destinationRecipient: address(0)
            }),
            memo: memo
        });

        _exits[exitIndex] = intent;
        exitId = _computeExitId(exitIndex, intent);

        emit ExitRequested(exitId, exitIndex, ExitKind.Swap);
    }

    /// @inheritdoc IZoneOutbox
    function requestSwapAndDepositExit(
        address tokenOut,
        uint128 amountIn,
        uint128 minAmountOut,
        uint64 destinationZoneId,
        address destinationRecipient,
        bytes32 memo
    ) external returns (bytes32 exitId) {
        if (amountIn == 0) {
            revert InvalidAmount();
        }

        _burnGasToken(msg.sender, amountIn);

        uint64 exitIndex = nextExitIndex++;

        ExitIntent memory intent = ExitIntent({
            kind: ExitKind.SwapAndDeposit,
            exitIndex: exitIndex,
            sender: msg.sender,
            transfer: TransferExit({
                recipient: address(0),
                amount: 0
            }),
            swap: SwapExit({
                recipient: address(0),
                tokenOut: address(0),
                amountIn: 0,
                minAmountOut: 0
            }),
            swapAndDeposit: SwapAndDepositExit({
                tokenOut: tokenOut,
                amountIn: amountIn,
                minAmountOut: minAmountOut,
                destinationZoneId: destinationZoneId,
                destinationRecipient: destinationRecipient
            }),
            memo: memo
        });

        _exits[exitIndex] = intent;
        exitId = _computeExitId(exitIndex, intent);

        emit ExitRequested(exitId, exitIndex, ExitKind.SwapAndDeposit);
    }

    function _burnGasToken(address from, uint128 amount) internal {
        (bool success, bytes memory data) = gasToken.call(
            abi.encodeWithSignature("transferFrom(address,address,uint256)", from, address(this), uint256(amount))
        );

        if (!success || (data.length > 0 && !abi.decode(data, (bool)))) {
            revert InsufficientBalance();
        }

        (success,) = gasToken.call(
            abi.encodeWithSignature("burn(uint256)", uint256(amount))
        );
    }

    function _computeExitId(uint64 exitIndex, ExitIntent memory intent) internal pure returns (bytes32) {
        return keccak256(abi.encode(exitIndex, intent));
    }
}
