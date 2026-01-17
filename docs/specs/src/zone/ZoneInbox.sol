// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { Deposit } from "./IZone.sol";

/// @title IZoneGasToken
/// @notice Interface for the zone's gas token (TIP-20 with mint/burn for system)
interface IZoneGasToken {
    function mint(address to, uint256 amount) external;
    function burn(address from, uint256 amount) external;
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
}

/// @title ZoneInbox
/// @notice Zone-side system contract for processing deposits from Tempo
/// @dev Called by sequencer as a system transaction at the start of each block
contract ZoneInbox {
    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    /// @notice The L1 portal address (for reference)
    address public immutable l1Portal;

    /// @notice The gas token (TIP-20 at same address as L1)
    IZoneGasToken public immutable gasToken;

    /// @notice The sequencer address (only caller for processDeposits)
    address public immutable sequencer;

    /// @notice Last processed deposit hash (matches L1's processedDepositsHash after batch)
    bytes32 public processedDepositsHash;

    /*//////////////////////////////////////////////////////////////
                                EVENTS
    //////////////////////////////////////////////////////////////*/

    event DepositProcessed(
        bytes32 indexed depositHash,
        address indexed sender,
        address indexed to,
        uint128 amount,
        bytes32 memo,
        bytes32 l1BlockHash,
        uint64 l1BlockNumber,
        uint64 l1Timestamp
    );

    /*//////////////////////////////////////////////////////////////
                                ERRORS
    //////////////////////////////////////////////////////////////*/

    error InvalidDepositChain();
    error OnlySequencer();

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(address _l1Portal, address _gasToken, address _sequencer) {
        l1Portal = _l1Portal;
        gasToken = IZoneGasToken(_gasToken);
        sequencer = _sequencer;
    }

    /*//////////////////////////////////////////////////////////////
                          DEPOSIT PROCESSING
    //////////////////////////////////////////////////////////////*/

    /// @notice Process deposits from Tempo. Called by sequencer as system transaction.
    /// @dev Deposits must be processed in order. The hash chain is verified.
    /// @param deposits Array of deposits to process (oldest first).
    /// @param expectedHash The expected hash after processing all deposits.
    function processDeposits(
        Deposit[] calldata deposits,
        bytes32 expectedHash
    ) external {
        if (msg.sender != sequencer) revert OnlySequencer();

        bytes32 currentHash = processedDepositsHash;

        for (uint256 i = 0; i < deposits.length; i++) {
            Deposit calldata d = deposits[i];

            // Mint gas tokens to the recipient
            gasToken.mint(d.to, d.amount);

            // Advance the hash chain
            // L1 builds: newHash = keccak256(abi.encode(deposit, prevHash))
            currentHash = keccak256(abi.encode(d, currentHash));

            emit DepositProcessed(
                currentHash,
                d.sender,
                d.to,
                d.amount,
                d.memo,
                d.l1BlockHash,
                d.l1BlockNumber,
                d.l1Timestamp
            );
        }

        // Verify we reached the expected hash
        if (currentHash != expectedHash) revert InvalidDepositChain();

        processedDepositsHash = currentHash;
    }
}
