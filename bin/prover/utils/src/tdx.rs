use super::*;
use alloy_primitives::FixedBytes;
use std::time::{SystemTime, UNIX_EPOCH};
use zone_prover::tdx::Policy;

#[derive(Debug, clap::Args)]
pub(super) struct TdxArgs {
    #[command(subcommand)]
    command: TdxCommand,
}

#[derive(Debug, clap::Subcommand)]
enum TdxCommand {
    /// Generate a raw quote inside a TDX guest for externally supplied 64-byte report data.
    Quote {
        #[arg(long)]
        report_data: FixedBytes<64>,
        #[arg(long)]
        output: PathBuf,
    },
    /// Verify a raw quote locally using Intel DCAP, trusted policy, and expected report data.
    VerifyQuote {
        #[arg(long)]
        quote: PathBuf,
        #[arg(long)]
        report_data: FixedBytes<64>,
        #[arg(long)]
        attestation_policy: PathBuf,
    },
    /// Verify a saved batch response's TDX evidence and commitments without calling L1.
    VerifyBatch {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        proof: PathBuf,
        #[arg(long)]
        attestation_policy: PathBuf,
    },
    /// Authenticate a fresh TDX TLS connection without sending a witness.
    Connect {
        #[arg(long)]
        target: String,
        #[arg(long)]
        attestation_policy: PathBuf,
    },
}

fn policy(path: &std::path::Path) -> Result<Policy> {
    let bytes =
        std::fs::read(path).wrap_err_with(|| format!("read TDX policy {}", path.display()))?;
    Policy::from_json(&bytes).context("parse explicit TDX policy")
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

pub(super) async fn run(args: TdxArgs) -> Result<()> {
    match args.command {
        TdxCommand::Quote {
            report_data,
            output,
        } => {
            let quote =
                tokio::task::spawn_blocking(move || zone_prover::tdx::quote(&report_data.0))
                    .await??;
            std::fs::write(output, quote).context("write raw TDX quote")?;
            println!("TDX quote generated");
        }
        TdxCommand::VerifyQuote {
            quote,
            report_data,
            attestation_policy,
        } => {
            let policy = policy(&attestation_policy)?;
            let quote = std::fs::read(quote).context("read raw TDX quote")?;
            let now = now()?;
            tokio::task::spawn_blocking(move || policy.verify(&quote, &report_data.0, now))
                .await??;
            println!("TDX quote verified");
        }
        TdxCommand::VerifyBatch {
            input,
            proof,
            attestation_policy,
        } => {
            let policy = policy(&attestation_policy)?;
            let input = std::fs::read(input).context("read original witness")?;
            let witness: BatchWitness =
                serde_json::from_slice(&input).context("parse original witness")?;
            let response: VerifyResponse = serde_json::from_slice(&std::fs::read(proof)?)
                .context("parse saved proof response")?;
            let (output, bundle) =
                validate_proof_response(&response, &format!("prove-{}", keccak256(&input)))?;
            let output = output.clone();
            let bundle = bundle.clone();
            let now = now()?;
            tokio::task::spawn_blocking(move || {
                policy.verify_batch(&bundle, &witness.public_inputs, &output, now)
            })
            .await??;
            println!("TDX batch evidence verified");
        }
        TdxCommand::Connect {
            target,
            attestation_policy,
        } => {
            // Require a TDX policy even though the shared transport also supports Nitro.
            policy(&attestation_policy)?;
            let config = RemoteProverConfig::from_policy_file(target, &attestation_policy)?;
            let _stream = config
                .connect()
                .await
                .context("authenticate fresh TDX TLS session")?;
            println!("TDX TLS authenticated");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_verification_requires_complete_external_challenge() {
        let base = [
            "tempo-zone-prover-utils",
            "tdx",
            "verify-quote",
            "--quote",
            "quote.bin",
            "--attestation-policy",
            "policy.json",
        ];
        assert!(Cli::try_parse_from(base).is_err());
        let mut args = base.to_vec();
        args.extend(["--report-data", "0x00"]);
        assert!(Cli::try_parse_from(args).is_err());
        let report_data = format!("0x{}", "01".repeat(64));
        let mut args = base.to_vec();
        args.extend(["--report-data", &report_data]);
        assert!(Cli::try_parse_from(args).is_ok());
    }
}
