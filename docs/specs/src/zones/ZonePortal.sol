// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {IZonePortal} from "./interfaces/IZonePortal.sol";
import {IZoneRegistry} from "./interfaces/IZoneRegistry.sol";
import {IVerifier} from "./interfaces/IVerifier.sol";
import {IZoneTypes} from "./interfaces/IZoneTypes.sol";
import {ITIP20} from "../interfaces/ITIP20.sol";
import {IStablecoinDEX} from "../interfaces/IStablecoinDEX.sol";

/// @title ZonePortal
/// @notice Per-zone portal that escrows the gas token and finalizes exits
contract ZonePortal is IZonePortal {
    /// @notice The Tempo Stablecoin DEX address
    address public constant DEX = 0xDEc0000000000000000000000000000000000000;

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

    /// @notice The zone factory
    address public immutable factory;

    /// @inheritdoc IZonePortal
    uint64 public nextDepositIndex;

    /// @inheritdoc IZonePortal
    bytes32 public stateRoot;

    /// @inheritdoc IZonePortal
    uint64 public batchIndex;

    /// @inheritdoc IZonePortal
    bytes32 public exitRoot;

    /// @notice Deposits by index
    mapping(uint64 => Deposit) private _deposits;

    /// @notice Claimed exits
    mapping(bytes32 => bool) private _exitClaimed;

    /// @notice Batch exit roots by batch index
    mapping(uint64 => bytes32) private _batchExitRoots;

    constructor(
        uint64 zoneId_,
        address gasToken_,
        address sequencer_,
        address verifier_,
        bytes32 genesisStateRoot_,
        address registry_,
        address factory_
    ) {
        zoneId = zoneId_;
        gasToken = gasToken_;
        sequencer = sequencer_;
        verifier = verifier_;
        stateRoot = genesisStateRoot_;
        registry = IZoneRegistry(registry_);
        factory = factory_;
    }

    /// @inheritdoc IZonePortal
    function deposits(uint64 index) external view returns (Deposit memory) {
        return _deposits[index];
    }

    /// @inheritdoc IZonePortal
    function exitClaimed(bytes32 exitId) external view returns (bool) {
        return _exitClaimed[exitId];
    }

    /// @inheritdoc IZonePortal
    function deposit(address to, uint256 amount, bytes32 memo) external returns (uint64 depositIndex) {
        ITIP20(gasToken).transferFrom(msg.sender, address(this), amount);

        depositIndex = nextDepositIndex++;
        _deposits[depositIndex] = Deposit({
            sender: msg.sender,
            to: to,
            amount: amount,
            memo: memo
        });

        emit DepositEnqueued(zoneId, depositIndex, msg.sender, to, amount, memo);
    }

    /// @inheritdoc IZonePortal
    function submitBatch(BatchCommitment calldata commitment, bytes calldata proof) external {
        if (msg.sender != sequencer) {
            revert OnlySequencer();
        }
        if (commitment.zoneId != zoneId) {
            revert InvalidBatchIndex();
        }
        if (commitment.batchIndex != batchIndex + 1) {
            revert InvalidBatchIndex();
        }
        if (commitment.prevStateRoot != stateRoot) {
            revert InvalidPrevStateRoot();
        }

        bytes32 commitmentHash = keccak256(abi.encode(commitment));
        if (!IVerifier(verifier).verify(commitmentHash, proof)) {
            revert InvalidProof();
        }

        batchIndex = commitment.batchIndex;
        stateRoot = commitment.newStateRoot;
        exitRoot = commitment.exitRoot;
        _batchExitRoots[commitment.batchIndex] = commitment.exitRoot;

        registry.updateBatchHead(zoneId, batchIndex, stateRoot, exitRoot);

        emit BatchAccepted(
            zoneId,
            commitment.batchIndex,
            commitment.prevStateRoot,
            commitment.newStateRoot,
            commitment.depositIndex,
            commitment.exitRoot,
            commitment.batchHash
        );
    }

    /// @inheritdoc IZonePortal
    function finalizeTransferExit(ExitIntent calldata intent, ExitProof calldata proof) external {
        _validateAndClaimExit(intent, proof);

        ITIP20(gasToken).transfer(intent.transfer.recipient, intent.transfer.amount);

        emit ExitFinalized(_exitId(intent), zoneId, ExitKind.Transfer);
    }

    /// @inheritdoc IZonePortal
    function finalizeSwapExit(ExitIntent calldata intent, ExitProof calldata proof) external {
        _validateAndClaimExit(intent, proof);

        ITIP20(gasToken).approve(DEX, intent.swap.amountIn);

        uint128 amountOut = IStablecoinDEX(DEX).swapExactAmountIn(
            gasToken,
            intent.swap.tokenOut,
            intent.swap.amountIn,
            intent.swap.minAmountOut
        );

        IStablecoinDEX(DEX).withdraw(intent.swap.tokenOut, amountOut);
        ITIP20(intent.swap.tokenOut).transfer(intent.swap.recipient, amountOut);

        emit ExitFinalized(_exitId(intent), zoneId, ExitKind.Swap);
    }

    /// @inheritdoc IZonePortal
    function finalizeSwapAndDepositExit(ExitIntent calldata intent, ExitProof calldata proof) external {
        _validateAndClaimExit(intent, proof);

        ITIP20(gasToken).approve(DEX, intent.swapAndDeposit.amountIn);

        uint128 amountOut = IStablecoinDEX(DEX).swapExactAmountIn(
            gasToken,
            intent.swapAndDeposit.tokenOut,
            intent.swapAndDeposit.amountIn,
            intent.swapAndDeposit.minAmountOut
        );

        IStablecoinDEX(DEX).withdraw(intent.swapAndDeposit.tokenOut, amountOut);

        IZoneTypes.ZoneInfo memory destZone = registry.getZone(intent.swapAndDeposit.destinationZoneId);
        if (destZone.portal == address(0)) {
            revert DestinationZoneNotFound();
        }

        ITIP20(intent.swapAndDeposit.tokenOut).approve(destZone.portal, amountOut);
        IZonePortal(destZone.portal).deposit(
            intent.swapAndDeposit.destinationRecipient,
            amountOut,
            intent.memo
        );

        emit ExitFinalized(_exitId(intent), zoneId, ExitKind.SwapAndDeposit);
    }

    function _validateAndClaimExit(ExitIntent calldata intent, ExitProof calldata proof) internal {
        bytes32 exitId = _exitId(intent);

        if (_exitClaimed[exitId]) {
            revert ExitAlreadyClaimed();
        }

        bytes32 batchExitRoot = _batchExitRoots[proof.batchIndex];
        if (batchExitRoot == bytes32(0)) {
            revert InvalidProof();
        }

        bytes32 leaf = keccak256(abi.encode(intent));
        if (!_verifyMerkleProof(proof.merkleProof, batchExitRoot, leaf)) {
            revert InvalidProof();
        }

        _exitClaimed[exitId] = true;
    }

    function _exitId(ExitIntent calldata intent) internal view returns (bytes32) {
        return keccak256(abi.encode(zoneId, intent.exitIndex, intent));
    }

    function _verifyMerkleProof(
        bytes32[] calldata proof,
        bytes32 root,
        bytes32 leaf
    ) internal pure returns (bool) {
        bytes32 computedHash = leaf;

        for (uint256 i = 0; i < proof.length; i++) {
            bytes32 proofElement = proof[i];

            if (computedHash <= proofElement) {
                computedHash = keccak256(abi.encodePacked(computedHash, proofElement));
            } else {
                computedHash = keccak256(abi.encodePacked(proofElement, computedHash));
            }
        }

        return computedHash == root;
    }
}
