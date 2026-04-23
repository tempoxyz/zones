// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { getBlockHash } from "./BlockHashHistory.sol";
import { DepositQueueLib } from "./DepositQueueLib.sol";
import { ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE } from "./EncryptedDeposit.sol";
import {
    BlockTransition,
    Deposit,
    DepositQueueTransition,
    DepositType,
    ENCRYPTION_KEY_GRACE_PERIOD,
    EncryptedDeposit,
    EncryptedDepositPayload,
    EncryptionKeyEntry,
    IVerifier,
    IZoneMessenger,
    IZonePortal,
    QueuedDeposit,
    TokenConfig,
    Withdrawal
} from "./IZone.sol";
import { WithdrawalQueue, WithdrawalQueueLib } from "./WithdrawalQueueLib.sol";
import { StdPrecompiles } from "tempo-std/StdPrecompiles.sol";
import { ITIP20 } from "tempo-std/interfaces/ITIP20.sol";
import { ITIP20Factory } from "tempo-std/interfaces/ITIP20Factory.sol";
import { ITIP403Registry } from "tempo-std/interfaces/ITIP403Registry.sol";

/// @title ZonePortal
/// @notice Per-zone portal that escrows zone tokens on Tempo and manages deposits/withdrawals
contract ZonePortal is IZonePortal {

    using WithdrawalQueueLib for WithdrawalQueue;

    /*//////////////////////////////////////////////////////////////
                               CONSTANTS
    //////////////////////////////////////////////////////////////*/

    /// @notice TIP-403 registry for transfer policy authorization checks
    ITIP403Registry internal constant TIP403_REGISTRY =
        ITIP403Registry(StdPrecompiles.TIP403_REGISTRY_ADDRESS);

    /// @notice Fixed gas value for deposit fee calculation
    /// @dev Set to 100,000 gas. Deposit fee = FIXED_DEPOSIT_GAS * zoneGasRate.
    ///      This provides a stable pricing basis for deposits while allowing sequencer
    ///      flexibility to adjust the zoneGasRate based on operational costs.
    uint64 public constant FIXED_DEPOSIT_GAS = 100_000;

    /// @notice Maximum allowed gas fee rate to prevent overflows
    uint128 public constant MAX_GAS_FEE_RATE = 1e18;

    /*//////////////////////////////////////////////////////////////
                                STORAGE
    //////////////////////////////////////////////////////////////*/

    uint32 public immutable zoneId;
    address public immutable messenger;
    address public immutable verifier;
    uint64 public immutable genesisTempoBlockNumber;

    /// @notice Current sequencer address
    address public sequencer;

    /// @notice Pending sequencer for two-step transfer
    address public pendingSequencer;

    /// @notice Zone gas rate (zone token units per gas unit on the zone)
    /// @dev Sequencer publishes this rate and takes the risk on zone gas costs.
    ///      Deposit fee = FIXED_DEPOSIT_GAS * zoneGasRate
    uint128 public zoneGasRate;
    uint64 public withdrawalBatchIndex;
    bytes32 public blockHash;

    /// @notice Current deposit queue hash (where new deposits land)
    bytes32 public currentDepositQueueHash;

    /// @notice Total number of deposits enqueued (monotonic counter, 1-indexed).
    /// @dev Each deposit(), depositEncrypted(), and withdrawal bounce-back increments this.
    ///      The deposit number is emitted in deposit events so users can track their position.
    uint64 public depositCount;

    /// @notice Last deposit number confirmed as processed by a batch proof.
    /// @dev Updated in submitBatch(). A deposit with number N is confirmed once
    ///      lastProcessedDepositNumber >= N.
    uint64 public lastProcessedDepositNumber;

    /// @notice Last Tempo block number the zone has synced to
    uint64 public lastSyncedTempoBlockNumber;

    /// @notice Historical encryption keys with activation blocks
    /// @dev Users specify which key they encrypted to (by index). Maintained for key rotation.
    ///      Stored at slot 6 in the ZonePortal storage layout.
    EncryptionKeyEntry[] internal _encryptionKeys;

    /// @notice Per-token configuration (stored at slot 7)
    /// @dev TokenConfig.enabled is permanent (write-once true); depositsActive can be toggled.
    mapping(address => TokenConfig) internal _tokenConfigs;

    /// @notice Append-only list of enabled tokens (stored at slot 8)
    /// @dev Tokens can never be removed from this list (non-custodial guarantee).
    address[] internal _enabledTokens;

    /// @notice Withdrawal queue (zone→Tempo): fixed-size ring buffer
    WithdrawalQueue internal _withdrawalQueue;

    /*//////////////////////////////////////////////////////////////
                              CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(
        uint32 _zoneId,
        address _initialToken,
        address _messenger,
        address _sequencer,
        address _verifier,
        bytes32 _genesisBlockHash,
        uint64 _genesisTempoBlockNumber
    ) {
        zoneId = _zoneId;
        messenger = _messenger;
        sequencer = _sequencer;
        verifier = _verifier;
        blockHash = _genesisBlockHash;
        genesisTempoBlockNumber = _genesisTempoBlockNumber;

        // Enable the initial token
        _enableTokenInternal(_initialToken);
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

    /// @notice Set zone gas rate. Only callable by sequencer.
    /// @dev Sequencer publishes this rate and takes the risk on zone gas costs.
    ///      If actual zone gas is higher, sequencer covers the difference.
    ///      If actual zone gas is lower, sequencer keeps the surplus.
    /// @param _zoneGasRate Zone token units per gas unit on the zone
    function setZoneGasRate(uint128 _zoneGasRate) external onlySequencer {
        if (_zoneGasRate > MAX_GAS_FEE_RATE) revert GasFeeRateTooHigh();
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

    function withdrawalQueueSlot(uint256 slot) external view returns (bytes32) {
        return _withdrawalQueue.slots[slot];
    }

    /*//////////////////////////////////////////////////////////////
                          TOKEN REGISTRY
    //////////////////////////////////////////////////////////////*/

    /// @notice Check if a token is enabled for bridging
    function isTokenEnabled(address _token) external view returns (bool) {
        return _tokenConfigs[_token].enabled;
    }

    /// @notice Check if deposits are currently active for a token
    function areDepositsActive(address _token) external view returns (bool) {
        TokenConfig storage cfg = _tokenConfigs[_token];
        return cfg.enabled && cfg.depositsActive;
    }

    /// @notice Get the token configuration for a specific token
    function tokenConfig(address _token) external view returns (TokenConfig memory) {
        return _tokenConfigs[_token];
    }

    /// @notice Get the number of enabled tokens
    function enabledTokenCount() external view returns (uint256) {
        return _enabledTokens.length;
    }

    /// @notice Get an enabled token by index
    function enabledTokenAt(uint256 index) external view returns (address) {
        return _enabledTokens[index];
    }

    /// @notice Enable a new TIP-20 token for bridging. Only callable by sequencer.
    /// @dev Irreversible: once enabled, a token cannot be disabled (non-custodial guarantee).
    ///      Validates the token is a TIP-20 and grants messenger max approval.
    function enableToken(address _token) external onlySequencer {
        if (_tokenConfigs[_token].enabled) revert TokenAlreadyEnabled();
        if (!ITIP20Factory(StdPrecompiles.TIP20_FACTORY_ADDRESS).isTIP20(_token)) {
            revert TokenNotEnabled();
        }
        _enableTokenInternal(_token);
    }

    /// @notice Pause deposits for a token. Only callable by sequencer.
    /// @dev Does not affect withdrawal processing (non-custodial guarantee).
    function pauseDeposits(address _token) external onlySequencer {
        if (!_tokenConfigs[_token].enabled) revert TokenNotEnabled();
        _tokenConfigs[_token].depositsActive = false;
        emit DepositsPaused(_token);
    }

    /// @notice Resume deposits for a token. Only callable by sequencer.
    function resumeDeposits(address _token) external onlySequencer {
        if (!_tokenConfigs[_token].enabled) revert TokenNotEnabled();
        _tokenConfigs[_token].depositsActive = true;
        emit DepositsResumed(_token);
    }

    /// @notice Internal function to enable a token (used by constructor and enableToken)
    function _enableTokenInternal(address _token) internal {
        _tokenConfigs[_token] = TokenConfig({ enabled: true, depositsActive: true });
        _enabledTokens.push(_token);

        // Give messenger max approval for this token
        ITIP20(_token).approve(messenger, type(uint256).max);

        // Read token metadata for the event so zone-side can create matching TIP-20
        string memory name = ITIP20(_token).name();
        string memory symbol = ITIP20(_token).symbol();
        string memory currency = ITIP20(_token).currency();

        emit TokenEnabled(_token, name, symbol, currency);
    }

    /*//////////////////////////////////////////////////////////////
                        ENCRYPTION KEY MANAGEMENT
    //////////////////////////////////////////////////////////////*/

    /// @notice Get the sequencer's current encryption public key
    /// @return x The X coordinate
    /// @return yParity The Y coordinate parity (0x02 or 0x03)
    function sequencerEncryptionKey() external view returns (bytes32 x, uint8 yParity) {
        if (_encryptionKeys.length == 0) revert NoEncryptionKeySet();
        EncryptionKeyEntry storage current = _encryptionKeys[_encryptionKeys.length - 1];
        return (current.x, current.yParity);
    }

    /// @notice Set the sequencer's encryption public key with proof of possession
    /// @dev Only callable by the sequencer. Appends to key history.
    ///      Requires a valid ECDSA signature over keccak256(abi.encode(address(this), x, yParity))
    ///      produced by the private key corresponding to (x, yParity). This prevents accidental
    ///      registration of keys the sequencer cannot decrypt with.
    /// @param x The X coordinate (must be valid secp256k1 point)
    /// @param yParity The Y coordinate parity (0x02 or 0x03)
    /// @param popV Recovery id of the proof-of-possession signature
    /// @param popR R component of the proof-of-possession signature
    /// @param popS S component of the proof-of-possession signature
    function setSequencerEncryptionKey(
        bytes32 x,
        uint8 yParity,
        uint8 popV,
        bytes32 popR,
        bytes32 popS
    )
        external
        onlySequencer
    {
        // Validate yParity
        if (yParity != 0x02 && yParity != 0x03) revert InvalidEphemeralPubkey();

        // Validate x is on the secp256k1 curve
        if (!_isValidSecp256k1X(x)) revert InvalidEphemeralPubkey();

        // Verify proof of possession: the sequencer must sign with the encryption key's private key
        bytes32 message = keccak256(abi.encode(address(this), x, yParity));
        address recovered = ecrecover(message, popV, popR, popS);
        address expected = _deriveAddressFromPubKey(x, yParity);
        if (recovered == address(0) || recovered != expected) revert InvalidProofOfPossession();

        uint64 activationBlock = uint64(block.number);
        _encryptionKeys.push(
            EncryptionKeyEntry({ x: x, yParity: yParity, activationBlock: activationBlock })
        );
        emit SequencerEncryptionKeyUpdated(x, yParity, _encryptionKeys.length - 1, activationBlock);
    }

    /// @notice Get the number of keys in the history
    function encryptionKeyCount() external view returns (uint256) {
        return _encryptionKeys.length;
    }

    /// @notice Get a historical encryption key by index
    /// @param index The index in the key history (0 = first key)
    /// @return entry The key entry with activation block
    function encryptionKeyAt(uint256 index)
        external
        view
        returns (EncryptionKeyEntry memory entry)
    {
        if (index >= _encryptionKeys.length) {
            revert InvalidEncryptionKeyIndex(index);
        }
        return _encryptionKeys[index];
    }

    /// @notice Get the encryption key that was active at a specific Tempo block
    /// @dev Binary search through key history to find the correct key
    /// @param tempoBlockNumber The Tempo block number to query
    /// @return x The X coordinate of the active key
    /// @return yParity The Y coordinate parity
    /// @return keyIndex The index of this key in history
    function encryptionKeyAtBlock(uint64 tempoBlockNumber)
        external
        view
        returns (bytes32 x, uint8 yParity, uint256 keyIndex)
    {
        uint256 len = _encryptionKeys.length;
        if (len == 0 || tempoBlockNumber < _encryptionKeys[0].activationBlock) {
            revert NoEncryptionKeyAtBlock(tempoBlockNumber);
        }

        uint256 low = 0;
        uint256 high = len - 1;
        while (low < high) {
            uint256 mid = (low + high + 1) / 2;
            if (_encryptionKeys[mid].activationBlock <= tempoBlockNumber) {
                low = mid;
            } else {
                high = mid - 1;
            }
        }

        EncryptionKeyEntry storage entry = _encryptionKeys[low];
        return (entry.x, entry.yParity, low);
    }

    /// @notice Check if an encryption key is still valid for new deposits
    /// @param keyIndex The key index to check
    /// @return valid True if the key can be used for new deposits
    /// @return expiresAtBlock Block number when this key expires (0 if current key)
    function isEncryptionKeyValid(uint256 keyIndex)
        public
        view
        returns (bool valid, uint64 expiresAtBlock)
    {
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
                       EPHEMERAL KEY VALIDATION
    //////////////////////////////////////////////////////////////*/

    /// @notice secp256k1 field prime
    uint256 internal constant SECP256K1_P =
        0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEFFFFFC2F;

    /// @notice (SECP256K1_P - 1) / 2 for Euler's criterion
    uint256 internal constant SECP256K1_HALF_PM1 =
        0x7FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF7FFFFE17;

    /// @notice (SECP256K1_P + 1) / 4 for modular square root (p ≡ 3 mod 4)
    uint256 internal constant SECP256K1_SQRT_EXP =
        0x3FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFBFFFFF0C;

    /// @notice Validate that an X coordinate corresponds to a valid secp256k1 point
    /// @dev Uses Euler's criterion via the MODEXP precompile (0x05):
    ///      x³ + 7 is a quadratic residue mod p iff (x³+7)^((p-1)/2) ≡ 1 (mod p)
    function _isValidSecp256k1X(bytes32 x) internal view returns (bool) {
        uint256 px = uint256(x);
        if (px == 0 || px >= SECP256K1_P) return false;

        // rhs = x³ + 7 mod p
        uint256 rhs = addmod(mulmod(mulmod(px, px, SECP256K1_P), px, SECP256K1_P), 7, SECP256K1_P);

        // Call MODEXP precompile: rhs^((p-1)/2) mod p
        // Input format: Bsize(32) || Esize(32) || Msize(32) || B || E || M
        bytes memory input = abi.encodePacked(
            uint256(32), uint256(32), uint256(32), rhs, SECP256K1_HALF_PM1, SECP256K1_P
        );

        (bool success, bytes memory result) = address(0x05).staticcall(input);
        if (!success || result.length != 32) return false;

        return uint256(bytes32(result)) == 1;
    }

    /// @notice Derive the Ethereum address corresponding to a compressed secp256k1 public key
    /// @dev Computes y from x using modular square root, then keccak256(x || y)
    /// @param x The X coordinate (must be a valid secp256k1 x-coordinate)
    /// @param yParity 0x02 (even y) or 0x03 (odd y)
    /// @return addr The derived Ethereum address
    function _deriveAddressFromPubKey(
        bytes32 x,
        uint8 yParity
    )
        internal
        view
        returns (address addr)
    {
        uint256 px = uint256(x);

        // Compute y² = x³ + 7 mod p
        uint256 rhs = addmod(mulmod(mulmod(px, px, SECP256K1_P), px, SECP256K1_P), 7, SECP256K1_P);

        // Compute y = rhs^((p+1)/4) mod p (valid because p ≡ 3 mod 4)
        bytes memory modexpInput = abi.encodePacked(
            uint256(32), uint256(32), uint256(32), rhs, SECP256K1_SQRT_EXP, SECP256K1_P
        );
        (bool success, bytes memory modexpResult) = address(0x05).staticcall(modexpInput);
        require(success && modexpResult.length == 32, "modexp failed");
        uint256 y = uint256(bytes32(modexpResult));

        // Select correct y based on parity: 0x02 = even, 0x03 = odd
        if ((y % 2 == 0) != (yParity == 0x02)) {
            y = SECP256K1_P - y;
        }

        // Address = last 20 bytes of keccak256(uncompressed public key)
        addr = address(uint160(uint256(keccak256(abi.encodePacked(px, y)))));
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

    /// @notice Deposit a TIP-20 token into the zone. Returns the new current deposit queue hash.
    /// @dev Fee is deducted from amount and paid to sequencer in the same token.
    ///      The token must be enabled and deposits must be active.
    /// @param _token The TIP-20 token to deposit
    /// @param to Recipient address on the zone
    /// @param amount Total amount to deposit (fee will be deducted)
    /// @param memo User-provided context
    /// @return newCurrentDepositQueueHash The new deposit queue hash after this deposit
    function deposit(
        address _token,
        address to,
        uint128 amount,
        bytes32 memo,
        address bouncebackRecipient
    )
        external
        returns (bytes32 newCurrentDepositQueueHash)
    {
        // Validate token is enabled and deposits are active
        TokenConfig storage cfg = _tokenConfigs[_token];
        if (!cfg.enabled) revert TokenNotEnabled();
        if (!cfg.depositsActive) revert DepositsNotActive();

        // Enforce TIP-403 policy: the recipient must be authorized under the
        // token's transfer policy before accepting the deposit.
        uint64 policyId = ITIP20(_token).transferPolicyId();
        if (!TIP403_REGISTRY.isAuthorizedRecipient(policyId, to)) {
            revert DepositPolicyForbids();
        }
        if (!TIP403_REGISTRY.isAuthorizedMintRecipient(policyId, to)) {
            revert DepositPolicyForbids();
        }

        // Validate bouncebackRecipient against TIP-403 at deposit time so that
        // bounceback transfers on L1 are guaranteed to succeed even if the
        // recipient is later blacklisted (portal has a TIP-403 bypass).
        if (bouncebackRecipient != address(0)) {
            if (!TIP403_REGISTRY.isAuthorizedRecipient(policyId, bouncebackRecipient)) {
                revert DepositPolicyForbids();
            }
        }

        // Calculate deposit fee
        uint128 fee = calculateDepositFee();
        if (amount <= fee) revert DepositTooSmall();
        uint128 netAmount = amount - fee;

        // Transfer full amount from sender to this contract
        // TIP-20 transfers revert on failure, so no boolean check is needed here.
        ITIP20(_token).transferFrom(msg.sender, address(this), amount);

        // Transfer fee to sequencer
        if (fee > 0) {
            ITIP20(_token).transfer(sequencer, fee);
        }

        // Build deposit struct with net amount (fee already paid to sequencer on Tempo)
        Deposit memory depositData = Deposit({
            token: _token,
            sender: msg.sender,
            to: to,
            amount: netAmount,
            memo: memo,
            bouncebackRecipient: bouncebackRecipient
        });

        // Insert deposit into queue
        newCurrentDepositQueueHash = DepositQueueLib.enqueue(currentDepositQueueHash, depositData);
        currentDepositQueueHash = newCurrentDepositQueueHash;
        uint64 thisDeposit = ++depositCount;

        emit DepositMade(
            newCurrentDepositQueueHash,
            msg.sender,
            _token,
            to,
            netAmount,
            fee,
            memo,
            bouncebackRecipient,
            thisDeposit
        );
    }

    /// @notice Deposit with encrypted recipient and memo
    /// @dev The encrypted payload contains (to, memo) encrypted to the sequencer's key.
    ///      The token identity is public (not encrypted) since the portal must escrow it.
    ///      Validates that keyIndex is valid (exists and not expired).
    ///      Charges the same deposit fee as regular deposits.
    /// @param _token The TIP-20 token to deposit
    /// @param amount Amount to deposit (fee deducted from this amount)
    /// @param keyIndex Index of the encryption key used (from encryptionKeyAt)
    /// @param encrypted The encrypted payload (recipient and memo)
    /// @return newCurrentDepositQueueHash The new deposit queue hash
    function depositEncrypted(
        address _token,
        uint128 amount,
        uint256 keyIndex,
        EncryptedDepositPayload calldata encrypted,
        address bouncebackRecipient
    )
        external
        returns (bytes32 newCurrentDepositQueueHash)
    {
        // Validate token is enabled and deposits are active
        TokenConfig storage cfg = _tokenConfigs[_token];
        if (!cfg.enabled) revert TokenNotEnabled();
        if (!cfg.depositsActive) revert DepositsNotActive();

        // Validate ephemeral public key is a valid secp256k1 point
        // Prevents griefing: invalid points make Chaum-Pedersen proofs impossible,
        // which would block chain progress on the zone side.
        if (encrypted.ephemeralPubkeyYParity != 0x02 && encrypted.ephemeralPubkeyYParity != 0x03) {
            revert InvalidEphemeralPubkey();
        }
        if (!_isValidSecp256k1X(encrypted.ephemeralPubkeyX)) revert InvalidEphemeralPubkey();

        // Validate ciphertext length — GCM ciphertext == plaintext length (tag is separate)
        // Prevents DoS: oversized ciphertexts inflate zone-side AES-GCM processing cost
        if (encrypted.ciphertext.length != ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE) {
            revert InvalidCiphertextLength(
                encrypted.ciphertext.length, ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE
            );
        }

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

        uint128 fee = calculateDepositFee();
        if (amount <= fee) revert DepositTooSmall();
        uint128 netAmount = amount - fee;

        // Transfer full amount from sender to this contract
        ITIP20(_token).transferFrom(msg.sender, address(this), amount);
        if (fee > 0) {
            ITIP20(_token).transfer(sequencer, fee);
        }

        // Build encrypted deposit struct
        EncryptedDeposit memory depositData = EncryptedDeposit({
            token: _token,
            sender: msg.sender,
            amount: netAmount,
            keyIndex: keyIndex,
            encrypted: encrypted,
            bouncebackRecipient: bouncebackRecipient
        });

        // Insert encrypted deposit into queue
        newCurrentDepositQueueHash =
            DepositQueueLib.enqueueEncrypted(currentDepositQueueHash, depositData);
        currentDepositQueueHash = newCurrentDepositQueueHash;
        uint64 thisDeposit = ++depositCount;

        emit EncryptedDepositMade(
            newCurrentDepositQueueHash,
            msg.sender,
            _token,
            netAmount,
            fee,
            keyIndex,
            encrypted.ephemeralPubkeyX,
            encrypted.ephemeralPubkeyYParity,
            encrypted.ciphertext,
            encrypted.nonce,
            encrypted.tag,
            thisDeposit
        );
    }

    /*//////////////////////////////////////////////////////////////
                             WITHDRAWALS
    //////////////////////////////////////////////////////////////*/

    /// @notice Process the next withdrawal from the queue. Only callable by the sequencer.
    /// @dev Fee is always paid to sequencer regardless of success/failure.
    ///      On failure, only the amount (not fee) is bounced back.
    ///      The token to transfer is read from the withdrawal struct.
    function processWithdrawal(
        Withdrawal calldata withdrawal,
        bytes32 remainingQueue
    )
        external
        onlySequencer
    {
        // Pop from withdrawal queue (library handles swap and hash verification)
        _withdrawalQueue.dequeue(withdrawal, remainingQueue);

        address _token = withdrawal.token;

        // Transfer fee to sequencer (always, regardless of withdrawal success)
        if (withdrawal.fee > 0) {
            ITIP20(_token).transfer(sequencer, withdrawal.fee);
        }

        // Execute the withdrawal
        if (withdrawal.gasLimit == 0) {
            // Simple transfer, no callback
            bool success;
            try ITIP20(_token).transfer(withdrawal.to, withdrawal.amount) returns (bool ok) {
                success = ok;
            } catch {
                success = false;
            }

            if (!success) {
                _enqueueWithdrawalBounceBack(
                    _token, withdrawal.amount, withdrawal.fallbackRecipient
                );
                emit WithdrawalProcessed(withdrawal.to, _token, withdrawal.amount, false);
                return;
            }

            emit WithdrawalProcessed(withdrawal.to, _token, withdrawal.amount, true);
            return;
        }

        // Try callback via messenger; revert is treated as failure
        try IZoneMessenger(messenger)
            .relayMessage(
                _token,
                withdrawal.senderTag,
                withdrawal.to,
                withdrawal.amount,
                withdrawal.gasLimit,
                withdrawal.callbackData
            ) {
            emit WithdrawalProcessed(withdrawal.to, _token, withdrawal.amount, true);
        } catch {
            // Callback failed: bounce back to zone (only amount, not fee)
            _enqueueWithdrawalBounceBack(_token, withdrawal.amount, withdrawal.fallbackRecipient);
            emit WithdrawalProcessed(withdrawal.to, _token, withdrawal.amount, false);
        }
    }

    /// @notice Enqueue a bounce-back deposit for a failed withdrawal callback
    /// @param _token The token from the failed withdrawal
    /// @param amount The amount to bounce back
    /// @param fallbackRecipient The zone address to receive the bounce-back
    function _enqueueWithdrawalBounceBack(
        address _token,
        uint128 amount,
        address fallbackRecipient
    )
        internal
    {
        Deposit memory depositData = Deposit({
            token: _token,
            sender: address(this),
            to: fallbackRecipient,
            amount: amount,
            memo: bytes32(0),
            bouncebackRecipient: address(0)
        });

        bytes32 newCurrentDepositQueueHash =
            DepositQueueLib.enqueue(currentDepositQueueHash, depositData);
        currentDepositQueueHash = newCurrentDepositQueueHash;
        uint64 thisDeposit = ++depositCount;

        emit WithdrawalBounceBack(
            newCurrentDepositQueueHash, fallbackRecipient, _token, amount, thisDeposit
        );
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
        bytes32 withdrawalQueueHash,
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

            anchorBlockHash = getBlockHash(tempoBlockNumber);
            if (anchorBlockHash == bytes32(0)) revert InvalidTempoBlockNumber();
        } else {
            // Ancestry mode: read recentTempoBlockNumber hash, proof verifies ancestry chain
            if (recentTempoBlockNumber <= tempoBlockNumber) revert InvalidTempoBlockNumber();
            if (recentTempoBlockNumber > block.number) revert InvalidTempoBlockNumber();

            anchorBlockNumber = recentTempoBlockNumber;
            anchorBlockHash = getBlockHash(recentTempoBlockNumber);
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
                withdrawalQueueHash,
                verifierConfig,
                proof
            );
        if (!valid) revert InvalidProof();

        // Update state
        withdrawalBatchIndex++;
        blockHash = blockTransition.nextBlockHash;
        lastSyncedTempoBlockNumber = tempoBlockNumber;
        lastProcessedDepositNumber = depositQueueTransition.nextDepositNumber;

        _withdrawalQueue.enqueue(withdrawalQueueHash);

        // Emit event after state updates
        emit BatchSubmitted(
            withdrawalBatchIndex,
            depositQueueTransition.nextProcessedHash,
            blockHash,
            withdrawalQueueHash,
            lastProcessedDepositNumber
        );
    }

}
