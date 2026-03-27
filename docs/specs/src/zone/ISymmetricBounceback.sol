// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/// @title ISymmetricBounceback
/// @notice Interface additions for symmetric bounceback support on zones.
///         This file defines the NEW types, events, errors, and function signatures
///         introduced by the symmetric bounceback spec. Existing types that are modified
///         (Deposit, EncryptedDeposit, DepositType) are redefined here with the changes.
/// @dev This is a SPEC-ONLY file — it defines the interface contract, not the implementation.

/*//////////////////////////////////////////////////////////////
                     MODIFIED DEPOSIT TYPES
//////////////////////////////////////////////////////////////*/

/// @notice Deposit type discriminator for the unified deposit queue (UPDATED)
/// @dev Added BounceBack type for portal-initiated bounce re-deposits.
///      Used in hash chain: keccak256(abi.encode(depositType, depositData, prevHash))
enum DepositType {
    Regular,    // Standard deposit with plaintext recipient and memo
    Encrypted,  // Encrypted deposit with hidden recipient and memo
    BounceBack  // NEW: bounce-triggered re-deposit from failed withdrawal callback
}

/// @notice Deposit struct (UPDATED with bouncebackAddress)
/// @dev The bouncebackAddress is the zone-side refund destination if the deposit
///      fails at zone processing time (e.g., recipient blacklisted on zone).
///      For bounce-back deposits (DepositType.BounceBack), bouncebackAddress is address(0)
///      to indicate a terminal deposit that cannot bounce again.
struct Deposit {
    address token;              // TIP-20 token being deposited
    address sender;             // Depositor on Tempo
    address to;                 // Recipient on zone
    uint128 amount;             // Net amount after fee
    bytes32 memo;               // User-provided context
    address bouncebackAddress;  // NEW: zone-side refund destination (address(0) for bounce deposits)
}

/// @notice Encrypted deposit struct (UPDATED with bouncebackAddress)
/// @dev bouncebackAddress is public (not encrypted) since the portal must validate it
///      against TIP-403 at deposit time.
struct EncryptedDeposit {
    address token;              // TIP-20 token being deposited (public, for escrow accounting)
    address sender;             // Depositor (public, for refunds)
    uint128 amount;             // Amount (public, for accounting)
    uint256 keyIndex;           // Index of encryption key used (specified by depositor)
    EncryptedDepositPayload encrypted; // Encrypted (to, memo)
    address bouncebackAddress;  // NEW: zone-side refund destination (public, not encrypted)
}

/// @notice Encrypted deposit payload (unchanged, included for completeness)
struct EncryptedDepositPayload {
    bytes32 ephemeralPubkeyX;
    uint8 ephemeralPubkeyYParity;
    bytes ciphertext;
    bytes12 nonce;
    bytes16 tag;
}

/*//////////////////////////////////////////////////////////////
                     NEW EVENTS AND ERRORS
//////////////////////////////////////////////////////////////*/

/// @title IZonePortalBounceback
/// @notice New events and errors added to IZonePortal for symmetric bouncebacks
interface IZonePortalBounceback {

    /// @notice Emitted when a deposit is made with a bounceback address (replaces DepositMade)
    /// @param newCurrentDepositQueueHash The new deposit queue hash
    /// @param sender The depositor on Tempo
    /// @param token The TIP-20 token deposited
    /// @param to The recipient on the zone
    /// @param netAmount Amount after fee deduction
    /// @param fee Fee paid to sequencer
    /// @param memo User-provided context
    /// @param bouncebackAddress The zone-side refund destination
    event DepositMade(
        bytes32 indexed newCurrentDepositQueueHash,
        address indexed sender,
        address token,
        address to,
        uint128 netAmount,
        uint128 fee,
        bytes32 memo,
        address bouncebackAddress  // NEW field
    );

    /// @notice Emitted when an encrypted deposit is made with a bounceback address (replaces EncryptedDepositMade)
    /// @dev bouncebackAddress is included in the event so off-chain code can reconstruct the
    ///      deposit queue hash chain (bouncebackAddress is part of the EncryptedDeposit struct).
    event EncryptedDepositMade(
        bytes32 indexed newCurrentDepositQueueHash,
        address indexed sender,
        address token,
        uint128 netAmount,
        uint128 fee,
        uint256 keyIndex,
        bytes32 ephemeralPubkeyX,
        uint8 ephemeralPubkeyYParity,
        bytes ciphertext,
        bytes12 nonce,
        bytes16 tag,
        address bouncebackAddress  // NEW field
    );

