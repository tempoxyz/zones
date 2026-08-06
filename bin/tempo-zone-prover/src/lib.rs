//! Versioned request protocol for the Tempo Zone prover service.

use std::{
    io::{self, Read, Write},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use tempo_chainspec::spec::chainspec_from_chain_id;
use zone_chainspec::ZoneChainSpec;
use zone_spf::{BatchOutput, BatchWitness, SpfConfig, prove_zone_batch};

pub const PROTOCOL_VERSION: u16 = 1;
pub const DEFAULT_VSOCK_PORT: u32 = 5000;
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 512 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyRequest {
    pub version: u16,
    pub request_id: String,
    pub tempo_chain_id: u64,
    pub witness: BatchWitness,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    tag = "status"
)]
pub enum VerifyResponse {
    #[serde(rename = "ok")]
    Ok {
        version: u16,
        request_id: String,
        output: BatchOutput,
    },
    #[serde(rename = "error")]
    Error {
        version: u16,
        #[serde(skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
        code: ErrorCode,
        message: String,
    },
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    MalformedRequest,
    UnsupportedVersion,
    UnsupportedChain,
    VerificationFailed,
    RequestTooLarge,
    TruncatedFrame,
    InternalError,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("request frame is truncated")]
    Truncated,
    #[error("request payload is {actual} bytes; maximum is {maximum} bytes")]
    TooLarge { actual: usize, maximum: usize },
    #[error("frame I/O failed: {0}")]
    Io(String),
}

pub fn read_frame(reader: &mut impl Read, maximum: usize) -> Result<Vec<u8>, FrameError> {
    let mut header = [0_u8; 4];
    read_exact_or_truncated(reader, &mut header)?;
    let length = u32::from_be_bytes(header) as usize;
    if length > maximum {
        return Err(FrameError::TooLarge {
            actual: length,
            maximum,
        });
    }

    let mut payload = vec![0_u8; length];
    read_exact_or_truncated(reader, &mut payload)?;
    Ok(payload)
}

pub fn write_frame(writer: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "response frame exceeds 4 GiB"))?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(payload)?;
    writer.flush()
}

pub fn process_payload(payload: &[u8]) -> VerifyResponse {
    let request = match serde_json::from_slice::<VerifyRequest>(payload) {
        Ok(request) => request,
        Err(error) => {
            return error_response(
                None,
                ErrorCode::MalformedRequest,
                format!("invalid request JSON: {error}"),
            );
        }
    };

    if request.version != PROTOCOL_VERSION {
        return error_response(
            Some(request.request_id),
            ErrorCode::UnsupportedVersion,
            format!(
                "unsupported protocol version {}; expected {PROTOCOL_VERSION}",
                request.version
            ),
        );
    }

    let Some(tempo_spec) = chainspec_from_chain_id(request.tempo_chain_id) else {
        return error_response(
            Some(request.request_id),
            ErrorCode::UnsupportedChain,
            format!("unsupported Tempo chain ID {}", request.tempo_chain_id),
        );
    };
    let config = SpfConfig::new(Arc::new(ZoneChainSpec::from(tempo_spec)));

    match prove_zone_batch(&config, request.witness) {
        Ok(output) => VerifyResponse::Ok {
            version: PROTOCOL_VERSION,
            request_id: request.request_id,
            output,
        },
        Err(error) => error_response(
            Some(request.request_id),
            ErrorCode::VerificationFailed,
            error.to_string(),
        ),
    }
}

pub fn frame_error_response(error: &FrameError) -> VerifyResponse {
    let code = match error {
        FrameError::TooLarge { .. } => ErrorCode::RequestTooLarge,
        FrameError::Truncated => ErrorCode::TruncatedFrame,
        FrameError::Io(_) => ErrorCode::InternalError,
    };
    error_response(None, code, error.to_string())
}

pub fn serialize_response(response: &VerifyResponse) -> Vec<u8> {
    // All response fields have infallible JSON representations.
    serde_json::to_vec(response).expect("serialize verifier response")
}

fn error_response(request_id: Option<String>, code: ErrorCode, message: String) -> VerifyResponse {
    VerifyResponse::Error {
        version: PROTOCOL_VERSION,
        request_id,
        code,
        message,
    }
}

