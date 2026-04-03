// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    IZoneConfig,
    IZoneOutbox,
    IZoneToken,
    IZoneTxContext,
    LastBatch,
    PendingWithdrawal,
    Withdrawal,
    ZONE_TX_CONTEXT
} from "./IZone.sol";

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

    /// @notice Length of a compressed secp256k1 public key
    uint256 public constant REVEAL_TO_KEY_LENGTH = 33;

    /// @notice Length of `encryptedSender` when selective reveal is enabled
    /// @dev compressed ephemeral pubkey (33) || nonce (12) || ciphertext (52) || tag (16)
    uint256 public constant AUTHENTICATED_WITHDRAWAL_CIPHERTEXT_LENGTH = 113;

    /// @notice secp256k1 field prime
    uint256 internal constant SECP256K1_P =
        0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC2F;

    /// @notice (SECP256K1_P - 1) / 2 for Euler's criterion
    uint256 internal constant SECP256K1_HALF_PM1 =
        0x7FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF7FFFFE17;

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
    PendingWithdrawal[] internal _pendingWithdrawals;
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
    error InvalidRevealTo();
    error InvalidCurrentTxHash();
    error InvalidEncryptedSenderCount(uint256 actual, uint256 expected);
    error InvalidEncryptedSenderLength(uint256 actual, uint256 expected);

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
        _requestWithdrawal(token, to, amount, memo, gasLimit, fallbackRecipient, data, "");
    }

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
    /// @param revealTo Optional compressed secp256k1 pubkey for encrypted sender reveal
    function requestWithdrawal(
        address token,
        address to,
        uint128 amount,
        bytes32 memo,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes calldata data,
        bytes calldata revealTo
    )
        external
    {
        _requestWithdrawal(token, to, amount, memo, gasLimit, fallbackRecipient, data, revealTo);
    }

    function _requestWithdrawal(
        address token,
        address to,
        uint128 amount,
        bytes32 memo,
        uint64 gasLimit,
        address fallbackRecipient,
        bytes memory data,
        bytes memory revealTo
    )
        internal
    {
        // Always require a valid fallback recipient
        if (fallbackRecipient == address(0)) {
            revert InvalidFallbackRecipient();
        }

        // Limit callback data size to prevent storage bloat and hash computation abuse
        if (data.length > MAX_CALLBACK_DATA_SIZE) {
            revert CallbackDataTooLarge();
        }

        _validateRevealTo(revealTo);

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
        bytes32 txHash = IZoneTxContext(ZONE_TX_CONTEXT).currentTxHash();
        if (txHash == bytes32(0)) revert InvalidCurrentTxHash();

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
            PendingWithdrawal({
                token: token,
                sender: msg.sender,
                txHash: txHash,
                to: to,
                amount: amount,
                fee: fee,
                memo: memo,
                gasLimit: gasLimit,
                fallbackRecipient: fallbackRecipient,
                callbackData: data,
                revealTo: revealTo
            })
        );

        // Emit event for observability
        uint64 index = nextWithdrawalIndex++;

        emit WithdrawalRequested(
            index,
            msg.sender,
            token,
            to,
            amount,
            fee,
            memo,
            gasLimit,
            fallbackRecipient,
            data,
            revealTo
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
    /// @param encryptedSenders One ciphertext per finalized withdrawal (empty for plaintext withdrawals)
    /// @return withdrawalQueueHash The hash chain (0 if no withdrawals)
    function finalizeWithdrawalBatch(
        uint256 count,
        uint64 blockNumber,
        bytes[] calldata encryptedSenders
    )
        external
        returns (bytes32 withdrawalQueueHash)
    {
        return _finalizeWithdrawalBatch(count, blockNumber, encryptedSenders);
    }

    function _finalizeWithdrawalBatch(
        uint256 count,
        uint64 blockNumber,
        bytes[] memory encryptedSenders
    )
        internal
        returns (bytes32 withdrawalQueueHash)
    {
        if (msg.sender != address(0) && msg.sender != config.sequencer()) revert OnlySequencer();
        if (blockNumber != uint64(block.number)) revert InvalidBlockNumber();

        uint256 pending = _pendingWithdrawals.length - _pendingWithdrawalsHead;

        // Clamp to actual pending count
        if (count > pending) {
            count = pending;
        }
        if (encryptedSenders.length != count) {
            revert InvalidEncryptedSenderCount(encryptedSenders.length, count);
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
                PendingWithdrawal memory pendingWithdrawal = _pendingWithdrawals[index];
                bytes memory encryptedSender = encryptedSenders[index - start];
                _validateEncryptedSender(pendingWithdrawal.revealTo, encryptedSender);

                Withdrawal memory w = Withdrawal({
                    token: pendingWithdrawal.token,
                    senderTag: keccak256(
                        abi.encodePacked(pendingWithdrawal.sender, pendingWithdrawal.txHash)
                    ),
                    to: pendingWithdrawal.to,
                    amount: pendingWithdrawal.amount,
                    fee: pendingWithdrawal.fee,
                    memo: pendingWithdrawal.memo,
                    gasLimit: pendingWithdrawal.gasLimit,
                    fallbackRecipient: pendingWithdrawal.fallbackRecipient,
                    callbackData: pendingWithdrawal.callbackData,
                    encryptedSender: encryptedSender
                });
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

    /// @notice Enqueue a bounce-back withdrawal for a failed deposit
    /// @dev Only callable by the ZoneInbox during advanceTempo. Creates a zero-fee,
    ///      zero-callback withdrawal that returns escrowed funds to the depositor's
    ///      bouncebackRecipient on L1.
    function enqueueDepositBounceBack(
        address token,
        uint128 amount,
        address bouncebackRecipient
    )
        external
    {
        if (msg.sender != address(0) && msg.sender != config.sequencer()) revert OnlySequencer();

        _pendingWithdrawals.push(
            PendingWithdrawal({
                token: token,
                sender: address(0),
                txHash: bytes32(0),
                to: bouncebackRecipient,
                amount: amount,
                fee: 0,
                memo: bytes32(0),
                gasLimit: 0,
                fallbackRecipient: address(0),
                callbackData: "",
                revealTo: ""
            })
        );
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

    function _validateRevealTo(bytes memory revealTo) internal view {
        if (revealTo.length == 0) {
            return;
        }
        if (revealTo.length != REVEAL_TO_KEY_LENGTH) revert InvalidRevealTo();
        bytes1 prefix = revealTo[0];
        if (prefix != 0x02 && prefix != 0x03) revert InvalidRevealTo();

        bytes32 x;
        assembly {
            x := mload(add(revealTo, 33))
        }
        if (!_isValidSecp256k1X(x)) revert InvalidRevealTo();
    }

    function _validateEncryptedSender(
        bytes memory revealTo,
        bytes memory encryptedSender
    )
        internal
        pure
    {
        uint256 expectedLength =
            revealTo.length == 0 ? 0 : AUTHENTICATED_WITHDRAWAL_CIPHERTEXT_LENGTH;
        if (encryptedSender.length != expectedLength) {
            revert InvalidEncryptedSenderLength(encryptedSender.length, expectedLength);
        }
    }

    /// @notice Validate that an X coordinate corresponds to a valid secp256k1 point
    /// @dev Uses Euler's criterion via the MODEXP precompile (0x05):
    ///      x^3 + 7 is a quadratic residue mod p iff (x^3 + 7)^((p-1)/2) == 1 (mod p)
    function _isValidSecp256k1X(bytes32 x) internal view returns (bool) {
        uint256 px = uint256(x);
        if (px == 0 || px >= SECP256K1_P) return false;

        uint256 rhs = addmod(mulmod(mulmod(px, px, SECP256K1_P), px, SECP256K1_P), 7, SECP256K1_P);

        bytes memory input = abi.encodePacked(
            uint256(32), uint256(32), uint256(32), rhs, SECP256K1_HALF_PM1, SECP256K1_P
        );

        (bool success, bytes memory result) = address(0x05).staticcall(input);
        if (!success || result.length != 32) return false;

        return uint256(bytes32(result)) == 1;
    }

}
