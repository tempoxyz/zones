// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/// @title Encrypted Deposit Types and Interface
/// @notice Enables privacy-preserving deposits where recipient and memo are encrypted
/// @dev Users encrypt (to, memo) using the sequencer's public key. Only the sequencer
///      can decrypt and credit the correct recipient on the zone.
///
///      Encryption scheme: ECIES with secp256k1
///      - Sequencer publishes a secp256k1 public key (compressed, 33 bytes stored as bytes32 + 1 byte)
///      - User generates ephemeral keypair, derives shared secret via ECDH
///      - Plaintext (to || memo) is encrypted with AES-256-GCM using derived key
///      - Ciphertext includes ephemeral public key for sequencer to derive same shared secret

/*//////////////////////////////////////////////////////////////
                        ENCRYPTED DEPOSIT TYPES
//////////////////////////////////////////////////////////////*/

/// @notice Encrypted deposit payload
/// @dev Contains the ciphertext of (recipient address, memo) encrypted to the sequencer's pubkey
struct EncryptedDepositPayload {
    bytes32 ephemeralPubkeyX;   // Ephemeral public key X coordinate (for ECDH)
    uint8 ephemeralPubkeyYParity; // Y coordinate parity (0x02 or 0x03)
    bytes ciphertext;           // AES-256-GCM encrypted (to || memo || padding)
    bytes12 nonce;              // GCM nonce
    bytes16 tag;                // GCM authentication tag
}

/// @notice Encrypted deposit stored in the queue
/// @dev The sender, amount, and deposit block are public; recipient and memo are encrypted.
///      The tempoBlockNumber is recorded so the prover knows which encryption key was valid.
struct EncryptedDeposit {
    address sender;             // Depositor (public, needed for refunds)
    uint128 amount;             // Deposit amount (public, needed for accounting)
    uint64 tempoBlockNumber;    // Tempo block when deposit was made (for key lookup)
    EncryptedDepositPayload encrypted; // Encrypted (to, memo)
}

/// @notice Decrypted deposit (after sequencer decryption)
/// @dev This is what the sequencer works with internally
struct DecryptedDeposit {
    address sender;
    address to;
    uint128 amount;
    bytes32 memo;
}

/*//////////////////////////////////////////////////////////////
                        ENCRYPTION CONSTANTS
//////////////////////////////////////////////////////////////*/

/// Size of the plaintext: 20 bytes (address) + 32 bytes (memo) = 52 bytes
/// Padded to 64 bytes for AES block alignment
uint256 constant ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE = 64;

/// Minimum ciphertext size (plaintext size, GCM doesn't expand)
uint256 constant MIN_CIPHERTEXT_SIZE = 64;

/*//////////////////////////////////////////////////////////////
                        PORTAL INTERFACE EXTENSION
//////////////////////////////////////////////////////////////*/

/// @title IEncryptedDeposits
/// @notice Extension interface for encrypted deposits on ZonePortal
interface IEncryptedDeposits {
    /// @notice Emitted when an encrypted deposit is made
    /// @dev Recipient and memo are NOT included - they're encrypted
    event EncryptedDepositMade(
        bytes32 indexed newCurrentDepositQueueHash,
        address indexed sender,
        uint128 amount,
        bytes32 ephemeralPubkeyX,
        uint8 ephemeralPubkeyYParity
    );

    /// @notice Deposit with encrypted recipient and memo
    /// @dev The encrypted payload contains (to, memo) encrypted to sequencerPubkey.
    ///      Only the sequencer can decrypt and credit the correct recipient.
    ///      If the sequencer cannot decrypt (malformed), they may reject or refund.
    /// @param amount Amount to deposit
    /// @param encrypted The encrypted payload (recipient and memo)
    /// @return newCurrentDepositQueueHash The new deposit queue hash
    function depositEncrypted(
        uint128 amount,
        EncryptedDepositPayload calldata encrypted
    ) external returns (bytes32 newCurrentDepositQueueHash);
}

/*//////////////////////////////////////////////////////////////
                        ENCRYPTION HELPERS
//////////////////////////////////////////////////////////////*/

/// @title EncryptedDepositLib
/// @notice Library for working with encrypted deposits
/// @dev These are reference implementations - actual encryption happens off-chain
library EncryptedDepositLib {
    /// @notice Compute the hash of an encrypted deposit for the queue
    /// @dev Uses the encrypted form, not the decrypted form
    function hash(EncryptedDeposit memory deposit) internal pure returns (bytes32) {
        return keccak256(abi.encode(deposit));
    }

    /// @notice Encode plaintext for encryption
    /// @dev Packs (to, memo) into 64 bytes for encryption
    function encodePlaintext(address to, bytes32 memo) internal pure returns (bytes memory) {
        bytes memory plaintext = new bytes(64);
        assembly {
            // Store address at offset 0 (left-padded to 32 bytes, we take last 20)
            mstore(add(plaintext, 32), shl(96, to))
            // Store memo at offset 20
            mstore(add(plaintext, 52), memo)
        }
        return plaintext;
    }

    /// @notice Decode plaintext after decryption
    /// @dev Unpacks (to, memo) from 64 bytes
    function decodePlaintext(bytes memory plaintext) internal pure returns (address to, bytes32 memo) {
        require(plaintext.length >= 52, "Invalid plaintext length");
        assembly {
            to := shr(96, mload(add(plaintext, 32)))
            memo := mload(add(plaintext, 52))
        }
    }
}

