// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {IZoneOutbox} from "./interfaces/IZoneOutbox.sol";

/// @title ZoneOutbox
/// @notice Zone predeploy for withdrawal management
/// @dev This contract is deployed as a predeploy on the zone. Withdrawals are
///      accumulated and committed to by the zone prover, then processed on Tempo.
contract ZoneOutbox is IZoneOutbox {
    /// @notice The zone ID
    uint64 public immutable zoneId;

    /// @notice The zone's gas token (TIP-20)
    address public immutable gasToken;

    /// @inheritdoc IZoneOutbox
    uint64 public nextWithdrawalIndex;

    /// @notice Withdrawals by index
    mapping(uint64 => Withdrawal) private _withdrawals;

    constructor(uint64 zoneId_, address gasToken_) {
        zoneId = zoneId_;
        gasToken = gasToken_;
    }

    /// @inheritdoc IZoneOutbox
    function withdrawalByIndex(uint64 index) external view returns (Withdrawal memory) {
        return _withdrawals[index];
    }

    /// @inheritdoc IZoneOutbox
    function requestWithdrawal(
        address to,
        uint128 amount,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes calldata data
    ) external returns (bytes32 withdrawalId) {
        if (amount == 0) revert InvalidAmount();

        _burnGasToken(msg.sender, amount);

        uint64 withdrawalIndex = nextWithdrawalIndex++;

        Withdrawal memory w = Withdrawal({
            sender: msg.sender,
            to: to,
            amount: amount,
            gasLimit: gasLimit,
            fallbackRecipient: fallbackRecipient,
            data: data
        });

        _withdrawals[withdrawalIndex] = w;
        withdrawalId = keccak256(abi.encode(zoneId, withdrawalIndex, w));

        emit WithdrawalRequested(withdrawalId, withdrawalIndex);
    }

    function _burnGasToken(address from, uint128 amount) internal {
        (bool success, bytes memory returnData) = gasToken.call(
            abi.encodeWithSignature("transferFrom(address,address,uint256)", from, address(this), uint256(amount))
        );

        if (!success || (returnData.length > 0 && !abi.decode(returnData, (bool)))) {
            revert InsufficientBalance();
        }

        (success,) = gasToken.call(
            abi.encodeWithSignature("burn(uint256)", uint256(amount))
        );
    }
}
