// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

import { IERC165 } from "openzeppelin-contracts/utils/introspection/IERC165.sol";

/**
 * @title ITokenHandler
 * @author Bridge
 * @notice Interface for token handlers that manage deposits and withdrawals in the DirectSwap
 * protocol
 * @dev Implementations handle the actual token transfers, minting, or burning based on the token
 * type
 */
interface ITokenHandler is IERC165 {
    /**
     * @notice Emitted when tokens are deposited through the handler
     * @param sender Address that initiated the deposit (typically DirectSwap contract)
     * @param token Address of the token being deposited
     * @param destination Address where tokens are sent or credited
     * @param amount Amount of tokens deposited
     */
    event Deposited(address indexed sender, address indexed token, address indexed destination, uint256 amount);

    /**
     * @notice Emitted when tokens are withdrawn through the handler
     * @param sender Address that initiated the withdrawal (typically DirectSwap contract)
     * @param token Address of the token being withdrawn
     * @param source Address from which tokens are sourced (address(0) for minting)
     * @param amount Amount of tokens withdrawn
     */
    event Withdrawn(address indexed sender, address indexed token, address indexed source, uint256 amount);

    /**
     * @notice Deposits tokens into the handler
     * @dev Called by DirectSwap to handle the input side of a swap
     * @param _token Address of the token to deposit
     * @param _amount Amount of tokens to deposit
     */
    function deposit(address _token, uint256 _amount) external;

    /**
     * @notice Withdraws tokens from the handler
     * @dev Called by DirectSwap to handle the output side of a swap
     * @param _token Address of the token to withdraw
     * @param _amount Amount of tokens to withdraw
     */
    function withdraw(address _token, uint256 _amount) external;
}
