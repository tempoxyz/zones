// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/// @title MockTempoState
/// @notice Mock TempoState for testing ZoneInbox
/// @dev Allows setting storage slot values and simulates finalizeTempo
contract MockTempoState {

    address public immutable sequencer;

    // Core fields (matching real TempoState)
    bytes32 public tempoBlockHash;
    uint64 public generalGasLimit;
    uint64 public sharedGasLimit;
    bytes32 public tempoParentHash;
    address public tempoBeneficiary;
    bytes32 public tempoStateRoot;
    bytes32 public tempoTransactionsRoot;
    bytes32 public tempoReceiptsRoot;
    uint64 public tempoBlockNumber;
    uint64 public tempoGasLimit;
    uint64 public tempoGasUsed;
    uint64 public tempoTimestamp;
    uint64 public tempoTimestampMillis;
    bytes32 public tempoPrevRandao;

    /// @notice Mock storage values for readTempoStorageSlot
    mapping(address => mapping(bytes32 => bytes32)) public mockStorageValues;

    error OnlySequencer();

    constructor(
        address _sequencer,
        bytes32 _genesisTempoBlockHash,
        uint64 _genesisTempoBlockNumber
    ) {
        sequencer = _sequencer;
        tempoBlockHash = _genesisTempoBlockHash;
        tempoBlockNumber = _genesisTempoBlockNumber;
    }

    /// @notice Set a mock storage value for readTempoStorageSlot
    function setMockStorageValue(address account, bytes32 slot, bytes32 value) external {
        mockStorageValues[account][slot] = value;
    }

    /// @notice Mock finalizeTempo - just advances block number
    /// @dev No sequencer check here - ZoneInbox already validates the caller
    function finalizeTempo(bytes calldata /* header */ ) external {
        tempoBlockNumber++;
        tempoBlockHash = keccak256(abi.encode(tempoBlockHash, tempoBlockNumber));
    }

    /// @notice Mock readTempoStorageSlot - returns preset values
    function readTempoStorageSlot(address account, bytes32 slot) external view returns (bytes32) {
        return mockStorageValues[account][slot];
    }

}
