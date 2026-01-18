// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { ITIP20 } from "../interfaces/ITIP20.sol";
import {
    IZonePortal,
    IVerifier,
    IWithdrawalReceiver,
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
    uint64 public immutable genesisTempoBlockNumber;

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
        bytes32 _genesisStateRoot,
        uint64 _genesisTempoBlockNumber
    ) {
        zoneId = _zoneId;
        token = _token;
        sequencer = _sequencer;
        verifier = _verifier;
        stateRoot = _genesisStateRoot;
        genesisTempoBlockNumber = _genesisTempoBlockNumber;
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

        // Build deposit struct
        Deposit memory depositData = Deposit({
            sender: msg.sender,
            to: to,
            amount: amount,
            memo: memo
        });

        // Insert deposit into queue
        newCurrentDepositQueueHash = _depositQueue.enqueue(depositData);

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
            ITIP20(token).transfer(withdrawal.to, withdrawal.amount);
            emit WithdrawalProcessed(withdrawal.to, withdrawal.amount, true);
            return;
        }

        // Try callback
        try this._executeWithdrawal(withdrawal) {
            emit WithdrawalProcessed(withdrawal.to, withdrawal.amount, true);
        } catch {
            // Callback failed: bounce back to zone
            _enqueueBounceBack(withdrawal.amount, withdrawal.fallbackRecipient);
            emit WithdrawalProcessed(withdrawal.to, withdrawal.amount, false);
        }
    }

    /// @notice Internal function for atomic transfer + callback (called via self-call)
    function _executeWithdrawal(Withdrawal calldata withdrawal) external {
        if (msg.sender != address(this)) revert NotSequencer(); // self-call only

        // Transfer before callback so receiver can use funds
        ITIP20(token).transfer(withdrawal.to, withdrawal.amount);

        // Call the receiver
        bytes4 selector = IWithdrawalReceiver(withdrawal.to).onWithdrawalReceived{gas: withdrawal.gasLimit}(
            withdrawal.sender,
            withdrawal.amount,
            withdrawal.callbackData
        );
        if (selector != IWithdrawalReceiver.onWithdrawalReceived.selector) {
            revert CallbackRejected();
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

        bytes32 newCurrentDepositQueueHash = _depositQueue.enqueue(depositData);

        emit BounceBack(newCurrentDepositQueueHash, fallbackRecipient, amount);
    }

    /*//////////////////////////////////////////////////////////////
                           BATCH SUBMISSION
    //////////////////////////////////////////////////////////////*/

    /// @notice Submit a batch and verify the proof. Only callable by the sequencer.
    function submitBatch(
        uint64 tempoBlockNumber,
        StateTransition calldata stateTransition,
        DepositQueueTransition calldata depositQueueTransition,
        WithdrawalQueueTransition calldata withdrawalQueueTransition,
        bytes calldata verifierConfig,
        bytes calldata proof
    ) external onlySequencer {
        // Validate tempoBlockNumber is within valid range for blockhash lookup
        // blockhash() only works for the last 256 blocks
        if (tempoBlockNumber < genesisTempoBlockNumber) revert InvalidTempoBlockNumber();
        if (tempoBlockNumber > block.number) revert InvalidTempoBlockNumber();
        if (block.number - tempoBlockNumber > 256) revert InvalidTempoBlockNumber();

        // Look up the actual Tempo block hash
        bytes32 tempoBlockHash = blockhash(tempoBlockNumber);
        if (tempoBlockHash == bytes32(0)) revert InvalidTempoBlockNumber();

        // Build deposit queue transition with current state for verifier
        DepositQueueTransition memory fullDepositTransition = DepositQueueTransition({
            prevSnapshotHash: _depositQueue.snapshot,
            prevProcessedHash: _depositQueue.processed,
            nextProcessedHash: depositQueueTransition.nextProcessedHash
        });

        // Call verifier with tempoBlockHash
        bool valid = IVerifier(verifier).verify(
            tempoBlockHash,
            StateTransition({
                prevStateRoot: stateRoot,
                nextStateRoot: stateTransition.nextStateRoot
            }),
            fullDepositTransition,
            withdrawalQueueTransition,
            verifierConfig,
            proof
        );
        if (!valid) revert InvalidProof();

        // Update state
        batchIndex++;
        stateRoot = stateTransition.nextStateRoot;

        // Update deposit queue via library (validates expected state matches)
        _depositQueue.dequeueWithProof(fullDepositTransition);

        // Update withdrawal queue via library
        _withdrawalQueue.enqueueWithProof(withdrawalQueueTransition);

        // Emit event after state updates (captures new state including actual withdrawal queue used)
        emit BatchSubmitted(
            batchIndex,
            tempoBlockNumber,
            _depositQueue.processed,
            stateRoot,
            _withdrawalQueue.pending
        );
    }
}
