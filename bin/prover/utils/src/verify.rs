use super::*;
use serde_json::Value;

#[derive(Debug, clap::Args)]
pub(super) struct VerifyArgs {
    /// Original batch witness JSON used by prove.
    #[arg(long, short, value_name = "PATH")]
    input: PathBuf,

    /// Complete proof response JSON saved by prove.
    #[arg(long, value_name = "PATH")]
    proof: PathBuf,

    /// L1 HTTP or WebSocket RPC URL on which to call the native verifier.
    #[arg(long, value_name = "URL")]
    rpc_url: String,
}

pub(super) async fn run(args: VerifyArgs) -> Result<()> {
    let total_started = Instant::now();
    let mut timings = Timings::default();
    info!(input = %args.input.display(), proof = %args.proof.display(), "verifying saved proof");
    let started = start_phase("read witness and proof");
    let input = std::fs::read(&args.input)
        .wrap_err_with(|| format!("read batch witness from {}", args.input.display()))?;
    let proof = std::fs::read(&args.proof)
        .wrap_err_with(|| format!("read proof response from {}", args.proof.display()))?;
    let witness: Value = serde_json::from_slice(&input).context("parse batch witness JSON")?;
    let response: VerifyResponse =
        serde_json::from_slice(&proof).context("parse proof response JSON")?;
    let (output, proof_bundle) =
        validate_proof_response(&response, &format!("prove-{}", keccak256(&input)))?;
    let response = serde_json::json!({ "output": output, "proofBundle": proof_bundle });
    info!(
        witness_bytes = input.len(),
        proof_bytes = proof.len(),
        "loaded witness and matching proof response"
    );
    timings.record("read witness and proof", started, ());

    let started = start_phase("encode verifier call");
    let request = verifier_request::build(&witness, &response)?;
    info!(zone_id = %request["arguments"]["zoneId"], batch_index = %request["arguments"]["expectedWithdrawalBatchIndex"], from = %request["rpc"]["params"][0]["from"], to = %request["rpc"]["params"][0]["to"], "encoded native verifier call");
    debug!(arguments = %request["arguments"], "verifier arguments");
    timings.record("encode verifier call", started, ());

    let started = start_phase("connect to L1");
    let provider = connect(&args.rpc_url, "L1 verifier").await?;
    let chain_id = provider.get_chain_id().await.context("read L1 chain ID")?;
    if request["chainId"].as_u64() != Some(chain_id) {
        bail!(
            "L1 RPC chain ID {chain_id} does not match witness parent chain ID {}",
            request["chainId"]
        );
    }
    info!(chain_id, "connected to witness parent chain");
    timings.record("connect to L1", started, ());

    let started = start_phase("verifier eth_call");
    info!(
        gas_limit = 30_000_000,
        block = "latest",
        "calling native verifier with eth_call"
    );
    call_verifier(&provider, request["rpc"]["params"].clone()).await?;
    timings.record("verifier eth_call", started, ());
    println!("Proof verified: true");
    timings.print(total_started.elapsed());
    Ok(())
}

async fn call_verifier(provider: &DynProvider<TempoNetwork>, params: Value) -> Result<()> {
    let result: Bytes = provider
        .raw_request("eth_call".into(), params)
        .await
        .context("native verifier eth_call failed")?;
    let mut expected = [0u8; 32];
    expected[31] = 1;
    if result.as_ref() != expected {
        bail!("native verifier did not return ABI-encoded true: {result}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_verify_without_a_wallet() {
        let cli = Cli::try_parse_from([
            "tempo-zone-prover-utils",
            "verify",
            "--input",
            "witness.json",
            "--proof",
            "proof.json",
            "--rpc-url",
            "http://localhost:8545",
        ])
        .unwrap();
        let Command::Verify(args) = cli.command else {
            panic!("expected verify")
        };
        assert_eq!(args.input, PathBuf::from("witness.json"));
        assert_eq!(args.proof, PathBuf::from("proof.json"));
        assert_eq!(args.rpc_url, "http://localhost:8545");
    }

    #[tokio::test]
    async fn requires_exact_true_from_verifier() {
        let asserter = alloy_transport::mock::Asserter::new();
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect_mocked_client(asserter.clone())
            .erased();
        let mut success = [0u8; 32];
        success[31] = 1;
        for (bytes, valid) in [
            (Bytes::from(success.to_vec()), true),
            (Bytes::from(vec![0; 32]), false),
            (Bytes::new(), false),
            (Bytes::from(vec![1]), false),
            (Bytes::from(vec![2; 32]), false),
        ] {
            asserter.push_success(&bytes);
            assert_eq!(
                call_verifier(&provider, serde_json::json!([{}, "latest"]))
                    .await
                    .is_ok(),
                valid
            );
        }
    }
}
