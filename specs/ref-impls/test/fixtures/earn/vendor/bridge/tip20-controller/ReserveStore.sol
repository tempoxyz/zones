// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import { ITIP20 } from "../../../src/interfaces/ITIP20.sol";

/// @title ReserveStore
/// @notice A minimal contract to hold reserve ledger tokens for a specific stablecoin
/// @dev Deployed per stablecoin to keep ledger token reserves separate for reconciliation purposes.
///      Pre-approves the controller to transfer tokens on its behalf.
contract ReserveStore {
    /// @notice Error thrown when zero address is used
    error ZeroAddress();
    error ApprovalFailed();

    /// @notice The reserve ledger token (backing token)
    ITIP20 public immutable RESERVE_LEDGER;

    /// @notice The controller that can move tokens from this store
    address public immutable CONTROLLER;

    /// @notice The stablecoin this store backs
    ITIP20 public immutable STABLECOIN;

    /// @param reserveLedger The reserve ledger token address
    /// @param controller The controller contract address
    /// @param stablecoin The stablecoin this store backs
    constructor(address reserveLedger, address controller, address stablecoin) {
        require(reserveLedger != address(0), ZeroAddress());
        require(controller != address(0), ZeroAddress());
        require(stablecoin != address(0), ZeroAddress());

        RESERVE_LEDGER = ITIP20(reserveLedger);
        CONTROLLER = controller;
        STABLECOIN = ITIP20(stablecoin);

        if (!RESERVE_LEDGER.approve(controller, type(uint256).max)) revert ApprovalFailed();
    }
}
