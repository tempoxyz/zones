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

        struct PendingWithdrawal {
            address token;
            address sender;
            bytes32 txHash;
            address to;
            uint128 amount;
            uint128 fee;
            bytes32 memo;
            uint64 gasLimit;
            address fallbackRecipient;
            bytes callbackData;
            bytes revealTo;
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

        event TempoGasRateUpdated(uint128 tempoGasRate);

        event MaxWithdrawalsPerBlockUpdated(uint256 maxWithdrawalsPerBlock);

        // -- Errors --

        error OnlySequencer();
        error GasLimitTooHigh();
        error OnlyZoneInbox();
        error InvalidWithdrawalCount(uint256 actual, uint256 expected);
        error InvalidEncryptedSenderCount(uint256 actual, uint256 expected);
        error InvalidEncryptedSenderLength(uint256 actual, uint256 expected);
        error InvalidFallbackRecipient();
        error CallbackDataTooLarge();
        error GasFeeRateTooHigh();
        error TransferFailed();
        error InvalidBlockNumber();
        error TooManyWithdrawalsThisBlock();
        error InvalidRevealTo();
        error InvalidCurrentTxHash();
        error TokenNotEnabled();

        // -- View functions --

        function config() external view returns (address);
        function tempoGasRate() external view returns (uint128);
        function maxWithdrawalsPerBlock() external view returns (uint256);
        function lastBatch() external view returns (LastBatch memory);
        function withdrawalBatchIndex() external view returns (uint64);
        function lastFinalizedTimestamp() external view returns (uint64);
        function nextWithdrawalIndex() external view returns (uint64);
        function pendingWithdrawalsCount() external view returns (uint256);
        function getPendingWithdrawals() external view returns (PendingWithdrawal[] memory);
        function calculateWithdrawalFee(uint64 gasLimit) external view returns (uint128 fee);
        function MAX_CALLBACK_DATA_SIZE() external view returns (uint256);
        function MAX_WITHDRAWAL_GAS_LIMIT() external view returns (uint64);
        function MAX_GAS_FEE_RATE() external view returns (uint128);
        function WITHDRAWAL_BASE_GAS() external view returns (uint64);
        function REVEAL_TO_KEY_LENGTH() external view returns (uint256);
        function AUTHENTICATED_WITHDRAWAL_CIPHERTEXT_LENGTH() external view returns (uint256);

        // -- State-changing functions --

        function setTempoGasRate(uint128 _tempoGasRate) external;
        function setMaxWithdrawalsPerBlock(uint256 _maxWithdrawalsPerBlock) external;
        function requestWithdrawal(
            address token,
            address to,
            uint128 amount,
            bytes32 memo,
            uint64 gasLimit,
            address fallbackRecipient,
            bytes calldata data
        ) external;
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
        function enqueueDepositBounceBack(
            address token,
            uint128 amount,
            address bouncebackRecipient
        ) external;
        function finalizeWithdrawalBatch(uint256 count, uint64 blockNumber, bytes[] calldata encryptedSenders) external returns (bytes32 withdrawalQueueHash);
    }
}
