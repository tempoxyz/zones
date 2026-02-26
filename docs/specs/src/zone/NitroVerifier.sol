// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { BlockTransition, DepositQueueTransition, IVerifier } from "./IZone.sol";

/// @title NitroVerifier
/// @notice Verifies zone batch proofs using TEE attestation-backed signatures.
///         Uses a two-step scheme:
///         1. Off-chain: Nitro attestation document is verified, attestation signer
///            registers the enclave's signing key on-chain via registerEnclaveKey()
///         2. On-chain: Each batch is verified by checking the enclave's signature
contract NitroVerifier is IVerifier {

    /*//////////////////////////////////////////////////////////////
                               TYPES
    //////////////////////////////////////////////////////////////*/

    /// @notice Per-portal enclave key registration
    struct EnclaveRegistration {
        address enclaveKey; // secp256k1 address derived from enclave pubkey
        bytes32 measurementHash; // keccak256(PCR0 || PCR1 || PCR2)
        uint64 expiresAt; // block.timestamp after which registration expires
        address sequencer; // sequencer address bound at registration
    }

    /*//////////////////////////////////////////////////////////////
                              CONSTANTS
    //////////////////////////////////////////////////////////////*/

    /// @notice Upper half of secp256k1 order for signature malleability check
    uint256 internal constant SECP256K1N_HALF =
        0x7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a0;

    /*//////////////////////////////////////////////////////////////
                               STORAGE
    //////////////////////////////////////////////////////////////*/

    /// @notice Trusted attestation signer (EOA/multisig that verifies Nitro attestation docs off-chain)
    address public immutable attestationSigner;

    /// @notice Per-portal enclave key registrations
    mapping(address portal => EnclaveRegistration) public registrations;

    /// @notice Per-portal registration nonce to prevent replay/downgrade attacks
    mapping(address portal => uint64) public registrationNonce;

    /*//////////////////////////////////////////////////////////////
                               ERRORS
    //////////////////////////////////////////////////////////////*/

    error InvalidAttesterSignature();
    error InvalidEnclaveSignature();
    error EnclaveKeyExpired();
    error EnclaveKeyNotRegistered();
    error InvalidNonce();
    error InvalidSignature();

    /*//////////////////////////////////////////////////////////////
                               EVENTS
    //////////////////////////////////////////////////////////////*/

    event EnclaveKeyRegistered(
        address indexed portal,
        address indexed enclaveKey,
        bytes32 measurementHash,
        uint64 expiresAt,
        uint64 nonce
    );

    /*//////////////////////////////////////////////////////////////
                             CONSTRUCTOR
    //////////////////////////////////////////////////////////////*/

    constructor(address _attestationSigner) {
        attestationSigner = _attestationSigner;
    }

    /*//////////////////////////////////////////////////////////////
                            VERIFICATION
    //////////////////////////////////////////////////////////////*/

    /// @inheritdoc IVerifier
    function verify(
        uint64 tempoBlockNumber,
        uint64 anchorBlockNumber,
        bytes32 anchorBlockHash,
        uint64 expectedWithdrawalBatchIndex,
        address sequencer,
        BlockTransition calldata blockTransition,
        DepositQueueTransition calldata depositQueueTransition,
        bytes32 withdrawalQueueHash,
        bytes calldata,
        bytes calldata proof
    )
        external
        view
        override
        returns (bool)
    {
        address portal = msg.sender;

        // Look up existing registration
        EnclaveRegistration storage reg = registrations[portal];
        if (reg.enclaveKey == address(0)) revert EnclaveKeyNotRegistered();
        if (reg.expiresAt < block.timestamp) revert EnclaveKeyExpired();

        // Decode proof: just the enclave signature (registration done separately)
        bytes memory enclaveSig = abi.decode(proof, (bytes));

        // Compute batch digest (must match what the enclave signs)
        bytes32 batchDigest = computeBatchDigest(
            tempoBlockNumber,
            anchorBlockNumber,
            anchorBlockHash,
            expectedWithdrawalBatchIndex,
            sequencer,
            blockTransition,
            depositQueueTransition,
            withdrawalQueueHash,
            portal
        );

        // Verify enclave signature
        if (enclaveSig.length != 65) revert InvalidEnclaveSignature();
        address signer = _recoverSigner(batchDigest, enclaveSig);
        if (signer != reg.enclaveKey) revert InvalidEnclaveSignature();

        return true;
    }

    /// @notice Register or refresh an enclave key for a portal.
    ///         Called by anyone — the attester signature proves authorization.
    ///         Uses a monotonic nonce to prevent replay/downgrade attacks.
    function registerEnclaveKey(
        address portal,
        address sequencer,
        address enclaveKey,
        bytes32 measurementHash,
        uint64 expiresAt,
        uint64 nonce,
        bytes calldata attesterSig
    )
        external
    {
        if (nonce != registrationNonce[portal]) revert InvalidNonce();

        bytes32 registrationDigest = keccak256(
            abi.encode(
                "NitroVerifier.RegisterEnclaveKey",
                block.chainid,
                address(this),
                portal,
                sequencer,
                enclaveKey,
                measurementHash,
                expiresAt,
                nonce
            )
        );

        address recovered = _recoverSigner(registrationDigest, attesterSig);
        if (recovered != attestationSigner) revert InvalidAttesterSignature();

        registrations[portal] = EnclaveRegistration({
            enclaveKey: enclaveKey,
            measurementHash: measurementHash,
            expiresAt: expiresAt,
            sequencer: sequencer
        });

        registrationNonce[portal] = nonce + 1;

        emit EnclaveKeyRegistered(portal, enclaveKey, measurementHash, expiresAt, nonce);
    }

    /*//////////////////////////////////////////////////////////////
                              HELPERS
    //////////////////////////////////////////////////////////////*/

    /// @notice Compute the batch digest that the enclave must sign.
    ///         This MUST match the digest computed by the enclave signer (Rust side).
    function computeBatchDigest(
        uint64 tempoBlockNumber,
        uint64 anchorBlockNumber,
        bytes32 anchorBlockHash,
        uint64 expectedWithdrawalBatchIndex,
        address sequencer,
        BlockTransition calldata blockTransition,
        DepositQueueTransition calldata depositQueueTransition,
        bytes32 withdrawalQueueHash,
        address portal
    )
        public
        view
        returns (bytes32)
    {
        return keccak256(
            abi.encode(
                "NitroVerifier.BatchDigest",
                block.chainid,
                address(this),
                portal,
                tempoBlockNumber,
                anchorBlockNumber,
                anchorBlockHash,
                expectedWithdrawalBatchIndex,
                sequencer,
                blockTransition.prevBlockHash,
                blockTransition.nextBlockHash,
                depositQueueTransition.prevProcessedHash,
                depositQueueTransition.nextProcessedHash,
                withdrawalQueueHash
            )
        );
    }

    /// @notice Recover signer from a hash and 65-byte signature (r || s || v)
    /// @dev Includes malleability protection (EIP-2) and strict v validation
    function _recoverSigner(
        bytes32 digest,
        bytes memory sig
    )
        internal
        pure
        returns (address)
    {
        bytes32 r;
        bytes32 s;
        uint8 v;
        assembly {
            r := mload(add(sig, 32))
            s := mload(add(sig, 64))
            v := byte(0, mload(add(sig, 96)))
        }
        if (v < 27) v += 27;
        if (v != 27 && v != 28) revert InvalidSignature();
        if (uint256(s) > SECP256K1N_HALF) revert InvalidSignature();

        address recovered = ecrecover(digest, v, r, s);
        if (recovered == address(0)) revert InvalidSignature();
        return recovered;
    }

}
