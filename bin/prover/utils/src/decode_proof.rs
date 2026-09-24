use std::{collections::BTreeMap, io::Write, path::PathBuf};

use alloy_primitives::Bytes;
use eyre::{Context, Result, ensure, eyre};
use minicbor::{Decoder, data::Type};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zone_prover::decode_exact;

#[derive(Debug, clap::Args)]
pub(super) struct DecodeProofArgs {
    /// Saved prover response JSON (current or legacy format).
    #[arg(long, short, value_name = "PATH")]
    input: PathBuf,

    /// Write decoded JSON to a file instead of stdout.
    #[arg(long, short, value_name = "PATH")]
    output: Option<PathBuf>,
}

// Nitro's COSE headers contain the integer algorithm label and identifier.
type Headers = BTreeMap<i64, i64>;

#[derive(Deserialize)]
struct CoseSign1(Bytes, Headers, Bytes, Bytes);

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AttestationDocument {
    module_id: String,
    digest: String,
    timestamp: u64,
    pcrs: BTreeMap<u8, Bytes>,
    certificate: Bytes,
    cabundle: Vec<Bytes>,
    public_key: Option<Bytes>,
    user_data: Option<Bytes>,
    nonce: Option<Bytes>,
}

pub(super) fn run(args: DecodeProofArgs) -> Result<()> {
    let input = std::fs::read(&args.input)
        .wrap_err_with(|| format!("read proof from {}", args.input.display()))?;
    let response: Value = serde_json::from_slice(&input).context("parse prover response JSON")?;
    let decoded = decode_response(&response)?;
    let mut output = serde_json::to_vec_pretty(&decoded)?;
    output.push(b'\n');
    if let Some(path) = args.output {
        std::fs::write(&path, output)
            .wrap_err_with(|| format!("write decoded proof to {}", path.display()))?;
    } else {
        std::io::stdout().lock().write_all(&output)?;
    }
    Ok(())
}

