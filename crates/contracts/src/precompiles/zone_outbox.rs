//! `ZoneOutbox` — deployed on the Zone L2.

pub use ZoneOutbox::LastBatch;

crate::sol! {
    #[derive(Debug)]
    contract ZoneOutbox {
        // -- Shared types --

        struct LastBatch {
            bytes32 withdrawalQueueHash;
            uint64 withdrawalBatchIndex;
        }

        // -- Events --

        event WithdrawalRequested(
            uint64 indexed withdrawalIndex,
            address indexed sender,
            address token,
            address to,
            uint128 amount,
            uint128 fee,
            bytes32 memo,
            uint64 gasLimit,
            address fallbackRecipient,
            bytes data,
            bytes revealTo
        );

        event BatchFinalized(bytes32 indexed withdrawalQueueHash, uint64 withdrawalBatchIndex);

        // -- Errors --

        error OnlySequencer();
        error GasLimitTooHigh();

        // -- View functions --

        function lastBatch() external view returns (LastBatch memory);
        function withdrawalBatchIndex() external view returns (uint64);
        function nextWithdrawalIndex() external view returns (uint64);
        function pendingWithdrawalsCount() external view returns (uint256);
        function calculateWithdrawalFee(uint64 gasLimit) external view returns (uint128 fee);
        function MAX_WITHDRAWAL_GAS_LIMIT() external view returns (uint64);

        // -- State-changing functions --

        function requestWithdrawal(
            address token,
            address to,
            uint128 amount,
            bytes32 memo,
            uint64 gasLimit,
            address fallbackRecipient,
            bytes calldata data,
            bytes calldata revealTo
        ) external;
        function finalizeWithdrawalBatch(uint256 count, uint64 blockNumber, bytes[] calldata encryptedSenders) external returns (bytes32 withdrawalQueueHash);
    }
}
