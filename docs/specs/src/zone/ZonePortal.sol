// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { ITIP20 } from "../interfaces/ITIP20.sol";
import {
    IZonePortal,
    IVerifier,
    IExitReceiver,
    DepositQueueMessage,
    DepositQueueMessageKind,
    L1Sync,
    Deposit,
    Withdrawal,
    StateTransition,
    DepositQueueTransition,
    WithdrawalQueueTransition
} from "./IZone.sol";
import { DepositQueue, DepositQueueLib } from "./DepositQueueLib.sol";
import { WithdrawalQueue, WithdrawalQueueLib } from "./WithdrawalQueueLib.sol";

/// @title ZonePortal
/// @notice Per-zone portal that escrows gas tokens on Tempo and manages deposits/withdrawals
contract ZonePortal is IZonePortal {
    using DepositQueueLib for DepositQueue;
    using WithdrawalQueueLib for WithdrawalQueue;

    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    uint64 public immutable zoneId;
    address public immutable token;
    address public immutable sequencer;
    address public immutable verifier;

    bytes32 public sequencerPubkey;
    uint64 public batchIndex;
    bytes32 public stateRoot;

    /// @notice Deposit queue (L1→L2): 3-slot ceiling pattern
    DepositQueue internal _depositQueue;

    /// @notice Withdrawal queue (L2→L1): 2-slot swap pattern
    WithdrawalQueue internal _withdrawalQueue;

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(
        uint64 _zoneId,
        address _token,
        address _sequencer,
        address _verifier,
        bytes32 _genesisStateRoot
    ) {
        zoneId = _zoneId;
        token = _token;
        sequencer = _sequencer;
        verifier = _verifier;
        stateRoot = _genesisStateRoot;
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

    function processedDepositQueueHash() external view returns (bytes32) {
        return _depositQueue.processed;
    }

    function snapshotDepositQueueHash() external view returns (bytes32) {
        return _depositQueue.snapshot;
    }

    function currentDepositQueueHash() external view returns (bytes32) {
        return _depositQueue.current;
    }

    function activeWithdrawalQueueHash() external view returns (bytes32) {
        return _withdrawalQueue.active;
    }

    function pendingWithdrawalQueueHash() external view returns (bytes32) {
        return _withdrawalQueue.pending;
    }

    /*//////////////////////////////////////////////////////////////
                               DEPOSITS
    //////////////////////////////////////////////////////////////*/

    /// @notice Deposit gas token into the zone. Returns the new current deposit queue hash.
    function deposit(address to, uint128 amount, bytes32 memo) external returns (bytes32 newCurrentDepositQueueHash) {
        // Transfer tokens into escrow
        ITIP20(token).transferFrom(msg.sender, address(this), amount);

        // Build deposit struct with L1 block info
        Deposit memory d = Deposit({
            l1ParentBlockHash: blockhash(block.number - 1),
            l1BlockNumber: uint64(block.number),
            l1Timestamp: uint64(block.timestamp),
            sender: msg.sender,
            to: to,
            amount: amount,
            memo: memo
        });

        // Build message and insert into deposit queue
        DepositQueueMessage memory m = DepositQueueMessage({
            kind: DepositQueueMessageKind.Deposit,
            data: abi.encode(d)
        });
        newCurrentDepositQueueHash = _depositQueue.enqueue(m);

        emit DepositMade(
            zoneId,
            newCurrentDepositQueueHash,
            msg.sender,
            to,
            amount,
            memo,
            d.l1ParentBlockHash,
            d.l1BlockNumber,
            d.l1Timestamp
        );
    }

    /// @notice Append an L1 sync message to the deposit queue. Only callable by the sequencer.
    function syncL1() external onlySequencer returns (bytes32 newCurrentDepositQueueHash) {
        L1Sync memory sync = L1Sync({
            l1ParentBlockHash: blockhash(block.number - 1),
            l1BlockNumber: uint64(block.number),
            l1Timestamp: uint64(block.timestamp)
        });

        DepositQueueMessage memory m = DepositQueueMessage({
            kind: DepositQueueMessageKind.L1Sync,
            data: abi.encode(sync)
        });
        newCurrentDepositQueueHash = _depositQueue.enqueue(m);

        emit L1SyncAppended(
            zoneId,
            newCurrentDepositQueueHash,
            sync.l1ParentBlockHash,
            sync.l1BlockNumber,
            sync.l1Timestamp
        );
    }

    /*//////////////////////////////////////////////////////////////
                             WITHDRAWALS
    //////////////////////////////////////////////////////////////*/

    /// @notice Process the next withdrawal from the queue. Only callable by the sequencer.
    function processWithdrawal(Withdrawal calldata w, bytes32 remainingQueue) external onlySequencer {
        // Pop from withdrawal queue (library handles swap and hash verification)
        _withdrawalQueue.dequeue(w, remainingQueue);

        // Execute the withdrawal
        if (w.gasLimit == 0) {
            // Simple transfer, no callback
            ITIP20(token).transfer(w.to, w.amount);
            emit WithdrawalProcessed(zoneId, w.to, w.amount, true);
            return;
        }

        // Try callback
        try this._executeWithdrawal(w) {
            emit WithdrawalProcessed(zoneId, w.to, w.amount, true);
        } catch {
            // Callback failed: bounce back to zone
            _enqueueBounceBack(w.amount, w.fallbackRecipient);
            emit WithdrawalProcessed(zoneId, w.to, w.amount, false);
        }
    }

    /// @notice Internal function for atomic transfer + callback (called via self-call)
    function _executeWithdrawal(Withdrawal calldata w) external {
        if (msg.sender != address(this)) revert NotSequencer(); // self-call only

        // Transfer before callback so receiver can use funds
        ITIP20(token).transfer(w.to, w.amount);

        // Call the receiver
        bytes4 selector = IExitReceiver(w.to).onExitReceived{gas: w.gasLimit}(
            w.sender,
            w.amount,
            w.data
        );
        if (selector != IExitReceiver.onExitReceived.selector) {
            revert CallbackRejected();
        }
    }

    /// @notice Enqueue a bounce-back deposit for failed callback
    function _enqueueBounceBack(uint128 amount, address fallbackRecipient) internal {
        Deposit memory d = Deposit({
            l1ParentBlockHash: blockhash(block.number - 1),
            l1BlockNumber: uint64(block.number),
            l1Timestamp: uint64(block.timestamp),
            sender: address(this),
            to: fallbackRecipient,
            amount: amount,
            memo: bytes32(0)
        });

        DepositQueueMessage memory m = DepositQueueMessage({
            kind: DepositQueueMessageKind.Deposit,
            data: abi.encode(d)
        });
        bytes32 newCurrentDepositQueueHash = _depositQueue.enqueue(m);

        emit BounceBack(zoneId, newCurrentDepositQueueHash, fallbackRecipient, amount);
    }

    /*//////////////////////////////////////////////////////////////
                           BATCH SUBMISSION
    //////////////////////////////////////////////////////////////*/

    /// @notice Submit a batch and verify the proof. Only callable by the sequencer.
    function submitBatch(
        StateTransition calldata stateTransition,
        DepositQueueTransition calldata depositQueueTransition,
        WithdrawalQueueTransition calldata withdrawalQueueTransition,
        bytes calldata verifierData,
        bytes calldata proof
    ) external onlySequencer {
        // Build deposit queue transition with current state for verifier
        DepositQueueTransition memory fullDepositTransition = DepositQueueTransition({
            prevSnapshotHash: _depositQueue.snapshot,
            prevProcessedHash: _depositQueue.processed,
            nextProcessedHash: depositQueueTransition.nextProcessedHash
        });

        // Call verifier
        bool valid = IVerifier(verifier).verify(
            StateTransition({
                prevStateRoot: stateRoot,
                nextStateRoot: stateTransition.nextStateRoot
            }),
            fullDepositTransition,
            withdrawalQueueTransition,
            verifierData,
            proof
        );
        if (!valid) revert InvalidProof();

        // Emit event before state updates (captures pre-state)
        _emitBatchSubmitted(stateTransition, fullDepositTransition, withdrawalQueueTransition);

        // Update state
        batchIndex++;
        stateRoot = stateTransition.nextStateRoot;

        // Update deposit queue via library (validates expected state matches)
        _depositQueue.dequeueWithProof(fullDepositTransition);

        // Update withdrawal queue via library
        _withdrawalQueue.enqueueWithProof(withdrawalQueueTransition);
    }

    /// @dev Extracted to avoid stack-too-deep in submitBatch
    function _emitBatchSubmitted(
        StateTransition calldata stateTransition,
        DepositQueueTransition memory depositTransition,
        WithdrawalQueueTransition calldata withdrawalTransition
    ) internal {
        emit BatchSubmitted(
            zoneId,
            batchIndex,
            depositTransition.prevSnapshotHash,
            depositTransition.prevProcessedHash,
            depositTransition.nextProcessedHash,
            stateRoot,
            stateTransition.nextStateRoot,
            withdrawalTransition.prevPendingHash,
            withdrawalTransition.nextPendingHashIfFull,
            withdrawalTransition.nextPendingHashIfEmpty
        );
    }
}
