// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { ITIP20 } from "./interfaces/ITIP20.sol";
import {
    IZonePortal,
    IVerifier,
    IExitReceiver,
    Deposit,
    Withdrawal,
    BatchCommitment
} from "./interfaces/IZone.sol";

/// @title ZonePortal
/// @notice Per-zone portal that escrows gas tokens on Tempo and manages deposits/withdrawals
contract ZonePortal is IZonePortal {
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

    // Deposit queue: 3-slot design
    bytes32 public processedDepositsHash;  // where proofs start
    bytes32 public pendingDepositsHash;    // stable target for proofs
    bytes32 public currentDepositsHash;    // head of deposit chain

    // Withdrawal queue: 2-queue system
    bytes32 public withdrawalQueue1;  // active queue
    bytes32 public withdrawalQueue2;  // pending queue

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
                               DEPOSITS
    //////////////////////////////////////////////////////////////*/

    /// @notice Deposit gas token into the zone. Returns the new current deposits hash.
    function deposit(address to, uint128 amount, bytes32 memo) external returns (bytes32 newCurrentDepositsHash) {
        // Transfer tokens into escrow
        ITIP20(token).transferFrom(msg.sender, address(this), amount);

        // Build deposit struct with L1 block info
        Deposit memory d = Deposit({
            l1BlockHash: blockhash(block.number - 1),
            l1BlockNumber: uint64(block.number),
            l1Timestamp: uint64(block.timestamp),
            sender: msg.sender,
            to: to,
            amount: amount,
            memo: memo
        });

        // Update deposit hash chain: new deposit wraps the outside (newest = outermost)
        newCurrentDepositsHash = keccak256(abi.encode(d, currentDepositsHash));
        currentDepositsHash = newCurrentDepositsHash;

        emit DepositMade(
            zoneId,
            newCurrentDepositsHash,
            msg.sender,
            to,
            amount,
            memo,
            d.l1BlockHash,
            d.l1BlockNumber,
            d.l1Timestamp
        );
    }

    /*//////////////////////////////////////////////////////////////
                             WITHDRAWALS
    //////////////////////////////////////////////////////////////*/

    /// @notice Process the next withdrawal from queue1. Only callable by the sequencer.
    function processWithdrawal(Withdrawal calldata w, bytes32 remainingQueue) external onlySequencer {
        // Swap in queue2 if queue1 is empty
        if (withdrawalQueue1 == bytes32(0)) {
            if (withdrawalQueue2 == bytes32(0)) revert NoWithdrawals();
            withdrawalQueue1 = withdrawalQueue2;
            withdrawalQueue2 = bytes32(0);
        }

        // Verify this is the head of queue1
        if (keccak256(abi.encode(w, remainingQueue)) != withdrawalQueue1) {
            revert InvalidWithdrawal();
        }

        // Pop the withdrawal regardless of success/failure
        if (remainingQueue == bytes32(0)) {
            withdrawalQueue1 = withdrawalQueue2;
            withdrawalQueue2 = bytes32(0);
        } else {
            withdrawalQueue1 = remainingQueue;
        }

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
            l1BlockHash: blockhash(block.number - 1),
            l1BlockNumber: uint64(block.number),
            l1Timestamp: uint64(block.timestamp),
            sender: address(this),
            to: fallbackRecipient,
            amount: amount,
            memo: bytes32(0)
        });

        bytes32 newCurrentDepositsHash = keccak256(abi.encode(d, currentDepositsHash));
        currentDepositsHash = newCurrentDepositsHash;

        emit BounceBack(zoneId, newCurrentDepositsHash, fallbackRecipient, amount);
    }

    /*//////////////////////////////////////////////////////////////
                           BATCH SUBMISSION
    //////////////////////////////////////////////////////////////*/

    /// @notice Submit a batch and verify the proof. Only callable by the sequencer.
    function submitBatch(
        BatchCommitment calldata commitment,
        bytes32 expectedQueue2,
        bytes32 updatedQueue2,
        bytes32 newWithdrawalsOnly,
        bytes calldata verifierData,
        bytes calldata proof
    ) external onlySequencer {
        // Call verifier
        bool valid = IVerifier(verifier).verify(
            processedDepositsHash,
            pendingDepositsHash,
            commitment.newProcessedDepositsHash,
            stateRoot,
            commitment.newStateRoot,
            expectedQueue2,
            updatedQueue2,
            newWithdrawalsOnly,
            verifierData,
            proof
        );
        if (!valid) revert InvalidProof();

        // Emit event before state updates (captures pre-state)
        emit BatchSubmitted(
            zoneId,
            batchIndex,
            processedDepositsHash,
            pendingDepositsHash,
            commitment.newProcessedDepositsHash,
            stateRoot,
            commitment.newStateRoot,
            expectedQueue2,
            updatedQueue2,
            newWithdrawalsOnly
        );

        // Update state
        batchIndex++;
        stateRoot = commitment.newStateRoot;

        // Update deposit chain
        processedDepositsHash = commitment.newProcessedDepositsHash;
        pendingDepositsHash = currentDepositsHash;

        // Update withdrawal queue
        if (withdrawalQueue2 == expectedQueue2) {
            withdrawalQueue2 = updatedQueue2;
        } else if (withdrawalQueue2 == bytes32(0)) {
            withdrawalQueue2 = newWithdrawalsOnly;
        } else {
            revert UnexpectedQueue2State();
        }
    }
}
