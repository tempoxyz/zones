// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/// @title Zone Types
/// @notice Common types used across Zone contracts
interface IZoneTypes {
    /// @notice Exit intent types for bridging out of a zone
    enum ExitKind {
        Transfer,
        Swap,
        SwapAndDeposit
    }

    /// @notice Zone metadata stored in the registry
    struct ZoneInfo {
        uint64 zoneId;
        address portal;
        address gasToken;
        address sequencer;
        address verifier;
        bytes32 genesisStateRoot;
    }

    /// @notice Batch commitment posted by the sequencer
    struct BatchCommitment {
        uint64 zoneId;
        uint64 batchIndex;
        bytes32 prevStateRoot;
        bytes32 newStateRoot;
        uint64 depositIndex;
        bytes32 exitRoot;
        bytes32 batchHash;
    }

    /// @notice Deposit from Tempo into a zone
    struct Deposit {
        address sender;
        address to;
        uint256 amount;
        bytes32 memo;
    }

    /// @notice Transfer exit: release gas token to a Tempo recipient
    struct TransferExit {
        address recipient;
        uint128 amount;
    }

    /// @notice Swap exit: release gas token, swap on DEX, send output to recipient
    struct SwapExit {
        address recipient;
        address tokenOut;
        uint128 amountIn;
        uint128 minAmountOut;
    }

    /// @notice Swap and deposit exit: swap then deposit into another zone
    struct SwapAndDepositExit {
        address tokenOut;
        uint128 amountIn;
        uint128 minAmountOut;
        uint64 destinationZoneId;
        address destinationRecipient;
    }

    /// @notice Full exit intent with all possible exit data
    struct ExitIntent {
        ExitKind kind;
        uint64 exitIndex;
        address sender;
        TransferExit transfer;
        SwapExit swap;
        SwapAndDepositExit swapAndDeposit;
        bytes32 memo;
    }

    /// @notice Merkle proof for exit finalization
    struct ExitProof {
        uint64 batchIndex;
        bytes32[] merkleProof;
    }
}
