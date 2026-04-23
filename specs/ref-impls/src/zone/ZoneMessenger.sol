// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { ITIP20 } from "tempo-std/interfaces/ITIP20.sol";
import { IWithdrawalReceiver, IZoneMessenger } from "./IZone.sol";

/// @title ZoneMessenger
/// @notice Per-zone messenger that handles withdrawal callbacks
/// @dev Deployed by ZoneFactory for each zone. The portal gives the messenger max approval
///      for each enabled token. Withdrawal callbacks originate from this contract, not the portal.
contract ZoneMessenger is IZoneMessenger {

    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    /// @notice The zone's portal address
    address public immutable portal;

    /*//////////////////////////////////////////////////////////////
                                ERRORS
    //////////////////////////////////////////////////////////////*/

    error OnlyPortal();
    error CallbackRejected();
    error TransferFailed();

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(address _portal) {
        portal = _portal;
    }

    /*//////////////////////////////////////////////////////////////
                               MODIFIERS
    //////////////////////////////////////////////////////////////*/

    modifier onlyPortal() {
        if (msg.sender != portal) revert OnlyPortal();
        _;
    }

    /*//////////////////////////////////////////////////////////////
                           MESSAGE RELAY
    //////////////////////////////////////////////////////////////*/

    /// @notice Relay a withdrawal message. Only callable by the portal.
    /// @dev Transfers tokens from portal to target via transferFrom, then executes callback.
    ///      If callback reverts, returns false (does not revert). The transfer is atomic
    ///      with the callback via a self-call pattern.
    /// @param _token The TIP-20 token to transfer
    /// @param senderTag The authenticated sender commitment from the zone
    /// @param target The Tempo recipient
    /// @param amount Tokens to transfer from portal to target
    /// @param gasLimit Max gas for the callback
    /// @param data Calldata for the target
    function relayMessage(
        address _token,
        bytes32 senderTag,
        address target,
        uint128 amount,
        uint64 gasLimit,
        bytes calldata data
    )
        external
        onlyPortal
    {
        // Atomic transfer + callback via self-call
        this._executeRelay(_token, senderTag, target, amount, gasLimit, data);
    }

    /// @notice Internal function for atomic transfer + callback (called via self-call)
    /// @dev This function reverts if either the transfer or callback fails
    function _executeRelay(
        address _token,
        bytes32 senderTag,
        address target,
        uint128 amount,
        uint64 gasLimit,
        bytes calldata data
    )
        external
    {
        // Only callable via self-call from relayMessage
        if (msg.sender != address(this)) revert OnlyPortal();

        // Transfer tokens from portal to target
        if (!ITIP20(_token).transferFrom(portal, target, amount)) {
            revert TransferFailed();
        }

        // Call the receiver (includes token address for multi-asset awareness)
        bytes4 selector = IWithdrawalReceiver(target).onWithdrawalReceived{ gas: gasLimit }(
            senderTag, _token, amount, data
        );

        // Verify the callback returned the correct selector
        if (selector != IWithdrawalReceiver.onWithdrawalReceived.selector) {
            revert CallbackRejected();
        }
    }

}
