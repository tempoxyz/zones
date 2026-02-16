// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { IZoneConfig, IZoneOutbox, IZoneToken, LastBatch, Withdrawal } from "./IZone.sol";

import { EMPTY_SENTINEL } from "./WithdrawalQueueLib.sol";

/// @title ZoneOutbox
/// @notice Zone-side predeploy for requesting withdrawals back to Tempo
/// @dev Burns zone tokens and stores pending withdrawals. Sequencer calls finalizeWithdrawalBatch()
///      at the end of a block to construct withdrawal queue hash on-chain.
contract ZoneOutbox is IZoneOutbox {

    /*//////////////////////////////////////////////////////////////
                               CONSTANTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Maximum size of callback data in bytes
    /// @dev Limits storage costs and hash computation overhead
    uint256 public constant MAX_CALLBACK_DATA_SIZE = 1024;

    /// @notice Maximum gas fee rate ($1 per gas for 6-decimal stablecoins)
    /// @dev Ensures gasLimit (uint64) * gasFeeRate fits in uint128 without overflow.
    ///      Any practical fee rate would be orders of magnitude lower.
    uint128 public constant MAX_GAS_FEE_RATE = 1e18;

    /// @notice Base gas cost for processing a withdrawal on Tempo (excluding callback)
    /// @dev Covers processWithdrawal overhead: queue dequeue, transfer, event emission
    uint64 public constant WITHDRAWAL_BASE_GAS = 50_000;

    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    /// @notice Zone configuration (reads sequencer from L1)
    IZoneConfig public immutable config;

    /// @notice Tempo gas rate (zone token units per gas unit on Tempo)
    /// @dev Sequencer publishes this rate and takes the risk on Tempo gas price changes.
    ///      Fee = (WITHDRAWAL_BASE_GAS + gasLimit) * tempoGasRate
    uint128 public tempoGasRate;

    /// @notice Next withdrawal index (monotonically increasing)
    uint64 public nextWithdrawalIndex;

    /// @notice Current withdrawal batch index (monotonically increasing)
    uint64 public withdrawalBatchIndex;

    /// @notice Last finalized batch parameters (for proof access via state root)
    /// @dev Written on each finalizeWithdrawalBatch() call so proofs can read from state
    ///      instead of parsing event logs
    LastBatch internal _lastBatch;

    /// @notice Pending withdrawals waiting to be batched
    Withdrawal[] internal _pendingWithdrawals;
    uint256 internal _pendingWithdrawalsHead;

    /// @notice Maximum number of withdrawal requests allowed per zone block (0 = unlimited)
    /// @dev Sequencer-configurable cap to prevent DoS via mass withdrawal requests.
    ///      This limits the number of requestWithdrawal() calls per block, complementing
    ///      the gas fee mechanism which already provides economic rate-limiting.
    uint256 public maxWithdrawalsPerBlock;

    /// @notice Number of withdrawal requests in the current block
    uint256 internal _withdrawalsThisBlock;

    /// @notice Block number for tracking per-block withdrawal count
    uint256 internal _currentBlockNumber;

    /*//////////////////////////////////////////////////////////////
                                ERRORS
    //////////////////////////////////////////////////////////////*/

    error InvalidFallbackRecipient();
    error CallbackDataTooLarge();
    error GasFeeRateTooHigh();
    error TransferFailed();
    error OnlySequencer();
    error InvalidBlockNumber();
    error TooManyWithdrawalsThisBlock();

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(address _config) {
        config = IZoneConfig(_config);
    }

    /*//////////////////////////////////////////////////////////////
                            FEE CONFIGURATION
    //////////////////////////////////////////////////////////////*/

    /// @notice Set Tempo gas rate. Only callable by sequencer.
    /// @dev Sequencer publishes this rate and takes the risk on Tempo gas price fluctuations.
    ///      If actual Tempo gas is higher, sequencer covers the difference.
    ///      If actual Tempo gas is lower, sequencer keeps the surplus.
    /// @param _tempoGasRate Zone token units per gas unit on Tempo
    function setTempoGasRate(uint128 _tempoGasRate) external {
        if (msg.sender != address(0) && msg.sender != config.sequencer()) revert OnlySequencer();
        if (_tempoGasRate > MAX_GAS_FEE_RATE) revert GasFeeRateTooHigh();
        tempoGasRate = _tempoGasRate;
        emit TempoGasRateUpdated(_tempoGasRate);
    }

    /// @notice Set maximum withdrawal requests per zone block. Only callable by sequencer.
    /// @dev Set to 0 for unlimited. Provides rate-limiting in addition to the gas fee mechanism.
    /// @param _maxWithdrawalsPerBlock The maximum number of requestWithdrawal() calls per block
    function setMaxWithdrawalsPerBlock(uint256 _maxWithdrawalsPerBlock) external {
        if (msg.sender != address(0) && msg.sender != config.sequencer()) revert OnlySequencer();
        maxWithdrawalsPerBlock = _maxWithdrawalsPerBlock;
        emit MaxWithdrawalsPerBlockUpdated(_maxWithdrawalsPerBlock);
    }

    /// @notice Calculate the fee for a withdrawal with the given gasLimit
    /// @dev Fee = (WITHDRAWAL_BASE_GAS + gasLimit) * tempoGasRate. User must estimate total gas needed.
    /// @param gasLimit Total gas limit (must cover processWithdrawal + any callback)
    /// @return fee The total fee in zone token units
    function calculateWithdrawalFee(uint64 gasLimit) public view returns (uint128 fee) {
        fee = uint128(WITHDRAWAL_BASE_GAS + gasLimit) * tempoGasRate;
    }

    /*//////////////////////////////////////////////////////////////
                          WITHDRAWAL REQUESTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Request a withdrawal from the zone back to Tempo
    /// @dev Caller must have approved the outbox to spend `amount + fee` of the specified token.
    ///      The outbox burns the tokens and stores the withdrawal. The sequencer
    ///      calls finalizeWithdrawalBatch() to construct the withdrawal queue hash.
    ///      The token must be enabled on the portal. Withdrawals can never be disabled
    ///      for an enabled token (non-custodial guarantee).
    /// @param token The TIP-20 token to withdraw
    /// @param to The Tempo recipient address
    /// @param amount Amount to send to recipient (fee is additional)
    /// @param memo User-provided context (e.g., payment reference)
    /// @param gasLimit Gas limit for IWithdrawalReceiver callback (0 = no callback)
    /// @param fallbackRecipient Zone address for bounce-back if callback fails
    /// @param data Calldata for IWithdrawalReceiver callback
    function requestWithdrawal(
        address token,
        address to,
        uint128 amount,
        bytes32 memo,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes calldata data
    )
        external
    {
        // Always require a valid fallback recipient
        if (fallbackRecipient == address(0)) {
            revert InvalidFallbackRecipient();
        }

        // Limit callback data size to prevent storage bloat and hash computation abuse
        if (data.length > MAX_CALLBACK_DATA_SIZE) {
            revert CallbackDataTooLarge();
        }

        // Enforce per-block withdrawal cap (0 = unlimited)
        if (maxWithdrawalsPerBlock > 0) {
            if (block.number != _currentBlockNumber) {
                _currentBlockNumber = block.number;
                _withdrawalsThisBlock = 0;
            }
            if (_withdrawalsThisBlock >= maxWithdrawalsPerBlock) {
                revert TooManyWithdrawalsThisBlock();
            }
            _withdrawalsThisBlock++;
        }

        // Calculate processing fee (locked in at request time)
        // Fee is paid in the same token being withdrawn
        uint128 fee = calculateWithdrawalFee(gasLimit);
        uint128 totalBurn = amount + fee;

        // Transfer tokens from sender to this contract, then burn
        // (Using transferFrom so user must approve first)
        IZoneToken zoneToken = IZoneToken(token);
        if (!zoneToken.transferFrom(msg.sender, address(this), totalBurn)) {
            revert TransferFailed();
        }

        // Burn the tokens (they'll be released on Tempo when withdrawal is processed)
        // Amount goes to recipient, fee goes to sequencer
        zoneToken.burn(totalBurn);

        // Store withdrawal in pending array
        _pendingWithdrawals.push(
            Withdrawal({
                token: token,
                sender: msg.sender,
                to: to,
                amount: amount,
                fee: fee,
                memo: memo,
                gasLimit: gasLimit,
                fallbackRecipient: fallbackRecipient,
                callbackData: data
            })
        );

        // Emit event for observability
        uint64 index = nextWithdrawalIndex++;

        emit WithdrawalRequested(
            index, msg.sender, token, to, amount, fee, memo, gasLimit, fallbackRecipient, data
        );
    }

    /*//////////////////////////////////////////////////////////////
                              BATCH OPERATIONS
    //////////////////////////////////////////////////////////////*/

    /// @notice Finalize the batch at end of block - build withdrawal hash and emit proof inputs
    /// @dev Only callable by sequencer at the end of a block.
    ///      The proof enforces that this is the last call in the block and that a batch
    ///      ends with exactly one finalizeWithdrawalBatch call (use count = 0 if no withdrawals).
    ///      Protocol and proof enforce this runs at the end of the final block in the batch.
    ///      Emits BatchFinalized for observability (proof reads from state).
    /// @param count Max number of withdrawals to process (avoids unbounded loops)
    /// @return withdrawalQueueHash The hash chain (0 if no withdrawals)
    function finalizeWithdrawalBatch(
        uint256 count,
        uint64 blockNumber
    )
        external
        returns (bytes32 withdrawalQueueHash)
    {
        if (msg.sender != address(0) && msg.sender != config.sequencer()) revert OnlySequencer();
        if (blockNumber != uint64(block.number)) revert InvalidBlockNumber();

        uint256 pending = _pendingWithdrawals.length - _pendingWithdrawalsHead;

        // Clamp to actual pending count
        if (count > pending) {
            count = pending;
        }

        // Build hash chain in reverse order (newest to oldest)
        // So oldest ends up outermost, matching Tempo expectations.
        // Process the oldest withdrawals first (FIFO).
        if (count > 0) {
            withdrawalQueueHash = EMPTY_SENTINEL;

            uint256 start = _pendingWithdrawalsHead;
            uint256 end = start + count;

            for (uint256 i = end; i > start;) {
                uint256 index = i - 1;
                Withdrawal memory w = _pendingWithdrawals[index];
                withdrawalQueueHash = keccak256(abi.encode(w, withdrawalQueueHash));
                delete _pendingWithdrawals[index];
                unchecked {
                    i--;
                }
            }

            _pendingWithdrawalsHead = end;

            if (_pendingWithdrawalsHead == _pendingWithdrawals.length) {
                delete _pendingWithdrawals;
                _pendingWithdrawalsHead = 0;
            }
        }

        // Increment withdrawal batch index (matches Tempo portal's next expected withdrawal batch index)
        withdrawalBatchIndex += 1;
        uint64 currentWithdrawalBatchIndex = withdrawalBatchIndex;

        // Write withdrawal batch parameters to state (for proof access via state root)
        _lastBatch = LastBatch({
            withdrawalQueueHash: withdrawalQueueHash,
            withdrawalBatchIndex: currentWithdrawalBatchIndex
        });

        // Emit event for observability (proof reads from state, not events)
        emit BatchFinalized(withdrawalQueueHash, currentWithdrawalBatchIndex);
    }

    /// @notice Number of pending withdrawals
    function pendingWithdrawalsCount() external view returns (uint256) {
        if (_pendingWithdrawalsHead >= _pendingWithdrawals.length) {
            return 0;
        }
        return _pendingWithdrawals.length - _pendingWithdrawalsHead;
    }

    /// @notice Last finalized batch parameters (for proof access via state root)
    function lastBatch() external view returns (LastBatch memory) {
        return _lastBatch;
    }

}
