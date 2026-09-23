use std::{io::Write, path::PathBuf};

use eyre::{Context, Result, bail, ensure, eyre};
use minicbor::{Decoder, data::Type};
use serde_json::{Map, Value, json};

#[derive(Debug, clap::Args)]
pub(super) struct DecodeProofArgs {
    /// Saved prover response JSON (current or legacy format).
    #[arg(long, short, value_name = "PATH")]
    input: PathBuf,

    /// Write decoded JSON to a file instead of stdout.
    #[arg(long, short, value_name = "PATH")]
    output: Option<PathBuf>,
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
    let bytes = hex_bytes(proof).context("decode proofBundle.proof hex")?;
    let mut decoder = Decoder::new(&bytes);
    if decoder.datatype()? == Type::Tag {
        ensure!(decoder.tag()?.as_u64() == 18, "expected COSE_Sign1 tag 18");
    }
    let envelope = value(&mut decoder, 0).context("decode COSE_Sign1")?;
    ensure!(
        decoder.position() == bytes.len(),
        "trailing bytes after COSE_Sign1"
    );
    let fields = envelope
        .as_array()
        .ok_or_else(|| eyre!("expected COSE_Sign1 array"))?;
    ensure!(fields.len() == 4, "expected four COSE_Sign1 fields");
    let protected_bytes = hex_bytes(&fields[0]).context("read protected headers")?;
    let protected = if protected_bytes.is_empty() {
        json!({})
    } else {
        decode_cbor(&protected_bytes).context("decode protected headers")?
    };
    ensure!(protected.is_object(), "expected protected header map");
    ensure!(fields[1].is_object(), "expected unprotected header map");
    let payload = decode_cbor(&hex_bytes(&fields[2]).context("read attestation payload")?)
        .context("decode attestation payload")?;
    ensure!(payload.is_object(), "expected attestation payload map");
    hex_bytes(&fields[3]).context("read COSE signature")?;
    Ok(json!({
        "protected": protected,
        "unprotected": fields[1],
        "payload": payload,
        "signature": fields[3],
    }))
}

fn hex_bytes(value: &Value) -> Result<Vec<u8>> {
    let hex = value
        .as_str()
        .and_then(|s| s.strip_prefix("0x"))
        .ok_or_else(|| eyre!("expected 0x-prefixed byte string"))?;
    const_hex::decode(hex).context("invalid hex byte string")
}

fn decode_cbor(bytes: &[u8]) -> Result<Value> {
    let mut decoder = Decoder::new(bytes);
    let result = value(&mut decoder, 0)?;
    ensure!(
        decoder.position() == bytes.len(),
        "trailing bytes after CBOR item"
    );
    Ok(result)
}

// Nitro uses this subset of CBOR. Byte strings stay hex; only the two known
// COSE fields above are recursively decoded as embedded CBOR.
fn value(decoder: &mut Decoder<'_>, depth: usize) -> Result<Value> {
    ensure!(depth < 32, "CBOR nesting exceeds 32 levels");
    Ok(match decoder.datatype()? {
        Type::U8 | Type::U16 | Type::U32 | Type::U64 => json!(decoder.u64()?),
        Type::I8 | Type::I16 | Type::I32 | Type::I64 => json!(decoder.i64()?),
        Type::Bool => json!(decoder.bool()?),
        Type::Null => {
            decoder.null()?;
            Value::Null
        }
        Type::Bytes | Type::BytesIndef => {
            let mut bytes = Vec::new();
            for chunk in decoder.bytes_iter()? {
                bytes.extend_from_slice(chunk?);
            }
            json!(format!("0x{}", const_hex::encode(bytes)))
        }
        Type::String | Type::StringIndef => {
            let mut text = String::new();
            for chunk in decoder.str_iter()? {
                text.push_str(chunk?);
            }
            json!(text)
        }
        Type::Array | Type::ArrayIndef => {
            let length = decoder.array()?;
            let mut items = Vec::new();
            while more(decoder, length, items.len())? {
                items.push(value(decoder, depth + 1)?);
            }
            Value::Array(items)
        }
        Type::Map | Type::MapIndef => {
            let length = decoder.map()?;
            let mut entries = Map::new();
            while more(decoder, length, entries.len())? {
                let key = match value(decoder, depth + 1)? {
                    Value::String(key) => key,
                    Value::Number(key) => key.to_string(),
                    _ => bail!("expected text or integer CBOR map key"),
                };
                let item = value(decoder, depth + 1)?;
                ensure!(
                    entries.insert(key.clone(), item).is_none(),
                    "duplicate JSON map key {key}"
                );
            }
            Value::Object(entries)
        }
        other => bail!("unsupported attestation CBOR type: {other}"),
    })
}

fn more(decoder: &mut Decoder<'_>, length: Option<u64>, count: usize) -> Result<bool> {
    if let Some(length) = length {
        return Ok((count as u64) < length);
    }
    if decoder.datatype()? == Type::Break {
        decoder.skip()?;
        return Ok(false);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proof() -> Vec<u8> {
        let mut payload = minicbor::Encoder::new(Vec::new());
        payload
            .begin_map()
            .unwrap()
            .str("pcrs")
            .unwrap()
            .map(1)
            .unwrap()
            .u8(0)
            .unwrap()
            .bytes(&[0; 48])
            .unwrap()
            .str("user_data")
            .unwrap()
            .bytes(&[0xab; 32])
            .unwrap()
            .str("nonce")
            .unwrap()
            .null()
            .unwrap()
            .end()
            .unwrap();
        let mut cose = minicbor::Encoder::new(Vec::new());
        cose.array(4)
            .unwrap()
            .bytes(&[0xa1, 1, 0x38, 0x22])
            .unwrap()
            .map(0)
            .unwrap()
            .bytes(&payload.into_writer())
            .unwrap()
            .bytes(&[0xcd; 96])
            .unwrap();
        cose.into_writer()
    }

    fn response(bytes: &[u8]) -> Value {
        json!({"proofBundle": {"proof": format!("0x{}", const_hex::encode(bytes))}})
    }

    #[test]
    fn decodes_current_legacy_and_tagged_proofs() {
        let proof = proof();
        let legacy = response(&proof);
        let decoded = decode_response(&legacy).unwrap();
        assert_eq!(decode_response(&json!({"ok": legacy})).unwrap(), decoded);
        assert_eq!(decoded["protected"]["1"], -35);
        assert_eq!(
            decoded["payload"]["pcrs"]["0"],
            format!("0x{}", "00".repeat(48))
        );
        assert_eq!(
            decoded["payload"]["user_data"],
            format!("0x{}", "ab".repeat(32))
        );
        assert!(decoded["payload"]["nonce"].is_null());
        let mut tagged = vec![0xd2];
        tagged.extend(proof);
        assert_eq!(decode_response(&response(&tagged)).unwrap(), decoded);
    }

    #[test]
    fn rejects_missing_truncated_and_trailing_data() {
        assert!(decode_response(&json!({"error": {}})).is_err());
        assert!(decode_response(&response(&[])).is_err());
        let mut bytes = proof();
        assert!(decode_response(&response(&bytes[..bytes.len() - 1])).is_err());
        bytes.push(0);
        assert!(decode_response(&response(&bytes)).is_err());
        assert!(decode_cbor(&[0xa2, 1, 0, 1, 1]).is_err());
        assert!(decode_cbor(&[0xa0, 0]).is_err());
    }
}
