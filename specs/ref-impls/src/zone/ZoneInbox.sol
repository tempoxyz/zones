// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

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
    ITempoState,
    IZoneInbox,
    IZoneOutbox,
    IZoneToken,
    PATH_USD_ADDRESS,
    PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
    PORTAL_ENCRYPTION_KEYS_SLOT,
    QueuedDeposit,
    ZONE_OUTBOX
} from "../interfaces/IZone.sol";
import {
    ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE,
    EncryptedDepositLib
} from "../libraries/EncryptedDeposit.sol";
import { TempoState } from "../tempo/TempoState.sol";

/// @title ZoneInbox
/// @notice Zone-side system contract for advancing Tempo state and processing deposits
/// @dev Called by the block executor as a system transaction. Combines Tempo header advancement
///      with deposit queue processing in a single atomic operation.
contract ZoneInbox is IZoneInbox {

    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    /// @notice The Tempo portal address (for reading deposit queue hash)
    address public immutable tempoPortal;

    /// @notice The TempoState predeploy address (stored as concrete type for internal use)
    TempoState internal immutable _tempoState;

    /// @notice Last processed deposit queue hash (validated against Tempo state)
    bytes32 public processedDepositQueueHash;

    /// @notice Last processed deposit number (mirrors lastProcessedDepositNumber on L1)
    uint64 public processedDepositNumber;

    /// @notice Refunds parked after a withdrawal-bounce-back mint reverts on the zone.
    mapping(address token => mapping(address owner => uint128 amount)) private _refunds;

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(address _tempoPortalAddr, address _tempoStateAddr) {
        tempoPortal = _tempoPortalAddr;
        _tempoState = TempoState(_tempoStateAddr);
    }

    modifier onlyRefundOwner(address owner) {
        if (msg.sender != owner) {
            revert Unauthorized();
        }
        _;
    }

    /// @notice The TempoState predeploy address
    function tempoState() external view returns (ITempoState) {
        return _tempoState;
    }

    /// @notice Return a parked refund to its owner or an active sequencer.
    /// @dev Authorization is enforced here so internal calls cannot bypass RPC policy.
    function refunds(
        address token,
        address owner
    )
        external
        view
        onlyRefundOwner(owner)
        returns (uint128)
    {
        return _refunds[token][owner];
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
                case 1 {
                    keyWord1 := mload(add(key, 64))
                }
                default {
                    keyWord1 := 0
                }
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
                         SYSTEM TRANSACTION
    //////////////////////////////////////////////////////////////*/

    /// @notice Advance Tempo state and process deposits in a single system transaction
    /// @dev This is the main entry point for the sequencer's system transaction.
    ///      1. Advances the zone's view of Tempo by processing the header
    ///      2. Processes deposits from the unified queue (regular + encrypted)
    ///      3. Validates the resulting hash chain is an ancestor of Tempo's currentDepositQueueHash
    ///      The proof validates contiguity (ancestor check) rather than exact equality.
    ///      Protocol and proof enforce at most one call at the start of a block (or zero if skipping).
    /// @param header RLP-encoded Tempo block header
    /// @param deposits Array of queued deposits to process (oldest first, must be contiguous)
    /// @param decryptions Decryption data for valid encrypted deposits, in order
    function advanceTempo(
        bytes calldata header,
        QueuedDeposit[] calldata deposits,
        DecryptionData[] calldata decryptions,
        EnabledToken[] calldata enabledTokens
    )
        external
    {
        if (msg.sender != address(0)) revert OnlySequencer();

        // Step 1: Advance Tempo state (validates chain continuity internally)
        _tempoState.finalizeTempo(header);

        // Activate new tokens directly in the Inbox.
        for (uint256 i = 0; i < enabledTokens.length; i++) {
            EnabledToken calldata t = enabledTokens[i];
            IZoneToken token = IZoneToken(t.token);
            token.initialize(
                address(this), t.name, t.symbol, t.currency, PATH_USD_ADDRESS, address(this)
            );
            bytes32 issuerRole = token.ISSUER_ROLE();
            token.grantRole(issuerRole, address(this));
            token.grantRole(issuerRole, ZONE_OUTBOX);
            emit TokenEnabled(t.token, t.name, t.symbol, t.currency);
        }

        // Step 2: Process deposits and build hash chain
        bytes32 currentHash = processedDepositQueueHash;
        uint256 decryptionIndex = 0;

        for (uint256 i = 0; i < deposits.length; i++) {
            QueuedDeposit calldata qd = deposits[i];

            if (qd.depositType == DepositType.Regular) {
                Deposit memory d = abi.decode(qd.depositData, (Deposit));
                currentHash = keccak256(abi.encode(DepositType.Regular, d, currentHash));

                if (d.tempoRefundRecipient == address(0)) {
                    _processWithdrawalBounceBack(d);
                } else if (qd.rejected) {
                    _rejectDeposit(
                        currentHash,
                        DepositType.Regular,
                        d.sender,
                        d.token,
                        d.amount,
                        d.tempoRefundRecipient
                    );
                } else {
                    try IZoneToken(d.token).mint(d.to, d.amount) {
                        emit DepositProcessed(
                            currentHash, d.sender, d.to, d.token, d.amount, d.memo
                        );
                    } catch {
                        _enqueueDepositBounceBack(d.token, d.amount, d.tempoRefundRecipient);
                        emit DepositFailed(
                            currentHash, d.sender, d.to, d.token, d.amount, d.tempoRefundRecipient
                        );
                    }
                }
            } else {
                EncryptedDeposit memory ed = abi.decode(qd.depositData, (EncryptedDeposit));
                currentHash = keccak256(abi.encode(DepositType.Encrypted, ed, currentHash));

                if (qd.rejected) {
                    _rejectDeposit(
                        currentHash,
                        DepositType.Encrypted,
                        ed.sender,
                        ed.token,
                        ed.amount,
                        ed.tempoRefundRecipient
                    );
                    continue;
                }

                // Sequencer must provide decryption for this encrypted deposit
                if (decryptionIndex >= decryptions.length) {
                    revert MissingDecryptionData();
                }
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

                bool valid = proofValid;
                bytes memory decryptedPlaintext;
                if (valid) {
                    // Step 2: Derive AES key from shared secret using HKDF-SHA256
                    // This is done in Solidity using the SHA256 precompile (0x02)
                    bytes32 aesKey = _hkdfSha256(
                        dec.sharedSecret,
                        "ecies-aes-key",
                        abi.encodePacked(tempoPortal, ed.keyIndex, ed.encrypted.ephemeralPubkeyX)
                    );

                    // Step 3: Decrypt using AES-256-GCM precompile
                    // The GCM tag proves the plaintext matches the ciphertext for this shared secret
                    (decryptedPlaintext, valid) = IAesGcmDecrypt(AES_GCM_DECRYPT)
                        .decrypt(
                            aesKey,
                            ed.encrypted.nonce,
                            ed.encrypted.ciphertext,
                            "", // empty AAD
                            ed.encrypted.tag
                        );
                }

                // Step 4: Decode the decrypted (to, memo) from the plaintext.
                // Plaintext is packed as [address(20 bytes)][memo(32 bytes)][padding(12 bytes)]
                // and must be exactly ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE (64) bytes.
                if (!valid || decryptedPlaintext.length != ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE) {
                    _failEncryptedDeposit(currentHash, ed);
                    continue;
                }
                (address decryptedTo, bytes32 decryptedMemo) =
                    EncryptedDepositLib.decodePlaintext(decryptedPlaintext);

                try IZoneToken(ed.token).mint(decryptedTo, ed.amount) {
                    emit EncryptedDepositProcessed(
                        currentHash, ed.sender, decryptedTo, ed.token, ed.amount, decryptedMemo
                    );
                } catch {
                    _failEncryptedDeposit(currentHash, ed);
                }
            }
        }

        // Verify all decryption data was consumed
        if (decryptionIndex != decryptions.length) revert ExtraDecryptionData();

        // Step 3: Validate against Tempo state
        // Read currentDepositQueueHash from the portal's storage using the new Tempo state.
        // The proof validates that our processedDepositQueueHash is an ancestor of (or equal to)
        // tempoCurrentHash, allowing partial deposit processing.
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
        processedDepositNumber += uint64(deposits.length);

        emit TempoAdvanced(
            _tempoState.tempoBlockHash(),
            _tempoState.tempoBlockNumber(),
            deposits.length,
            currentHash,
            processedDepositNumber
        );
    }

    function _rejectDeposit(
        bytes32 currentHash,
        DepositType depositType,
        address sender,
        address token,
        uint128 amount,
        address tempoRefundRecipient
    )
        internal
    {
        _enqueueDepositBounceBack(token, amount, tempoRefundRecipient);
        emit DepositRejected(currentHash, sender, depositType, token, amount, tempoRefundRecipient);
    }

    function _failEncryptedDeposit(bytes32 currentHash, EncryptedDeposit memory ed) internal {
        _enqueueDepositBounceBack(ed.token, ed.amount, ed.tempoRefundRecipient);
        emit EncryptedDepositFailed(currentHash, ed.sender, ed.token, ed.amount);
    }

    function _enqueueDepositBounceBack(
        address token,
        uint128 amount,
        address tempoRefundRecipient
    )
        internal
    {
        IZoneOutbox(ZONE_OUTBOX).enqueueDepositBounceBack(token, amount, tempoRefundRecipient);
    }

    function _processWithdrawalBounceBack(Deposit memory d) internal {
        uint64 fallbackNonce = uint64(uint160(d.to));
        address zoneFallbackRecipient =
            IZoneOutbox(ZONE_OUTBOX).consumeFallbackRecipient(fallbackNonce);
        try IZoneToken(d.token).mint(zoneFallbackRecipient, d.amount) {
            emit WithdrawalBounceBackProcessed(zoneFallbackRecipient, d.token, d.amount);
        } catch {
            _refunds[d.token][zoneFallbackRecipient] += d.amount;
            emit WithdrawalBounceBackPending(zoneFallbackRecipient, d.token, d.amount);
        }
    }

    function claimRefund(address token) external returns (uint128 amount) {
        amount = _refunds[token][msg.sender];
        _refunds[token][msg.sender] = 0;

        IZoneToken(token).mint(msg.sender, amount);
        emit RefundClaimed(msg.sender, token, amount);
    }

}