    /// @notice Error when bounceback address is address(0) on a user deposit
    error InvalidBouncebackAddress();

    /// @notice Error when the bounceback address is not authorized under TIP-403
    error BouncebackPolicyForbids();

    /// @notice Deposit a TIP-20 token into the zone with a bounceback address (UPDATED)
    /// @dev Fee is deducted from amount and paid to sequencer in the same token.
    ///      Both `to` and `bouncebackAddress` are validated against TIP-403 at deposit time.
    ///      bouncebackAddress MUST NOT be address(0).
    /// @param token The TIP-20 token to deposit
    /// @param to Recipient address on the zone
    /// @param amount Total amount to deposit (fee will be deducted)
    /// @param memo User-provided context
    /// @param bouncebackAddress Zone-side address for refund if deposit fails on zone
    /// @return newCurrentDepositQueueHash The new deposit queue hash after this deposit
    function deposit(
        address token,
        address to,
        uint128 amount,
        bytes32 memo,
        address bouncebackAddress
    )
        external
        returns (bytes32 newCurrentDepositQueueHash);

    /// @notice Deposit with encrypted recipient and memo, with bounceback address (UPDATED)
    /// @dev bouncebackAddress is public (not encrypted). Validated against TIP-403 at deposit time.
    /// @param token The TIP-20 token to deposit
    /// @param amount Amount to deposit (fee deducted from this amount)
    /// @param keyIndex Index of the encryption key used
    /// @param encrypted The encrypted payload (recipient and memo)
    /// @param bouncebackAddress Zone-side address for refund if deposit fails on zone
    /// @return newCurrentDepositQueueHash The new deposit queue hash
    function depositEncrypted(
        address token,
        uint128 amount,
        uint256 keyIndex,
        EncryptedDepositPayload calldata encrypted,
        address bouncebackAddress
    )
        external
        returns (bytes32 newCurrentDepositQueueHash);
}

/*//////////////////////////////////////////////////////////////
                     ZONE INBOX ADDITIONS
//////////////////////////////////////////////////////////////*/

/// @title IZoneInboxBounceback
/// @notice New events added to IZoneInbox for symmetric bouncebacks
interface IZoneInboxBounceback {

    /// @notice Emitted when a deposit is bounced back to the bounceback address on the zone
    /// @dev This occurs when the zone-side mint to `to` fails (e.g., recipient blacklisted)
    ///      and the funds are instead minted to the bouncebackAddress.
    /// @param depositHash The deposit queue hash at this point
    /// @param sender The original depositor on Tempo
    /// @param intendedRecipient The original intended recipient (who was blocked)
    /// @param bouncebackAddress The address that received the funds instead
    /// @param token The TIP-20 token
    /// @param amount The amount minted to bouncebackAddress
    event DepositBounced(
        bytes32 indexed depositHash,
        address indexed sender,
        address indexed intendedRecipient,
        address bouncebackAddress,
        address token,
        uint128 amount
    );

    /// @notice Emitted when a bounce-back deposit is processed on the zone
    /// @dev These are terminal — if this mint fails, the zone block reverts.
    /// @param depositHash The deposit queue hash at this point
    /// @param to The bounce-back recipient (fallbackRecipient from the failed withdrawal)
    /// @param token The TIP-20 token
    /// @param amount The amount minted
    event DepositBounceBackProcessed(
        bytes32 indexed depositHash,
        address indexed to,
        address token,
        uint128 amount
    );
}

/*//////////////////////////////////////////////////////////////
                     DEPOSIT QUEUE LIB ADDITION
//////////////////////////////////////////////////////////////*/

/// @title IDepositQueueLibBounceback
/// @notice Additional function for DepositQueueLib to support BounceBack deposits
/// @dev The hash chain uses DepositType.BounceBack as the type discriminator:
///      newHash = keccak256(abi.encode(DepositType.BounceBack, deposit, prevHash))
interface IDepositQueueLibBounceback {
    // This is a library addition, not an interface. Shown here for spec clarity.
    //
    // function enqueueBounceBack(
    //     bytes32 currentHash,
    //     Deposit memory depositData
    // ) internal pure returns (bytes32 newHash) {
    //     newHash = keccak256(abi.encode(DepositType.BounceBack, depositData, currentHash));
    // }
}
