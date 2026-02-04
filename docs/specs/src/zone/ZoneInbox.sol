// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    Deposit,
    EncryptedDeposit,
    DepositType,
    QueuedDeposit,
    DecryptionData,
    ITempoState,
    IZoneConfig,
    IZoneInbox,
    IZoneToken
} from "./IZone.sol";
import { TempoState } from "./TempoState.sol";

/// @title ZoneInbox
/// @notice Zone-side system contract for advancing Tempo state and processing deposits
/// @dev Called by sequencer. Combines Tempo header advancement
///      with deposit queue processing in a single atomic operation.
contract ZoneInbox is IZoneInbox {

    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    /// @notice Zone configuration (reads sequencer from L1)
    IZoneConfig public immutable config;

    /// @notice The Tempo portal address (for reading deposit queue hash)
    address public immutable tempoPortal;

    /// @notice The TempoState predeploy address (stored as concrete type for internal use)
    TempoState internal immutable _tempoState;

    /// @notice The zone token (TIP-20 at same address as Tempo)
    IZoneToken public immutable gasToken;

    /// @notice Last processed deposit queue hash (validated against Tempo state)
    bytes32 public processedDepositQueueHash;

    /// @notice Storage slot for currentDepositQueueHash in ZonePortal
    /// @dev ZonePortal storage layout (non-immutable variables only):
    ///      slot 0: sequencer (address)
    ///      slot 1: pendingSequencer (address)
    ///      slot 2: sequencerPubkey (bytes32)
    ///      slot 3: withdrawalBatchIndex (uint64)
    ///      slot 4: blockHash (bytes32)
    ///      slot 5: currentDepositQueueHash (bytes32) ← this one
    ///      slot 6: lastSyncedTempoBlockNumber (uint64)
    bytes32 internal constant CURRENT_DEPOSIT_QUEUE_HASH_SLOT = bytes32(uint256(5));

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(
        address _config,
        address _tempoPortalAddr,
        address _tempoStateAddr,
        address _gasToken
    ) {
        config = IZoneConfig(_config);
        tempoPortal = _tempoPortalAddr;
        _tempoState = TempoState(_tempoStateAddr);
        gasToken = IZoneToken(_gasToken);
    }

    /// @notice The TempoState predeploy address
    function tempoState() external view returns (ITempoState) {
        return _tempoState;
    }

    /// @notice Advance Tempo state and process deposits in a single sequencer-only call
    /// @dev This is the main entry point for the sequencer at block start when Tempo is advanced.
    ///      1. Advances the zone's view of Tempo by processing the header
    ///      2. Processes deposits from the unified queue (regular + encrypted)
    ///      3. Validates the resulting hash against Tempo's currentDepositQueueHash
    ///      Protocol and proof enforce at most one call at the start of a block (or zero if skipping).
    /// @param header RLP-encoded Tempo block header
    /// @param deposits Array of queued deposits to process (oldest first, must be contiguous)
    /// @param decryptions Decryption data for encrypted deposits (1:1 with encrypted deposits, in order)
    function advanceTempo(
        bytes calldata header,
        QueuedDeposit[] calldata deposits,
        DecryptionData[] calldata decryptions
    ) external {
        if (msg.sender != config.sequencer()) revert OnlySequencer();

        // Step 1: Advance Tempo state (validates chain continuity internally)
        _tempoState.finalizeTempo(header);

        // Step 2: Process deposits and build hash chain
        bytes32 currentHash = processedDepositQueueHash;
        uint256 decryptionIndex = 0;

        for (uint256 i = 0; i < deposits.length; i++) {
            QueuedDeposit calldata qd = deposits[i];

            if (qd.depositType == DepositType.Regular) {
                // Decode regular deposit
                Deposit memory d = abi.decode(qd.depositData, (Deposit));

                // Advance the hash chain with type discriminator
                currentHash = keccak256(abi.encode(DepositType.Regular, d, currentHash));

                // Mint zone tokens to the recipient
                gasToken.mint(d.to, d.amount);

                emit DepositProcessed(currentHash, d.sender, d.to, d.amount, d.memo);
            } else {
                // Decode encrypted deposit
                EncryptedDeposit memory ed = abi.decode(qd.depositData, (EncryptedDeposit));

                // Sequencer must provide decryption for this encrypted deposit
                if (decryptionIndex >= decryptions.length) revert MissingDecryptionData();
                DecryptionData calldata dec = decryptions[decryptionIndex++];

                // Advance the hash chain with type discriminator
                currentHash = keccak256(abi.encode(DepositType.Encrypted, ed, currentHash));

                // Mint zone tokens to the decrypted recipient
                // Note: Proof/TEE validates the decryption is correct
                gasToken.mint(dec.to, ed.amount);

                emit EncryptedDepositProcessed(currentHash, ed.sender, dec.to, ed.amount, dec.memo);
            }
        }

        // Verify all decryption data was consumed
        if (decryptionIndex != decryptions.length) revert ExtraDecryptionData();

        // Step 3: Validate against Tempo state
        // Read currentDepositQueueHash from the portal's storage using the new Tempo state
        bytes32 tempoCurrentHash =
            _tempoState.readTempoStorageSlot(tempoPortal, CURRENT_DEPOSIT_QUEUE_HASH_SLOT);

        // Our processed hash must match Tempo's current hash for now.
        // TODO: Implement recursive ancestor check in proof or on-chain as a fallback.
        if (currentHash != tempoCurrentHash) {
            revert InvalidDepositQueueHash();
        }

        // Step 4: Update state
        processedDepositQueueHash = currentHash;

        emit TempoAdvanced(
            _tempoState.tempoBlockHash(),
            _tempoState.tempoBlockNumber(),
            deposits.length,
            currentHash
        );
    }

}
