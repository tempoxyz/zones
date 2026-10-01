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
