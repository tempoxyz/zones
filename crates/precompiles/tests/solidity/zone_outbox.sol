// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

struct PendingWithdrawal {
    address token;
    address to;
    uint128 amount;
    bytes32 memo;
    uint64 gasLimit;
    address fallbackRecipient;
    bytes callbackData;
    bytes32 txHash;
    address feePayer;
}

/// Storage-only schema for the native ZoneOutbox precompile.
contract ZoneOutboxStorage {
    uint128 public tempoGasRate;
    uint64 public nextWithdrawalIndex;
    bytes32 internal _withdrawalQueueHash;
    uint64 internal _withdrawalBatchIndex;
    uint32 public maxWithdrawalsPerBlock;
    uint32 internal _withdrawalsThisBlock;
    uint64 internal _currentBlockNumber;
    uint64 public lastFinalizedTimestamp;
    PendingWithdrawal[] internal _pendingWithdrawals;
    uint64 public lastFallbackNonce;
    mapping(uint64 fallbackNonce => address zoneFallbackRecipient) internal _zoneFallbackRecipients;
}