fn read_exact_or_truncated(reader: &mut impl Read, buffer: &mut [u8]) -> Result<(), FrameError> {
    match reader.read_exact(buffer) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Err(FrameError::Truncated),
        Err(error) => Err(FrameError::Io(error.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use alloy_consensus::Header;
    use alloy_primitives::{Address, B256, Bytes};
    use reth_trie_common::EMPTY_ROOT_HASH;
    use tempo_primitives::TempoHeader;
    use zone_spf::{PublicInputs, TempoStateWitness, ZoneStateWitness};

    use super::*;

    #[test]
    fn frame_round_trip() {
        let payload = br#"{"version":1}"#;
        let mut encoded = Vec::new();
        write_frame(&mut encoded, payload).unwrap();

        assert_eq!(
            read_frame(&mut Cursor::new(encoded), 1024).unwrap(),
            payload
        );
    }

    #[test]
    fn rejects_oversized_and_truncated_frames() {
        let oversized = 10_u32.to_be_bytes();
        assert_eq!(
            read_frame(&mut Cursor::new(oversized), 4),
            Err(FrameError::TooLarge {
                actual: 10,
                maximum: 4,
            })
        );

        let truncated = [0, 0, 0, 2, 1];
        assert_eq!(
            read_frame(&mut Cursor::new(truncated), 4),
            Err(FrameError::Truncated)
        );
    }

    #[test]
    fn malformed_json_has_a_stable_error() {
        let response = process_payload(b"not json");
        assert!(matches!(
            response,
            VerifyResponse::Error {
                request_id: None,
                code: ErrorCode::MalformedRequest,
                ..
            }
        ));
    }

    #[test]
    fn error_response_uses_the_wire_field_names() {
        let encoded = serialize_response(&error_response(
            Some("wire-test".into()),
            ErrorCode::UnsupportedChain,
            "unsupported".into(),
        ));
        let json: serde_json::Value = serde_json::from_slice(&encoded).unwrap();

        assert_eq!(json["status"], "error");
        assert_eq!(json["version"], PROTOCOL_VERSION);
        assert_eq!(json["requestId"], "wire-test");
        assert_eq!(json["code"], "unsupported_chain");
    }

    #[test]
    fn rejects_unsupported_protocol_version() {
        let request = VerifyRequest {
            version: 2,
            request_id: "version-test".into(),
            tempo_chain_id: 42_431,
            witness: empty_witness(),
        };
        let response = process_payload(&serde_json::to_vec(&request).unwrap());

        assert!(matches!(
            response,
            VerifyResponse::Error {
                request_id: Some(id),
                code: ErrorCode::UnsupportedVersion,
                ..
            } if id == "version-test"
        ));
    }

    #[test]
    fn rejects_unknown_chain_before_execution() {
        let request = VerifyRequest {
            version: PROTOCOL_VERSION,
            request_id: "chain-test".into(),
            tempo_chain_id: 99,
            witness: empty_witness(),
        };
        let response = process_payload(&serde_json::to_vec(&request).unwrap());

        assert!(matches!(
            response,
            VerifyResponse::Error {
                request_id: Some(id),
                code: ErrorCode::UnsupportedChain,
                ..
            } if id == "chain-test"
        ));
    }

    #[test]
    fn reports_spf_verification_errors_without_panicking() {
        let request = VerifyRequest {
            version: PROTOCOL_VERSION,
            request_id: "spf-test".into(),
            tempo_chain_id: 42_431,
            witness: empty_witness(),
        };
        let response = process_payload(&serde_json::to_vec(&request).unwrap());

        assert!(matches!(
            response,
            VerifyResponse::Error {
                request_id: Some(id),
                code: ErrorCode::VerificationFailed,
                ..
            } if id == "spf-test"
        ));
    }

    fn empty_witness() -> BatchWitness {
        let tempo_header = TempoHeader {
            inner: Header {
                number: 2,
                state_root: EMPTY_ROOT_HASH,
                ..Default::default()
            },
            ..Default::default()
        };
        BatchWitness {
            public_inputs: PublicInputs {
                zone_id: 1,
                portal: Address::repeat_byte(0x11),
                tempo_block_number: 2,
                anchor_block_number: 2,
                anchor_block_hash: B256::ZERO,
                expected_withdrawal_batch_index: 3,
            },
            parent_header: TempoHeader::default(),
            zone_blocks: Vec::new(),
            zone_state_witness: ZoneStateWitness {
                node_pool: Vec::new(),
                bytecodes: Vec::new(),
            },
            tempo_state_witness: TempoStateWitness {
                initial_tempo_header_rlp: Bytes::from(alloy_rlp::encode(tempo_header)),
                node_pool: Vec::new(),
            },
            tempo_ancestry_headers: Vec::new(),
        }
    }
}
