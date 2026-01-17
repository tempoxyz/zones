// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { Deposit, DepositQueueMessage, DepositQueueMessageKind, L1Sync } from "./IZone.sol";

/// @title IZoneGasToken
/// @notice Interface for the zone's gas token (TIP-20 with mint/burn for system)
interface IZoneGasToken {
    function mint(address to, uint256 amount) external;
    function burn(address from, uint256 amount) external;
    function transfer(address to, uint256 amount) external returns (bool);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function balanceOf(address account) external view returns (uint256);
}

/// @title ZoneDepositQueue
/// @notice Zone-side system contract for processing deposit queue messages from Tempo
/// @dev Called by sequencer as a system transaction at the start of each block
contract ZoneDepositQueue {
    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    /// @notice The L1 portal address (for reference)
    address public immutable l1Portal;

    /// @notice The gas token (TIP-20 at same address as L1)
    IZoneGasToken public immutable gasToken;

    /// @notice The sequencer address (only caller for processDepositQueue)
    address public immutable sequencer;

    /// @notice Last processed deposit queue hash (matches L1's processedDepositQueueHash after batch)
    bytes32 public processedDepositQueueHash;

    /// @notice Latest L1 head observed from deposit queue messages
    bytes32 public l1ParentBlockHash;
    uint64 public l1BlockNumber;
    uint64 public l1Timestamp;

    /*//////////////////////////////////////////////////////////////
                                EVENTS
    //////////////////////////////////////////////////////////////*/

    event DepositProcessed(
        bytes32 indexed depositHash,
        address indexed sender,
        address indexed to,
        uint128 amount,
        bytes32 memo,
        bytes32 l1ParentBlockHash,
        uint64 l1BlockNumber,
        uint64 l1Timestamp
    );

    event L1SyncProcessed(
        bytes32 indexed messageHash,
        bytes32 l1ParentBlockHash,
        uint64 l1BlockNumber,
        uint64 l1Timestamp
    );

    /*//////////////////////////////////////////////////////////////
                                ERRORS
    //////////////////////////////////////////////////////////////*/

    error InvalidDepositQueueChain();
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
                     DEPOSIT QUEUE PROCESSING
    //////////////////////////////////////////////////////////////*/

    /// @notice Process deposit queue messages from Tempo. Called by sequencer as system transaction.
    /// @dev Messages must be processed in order. The hash chain is verified.
    /// @param messages Array of deposit queue messages to process (oldest first).
    /// @param expectedHash The expected hash after processing all messages.
    function processDepositQueue(
        DepositQueueMessage[] calldata messages,
        bytes32 expectedHash
    ) external {
        if (msg.sender != sequencer) revert OnlySequencer();

        bytes32 currentHash = processedDepositQueueHash;

        for (uint256 i = 0; i < messages.length; i++) {
            DepositQueueMessage calldata m = messages[i];

            // Advance the hash chain
            // L1 builds: newHash = keccak256(abi.encode(message, prevHash))
            currentHash = keccak256(abi.encode(m, currentHash));

            if (m.kind == DepositQueueMessageKind.Deposit) {
                Deposit memory d = abi.decode(m.data, (Deposit));

                // Mint gas tokens to the recipient
                gasToken.mint(d.to, d.amount);

                _updateL1Head(d.l1ParentBlockHash, d.l1BlockNumber, d.l1Timestamp);

                emit DepositProcessed(
                    currentHash,
                    d.sender,
                    d.to,
                    d.amount,
                    d.memo,
                    d.l1ParentBlockHash,
                    d.l1BlockNumber,
                    d.l1Timestamp
                );
            } else if (m.kind == DepositQueueMessageKind.L1Sync) {
                L1Sync memory sync = abi.decode(m.data, (L1Sync));

                _updateL1Head(sync.l1ParentBlockHash, sync.l1BlockNumber, sync.l1Timestamp);

                emit L1SyncProcessed(
                    currentHash,
                    sync.l1ParentBlockHash,
                    sync.l1BlockNumber,
                    sync.l1Timestamp
                );
            } else {
                revert InvalidDepositQueueChain();
            }
        }

        // Verify we reached the expected hash
        if (currentHash != expectedHash) revert InvalidDepositQueueChain();

        processedDepositQueueHash = currentHash;
    }

    function _updateL1Head(bytes32 blockHash, uint64 blockNumber, uint64 timestamp) internal {
        l1ParentBlockHash = blockHash;
        l1BlockNumber = blockNumber;
        l1Timestamp = timestamp;
    }
}
