use alloy_primitives::{B256, Bytes, keccak256};
use alloy_sol_types::SolValue as _;
use serde::{Deserialize, Serialize};
use tempo_zone_contracts::ZONE_VERIFIER_ADDRESS;
use zone_spf::{BatchOutput, BatchWitness, PublicInputs};

/// Current version of the prover request and response wire format.
pub const PROTOCOL_VERSION: u16 = 1;

/// Canonical verifier configuration for the first Nitro-backed verifier policy.
pub const NITRO_VERIFIER_CONFIG_V1: &[u8] = &[1];

/// Type hash for the data placed in the Nitro attestation document's `user_data` field.
pub const NITRO_BATCH_ATTESTATION_TYPE: &str = "NitroBatchAttestation(uint256 parentChainId,address verifier,address portal,uint32 zoneId,uint64 tempoBlockNumber,uint64 anchorBlockNumber,bytes32 anchorBlockHash,uint64 expectedWithdrawalBatchIndex,bytes32 prevBlockHash,bytes32 nextBlockHash,bytes32 prevProcessedHash,bytes32 nextProcessedHash,uint64 prevDepositNumber,uint64 nextDepositNumber,bytes32 withdrawalQueueHash,bytes32 verifierConfigHash)";

/// Proof material returned by an attesting prover.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProofBundle {
    /// Opaque configuration passed to `IVerifier` and committed by the settlement certificate.
    pub verifier_config: Bytes,
    /// Raw COSE/CBOR Nitro attestation document.
    pub proof: Bytes,
}

/// Request to verify a Zone batch witness against a trusted Tempo chain.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyRequest {
    /// Wire-format version expected by the sender.
    pub version: u16,
    /// Caller-selected identifier echoed in the corresponding response.
    pub request_id: String,
    /// Complete input to the Zone stateless proof function.
    pub witness: BatchWitness,
}

/// Result of processing a [`VerifyRequest`].
#[derive(Debug, Deserialize, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "status"
)]
pub enum VerifyResponse {
    /// The witness was verified successfully.
    #[serde(rename = "ok")]
    Ok {
        /// Wire-format version used by the prover.
        version: u16,
        /// Identifier copied from the request.
        request_id: String,
        /// Output produced by the Zone stateless proof function.
        output: Box<BatchOutput>,
        /// Nitro proof material returned by an attesting prover.
        #[serde(skip_serializing_if = "Option::is_none")]
        proof_bundle: Option<ProofBundle>,
    },
    /// The request could not be decoded, accepted, or verified.
    #[serde(rename = "error")]
    Error {
        /// Wire-format version used by the prover.
        version: u16,
        /// Request identifier, when it could be recovered from the request.
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        /// Stable, machine-readable error category.
        code: ErrorCode,
        /// Human-readable diagnostic detail.
        message: String,
    },
}

/// Stable error categories returned by the prover protocol.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The request is not valid JSON or does not match the request schema.
    MalformedRequest,
    /// The request uses a wire-format version unsupported by the prover.
    UnsupportedVersion,
    /// The requested Tempo chain is not trusted by the prover.
    UnsupportedChain,
    /// Stateless proof execution rejected the supplied witness.
    VerificationFailed,
    /// The prover could not obtain an attestation from the Nitro Secure Module.
    AttestationUnavailable,
    /// The framed request exceeds the configured size limit.
    RequestTooLarge,
    /// The connection ended before the complete frame was received.
    TruncatedFrame,
    /// An unexpected transport or prover failure occurred.
    InternalError,
}

/// Construct the exact digest committed as Nitro attestation `user_data`.
///
/// The calling portal is bound independently of the Zone ID because every portal delegates to the
/// same fixed verifier. The parent chain, verifier, and portal prevent cross-domain replay; every
/// portal-facing commitment prevents reuse for a different batch.
pub fn nitro_batch_attestation_hash(public_inputs: &PublicInputs, output: &BatchOutput) -> B256 {
    let type_hash = keccak256(NITRO_BATCH_ATTESTATION_TYPE);
    let verifier_config_hash = keccak256(NITRO_VERIFIER_CONFIG_V1);
    keccak256(
        (
            type_hash,
            alloy_primitives::U256::from(public_inputs.parent_chain_id),
            ZONE_VERIFIER_ADDRESS,
            public_inputs.portal,
            public_inputs.zone_id,
            public_inputs.tempo_block_number,
            public_inputs.anchor_block_number,
            public_inputs.anchor_block_hash,
            public_inputs.expected_withdrawal_batch_index,
            output.block_transition.prevBlockHash,
            output.block_transition.nextBlockHash,
            output.deposit_queue_transition.prevProcessedHash,
            output.deposit_queue_transition.nextProcessedHash,
            output.deposit_queue_transition.prevDepositNumber,
            output.deposit_queue_transition.nextDepositNumber,
            output.withdrawal_queue_hash,
            verifier_config_hash,
        )
            .abi_encode(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use zone_spf::{BatchOutput, BlockTransition, DepositQueueTransition, LastBatchCommitment};

    #[test]
    fn error_variant_uses_the_wire_field_names() {
        let encoded = serde_json::to_vec(&VerifyResponse::Error {
            version: PROTOCOL_VERSION,
            request_id: Some("wire-test".into()),
            code: ErrorCode::UnsupportedChain,
            message: "unsupported".into(),
        })
        .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&encoded).unwrap();

        assert_eq!(json["status"], "error");
        assert_eq!(json["version"], PROTOCOL_VERSION);
        assert_eq!(json["requestId"], "wire-test");
        assert_eq!(json["code"], "unsupported_chain");
    }

    #[test]
    fn verifier_config_is_versioned_and_non_empty() {
        assert_eq!(NITRO_VERIFIER_CONFIG_V1, [1]);
        assert_ne!(keccak256(NITRO_VERIFIER_CONFIG_V1), keccak256([]));
    }

    #[test]
    fn batch_attestation_hash_matches_solidity_golden_vector() {
        let output = BatchOutput {
            block_transition: BlockTransition {
                prevBlockHash: B256::with_last_byte(1),
                nextBlockHash: B256::with_last_byte(2),
            },
            deposit_queue_transition: DepositQueueTransition {
                prevProcessedHash: B256::with_last_byte(3),
                nextProcessedHash: B256::with_last_byte(4),
                prevDepositNumber: 5,
                nextDepositNumber: 6,
            },
            withdrawal_queue_hash: B256::with_last_byte(7),
            last_batch_commitment: LastBatchCommitment {
                withdrawal_batch_index: 8,
            },
        };

        let public_inputs = PublicInputs {
            parent_chain_id: 42_431,
            portal: alloy_primitives::address!("0x1111111111111111111111111111111111111111"),
            zone_id: 12,
            tempo_block_number: 9,
            anchor_block_number: 10,
            anchor_block_hash: B256::with_last_byte(11),
            expected_withdrawal_batch_index: 13,
        };
        assert_eq!(
            nitro_batch_attestation_hash(&public_inputs, &output),
            alloy_primitives::b256!(
                "0x36f73f631fe83f439268204b16e9ae3083c6f0d49fad0facaa66431ee50cd40b"
            ),
        );
    }
}
