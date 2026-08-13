use serde::{Deserialize, Serialize};
use zone_spf::{BatchOutput, BatchWitness};

/// Current version of the prover request and response wire format.
pub const PROTOCOL_VERSION: u16 = 1;

/// Request to verify a Zone batch witness against a trusted Tempo chain.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyRequest {
    /// Wire-format version expected by the sender.
    pub version: u16,
    /// Caller-selected identifier echoed in the corresponding response.
    pub request_id: String,
    /// Tempo chain against which the witness must be verified.
    pub tempo_chain_id: u64,
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
        output: BatchOutput,
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
    /// The framed request exceeds the configured size limit.
    RequestTooLarge,
    /// The connection ended before the complete frame was received.
    TruncatedFrame,
    /// An unexpected transport or prover failure occurred.
    InternalError,
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
