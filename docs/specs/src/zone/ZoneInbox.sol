// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    AES_GCM_DECRYPT,
    CHAUM_PEDERSEN_VERIFY,
    DecryptionData,
    Deposit,
    DepositType,
    EncryptedDeposit,
    IAesGcmDecrypt,
    IChaumPedersenVerify,
    ITempoState,
    IZoneConfig,
    IZoneInbox,
    IZoneToken,
    QueuedDeposit
} from "./IZone.sol";
import { EncryptedDepositLib } from "./EncryptedDeposit.sol";
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
    ///      slot 3: zoneGasRate (uint128) + withdrawalBatchIndex (uint64) [packed]
    ///      slot 4: blockHash (bytes32)
    ///      slot 5: currentDepositQueueHash (bytes32)
    ///      slot 6: lastSyncedTempoBlockNumber (uint64)
    ///      slot 7: _encryptionKeys (EncryptionKeyEntry[])
    bytes32 internal constant CURRENT_DEPOSIT_QUEUE_HASH_SLOT = bytes32(uint256(5));
    bytes32 internal constant ENCRYPTION_KEYS_SLOT = bytes32(uint256(7));

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

    /*//////////////////////////////////////////////////////////////
                         CRYPTOGRAPHIC HELPERS
    //////////////////////////////////////////////////////////////*/

    /// @notice HMAC-SHA256 implementation using the SHA256 precompile
    /// @dev HMAC(key, message) = SHA256((key ⊕ opad) || SHA256((key ⊕ ipad) || message))
    ///      where ipad = 0x36 repeated, opad = 0x5c repeated
    /// @param key The HMAC key (will be padded/hashed if longer than 64 bytes)
    /// @param message The message to authenticate
    /// @return result The 32-byte HMAC-SHA256 output
    function _hmacSha256(
        bytes memory key,
        bytes memory message
    )
        internal
        view
        returns (bytes32 result)
    {
        // SHA256 block size is 64 bytes
        bytes memory paddedKey = new bytes(64);

        if (key.length > 64) {
            // If key is longer than block size, hash it first
            bytes32 hashedKey = sha256(key);
            assembly {
                mstore(add(paddedKey, 32), hashedKey)
            }
        } else {
            // Copy key and pad with zeros
            for (uint256 i = 0; i < key.length; i++) {
                paddedKey[i] = key[i];
            }
        }

        // Compute inner and outer padded keys
        bytes memory innerKey = new bytes(64);
        bytes memory outerKey = new bytes(64);
        for (uint256 i = 0; i < 64; i++) {
            innerKey[i] = bytes1(uint8(paddedKey[i]) ^ 0x36);
            outerKey[i] = bytes1(uint8(paddedKey[i]) ^ 0x5c);
        }

        // Inner hash: SHA256((key ⊕ ipad) || message)
        bytes memory innerData = bytes.concat(innerKey, message);
        bytes32 innerHash = sha256(innerData);

        // Outer hash: SHA256((key ⊕ opad) || innerHash)
        bytes memory outerData = bytes.concat(outerKey, abi.encodePacked(innerHash));
        result = sha256(outerData);
    }

    /// @notice HKDF-SHA256 key derivation (simplified single-output version)
    /// @dev Implements HKDF-Extract and HKDF-Expand to derive a 32-byte key
    ///      from the input key material (shared secret).
    /// @param ikm Input key material (the ECDH shared secret)
    /// @param salt Salt value (use "ecies-aes-key" for ECIES)
    /// @param info Context-specific info (typically empty for ECIES)
    /// @return okm Output key material (32 bytes for AES-256)
    function _hkdfSha256(
        bytes32 ikm,
        bytes memory salt,
        bytes memory info
    )
        internal
        view
        returns (bytes32 okm)
    {
        // HKDF-Extract: PRK = HMAC-SHA256(salt, IKM)
        bytes32 prk = _hmacSha256(salt, abi.encodePacked(ikm));

        // HKDF-Expand: OKM = HMAC-SHA256(PRK, info || 0x01)
        // We only need 32 bytes (one block), so N=1 and we append 0x01
        bytes memory expandInput = bytes.concat(info, hex"01");
        okm = _hmacSha256(abi.encodePacked(prk), expandInput);
    }

    function _readEncryptionKey(uint256 keyIndex) internal view returns (bytes32 x, uint8 yParity) {
        uint256 base = uint256(keccak256(abi.encode(uint256(ENCRYPTION_KEYS_SLOT))));
        uint256 slotX = base + (keyIndex * 2);
        uint256 slotMeta = slotX + 1;
        bytes32 xSlot = _tempoState.readTempoStorageSlot(tempoPortal, bytes32(slotX));
        if (xSlot == bytes32(0)) revert InvalidSharedSecretProof();
        bytes32 metaSlot = _tempoState.readTempoStorageSlot(tempoPortal, bytes32(slotMeta));
        return (xSlot, uint8(uint256(metaSlot) & 0xff));
    }

    /*//////////////////////////////////////////////////////////////
                         SYSTEM TRANSACTION
    //////////////////////////////////////////////////////////////*/

    /// @notice Advance Tempo state and process deposits in a single system transaction
    /// @dev This is the main entry point for the sequencer's system transaction.
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
    )
        external
    {
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

                // Step 1: Verify Chaum-Pedersen proof of correct shared secret derivation
                // This prevents griefing attacks where users encrypt with wrong keys,
                // without exposing the sequencer's private key to the EVM.
                // The proof verifies that sharedSecret = privSeq * ephemeralPub without revealing privSeq.
                (bytes32 expectedPubX, uint8 expectedYParity) = _readEncryptionKey(ed.keyIndex);
                if (dec.sequencerPubX != expectedPubX || dec.sequencerPubYParity != expectedYParity)
                {
                    revert InvalidSharedSecretProof();
                }
                bool proofValid = IChaumPedersenVerify(CHAUM_PEDERSEN_VERIFY)
                    .verifyProof(
                        ed.encrypted.ephemeralPubkeyX,
                        ed.encrypted.ephemeralPubkeyYParity,
                        dec.sharedSecret,
                        dec.sequencerPubX,
                        dec.sequencerPubYParity,
                        dec.cpProof
                    );
                if (!proofValid) revert InvalidSharedSecretProof();

                // Step 2: Derive AES key from shared secret using HKDF-SHA256
                // This is done in Solidity using the SHA256 precompile (0x02)
                bytes32 aesKey = _hkdfSha256(
                    dec.sharedSecret,
                    "ecies-aes-key", // salt
                    "" // info (empty)
                );

                // Step 3: Decrypt using AES-256-GCM precompile
                // The GCM tag proves the plaintext matches the ciphertext for this shared secret
                (bytes memory decryptedPlaintext, bool valid) = IAesGcmDecrypt(AES_GCM_DECRYPT)
                    .decrypt(
                        aesKey,
                        ed.encrypted.nonce,
                        ed.encrypted.ciphertext,
                        "", // empty AAD
                        ed.encrypted.tag
                    );

                // Step 4: Verify decrypted plaintext matches claimed (to, memo)
                // Plaintext is packed as [address(20 bytes)][memo(32 bytes)][padding(12 bytes)]
                // Must use packed decoding (not abi.decode which expects 32-byte padded fields)
                if (valid && decryptedPlaintext.length >= 52) {
                    (address decryptedTo, bytes32 decryptedMemo) =
                        EncryptedDepositLib.decodePlaintext(decryptedPlaintext);
                    valid = (decryptedTo == dec.to && decryptedMemo == dec.memo);
                } else {
                    valid = false;
                }

                // Advance the hash chain with type discriminator
                currentHash = keccak256(abi.encode(DepositType.Encrypted, ed, currentHash));

                if (!valid) {
                    // Decryption failed (user encrypted garbage or corrupted data)
                    // Return funds to sender instead of blocking chain progress
                    gasToken.mint(ed.sender, ed.amount);
                    emit EncryptedDepositFailed(currentHash, ed.sender, ed.amount);
                } else {
                    // Decryption succeeded - mint to the decrypted recipient
                    gasToken.mint(dec.to, ed.amount);
                    emit EncryptedDepositProcessed(
                        currentHash, ed.sender, dec.to, ed.amount, dec.memo
                    );
                }
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
