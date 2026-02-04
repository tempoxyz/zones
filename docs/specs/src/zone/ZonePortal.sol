// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { ITIP20 } from "../interfaces/ITIP20.sol";

import { BLOCKHASH_HISTORY, IBlockHashHistory } from "./BlockHashHistory.sol";
import { DepositQueueLib } from "./DepositQueueLib.sol";
import {
    IZonePortal,
    IZoneMessenger,
    IVerifier,
    BlockTransition,
    Deposit,
    EncryptedDeposit,
    EncryptedDepositPayload,
    DepositType,
    QueuedDeposit,
    EncryptionKeyEntry,
    ENCRYPTION_KEY_GRACE_PERIOD,
    DepositQueueTransition,
    Withdrawal,
    WithdrawalQueueTransition
} from "./IZone.sol";
import { WithdrawalQueue, WithdrawalQueueLib } from "./WithdrawalQueueLib.sol";

/// @title ZonePortal
/// @notice Per-zone portal that escrows zone tokens on Tempo and manages deposits/withdrawals
contract ZonePortal is IZonePortal {

    using WithdrawalQueueLib for WithdrawalQueue;

    /*//////////////////////////////////////////////////////////////
                               CONSTANTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Fixed gas value for deposit fee calculation
    /// @dev Set to 100,000 gas. Deposit fee = FIXED_DEPOSIT_GAS * zoneGasRate.
    ///      This provides a stable pricing basis for deposits while allowing sequencer
    ///      flexibility to adjust the zoneGasRate based on operational costs.
    uint64 public constant FIXED_DEPOSIT_GAS = 100_000;

    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    uint64 public immutable zoneId;
    address public immutable token;
    address public immutable messenger;
    address public immutable verifier;
    uint64 public immutable genesisTempoBlockNumber;

    /// @notice Current sequencer address
    address public sequencer;

    /// @notice Pending sequencer for two-step transfer
    address public pendingSequencer;

    /// @notice Historical encryption keys with activation blocks
    /// @dev Users specify which key they encrypted to (by index). Maintained for key rotation.
    ///      This is stored at slot 8 in the storage layout.
    EncryptionKeyEntry[] internal _encryptionKeys;

    /// @notice Zone gas rate (zone token units per gas unit on the zone)
    /// @dev Sequencer publishes this rate and takes the risk on zone gas costs.
    ///      Deposit fee = FIXED_DEPOSIT_GAS * zoneGasRate
    uint128 public zoneGasRate;
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

        // Give messenger max approval for the zone token
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
                           SEQUENCER MANAGEMENT
    //////////////////////////////////////////////////////////////*/

    /// @notice Start a sequencer transfer. Only callable by current sequencer.
    /// @param newSequencer The address that will become sequencer after accepting.
    function transferSequencer(address newSequencer) external onlySequencer {
        pendingSequencer = newSequencer;
        emit SequencerTransferStarted(sequencer, newSequencer);
    }

    /// @notice Accept a pending sequencer transfer. Only callable by pending sequencer.
    function acceptSequencer() external {
        if (msg.sender != pendingSequencer) revert NotPendingSequencer();
        address previousSequencer = sequencer;
        sequencer = pendingSequencer;
        pendingSequencer = address(0);
        emit SequencerTransferred(previousSequencer, sequencer);
    }

    function setSequencerPubkey(bytes32 pubkey) external onlySequencer {
        sequencerPubkey = pubkey;
    }

    /// @notice Set zone gas rate. Only callable by sequencer.
    /// @dev Sequencer publishes this rate and takes the risk on zone gas costs.
    ///      If actual zone gas is higher, sequencer covers the difference.
    ///      If actual zone gas is lower, sequencer keeps the surplus.
    /// @param _zoneGasRate Zone token units per gas unit on the zone
    function setZoneGasRate(uint128 _zoneGasRate) external onlySequencer {
        zoneGasRate = _zoneGasRate;
        emit ZoneGasRateUpdated(_zoneGasRate);
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
                        ENCRYPTION KEY MANAGEMENT
    //////////////////////////////////////////////////////////////*/

    /// @notice Get the sequencer's current encryption public key
    /// @return x The X coordinate
    /// @return yParity The Y coordinate parity (0x02 or 0x03)
    function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity) {
        if (_encryptionKeys.length == 0) return (bytes32(0), 0);
        EncryptionKeyEntry storage current = _encryptionKeys[_encryptionKeys.length - 1];
        return (current.x, current.yParity);
    }

    /// @notice Set the sequencer's encryption public key
    /// @dev Only callable by the sequencer. Appends to key history.
    /// @param x The X coordinate
    /// @param yParity The Y coordinate parity (0x02 or 0x03)
    function setSequencerEncryptionKey(bytes32 x, uint8 yParity) external onlySequencer {
        uint64 activationBlock = uint64(block.number);
        _encryptionKeys.push(EncryptionKeyEntry({
            x: x,
            yParity: yParity,
            activationBlock: activationBlock
        }));
        emit SequencerEncryptionKeyUpdated(x, yParity, _encryptionKeys.length - 1, activationBlock);
    }

    /// @notice Get the number of keys in the history
    function encryptionKeyCount() external view returns (uint256) {
        return _encryptionKeys.length;
    }

    /// @notice Get a historical encryption key by index
    /// @param index The index in the key history (0 = first key)
    /// @return entry The key entry with activation block
    function encryptionKeyAt(uint256 index) external view returns (EncryptionKeyEntry memory entry) {
        if (index >= _encryptionKeys.length) revert InvalidEncryptionKeyIndex(index);
        return _encryptionKeys[index];
    }

    /// @notice Check if an encryption key is still valid for new deposits
    /// @param keyIndex The key index to check
    /// @return valid True if the key can be used for new deposits
    /// @return expiresAtBlock Block number when this key expires (0 if current key)
    function isEncryptionKeyValid(uint256 keyIndex) public view returns (bool valid, uint64 expiresAtBlock) {
        if (keyIndex >= _encryptionKeys.length) {
            return (false, 0);
        }

        // Current key (latest) never expires
        if (keyIndex == _encryptionKeys.length - 1) {
            return (true, 0);
        }

        // Old keys are valid during grace period after supersession
        EncryptionKeyEntry storage nextKey = _encryptionKeys[keyIndex + 1];
        uint64 expiration = nextKey.activationBlock + ENCRYPTION_KEY_GRACE_PERIOD;

        valid = block.number < expiration;
        expiresAtBlock = expiration;
    }

    /*//////////////////////////////////////////////////////////////
                               DEPOSITS
    //////////////////////////////////////////////////////////////*/

    /// @notice Calculate the fee for a deposit
    /// @dev Fee = FIXED_DEPOSIT_GAS * zoneGasRate
    /// @return fee The deposit fee in zone token units
    function calculateDepositFee() public view returns (uint128 fee) {
        fee = uint128(FIXED_DEPOSIT_GAS) * zoneGasRate;
    }

    /// @notice Deposit zone token into the zone. Returns the new current deposit queue hash.
    /// @dev Fee is deducted from amount and paid to sequencer. Net amount is credited on zone.
    /// @param to Recipient address on the zone
    /// @param amount Total amount to deposit (fee will be deducted)
    /// @param memo User-provided context
    /// @return newCurrentDepositQueueHash The new deposit queue hash after this deposit
    function deposit(
        address to,
        uint128 amount,
        bytes32 memo
    )
        external
        returns (bytes32 newCurrentDepositQueueHash)
    {
        // Calculate deposit fee
        uint128 fee = calculateDepositFee();
        if (amount <= fee) revert DepositTooSmall();
        uint128 netAmount = amount - fee;

        // Transfer full amount from sender to this contract
        // TIP-20 transfers revert on failure, so no boolean check is needed here.
        ITIP20(token).transferFrom(msg.sender, address(this), amount);

        // Transfer fee to sequencer
        if (fee > 0) {
            ITIP20(token).transfer(sequencer, fee);
        }

        // Build deposit struct with net amount (fee already paid to sequencer on Tempo)
        Deposit memory depositData =
            Deposit({ sender: msg.sender, to: to, amount: netAmount, memo: memo });

        // Insert deposit into queue
        newCurrentDepositQueueHash = DepositQueueLib.enqueue(currentDepositQueueHash, depositData);
        currentDepositQueueHash = newCurrentDepositQueueHash;

        emit DepositMade(newCurrentDepositQueueHash, msg.sender, to, netAmount, fee, memo);
    }

    /// @notice Deposit with encrypted recipient and memo
    /// @dev The encrypted payload contains (to, memo) encrypted to the sequencer's key.
    ///      Validates that keyIndex is valid (exists and not expired).
    ///      Fee is NOT charged for encrypted deposits (no amount deduction).
    /// @param amount Amount to deposit (full amount, no fee deducted)
    /// @param keyIndex Index of the encryption key used (from encryptionKeyAt)
    /// @param encrypted The encrypted payload (recipient and memo)
    /// @return newCurrentDepositQueueHash The new deposit queue hash
    function depositEncrypted(
        uint128 amount,
        uint256 keyIndex,
        EncryptedDepositPayload calldata encrypted
    ) external returns (bytes32 newCurrentDepositQueueHash) {
        // Validate encryption key
        (bool valid, uint64 expiresAtBlock) = isEncryptionKeyValid(keyIndex);
        if (!valid) {
            if (keyIndex >= _encryptionKeys.length) {
                revert InvalidEncryptionKeyIndex(keyIndex);
            }
            EncryptionKeyEntry storage key = _encryptionKeys[keyIndex];
            EncryptionKeyEntry storage nextKey = _encryptionKeys[keyIndex + 1];
            revert EncryptionKeyExpired(keyIndex, key.activationBlock, nextKey.activationBlock);
        }

        // Transfer full amount from sender to this contract (no fee for encrypted deposits)
        ITIP20(token).transferFrom(msg.sender, address(this), amount);

        // Build encrypted deposit struct
        EncryptedDeposit memory depositData = EncryptedDeposit({
            sender: msg.sender,
            amount: amount,
            keyIndex: keyIndex,
            encrypted: encrypted
        });

        // Insert encrypted deposit into queue with type discriminator
        newCurrentDepositQueueHash = keccak256(abi.encode(
            DepositType.Encrypted,
            depositData,
            currentDepositQueueHash
        ));
        currentDepositQueueHash = newCurrentDepositQueueHash;

        emit EncryptedDepositMade(
            newCurrentDepositQueueHash,
            msg.sender,
            amount,
            keyIndex,
            encrypted.ephemeralPubkeyX,
            encrypted.ephemeralPubkeyYParity
        );
    }

    /*//////////////////////////////////////////////////////////////
                             WITHDRAWALS
    //////////////////////////////////////////////////////////////*/

    /// @notice Process the next withdrawal from the queue. Only callable by the sequencer.
    /// @dev Fee is always paid to sequencer regardless of success/failure.
    ///      On failure, only the amount (not fee) is bounced back.
    function processWithdrawal(
        Withdrawal calldata withdrawal,
        bytes32 remainingQueue
    )
        external
        onlySequencer
    {
        // Pop from withdrawal queue (library handles swap and hash verification)
        _withdrawalQueue.dequeue(withdrawal, remainingQueue);

        // Transfer fee to sequencer (always, regardless of withdrawal success)
        if (withdrawal.fee > 0) {
            ITIP20(token).transfer(sequencer, withdrawal.fee);
        }

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
        try IZoneMessenger(messenger)
            .relayMessage(
                withdrawal.sender,
                withdrawal.to,
                withdrawal.amount,
                withdrawal.gasLimit,
                withdrawal.callbackData
            ) {
            emit WithdrawalProcessed(withdrawal.to, withdrawal.amount, true);
        } catch {
            // Callback failed: bounce back to zone (only amount, not fee)
            _enqueueBounceBack(withdrawal.amount, withdrawal.fallbackRecipient);
            emit WithdrawalProcessed(withdrawal.to, withdrawal.amount, false);
        }
    }

    /// @notice Enqueue a bounce-back deposit for failed callback
    function _enqueueBounceBack(uint128 amount, address fallbackRecipient) internal {
        Deposit memory depositData = Deposit({
            sender: address(this), to: fallbackRecipient, amount: amount, memo: bytes32(0)
        });

        bytes32 newCurrentDepositQueueHash =
            DepositQueueLib.enqueue(currentDepositQueueHash, depositData);
        currentDepositQueueHash = newCurrentDepositQueueHash;

        emit BounceBack(newCurrentDepositQueueHash, fallbackRecipient, amount);
    }

    /*//////////////////////////////////////////////////////////////
                           BATCH SUBMISSION
    //////////////////////////////////////////////////////////////*/

    /// @notice Submit a batch and verify the proof. Only callable by the sequencer.
    /// @param tempoBlockNumber Block number zone committed to (from zone's TempoState)
    /// @param recentTempoBlockNumber Optional recent block for ancestry proof (0 = use direct lookup)
    function submitBatch(
        uint64 tempoBlockNumber,
        uint64 recentTempoBlockNumber,
        BlockTransition calldata blockTransition,
        DepositQueueTransition calldata depositQueueTransition,
        WithdrawalQueueTransition calldata withdrawalQueueTransition,
        bytes calldata verifierConfig,
        bytes calldata proof
    )
        external
        onlySequencer
    {
        if (blockTransition.prevBlockHash != blockHash) {
            revert InvalidProof();
        }

        // Validate tempoBlockNumber is valid (applies to both direct and ancestry modes)
        if (tempoBlockNumber < genesisTempoBlockNumber) revert InvalidTempoBlockNumber();

        // Determine anchor block: either tempoBlockNumber (direct) or recentTempoBlockNumber (ancestry)
        uint64 anchorBlockNumber;
        bytes32 anchorBlockHash;

        if (recentTempoBlockNumber == 0) {
            // Direct mode: read tempoBlockNumber hash from EIP-2935
            anchorBlockNumber = tempoBlockNumber;
            if (tempoBlockNumber > block.number) revert InvalidTempoBlockNumber();

            anchorBlockHash = IBlockHashHistory(BLOCKHASH_HISTORY).getBlockHash(tempoBlockNumber);
            if (anchorBlockHash == bytes32(0)) revert InvalidTempoBlockNumber();
        } else {
            // Ancestry mode: read recentTempoBlockNumber hash, proof verifies ancestry chain
            if (recentTempoBlockNumber <= tempoBlockNumber) revert InvalidTempoBlockNumber();
            if (recentTempoBlockNumber > block.number) revert InvalidTempoBlockNumber();

            anchorBlockNumber = recentTempoBlockNumber;
            anchorBlockHash =
                IBlockHashHistory(BLOCKHASH_HISTORY).getBlockHash(recentTempoBlockNumber);
            if (anchorBlockHash == bytes32(0)) revert InvalidTempoBlockNumber();
        }

        // Verify proof (handles both direct and ancestry modes)
        bool valid = IVerifier(verifier)
            .verify(
                tempoBlockNumber,
                anchorBlockNumber,
                anchorBlockHash,
                withdrawalBatchIndex + 1,
                sequencer,
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
