// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {EncryptedDepositPayload, EncryptedDeposit, EncryptionKeyEntry} from "./IZone.sol";

/// @title Encrypted Deposit Helpers
/// @notice Enables privacy-preserving deposits where recipient and memo are encrypted
/// @dev Users encrypt (to, memo) using the sequencer's public key. Only the sequencer
///      can decrypt and credit the correct recipient on the zone.
///
///      Encryption scheme: ECIES with secp256k1
///      - Sequencer publishes a secp256k1 public key (compressed, 33 bytes stored as bytes32 + 1 byte)
///      - User generates ephemeral keypair, derives shared secret via ECDH
///      - Plaintext (to || memo) is encrypted with AES-256-GCM using derived key
///      - Ciphertext includes ephemeral public key for sequencer to derive same shared secret
///
///      Types (EncryptedDepositPayload, EncryptedDeposit, EncryptionKeyEntry) are defined in IZone.sol.
///      Portal interface (depositEncrypted, key management) is in IZonePortal.

/*//////////////////////////////////////////////////////////////
                        ENCRYPTION CONSTANTS
//////////////////////////////////////////////////////////////*/

/// Size of the plaintext: 20 bytes (address) + 32 bytes (memo) = 52 bytes
/// Padded to 64 bytes for AES block alignment
uint256 constant ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE = 64;

/// Minimum ciphertext size (plaintext size, GCM doesn't expand)
uint256 constant MIN_CIPHERTEXT_SIZE = 64;

/*//////////////////////////////////////////////////////////////
                        ENCRYPTION HELPERS
//////////////////////////////////////////////////////////////*/

/// @notice Decrypted deposit (after sequencer decryption)
/// @dev This is what the sequencer works with internally on the zone
struct DecryptedDeposit {
    address sender;
    address to;
    uint128 amount;
    bytes32 memo;
}

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
                    UNIFIED DEPOSIT QUEUE PROCESSING
//////////////////////////////////////////////////////////////*/

/// @dev The deposit queue on Tempo contains both regular and encrypted deposits
///      in a single ordered sequence. The sequencer must provide decryption data
///      for encrypted deposits when processing the queue on the zone.
///
///      Queue hash chain includes type discriminator:
///      - Regular:   keccak256(abi.encode(DepositType.Regular, deposit, prevHash))
///      - Encrypted: keccak256(abi.encode(DepositType.Encrypted, encryptedDeposit, prevHash))
///
///      The zone's advanceTempo() processes deposits in order. For encrypted deposits,
///      the sequencer provides the decrypted (to, memo) alongside the encrypted data.
///      The proof/TEE validates that decryptions are correct.
///
///      Types QueuedDeposit and DecryptionData are defined in IZone.sol.
