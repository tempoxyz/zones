use std::{fs, path::PathBuf, time::Duration};

use eyre::{Context, Result, ensure};
use serde::Serialize;
use zone_spf::{SpfConfig, ZoneBlock};

use crate::{
    GenerateInputArgs, ZONE_HEAD_POLL_INTERVAL, connect, discover, generate_batch, load_chain,
    portal_parent_number,
};

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[arg(long)]
    tempo_rpc_url: String,
    #[arg(long)]
    zone_rpc_url: String,
    /// Zone genesis JSON (path, inline JSON, or URL).
    #[arg(long)]
    chain: String,
    /// First measured block; the containing batch is included in full.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    from_block: u64,
    /// Last measured block; the containing batch is included in full.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    to_block: u64,
    /// Maximum seconds to wait for each batch to be submitted.
    #[arg(long, default_value_t = 120)]
    wait_timeout: u64,
    /// Fail if the measured blocks contain no user transactions.
    #[arg(long)]
    require_user_transactions: bool,
    /// Completion manifest; witnesses are saved in a sibling .batches directory.
    #[arg(long)]
    output: PathBuf,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    from_block: u64,
    to_block: u64,
    user_transactions: usize,
    batches: Vec<BatchSummary>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BatchSummary {
    from_block: u64,
    to_block: u64,
    witness: PathBuf,
    measured_user_transactions: usize,
}

impl Manifest {
    fn next_block(&self) -> Option<u64> {
        match self.batches.last() {
            Some(batch) if batch.to_block >= self.to_block => None,
            Some(batch) => Some(batch.to_block + 1),
            None => Some(self.from_block),
        }
    }

    // Blocks have already passed native SPF validation; only check range coverage here.
    fn record(&mut self, blocks: &[ZoneBlock], witness: PathBuf) -> Result<()> {
        let cursor = self
            .next_block()
            .ok_or_else(|| eyre::eyre!("range already complete"))?;
        let first = blocks
            .first()
            .ok_or_else(|| eyre::eyre!("empty validated batch"))?
            .number;
        let last = blocks.last().expect("non-empty batch").number;
        ensure!(
            first <= cursor && cursor <= last,
            "batch does not contain block {cursor}"
        );
        ensure!(
            self.batches.is_empty() || first == cursor,
            "overlapping batches at block {cursor}"
        );
        let count = blocks
            .iter()
            .filter(|block| (self.from_block..=self.to_block).contains(&block.number))
            .map(|block| block.transactions.len())
            .sum();
        self.user_transactions += count;
        self.batches.push(BatchSummary {
            from_block: first,
            to_block: last,
            witness,
            measured_user_transactions: count,
        });
        Ok(())
    }
}

pub(crate) async fn run(args: Args) -> Result<()> {
    ensure!(args.from_block <= args.to_block, "reversed measured range");
    ensure!(
        !args.output.try_exists()?,
        "output already exists: {}",
        args.output.display()
    );
    let directory = args.output.with_extension("batches");
    if let Some(parent) = args
        .output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    // Exclusive creation prevents reuse of witnesses or a stale completion marker.
    fs::create_dir(&directory).context("create new batch output directory")?;
    let config = SpfConfig::new(load_chain(&args.chain).await?);
    let tempo = connect(&args.tempo_rpc_url, "Tempo").await?;
    let zone = connect(&args.zone_rpc_url, "unrestricted Zone").await?;
    let mut manifest = Manifest {
        from_block: args.from_block,
        to_block: args.to_block,
        user_transactions: 0,
        batches: Vec::new(),
    };
    while let Some(block) = manifest.next_block() {
        tokio::time::timeout(Duration::from_secs(args.wait_timeout), async {
            loop {
                let discovery = discover(&tempo, &zone).await?;
                if portal_parent_number(&zone, &discovery).await? >= block {
                    return Ok::<_, eyre::Report>(());
                }
                tokio::time::sleep(ZONE_HEAD_POLL_INTERVAL).await;
            }
        })
        .await
        .wrap_err_with(|| format!("timed out waiting for batch containing block {block}"))??;
        let path = directory.join(format!("block-{block}.json"));
        let input = GenerateInputArgs {
            tempo_rpc_url: args.tempo_rpc_url.clone(),
            zone_rpc_url: args.zone_rpc_url.clone(),
            chain: args.chain.clone(),
            block: Some(block.into()),
            from_block: None,
            to_block: None,
            zone_block_count: None,
            wait_timeout: None,
            output: Some(path.clone()),
            target: None,
        };
        let witness = generate_batch(&input, &tempo, &zone, &config).await?;
        manifest.record(&witness.zone_blocks, path)?;
    }
    ensure!(
        !args.require_user_transactions || manifest.user_transactions > 0,
        "measured range contains no Zone user transactions"
    );
    // Publish only after every overlapping batch passes. CI checks this even if Samply
    // masks our exit status. Keep the rename on the same filesystem.
    let temporary = directory.join("manifest.json");
    fs::write(&temporary, serde_json::to_vec_pretty(&manifest)?)?;
    fs::rename(temporary, &args.output)?;
    println!(
        "Validated {} submitted batches covering measured Zone blocks {}..={} ({} user transactions)",
        manifest.batches.len(),
        manifest.from_block,
        manifest.to_block,
        manifest.user_transactions
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, B256, Bytes};
    use zone_spf::TempoImport;

    #[test]
    fn covers_complete_batches_but_counts_only_measured_transactions() {
        let blocks = |from, to| {
            (from..=to)
                .map(|number| ZoneBlock {
                    number,
                    parent_hash: B256::ZERO,
                    timestamp: 0,
                    timestamp_millis_part: 0,
                    beneficiary: Address::ZERO,
                    tempo_import: TempoImport::CheckpointOnly {
                        headers_rlp: vec![],
                    },
                    finalize_withdrawal_batch_count: None,
                    finalize_withdrawal_batch_encrypted_senders: vec![],
                    transactions: vec![Bytes::new()],
                })
                .collect::<Vec<_>>()
        };
        let mut manifest = Manifest {
            from_block: 2,
            to_block: 6,
            user_transactions: 0,
            batches: vec![],
        };
        assert_eq!(manifest.next_block(), Some(2));
        manifest.record(&blocks(1, 2), "first.json".into()).unwrap();
        assert_eq!(manifest.next_block(), Some(3));
        assert!(manifest.record(&blocks(4, 5), "gap.json".into()).is_err());
        assert!(
            manifest
                .record(&blocks(2, 4), "overlap.json".into())
                .is_err()
        );
        manifest
            .record(&blocks(3, 4), "middle.json".into())
            .unwrap();
        manifest.record(&blocks(5, 7), "last.json".into()).unwrap();
        assert_eq!(manifest.next_block(), None);
        assert_eq!(manifest.batches.len(), 3);
        assert_eq!(manifest.user_transactions, 5);
    }
}
