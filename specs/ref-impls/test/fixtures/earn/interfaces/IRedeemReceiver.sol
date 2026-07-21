// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// @notice Optional callback surface a `requestRedeemAsync` receiver may advertise. `VaultAdapter`
///         invokes these AFTER moving value to the receiver, inside a try/catch, so an EOA receiver
///         simply keeps the transferred tokens and a contract receiver gets a completion callback.
/// @dev A cancellation returns the current-value EarnToken minted by the adapter to the SAME stored
///      receiver the finalize path would have used. The amount may differ from the tokens burned at
///      request time because the queued position can gain or lose value.
interface IRedeemReceiver {

    function onRedeemFinalized(bytes32 requestId, address asset, uint256 amount) external;
    function onRedeemCancelled(bytes32 requestId, address shareToken, uint256 shares) external;

}
