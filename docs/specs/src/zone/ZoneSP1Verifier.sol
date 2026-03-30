// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { BlockTransition, DepositQueueTransition, IVerifier } from "./IZone.sol";
import { ISP1Verifier } from "@sp1-contracts/ISP1Verifier.sol";

/// @title ZoneSP1Verifier
/// @notice Zone proof verifier backed by Succinct's SP1 prover network.
///         Decodes the verifierConfig to extract the program vkey and public values,
///         validates the public values match the submitBatch calldata, then delegates
///         cryptographic verification to the SP1VerifierGateway.
contract ZoneSP1Verifier is IVerifier {

    /// @notice SP1 verifier gateway (PLONK). Reverts if the proof is invalid.
    ISP1Verifier public immutable sp1Verifier;

    /// @notice Expected SP1 program verification key. Pins the exact zone-stf binary.
    bytes32 public immutable programVKey;

    /// @notice Expected public values length (192 bytes for BatchOutput).
    uint256 private constant PUBLIC_VALUES_LEN = 192;

    /// @notice verifierConfig version we accept.
    uint8 private constant CONFIG_VERSION = 1;

    /// @notice Proof system identifier for SP1 PLONK.
    uint8 private constant PROOF_SYSTEM_SP1_PLONK = 1;

    error InvalidConfigVersion(uint8 version);
    error InvalidProofSystem(uint8 proofSystem);
    error InvalidVKey(bytes32 got, bytes32 expected);
    error InvalidPublicValuesLength(uint256 got, uint256 expected);
    error PublicValuesMismatch(string field);

    constructor(address _sp1Verifier, bytes32 _programVKey) {
        sp1Verifier = ISP1Verifier(_sp1Verifier);
        programVKey = _programVKey;
    }

    /// @inheritdoc IVerifier
    function verify(
        uint64,
        uint64,
        bytes32,
        uint64 expectedWithdrawalBatchIndex,
        address,
        BlockTransition calldata blockTransition,
        DepositQueueTransition calldata depositQueueTransition,
        bytes32 withdrawalQueueHash,
        bytes calldata verifierConfig,
        bytes calldata proof
    )
        external
        view
        override
        returns (bool)
    {
        // --- 1. Decode verifierConfig ---
        // Layout:
        //   [0]      version (must be 1)
        //   [1]      proofSystem (must be 1 = SP1 PLONK)
        //   [2..34]  vkHash (bytes32)
        //   [34..38] publicValuesLen (uint32 big-endian)
        //   [38..]   publicValues (192 bytes)

        uint8 version = uint8(verifierConfig[0]);
        if (version != CONFIG_VERSION) revert InvalidConfigVersion(version);

        uint8 proofSystem = uint8(verifierConfig[1]);
        if (proofSystem != PROOF_SYSTEM_SP1_PLONK) revert InvalidProofSystem(proofSystem);

        bytes32 vkHash;
        assembly {
            // verifierConfig is calldata; load 32 bytes starting at offset 2
            vkHash := calldataload(add(verifierConfig.offset, 2))
        }
        if (vkHash != programVKey) revert InvalidVKey(vkHash, programVKey);

        uint32 pubValuesLen;
        assembly {
            // Load 4 bytes at offset 34, shift right to get uint32
            pubValuesLen := shr(224, calldataload(add(verifierConfig.offset, 34)))
        }
        if (pubValuesLen != PUBLIC_VALUES_LEN) {
            revert InvalidPublicValuesLength(pubValuesLen, PUBLIC_VALUES_LEN);
        }

        bytes calldata publicValues = verifierConfig[38:38 + PUBLIC_VALUES_LEN];

        // --- 2. Validate public values match submitBatch calldata ---
        // Public values layout (192 bytes):
        //   [0..32]    prevBlockHash
        //   [32..64]   nextBlockHash
        //   [64..96]   prevProcessedDepositHash
        //   [96..128]  nextProcessedDepositHash
        //   [128..160] withdrawalQueueHash
        //   [160..192] withdrawalBatchIndex (uint256 big-endian)

        bytes32 pvPrevBlockHash = bytes32(publicValues[0:32]);
        bytes32 pvNextBlockHash = bytes32(publicValues[32:64]);
        bytes32 pvPrevDepositHash = bytes32(publicValues[64:96]);
        bytes32 pvNextDepositHash = bytes32(publicValues[96:128]);
        bytes32 pvWithdrawalQueueHash = bytes32(publicValues[128:160]);
        uint64 pvWithdrawalBatchIndex = uint64(uint256(bytes32(publicValues[160:192])));

        if (pvPrevBlockHash != blockTransition.prevBlockHash) {
            revert PublicValuesMismatch("prevBlockHash");
        }
        if (pvNextBlockHash != blockTransition.nextBlockHash) {
            revert PublicValuesMismatch("nextBlockHash");
        }
        if (pvPrevDepositHash != depositQueueTransition.prevProcessedHash) {
            revert PublicValuesMismatch("prevProcessedDepositHash");
        }
        if (pvNextDepositHash != depositQueueTransition.nextProcessedHash) {
            revert PublicValuesMismatch("nextProcessedDepositHash");
        }
        if (pvWithdrawalQueueHash != withdrawalQueueHash) {
            revert PublicValuesMismatch("withdrawalQueueHash");
        }
        if (pvWithdrawalBatchIndex != expectedWithdrawalBatchIndex) {
            revert PublicValuesMismatch("withdrawalBatchIndex");
        }

        // --- 3. Verify SP1 proof (reverts if invalid) ---
        sp1Verifier.verifyProof(programVKey, publicValues, proof);

        return true;
    }

}