fn decode_response(response: &Value) -> Result<Value> {
    let body = response.get("ok").unwrap_or(response);
    let proof = body
        .pointer("/proofBundle/proof")
        .ok_or_else(|| eyre!("missing proofBundle.proof in prover response"))?;
    let bytes: Bytes =
        serde_json::from_value(proof.clone()).context("decode proofBundle.proof hex")?;
    let mut decoder = Decoder::new(&bytes);
    if decoder.datatype()? == Type::Tag {
        ensure!(decoder.tag()?.as_u64() == 18, "expected COSE_Sign1 tag 18");
    }
    let CoseSign1(protected, unprotected, payload, signature) =
        decode_exact(&bytes[decoder.position()..]).context("decode COSE_Sign1")?;
    let protected: Headers = if protected.is_empty() {
        Headers::new()
    } else {
        decode_exact(&protected).context("decode protected headers")?
    };
    let payload: AttestationDocument =
        decode_exact(&payload).context("decode attestation payload")?;
    Ok(json!({
        "protected": protected,
        "unprotected": unprotected,
        "payload": payload,
        "signature": signature,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> Vec<u8> {
        payload_with_optional_fields(true)
    }

    fn payload_with_optional_fields(include_optional: bool) -> Vec<u8> {
        let mut payload = minicbor::Encoder::new(Vec::new());
        payload
            .begin_map()
            .unwrap()
            .str("module_id")
            .unwrap()
            .str("test-enclave")
            .unwrap()
            .str("digest")
            .unwrap()
            .str("SHA384")
            .unwrap()
            .str("timestamp")
            .unwrap()
            .u64(1_700_000_000_000)
            .unwrap()
            .str("pcrs")
            .unwrap()
            .map(1)
            .unwrap()
            .u8(0)
            .unwrap()
            .bytes(&[0; 48])
            .unwrap()
            .str("certificate")
            .unwrap()
            .bytes(&[0x30, 0x01])
            .unwrap()
            .str("cabundle")
            .unwrap()
            .array(1)
            .unwrap()
            .bytes(&[0x30, 0x02])
            .unwrap();
        if include_optional {
            payload
                .str("public_key")
                .unwrap()
                .bytes(&[0xef; 32])
                .unwrap()
                .str("user_data")
                .unwrap()
                .bytes(&[0xab; 32])
                .unwrap()
                .str("nonce")
                .unwrap()
                .null()
                .unwrap();
        }
        payload.end().unwrap();
        payload.into_writer()
    }

    fn proof_with(protected: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut cose = minicbor::Encoder::new(Vec::new());
        cose.array(4)
            .unwrap()
            .bytes(protected)
            .unwrap()
            .map(0)
            .unwrap()
            .bytes(payload)
            .unwrap()
            .bytes(&[0xcd; 96])
            .unwrap();
        cose.into_writer()
    }

    fn proof() -> Vec<u8> {
        proof_with(&[0xa1, 1, 0x38, 0x22], &payload())
    }

    fn response(bytes: &[u8]) -> Value {
        json!({"proofBundle": {"proof": Bytes::copy_from_slice(bytes)}})
    }

    #[test]
    fn decodes_current_legacy_and_tagged_proofs() {
        let proof = proof();
        let legacy = response(&proof);
        let decoded = decode_response(&legacy).unwrap();
        assert_eq!(decode_response(&json!({"ok": legacy})).unwrap(), decoded);
        assert_eq!(decoded["protected"]["1"], -35);
        assert_eq!(decoded["unprotected"], json!({}));
        assert_eq!(decoded["payload"]["module_id"], "test-enclave");
        assert_eq!(decoded["payload"]["digest"], "SHA384");
        assert_eq!(decoded["payload"]["timestamp"], 1_700_000_000_000_u64);
        assert_eq!(
            decoded["payload"]["pcrs"]["0"],
            format!("0x{}", "00".repeat(48))
        );
        assert_eq!(decoded["payload"]["certificate"], "0x3001");
        assert_eq!(decoded["payload"]["cabundle"], json!(["0x3002"]));
        assert_eq!(
            decoded["payload"]["public_key"],
            format!("0x{}", "ef".repeat(32))
        );
        assert_eq!(
            decoded["payload"]["user_data"],
            format!("0x{}", "ab".repeat(32))
        );
        assert!(decoded["payload"]["nonce"].is_null());
        assert_eq!(decoded["signature"], format!("0x{}", "cd".repeat(96)));
        let mut tagged = vec![0xd2];
        tagged.extend(proof);
        assert_eq!(decode_response(&response(&tagged)).unwrap(), decoded);
    }

    #[test]
    fn accepts_empty_protected_headers() {
        let decoded = decode_response(&response(&proof_with(&[], &payload()))).unwrap();
        assert_eq!(decoded["protected"], json!({}));
    }

    #[test]
    fn decodes_absent_optional_fields() {
        let proof = proof_with(&[], &payload_with_optional_fields(false));
        let decoded = decode_response(&response(&proof)).unwrap();
        for field in ["public_key", "user_data", "nonce"] {
            assert_eq!(decoded["payload"].get(field), Some(&Value::Null));
        }
    }

    #[test]
    fn rejects_missing_truncated_and_trailing_data() {
        assert!(decode_response(&json!({"error": {}})).is_err());
        assert!(decode_response(&response(&[])).is_err());
        let mut bytes = proof();
        assert!(decode_response(&response(&bytes[..bytes.len() - 1])).is_err());
        bytes.push(0);
        assert!(decode_response(&response(&bytes)).is_err());
        assert!(decode_response(&response(&proof_with(&[0xa0, 0], &payload()))).is_err());
        let mut payload = payload();
        payload.push(0);
        assert!(decode_response(&response(&proof_with(&[], &payload))).is_err());
    }

    #[test]
    fn rejects_wrong_tag_shape_and_payload_schema() {
        let mut tagged = vec![0xd1];
        tagged.extend(proof());
        assert!(decode_response(&response(&tagged)).is_err());
        let mut extra_field = proof();
        extra_field[0] = 0x85;
        extra_field.push(0);
        assert!(decode_response(&response(&extra_field)).is_err());
        assert!(decode_response(&response(&proof_with(&[0x80], &payload()))).is_err());
        assert!(decode_response(&response(&proof_with(&[], &[0xa0]))).is_err());
        let mut duplicate_field = payload();
        duplicate_field.pop();
        duplicate_field.extend_from_slice(&[0x65, b'n', b'o', b'n', b'c', b'e', 0xf6, 0xff]);
        assert!(decode_response(&response(&proof_with(&[], &duplicate_field))).is_err());
    }
}
