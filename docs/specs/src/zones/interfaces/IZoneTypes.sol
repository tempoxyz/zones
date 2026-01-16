// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/// @title Zone Types
/// @notice Common types used across Zone contracts
interface IZoneTypes {
    /// @notice Zone metadata stored in the registry
    struct ZoneInfo {
        uint64 zoneId;
        address portal;
        address gasToken;
        address sequencer;
        address verifier;
        bytes32 genesisStateRoot;
    }

    /// @notice Batch commitment for state transition
    struct BatchCommitment {
        bytes32 newProcessedDepositsHash;
        bytes32 newStateRoot;
    }

    /// @notice Deposit from Tempo into a zone (includes L1 block info)
    struct Deposit {
        bytes32 l1BlockHash;
        uint64 l1BlockNumber;
        uint64 l1Timestamp;
        address sender;
        address to;
        uint128 amount;
        bytes32 memo;
    }

    /// @notice Withdrawal from zone to Tempo
    struct Withdrawal {
        address sender;
        address to;
        uint128 amount;
        uint64 gasLimit;
        address fallbackRecipient;
        bytes data;
    }
}