/*//////////////////////////////////////////////////////////////
                        SEQUENCER KEY MANAGEMENT
//////////////////////////////////////////////////////////////*/

/// @notice Sequencer public key format for ECIES
/// @dev Compressed secp256k1 public key: 1 byte prefix (0x02/0x03) + 32 bytes X coordinate
///      Stored as bytes32 (X) + uint8 (parity) to save gas
struct SequencerEncryptionKey {
    bytes32 x;      // X coordinate of the public key
    uint8 yParity;  // 0x02 for even Y, 0x03 for odd Y
}

/// @notice Historical record of an encryption key with its activation block
/// @dev Used to track key rotations so the prover can determine which key
///      was valid when a deposit was made
struct EncryptionKeyEntry {
    bytes32 x;              // X coordinate of the public key
    uint8 yParity;          // Y coordinate parity (0x02 or 0x03)
    uint64 activationBlock; // Tempo block number when this key became active
}

/// @title ISequencerEncryptionKey
/// @notice Interface for managing sequencer encryption keys with history
/// @dev Key history is maintained so that:
///      1. Users can verify they're encrypting to the current key
///      2. The prover can determine which key was valid for any deposit
///      3. Deposits made with old keys (during rotation) can still be decrypted
interface ISequencerEncryptionKey {
    /// @notice Emitted when sequencer updates their encryption key
    /// @param x The X coordinate of the new key
    /// @param yParity The Y coordinate parity (0x02 or 0x03)
    /// @param keyIndex The index of this key in the history array
    /// @param activationBlock The Tempo block when this key becomes active
    event SequencerEncryptionKeyUpdated(
        bytes32 x, 
        uint8 yParity, 
        uint256 keyIndex,
        uint64 activationBlock
    );

    /// @notice Get the sequencer's current encryption public key
    /// @return x The X coordinate
    /// @return yParity The Y coordinate parity (0x02 or 0x03)
    function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity);

    /// @notice Set the sequencer's encryption public key
    /// @dev Only callable by the sequencer. Appends to key history.
    ///      The new key becomes active at the current Tempo block.
    /// @param x The X coordinate
    /// @param yParity The Y coordinate parity (0x02 or 0x03)
    function setSequencerEncryptionKey(bytes32 x, uint8 yParity) external;

    /// @notice Get the number of keys in the history
    /// @return The total count of keys (including current)
    function encryptionKeyCount() external view returns (uint256);

    /// @notice Get a historical encryption key by index
    /// @param index The index in the key history (0 = first key)
    /// @return entry The key entry with activation block
    function encryptionKeyAt(uint256 index) external view returns (EncryptionKeyEntry memory entry);

    /// @notice Get the encryption key that was active at a specific Tempo block
    /// @dev Binary search through key history to find the correct key
    /// @param tempoBlockNumber The Tempo block number to query
    /// @return x The X coordinate of the active key
    /// @return yParity The Y coordinate parity
    /// @return keyIndex The index of this key in history
    function encryptionKeyAtBlock(uint64 tempoBlockNumber) 
        external view returns (bytes32 x, uint8 yParity, uint256 keyIndex);
}

/*//////////////////////////////////////////////////////////////
                        ZONE INBOX EXTENSION
//////////////////////////////////////////////////////////////*/

/// @title IEncryptedDepositProcessor
/// @notice Extension interface for ZoneInbox to process encrypted deposits
interface IEncryptedDepositProcessor {
    /// @notice Emitted when an encrypted deposit is processed (decrypted and credited)
    event EncryptedDepositProcessed(
        bytes32 indexed depositHash,
        address indexed sender,
        address indexed to,  // Now revealed after decryption
        uint128 amount,
        bytes32 memo         // Now revealed after decryption
    );

    /// @notice Advance Tempo state and process deposits including encrypted ones
    /// @dev The sequencer provides decrypted versions of encrypted deposits.
    ///      The proof must validate that decryptions are correct (or use TEE attestation).
    /// @param header RLP-encoded Tempo block header
    /// @param deposits Array of regular deposits
    /// @param encryptedDeposits Array of encrypted deposits (from Tempo)
    /// @param decryptedRecipients Decrypted recipients for encrypted deposits (same order)
    /// @param decryptedMemos Decrypted memos for encrypted deposits (same order)
    function advanceTempoWithEncrypted(
        bytes calldata header,
        /* regular Deposit[] */ bytes calldata deposits,
        EncryptedDeposit[] calldata encryptedDeposits,
        address[] calldata decryptedRecipients,
        bytes32[] calldata decryptedMemos
    ) external;
}
