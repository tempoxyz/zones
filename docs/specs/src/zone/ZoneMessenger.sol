// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { ITIP20 } from "../interfaces/ITIP20.sol";
import { IZoneMessenger, IWithdrawalReceiver } from "./IZone.sol";

/// @title ZoneMessenger
/// @notice Per-zone messenger that handles withdrawal callbacks
/// @dev Deployed by ZoneFactory for each zone. The portal gives the messenger max approval
///      for the zone token. Withdrawal callbacks originate from this contract, not the portal.
contract ZoneMessenger is IZoneMessenger {
    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    /// @notice The zone's portal address
    address public immutable portal;

    /// @notice The zone token address
    address public immutable token;

    /// @notice The L2 sender during callback execution (transient)
    address internal _xDomainMessageSender;

    /*//////////////////////////////////////////////////////////////
                                ERRORS
    //////////////////////////////////////////////////////////////*/

    error OnlyPortal();
    error NotInCallback();
    error CallbackRejected();
    error TransferFailed();

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(address _portal, address _token) {
        portal = _portal;
        token = _token;
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

    /// @notice Returns the L2 sender during callback execution
    /// @dev Reverts if not in a callback context
    function xDomainMessageSender() external view returns (address) {
        if (_xDomainMessageSender == address(0)) revert NotInCallback();
        return _xDomainMessageSender;
    }

    /// @notice Relay a withdrawal message. Only callable by the portal.
    /// @dev Transfers tokens from portal to target via transferFrom, then executes callback.
    ///      If callback reverts, returns false (does not revert). The transfer is atomic
    ///      with the callback via a self-call pattern.
    /// @param sender The L2 origin address
    /// @param target The Tempo recipient
    /// @param amount Tokens to transfer from portal to target
    /// @param gasLimit Max gas for the callback
    /// @param data Calldata for the target
    function relayMessage(
        address sender,
        address target,
        uint128 amount,
        uint64 gasLimit,
        bytes calldata data
    ) external onlyPortal {
        // Atomic transfer + callback via self-call
        this._executeRelay(sender, target, amount, gasLimit, data);
    }

    /// @notice Internal function for atomic transfer + callback (called via self-call)
    /// @dev This function reverts if either the transfer or callback fails
    function _executeRelay(
        address sender,
        address target,
        uint128 amount,
        uint64 gasLimit,
        bytes calldata data
    ) external {
        // Only callable via self-call from relayMessage
        if (msg.sender != address(this)) revert OnlyPortal();

        // Transfer tokens from portal to target
        if (!ITIP20(token).transferFrom(portal, target, amount)) {
            revert TransferFailed();
        }

        // Set the L2 sender for the callback
        _xDomainMessageSender = sender;

        // Call the receiver
        bytes4 selector = IWithdrawalReceiver(target).onWithdrawalReceived{gas: gasLimit}(
            sender,
            amount,
            data
        );

        // Clear the L2 sender
        _xDomainMessageSender = address(0);

        // Verify the callback returned the correct selector
        if (selector != IWithdrawalReceiver.onWithdrawalReceived.selector) {
            revert CallbackRejected();
        }
    }
}
