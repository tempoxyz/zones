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
/// @notice Zone-side system contract for processing deposit queue messages from Tempo
/// @dev Called by sequencer as a system transaction at the start of each block
contract ZoneInbox {
    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    /// @notice The Tempo portal address (for reference)
    address public immutable tempoPortal;

    /// @notice The gas token (TIP-20 at same address as Tempo)
    IZoneGasToken public immutable gasToken;

    /// @notice The sequencer address (only caller for processDepositQueue)
    address public immutable sequencer;

    /// @notice Last processed deposit queue hash (matches Tempo's processedDepositQueueHash after batch)
    bytes32 public processedDepositQueueHash;

    /*//////////////////////////////////////////////////////////////
                                EVENTS
    //////////////////////////////////////////////////////////////*/

    event DepositProcessed(
        bytes32 indexed depositHash,
        address indexed sender,
        address indexed to,
        uint128 amount,
        bytes32 memo
    );

    /*//////////////////////////////////////////////////////////////
                                ERRORS
    //////////////////////////////////////////////////////////////*/

    error InvalidDepositQueueChain();
    error OnlySequencer();

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(address _tempoPortal, address _gasToken, address _sequencer) {
        tempoPortal = _tempoPortal;
        gasToken = IZoneGasToken(_gasToken);
        sequencer = _sequencer;
    }

    /*//////////////////////////////////////////////////////////////
                     DEPOSIT QUEUE PROCESSING
    //////////////////////////////////////////////////////////////*/

    /// @notice Process deposits from Tempo. Called by sequencer as system transaction.
    /// @dev Deposits must be processed in order. The hash chain is verified.
    /// @param deposits Array of deposits to process (oldest first).
    /// @param expectedHash The expected hash after processing all deposits.
    function processDepositQueue(
        Deposit[] calldata deposits,
        bytes32 expectedHash
    ) external {
        if (msg.sender != sequencer) revert OnlySequencer();

        bytes32 currentHash = processedDepositQueueHash;

        for (uint256 i = 0; i < deposits.length; i++) {
            Deposit calldata depositData = deposits[i];

            // Advance the hash chain
            // Tempo builds: newHash = keccak256(abi.encode(deposit, prevHash))
            currentHash = keccak256(abi.encode(depositData, currentHash));

            // Mint gas tokens to the recipient
            gasToken.mint(depositData.to, depositData.amount);

            emit DepositProcessed(
                currentHash,
                depositData.sender,
                depositData.to,
                depositData.amount,
                depositData.memo
            );
        }

        // Verify we reached the expected hash
        if (currentHash != expectedHash) revert InvalidDepositQueueChain();

        processedDepositQueueHash = currentHash;
    }
}
