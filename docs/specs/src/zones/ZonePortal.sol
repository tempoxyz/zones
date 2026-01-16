// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {IZonePortal} from "./interfaces/IZonePortal.sol";
import {IZoneRegistry} from "./interfaces/IZoneRegistry.sol";
import {IVerifier} from "./interfaces/IVerifier.sol";
import {IExitReceiver} from "./interfaces/IExitReceiver.sol";
import {ITIP20} from "../interfaces/ITIP20.sol";

/// @title ZonePortal
/// @notice Per-zone portal that escrows the gas token and processes withdrawals
contract ZonePortal is IZonePortal {
    /// @inheritdoc IZonePortal
    uint64 public immutable zoneId;

    /// @inheritdoc IZonePortal
    address public immutable gasToken;

    /// @inheritdoc IZonePortal
    address public immutable sequencer;

    /// @inheritdoc IZonePortal
    address public immutable verifier;

    /// @notice The zone registry
    IZoneRegistry public immutable registry;

    /// @inheritdoc IZonePortal
    bytes32 public sequencerPubkey;

    /// @inheritdoc IZonePortal
    uint64 public batchIndex;

    /// @inheritdoc IZonePortal
    bytes32 public stateRoot;

    /// @inheritdoc IZonePortal
    bytes32 public currentDepositsHash;

    /// @inheritdoc IZonePortal
    bytes32 public checkpointedDepositsHash;

    /// @inheritdoc IZonePortal
    bytes32 public withdrawalQueue1;

    /// @inheritdoc IZonePortal
    bytes32 public withdrawalQueue2;

    modifier onlySequencer() {
        if (msg.sender != sequencer) revert OnlySequencer();
        _;
    }

    constructor(
        uint64 zoneId_,
        address gasToken_,
        address sequencer_,
        address verifier_,
        bytes32 genesisStateRoot_,
        address registry_
    ) {
        zoneId = zoneId_;
        gasToken = gasToken_;
        sequencer = sequencer_;
        verifier = verifier_;
        stateRoot = genesisStateRoot_;
        registry = IZoneRegistry(registry_);
    }

    /// @inheritdoc IZonePortal
    function setSequencerPubkey(bytes32 pubkey) external onlySequencer {
        sequencerPubkey = pubkey;
    }

    /// @inheritdoc IZonePortal
    function deposit(address to, uint128 amount, bytes32 memo) external returns (bytes32 newCurrentDepositsHash) {
        ITIP20(gasToken).transferFrom(msg.sender, address(this), amount);

        Deposit memory dep = Deposit({
            l1BlockHash: blockhash(block.number - 1),
            l1BlockNumber: uint64(block.number),
            l1Timestamp: uint64(block.timestamp),
            sender: msg.sender,
            to: to,
            amount: amount,
            memo: memo
        });

        newCurrentDepositsHash = keccak256(abi.encode(dep, currentDepositsHash));
        currentDepositsHash = newCurrentDepositsHash;

        emit DepositEnqueued(
            zoneId,
            newCurrentDepositsHash,
            msg.sender,
            to,
            amount,
            memo,
            dep.l1BlockHash,
            dep.l1BlockNumber,
            dep.l1Timestamp
        );
    }

    /// @inheritdoc IZonePortal
    function processWithdrawal(Withdrawal calldata w, bytes32 remainingQueue) external onlySequencer {
        if (withdrawalQueue1 == bytes32(0)) {
            if (withdrawalQueue2 == bytes32(0)) revert NoWithdrawals();
            withdrawalQueue1 = withdrawalQueue2;
            withdrawalQueue2 = bytes32(0);
        }

        if (keccak256(abi.encode(w, remainingQueue)) != withdrawalQueue1) {
            revert InvalidWithdrawal();
        }

        _executeWithdrawal(w);

        if (remainingQueue == bytes32(0)) {
            withdrawalQueue1 = withdrawalQueue2;
            withdrawalQueue2 = bytes32(0);
        } else {
            withdrawalQueue1 = remainingQueue;
        }
    }

    /// @inheritdoc IZonePortal
    function submitBatch(
        BatchCommitment calldata commitment,
        bytes32 expectedQueue2,
        bytes32 updatedQueue2,
        bytes32 newWithdrawalsOnly,
        bytes calldata proof
    ) external onlySequencer {
        bool valid = IVerifier(verifier).verify(
            checkpointedDepositsHash,
            commitment.newProcessedDepositsHash,
            stateRoot,
            commitment.newStateRoot,
            expectedQueue2,
            updatedQueue2,
            newWithdrawalsOnly,
            proof
        );

        if (!valid) revert InvalidProof();

        if (withdrawalQueue2 == expectedQueue2) {
            withdrawalQueue2 = updatedQueue2;
        } else if (withdrawalQueue2 == bytes32(0)) {
            withdrawalQueue2 = newWithdrawalsOnly;
        } else {
            revert UnexpectedQueue2State();
        }

        checkpointedDepositsHash = commitment.newProcessedDepositsHash;
        stateRoot = commitment.newStateRoot;
        batchIndex++;

        registry.updateBatchHead(zoneId, batchIndex, stateRoot);

        emit BatchSubmitted(
            zoneId,
            batchIndex,
            commitment.newProcessedDepositsHash,
            commitment.newStateRoot
        );
    }

    function _executeWithdrawal(Withdrawal calldata w) internal {
        ITIP20(gasToken).transfer(w.to, w.amount);

        emit WithdrawalProcessed(zoneId, w.to, w.amount);

        if (w.gasLimit > 0) {
            try IExitReceiver(w.to).onExitReceived{gas: w.gasLimit}(
                w.sender,
                w.amount,
                w.data
            ) returns (bytes4 selector) {
                if (selector != IExitReceiver.onExitReceived.selector) {
                    _enqueueBounceBack(w.amount, w.fallbackRecipient);
                }
            } catch {
                _enqueueBounceBack(w.amount, w.fallbackRecipient);
            }
        }
    }

    function _enqueueBounceBack(uint128 amount, address fallbackRecipient) internal {
        Deposit memory dep = Deposit({
            l1BlockHash: blockhash(block.number - 1),
            l1BlockNumber: uint64(block.number),
            l1Timestamp: uint64(block.timestamp),
            sender: address(this),
            to: fallbackRecipient,
            amount: amount,
            memo: bytes32(0)
        });

        bytes32 newHash = keccak256(abi.encode(dep, currentDepositsHash));
        currentDepositsHash = newHash;

        emit WithdrawalBouncedBack(zoneId, fallbackRecipient, amount);

        emit DepositEnqueued(
            zoneId,
            newHash,
            address(this),
            fallbackRecipient,
            amount,
            bytes32(0),
            dep.l1BlockHash,
            dep.l1BlockNumber,
            dep.l1Timestamp
        );
    }
}
