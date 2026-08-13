//! Versioned request protocol for the Tempo Zone prover service.

use std::{collections::HashMap, io, sync::Arc};

use serde::{Deserialize, Serialize};
use tempo_chainspec::{TempoChainSpec, spec::chainspec_from_chain_id};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_util::codec::{Framed, LengthDelimitedCodec, LengthDelimitedCodecError};
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

/// Tempo chain specifications trusted by this prover in addition to built-in networks.
#[derive(Debug, Default)]
pub struct TrustedChainSpecs {
    custom: HashMap<u64, Arc<TempoChainSpec>>,
}

impl TrustedChainSpecs {
    /// Registers an immutable custom chain specification by chain ID.
    pub fn insert(
        &mut self,
        chain_id: u64,
        spec: Arc<TempoChainSpec>,
    ) -> Result<(), TrustedChainSpecError> {
        if chainspec_from_chain_id(chain_id).is_some() {
            return Err(TrustedChainSpecError::BuiltIn(chain_id));
        }
        if self.custom.insert(chain_id, spec).is_some() {
            return Err(TrustedChainSpecError::Duplicate(chain_id));
        }
        Ok(())
    }

    fn resolve(&self, chain_id: u64) -> Option<Arc<TempoChainSpec>> {
        self.custom
            .get(&chain_id)
            .cloned()
            .or_else(|| chainspec_from_chain_id(chain_id))
    }

    /// Returns whether this prover supports the supplied Tempo chain ID.
    pub fn supports(&self, chain_id: u64) -> bool {
        self.resolve(chain_id).is_some()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TrustedChainSpecError {
    #[error("Tempo chain ID {0} is built in and cannot be overridden")]
    BuiltIn(u64),
    #[error("duplicate custom Tempo chain ID {0}")]
    Duplicate(u64),
}

/// Wraps an I/O stream in the prover's four-byte, big-endian length-delimited protocol.
pub fn framed<T>(io: T, maximum: usize) -> Framed<T, LengthDelimitedCodec>
where
    T: AsyncRead + AsyncWrite,
{
    LengthDelimitedCodec::builder()
        .max_frame_length(maximum)
        .new_framed(io)
}

/// Converts a framing I/O error directly into a prover protocol response.
pub fn framing_error_response(error: &io::Error, maximum: usize) -> VerifyResponse {
    let (code, message) = if error
        .get_ref()
        .is_some_and(|source| source.is::<LengthDelimitedCodecError>())
    {
        (
            ErrorCode::RequestTooLarge,
            format!("request payload exceeds the maximum of {maximum} bytes"),
        )
    } else if error.kind() == io::ErrorKind::Other
        && error.to_string() == "bytes remaining on stream"
    {
        (
            ErrorCode::TruncatedFrame,
            "request frame is truncated".into(),
        )
    } else {
        (
            ErrorCode::InternalError,
            format!("frame I/O failed: {error}"),
        )
    };
    error_response(None, code, message)
}

pub fn process_payload(payload: &[u8], specs: &TrustedChainSpecs) -> VerifyResponse {
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

    let Some(tempo_spec) = specs.resolve(request.tempo_chain_id) else {
        return error_response(
            Some(request.request_id),
            ErrorCode::UnsupportedChain,
            format!("unsupported Tempo chain ID {}", request.tempo_chain_id),
        );
    };
    let config = SpfConfig::new(
        Arc::new(ZoneChainSpec::from(tempo_spec)),
        request.witness.public_inputs.portal,
    );

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

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;
    use alloy_primitives::{Address, B256, Bytes};
    use reth_trie_common::EMPTY_ROOT_HASH;
    use tempo_primitives::TempoHeader;
    use zone_spf::{PublicInputs, TempoStateWitness, ZoneStateWitness};

    use super::*;

    #[tokio::test]
    async fn frame_round_trip() {
        use futures::{SinkExt as _, StreamExt as _};

        let payload = br#"{"version":1}"#;
        let (client, server) = tokio::io::duplex(1024);
        let mut client = framed(client, 1024);
        let mut server = framed(server, 1024);
        client.send(payload.as_slice().into()).await.unwrap();

        assert_eq!(server.next().await.unwrap().unwrap(), payload.as_slice());
    }

    #[tokio::test]
    async fn rejects_oversized_and_truncated_frames() {
        use futures::StreamExt as _;
        use tokio::io::AsyncWriteExt as _;

        let (mut writer, reader) = tokio::io::duplex(1024);
        writer.write_all(&10_u32.to_be_bytes()).await.unwrap();
        let error = framed(reader, 4).next().await.unwrap().unwrap_err();
        assert!(matches!(
            framing_error_response(&error, 4),
            VerifyResponse::Error {
                code: ErrorCode::RequestTooLarge,
                ..
            }
        ));

        let (mut writer, reader) = tokio::io::duplex(1024);
        writer.write_all(&[0, 0, 0, 2, 1]).await.unwrap();
        writer.shutdown().await.unwrap();
        let error = framed(reader, 4).next().await.unwrap().unwrap_err();
        assert!(matches!(
            framing_error_response(&error, 4),
            VerifyResponse::Error {
                code: ErrorCode::TruncatedFrame,
                ..
            }
        ));
    }

    #[test]
    fn malformed_json_has_a_stable_error() {
        let response = process_payload(b"not json", &TrustedChainSpecs::default());
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
        let response = process_payload(
            &serde_json::to_vec(&request).unwrap(),
            &TrustedChainSpecs::default(),
        );

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
        let response = process_payload(
            &serde_json::to_vec(&request).unwrap(),
            &TrustedChainSpecs::default(),
        );

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
        let response = process_payload(
            &serde_json::to_vec(&request).unwrap(),
            &TrustedChainSpecs::default(),
        );

        assert!(matches!(
            response,
            VerifyResponse::Error {
                request_id: Some(id),
                code: ErrorCode::VerificationFailed,
                ..
            } if id == "spf-test"
        ));
    }

    #[test]
    fn accepts_a_configured_custom_chain() {
        let chain_id = 31_318;
        let mut genesis = alloy_genesis::Genesis::default();
        genesis.config.chain_id = chain_id;
        let mut specs = TrustedChainSpecs::default();
        specs
            .insert(chain_id, Arc::new(TempoChainSpec::from_genesis(genesis)))
            .unwrap();
        let request = VerifyRequest {
            version: PROTOCOL_VERSION,
            request_id: "custom-chain-test".into(),
            tempo_chain_id: chain_id,
            witness: empty_witness(),
        };

        let response = process_payload(&serde_json::to_vec(&request).unwrap(), &specs);

        assert!(matches!(
            response,
            VerifyResponse::Error {
                request_id: Some(id),
                code: ErrorCode::VerificationFailed,
                ..
            } if id == "custom-chain-test"
        ));
    }

    #[test]
    fn custom_chains_cannot_override_builtins() {
        let error = TrustedChainSpecs::default()
            .insert(42_431, tempo_chainspec::spec::MODERATO.clone())
            .unwrap_err();

        assert_eq!(error, TrustedChainSpecError::BuiltIn(42_431));
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
                parent_chain_id: 42_431,
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
