// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

/// @notice Optional engine capability for accepting the venue shares it already uses as backing.
/// @dev The paired EarnVault passes the original caller as `from`; the engine pulls the shares directly
///      from that account and returns its measured custody increase. Engines whose venue shares are
///      not transferable or cannot be safely accepted should not implement this interface.
interface IEarnEngineInKindDeposit {
    function depositInKind(uint256 venueShares, address from) external returns (uint256 engineSharesReceived);
}
