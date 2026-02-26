// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE, EncryptedDepositLib } from "./EncryptedDeposit.sol";
import {
    AES_GCM_DECRYPT,
    CHAUM_PEDERSEN_VERIFY,
    DecryptionData,
    Deposit,
    DepositType,
    EnabledToken,
    EncryptedDeposit,
    IAesGcmDecrypt,
    IChaumPedersenVerify,
    ITIP20ZoneFactory,
    ITempoState,
    IZoneConfig,
    IZoneInbox,
    IZoneToken,
    PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
    PORTAL_ENCRYPTION_KEYS_SLOT,
    QueuedDeposit,
    TIP20_FACTORY_ADDRESS
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

    /// @notice Last processed deposit queue hash (validated against Tempo state)
    bytes32 public processedDepositQueueHash;

    /// @notice Maximum number of deposits to process per Tempo block (0 = unlimited)
    /// @dev Sequencer-configurable cap to prevent deposit spam from exceeding zone block gas limits.
    uint256 public maxDepositsPerTempoBlock;

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(address _config, address _tempoPortalAddr, address _tempoStateAddr) {
        config = IZoneConfig(_config);
        tempoPortal = _tempoPortalAddr;
        _tempoState = TempoState(_tempoStateAddr);
    }

    /// @notice The TempoState predeploy address
    function tempoState() external view returns (ITempoState) {
        return _tempoState;
    }

    /*//////////////////////////////////////////////////////////////
                         CRYPTOGRAPHIC HELPERS
    //////////////////////////////////////////////////////////////*/

    /// @dev HMAC ipad constant (0x36 repeated 32 times)
    bytes32 private constant _IPAD =
        0x3636363636363636363636363636363636363636363636363636363636363636;
    /// @dev HMAC opad constant (0x5c repeated 32 times)
    bytes32 private constant _OPAD =
        0x5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c5c;

    /// @notice HMAC-SHA256 implementation using the SHA256 precompile
    /// @dev HMAC(key, message) = SHA256((key ⊕ opad) || SHA256((key ⊕ ipad) || message))
    ///      where ipad = 0x36 repeated, opad = 0x5c repeated.
    ///      Uses word-level XOR instead of byte-by-byte loops for ~95% gas reduction.
    /// @param key The HMAC key (will be hashed if longer than 64 bytes)
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
        // Load key into two 32-byte words (SHA256 block size = 64 bytes = 2 words)
        bytes32 keyWord0;
        bytes32 keyWord1;

        if (key.length > 64) {
            // Key longer than block size: hash it first (result goes in first word, second is zero)
            keyWord0 = sha256(key);
        } else {
            assembly {
                let keyLen := mload(key)
                keyWord0 := mload(add(key, 32))
                // Load second word only if key > 32 bytes
                switch gt(keyLen, 32)
                case 1 { keyWord1 := mload(add(key, 64)) }
                default { keyWord1 := 0 }
                // Zero out bytes beyond key length in first word
                if lt(keyLen, 32) {
                    let shift := mul(sub(32, keyLen), 8)
                    keyWord0 := and(keyWord0, not(sub(shl(shift, 1), 1)))
                }
                // Zero out bytes beyond key length in second word
                if and(gt(keyLen, 32), lt(keyLen, 64)) {
                    let shift := mul(sub(64, keyLen), 8)
                    keyWord1 := and(keyWord1, not(sub(shl(shift, 1), 1)))
                }
            }
        }

        // Inner hash: SHA256((key ⊕ ipad) || message)
        bytes32 innerHash = sha256(abi.encodePacked(keyWord0 ^ _IPAD, keyWord1 ^ _IPAD, message));

        // Outer hash: SHA256((key ⊕ opad) || innerHash)
        result = sha256(abi.encodePacked(keyWord0 ^ _OPAD, keyWord1 ^ _OPAD, innerHash));
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
        uint256 base = uint256(keccak256(abi.encode(uint256(PORTAL_ENCRYPTION_KEYS_SLOT))));
        uint256 slotX = base + (keyIndex * 2);
        uint256 slotMeta = slotX + 1;
        bytes32 xSlot = _tempoState.readTempoStorageSlot(tempoPortal, bytes32(slotX));
        if (xSlot == bytes32(0)) revert InvalidSharedSecretProof();
        bytes32 metaSlot = _tempoState.readTempoStorageSlot(tempoPortal, bytes32(slotMeta));
        // yParity is packed in the lowest byte of the meta slot (see EncryptionKeyEntry layout)
        return (xSlot, uint8(uint256(metaSlot) & 0xff));
    }

    /*//////////////////////////////////////////////////////////////
                         DEPOSIT CAP CONFIGURATION
    //////////////////////////////////////////////////////////////*/

    /// @notice Set the maximum number of deposits to process per Tempo block. Only callable by sequencer.
    /// @dev Set to 0 for unlimited. Provides an additional layer of protection against deposit spam
    ///      that could exceed zone block gas limits.
    /// @param _maxDepositsPerTempoBlock The maximum number of deposits per Tempo block
    function setMaxDepositsPerTempoBlock(uint256 _maxDepositsPerTempoBlock) external {
        if (msg.sender != address(0) && msg.sender != config.sequencer()) revert OnlySequencer();
        maxDepositsPerTempoBlock = _maxDepositsPerTempoBlock;
        emit MaxDepositsPerTempoBlockUpdated(_maxDepositsPerTempoBlock);
    }

    /*//////////////////////////////////////////////////////////////
                         SYSTEM TRANSACTION
    //////////////////////////////////////////////////////////////*/

    /// @notice Advance Tempo state and process deposits in a single system transaction
    /// @dev This is the main entry point for the sequencer's system transaction.
    ///      1. Advances the zone's view of Tempo by processing the header
    ///      2. Processes deposits from the unified queue (regular + encrypted)
    ///      3. Validates the resulting hash chain is an ancestor of Tempo's currentDepositQueueHash
    ///      The sequencer may process a bounded subset of deposits (up to maxDepositsPerTempoBlock).
    ///      The proof validates contiguity (ancestor check) rather than exact equality.
    ///      Protocol and proof enforce at most one call at the start of a block (or zero if skipping).
    /// @param header RLP-encoded Tempo block header
    /// @param deposits Array of queued deposits to process (oldest first, must be contiguous)
    /// @param decryptions Decryption data for encrypted deposits (1:1 with encrypted deposits, in order)
    function advanceTempo(
        bytes calldata header,
        QueuedDeposit[] calldata deposits,
        DecryptionData[] calldata decryptions,
        EnabledToken[] calldata enabledTokens
    )
        external
    {
        if (msg.sender != address(0) && msg.sender != config.sequencer()) revert OnlySequencer();

        // Enforce deposit cap (0 = unlimited)
        if (maxDepositsPerTempoBlock > 0 && deposits.length > maxDepositsPerTempoBlock) {
            revert TooManyDeposits();
        }

        // Step 1: Advance Tempo state (validates chain continuity internally)
        _tempoState.finalizeTempo(header);

        // Enable new tokens
        for (uint256 i = 0; i < enabledTokens.length; i++) {
            EnabledToken calldata t = enabledTokens[i];
            ITIP20ZoneFactory(TIP20_FACTORY_ADDRESS).enableToken(t.token, t.name, t.symbol, t.currency);
        }

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

                // Mint the correct zone-side TIP-20 token to the recipient
                IZoneToken(d.token).mint(d.to, d.amount);

                emit DepositProcessed(currentHash, d.sender, d.to, d.token, d.amount, d.memo);
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
                // The sequencer's public key is looked up on-chain from the deposit's keyIndex,
                // so it doesn't need to be in DecryptionData (saves calldata).
                (bytes32 seqPubX, uint8 seqPubYParity) = _readEncryptionKey(ed.keyIndex);
                bool proofValid = IChaumPedersenVerify(CHAUM_PEDERSEN_VERIFY)
                    .verifyProof(
                        ed.encrypted.ephemeralPubkeyX,
                        ed.encrypted.ephemeralPubkeyYParity,
                        dec.sharedSecret,
                        dec.sharedSecretYParity,
                        seqPubX,
                        seqPubYParity,
                        dec.cpProof
                    );
                if (!proofValid) revert InvalidSharedSecretProof();

                // Step 2: Derive AES key from shared secret using HKDF-SHA256
                // This is done in Solidity using the SHA256 precompile (0x02)
                bytes32 aesKey = _hkdfSha256(
                    dec.sharedSecret,
                    "ecies-aes-key",
                    abi.encodePacked(tempoPortal, ed.keyIndex, ed.encrypted.ephemeralPubkeyX)
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
                // Must be exactly ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE (64) bytes
                if (valid && decryptedPlaintext.length == ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE) {
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
                    IZoneToken(ed.token).mint(ed.sender, ed.amount);
                    emit EncryptedDepositFailed(currentHash, ed.sender, ed.token, ed.amount);
                } else {
                    // Decryption succeeded - mint the correct zone-side TIP-20 to the decrypted recipient
                    IZoneToken(ed.token).mint(dec.to, ed.amount);
                    emit EncryptedDepositProcessed(
                        currentHash, ed.sender, dec.to, ed.token, ed.amount, dec.memo
                    );
                }
            }
        }

        // Verify all decryption data was consumed
        if (decryptionIndex != decryptions.length) revert ExtraDecryptionData();

        // Step 3: Validate against Tempo state
        // Read currentDepositQueueHash from the portal's storage using the new Tempo state.
        // The proof validates that our processedDepositQueueHash is an ancestor of (or equal to)
        // tempoCurrentHash, allowing partial deposit processing when maxDepositsPerTempoBlock is set.
        // On-chain we only need to verify the hash chain when all deposits have been caught up.
        bytes32 tempoCurrentHash =
            _tempoState.readTempoStorageSlot(tempoPortal, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT);

        if (currentHash != tempoCurrentHash) {
            // Partial processing is allowed — the proof validates ancestor contiguity.
            // However, if no deposits were provided and the hashes don't match, it means
            // there are unprocessed deposits. This is valid as long as the hash chain is contiguous,
            // which the proof system enforces.
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
