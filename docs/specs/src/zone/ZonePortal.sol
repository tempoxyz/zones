// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { ITIP20 } from "../interfaces/ITIP20.sol";
import {
    IZonePortal,
    IZoneMessenger,
    IVerifier,
    Deposit,
    Withdrawal,
    BlockTransition,
    DepositQueueTransition,
    WithdrawalQueueTransition
} from "./IZone.sol";
import { DepositQueueLib } from "./DepositQueueLib.sol";
import { WithdrawalQueue, WithdrawalQueueLib } from "./WithdrawalQueueLib.sol";
import { BLOCKHASH_HISTORY, IBlockHashHistory } from "./BlockHashHistory.sol";

/// @title ZonePortal
/// @notice Per-zone portal that escrows gas tokens on Tempo and manages deposits/withdrawals
contract ZonePortal is IZonePortal {
    using WithdrawalQueueLib for WithdrawalQueue;

    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    uint64 public immutable zoneId;
    address public immutable token;
    address public immutable messenger;
    address public immutable sequencer;
    address public immutable verifier;
    uint64 public immutable genesisTempoBlockNumber;

    bytes32 public sequencerPubkey;
    uint64 public withdrawalBatchIndex;
    bytes32 public blockHash;

    /// @notice Current deposit queue hash (where new deposits land)
    bytes32 public currentDepositQueueHash;

    /// @notice Last Tempo block number the zone has synced to
    uint64 public lastSyncedTempoBlockNumber;

    /// @notice Withdrawal queue (zone→Tempo): unbounded buffer
    WithdrawalQueue internal _withdrawalQueue;

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(
        uint64 _zoneId,
        address _token,
        address _messenger,
        address _sequencer,
        address _verifier,
        bytes32 _genesisBlockHash,
        uint64 _genesisTempoBlockNumber
    ) {
        zoneId = _zoneId;
        token = _token;
        messenger = _messenger;
        sequencer = _sequencer;
        verifier = _verifier;
        blockHash = _genesisBlockHash;
        genesisTempoBlockNumber = _genesisTempoBlockNumber;

        // Give messenger max approval for the gas token
        ITIP20(_token).approve(_messenger, type(uint256).max);
    }

    /*//////////////////////////////////////////////////////////////
                               MODIFIERS
    //////////////////////////////////////////////////////////////*/

    modifier onlySequencer() {
        if (msg.sender != sequencer) revert NotSequencer();
        _;
    }

    /*//////////////////////////////////////////////////////////////
                           SEQUENCER CONFIG
    //////////////////////////////////////////////////////////////*/

    function setSequencerPubkey(bytes32 pubkey) external onlySequencer {
        sequencerPubkey = pubkey;
    }

    /*//////////////////////////////////////////////////////////////
                           QUEUE ACCESSORS
    //////////////////////////////////////////////////////////////*/

    function withdrawalQueueHead() external view returns (uint256) {
        return _withdrawalQueue.head;
    }

    function withdrawalQueueTail() external view returns (uint256) {
        return _withdrawalQueue.tail;
    }

    function withdrawalQueueMaxSize() external view returns (uint256) {
        return _withdrawalQueue.maxSize;
    }

    function withdrawalQueueSlot(uint256 slot) external view returns (bytes32) {
        return _withdrawalQueue.slots[slot];
    }

    /*//////////////////////////////////////////////////////////////
                               DEPOSITS
    //////////////////////////////////////////////////////////////*/

    /// @notice Deposit gas token into the zone. Returns the new current deposit queue hash.
    function deposit(address to, uint128 amount, bytes32 memo) external returns (bytes32 newCurrentDepositQueueHash) {
        // TIP-20 transfers revert on failure, so no boolean check is needed here.
        ITIP20(token).transferFrom(msg.sender, address(this), amount);

        // Build deposit struct
        Deposit memory depositData = Deposit({
            sender: msg.sender,
            to: to,
            amount: amount,
            memo: memo
        });

        // Insert deposit into queue
        newCurrentDepositQueueHash = DepositQueueLib.enqueue(currentDepositQueueHash, depositData);
        currentDepositQueueHash = newCurrentDepositQueueHash;

        emit DepositMade(
            newCurrentDepositQueueHash,
            msg.sender,
            to,
            amount,
            memo
        );
    }

    /*//////////////////////////////////////////////////////////////
                             WITHDRAWALS
    //////////////////////////////////////////////////////////////*/

    /// @notice Process the next withdrawal from the queue. Only callable by the sequencer.
    function processWithdrawal(Withdrawal calldata withdrawal, bytes32 remainingQueue) external onlySequencer {
        // Pop from withdrawal queue (library handles swap and hash verification)
        _withdrawalQueue.dequeue(withdrawal, remainingQueue);

        // Execute the withdrawal
        if (withdrawal.gasLimit == 0) {
            // Simple transfer, no callback
            bool success;
            try ITIP20(token).transfer(withdrawal.to, withdrawal.amount) returns (bool ok) {
                success = ok;
            } catch {
                success = false;
            }

            if (!success) {
                _enqueueBounceBack(withdrawal.amount, withdrawal.fallbackRecipient);
                emit WithdrawalProcessed(withdrawal.to, withdrawal.amount, false);
                return;
            }

            emit WithdrawalProcessed(withdrawal.to, withdrawal.amount, true);
            return;
        }

        // Try callback via messenger; revert is treated as failure
        try IZoneMessenger(messenger).relayMessage(
            withdrawal.sender,
            withdrawal.to,
            withdrawal.amount,
            withdrawal.gasLimit,
            withdrawal.callbackData
        ) {
            emit WithdrawalProcessed(withdrawal.to, withdrawal.amount, true);
        } catch {
            // Callback failed: bounce back to zone
            _enqueueBounceBack(withdrawal.amount, withdrawal.fallbackRecipient);
            emit WithdrawalProcessed(withdrawal.to, withdrawal.amount, false);
        }
    }

    /// @notice Enqueue a bounce-back deposit for failed callback
    function _enqueueBounceBack(uint128 amount, address fallbackRecipient) internal {
        Deposit memory depositData = Deposit({
            sender: address(this),
            to: fallbackRecipient,
            amount: amount,
            memo: bytes32(0)
        });

        bytes32 newCurrentDepositQueueHash = DepositQueueLib.enqueue(currentDepositQueueHash, depositData);
        currentDepositQueueHash = newCurrentDepositQueueHash;

        emit BounceBack(newCurrentDepositQueueHash, fallbackRecipient, amount);
    }

    /*//////////////////////////////////////////////////////////////
                           BATCH SUBMISSION
    //////////////////////////////////////////////////////////////*/

    /// @notice Submit a batch and verify the proof. Only callable by the sequencer.
    function submitBatch(
        uint64 tempoBlockNumber,
        BlockTransition calldata blockTransition,
        DepositQueueTransition calldata depositQueueTransition,
        WithdrawalQueueTransition calldata withdrawalQueueTransition,
        bytes calldata verifierConfig,
        bytes calldata proof
    ) external onlySequencer {
        if (blockTransition.prevBlockHash != blockHash) revert InvalidProof();

        // Validate tempoBlockNumber is within valid range for history lookup
        if (tempoBlockNumber < genesisTempoBlockNumber) revert InvalidTempoBlockNumber();
        if (tempoBlockNumber > block.number) revert InvalidTempoBlockNumber();

        // Look up the actual Tempo block hash via EIP-2935 history precompile
        bytes32 tempoBlockHash = IBlockHashHistory(BLOCKHASH_HISTORY).getBlockHash(tempoBlockNumber);
        if (tempoBlockHash == bytes32(0)) revert InvalidTempoBlockNumber();

        // Call verifier with tempoBlockHash
        // The proof reads currentDepositQueueHash from Tempo state to validate ancestry
        bool valid = IVerifier(verifier).verify(
            tempoBlockNumber,
            tempoBlockHash,
            blockTransition,
            depositQueueTransition,
            withdrawalQueueTransition,
            verifierConfig,
            proof
        );
        if (!valid) revert InvalidProof();

        // Update state
        withdrawalBatchIndex++;
        blockHash = blockTransition.nextBlockHash;
        lastSyncedTempoBlockNumber = tempoBlockNumber;

        // Update withdrawal queue - each batch gets its own slot
        // Gas note: charge new storage only when (tail - head) exceeds maxSize.
        _withdrawalQueue.enqueue(withdrawalQueueTransition);

        // Emit event after state updates
        emit BatchSubmitted(
            withdrawalBatchIndex,
            depositQueueTransition.nextProcessedHash,
            blockHash,
            withdrawalQueueTransition.withdrawalQueueHash
        );
    }
}
