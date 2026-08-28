//! Compares finalized Zone history across operator RPC endpoints.

use std::{collections::HashSet, future::Future, time::Duration};

use alloy::{
    primitives::{B256, U64},
    providers::{Provider, ProviderBuilder},
};
use eyre::{Context as _, ensure, eyre};
use futures::future::join_all;
use serde::Deserialize;
use tempo_alloy::TempoNetwork;
use tokio::time::timeout;

#[derive(Debug, clap::Parser)]
pub(crate) struct OperatorAgreement {
    /// Operator HTTP RPC URLs to compare.
    #[arg(value_name = "RPC", num_args = 2..)]
    operator_rpcs: Vec<String>,

    /// Timeout for each request, in seconds.
    #[arg(long, default_value_t = 10)]
    timeout_seconds: u64,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct RpcBlock {
    number: U64,
    hash: B256,
    state_root: B256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AgreedBlock {
    height: u64,
    hash: B256,
    state_root: B256,
}

impl From<RpcBlock> for AgreedBlock {
    fn from(block: RpcBlock) -> Self {
        Self {
            height: block.number.to::<u64>(),
            hash: block.hash,
            state_root: block.state_root,
        }
    }
}

impl OperatorAgreement {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        ensure!(
            self.timeout_seconds > 0,
            "--timeout-seconds must be nonzero"
        );
        let mut unique_rpcs = HashSet::new();
        for rpc in &self.operator_rpcs {
            ensure!(
                rpc.starts_with("http://") || rpc.starts_with("https://"),
                "operator RPC must be an http:// or https:// URL: {rpc}"
            );
            ensure!(unique_rpcs.insert(rpc), "duplicate operator RPC: {rpc}");
        }

        let rpc_timeout = Duration::from_secs(self.timeout_seconds);
        let finalized =
            sample_operator_blocks(&self.operator_rpcs, "finalized", rpc_timeout).await?;

        println!("Finalized operator views");
        print_blocks(&self.operator_rpcs, &finalized);

        if agreed_block(&finalized).is_some() {
            println!(
                "\nAll {} operators agree at finalized Zone block {}.",
                finalized.len(),
                finalized[0].number
            );
            return Ok(());
        }

        let upper = finalized
            .iter()
            .map(|block| block.number.to::<u64>())
            .min()
            .expect("at least two finalized blocks were requested");
        let shared =
            sample_operator_blocks(&self.operator_rpcs, &hex_height(upper), rpc_timeout).await?;
        if let Some(block) = agreed_block(&shared) {
            println!(
                "\nOperators agree at shared finalized block {}, but one or more operators have advanced beyond it.",
                block.height
            );
            print_blocks(&self.operator_rpcs, &shared);
            return Err(eyre!("operator finalized tips are lagging or inconsistent"));
        }

        let (last_agreed, first_divergent_height) = bisect_last_agreed(upper, |height| {
            let rpcs = &self.operator_rpcs;
            async move {
                let blocks = sample_operator_blocks(rpcs, &hex_height(height), rpc_timeout).await?;
                Ok(agreed_block(&blocks))
            }
        })
        .await?;

        println!("\nOperators have divergent finalized Zone history.");
        match last_agreed {
            Some(block) => {
                println!(
                    "Last agreed block: {} {} {}",
                    block.height, block.hash, block.state_root
                );
                println!("First divergent height: {first_divergent_height}");
            }
            None => println!("No shared block could be proven through height {upper}."),
        }

        Err(eyre!("operator finalized history diverged"))
    }
}

async fn sample_operator_blocks(
    operator_rpcs: &[String],
    block: &str,
    rpc_timeout: Duration,
) -> eyre::Result<Vec<RpcBlock>> {
    let results = join_all(operator_rpcs.iter().map(|rpc| async move {
        let request = async {
            let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
                .connect(rpc)
                .await
                .wrap_err_with(|| format!("failed connecting to {rpc}"))?;
            let response: Option<RpcBlock> = provider
                .raw_request("eth_getBlockByNumber".into(), (block.to_owned(), false))
                .await
                .wrap_err_with(|| format!("eth_getBlockByNumber({block}) failed at {rpc}"))?;
            response.ok_or_else(|| eyre!("{rpc} returned no block for {block}"))
        };
        timeout(rpc_timeout, request)
            .await
            .map_err(|_| eyre!("eth_getBlockByNumber({block}) timed out at {rpc}"))?
    }))
    .await;

    results.into_iter().collect()
}

fn agreed_block(blocks: &[RpcBlock]) -> Option<AgreedBlock> {
    let first = *blocks.first()?;
    blocks
        .iter()
        .all(|block| *block == first)
        .then(|| first.into())
}

async fn bisect_last_agreed<F, Fut>(
    upper: u64,
    mut sample: F,
) -> eyre::Result<(Option<AgreedBlock>, u64)>
where
    F: FnMut(u64) -> Fut,
    Fut: Future<Output = eyre::Result<Option<AgreedBlock>>>,
{
    // Zone canonical history is append-only, so nodes cannot re-converge at a
    // later height after disagreeing without first adopting the same chain.
    let mut low = 0;
    let mut high = upper;
    let mut last_agreed = None;
    let mut first_divergent = upper;

    while low <= high {
        let middle = low + (high - low) / 2;
        if let Some(block) = sample(middle).await? {
            last_agreed = Some(block);
            match middle.checked_add(1) {
                Some(next) => low = next,
                None => break,
            }
        } else {
            first_divergent = middle;
            match middle.checked_sub(1) {
                Some(previous) => high = previous,
                None => break,
            }
        }
    }

    Ok((last_agreed, first_divergent))
}

fn print_blocks(operator_rpcs: &[String], blocks: &[RpcBlock]) {
    for (rpc, block) in operator_rpcs.iter().zip(blocks) {
        println!(
            "  {rpc}: height={} hash={} stateRoot={}",
            block.number, block.hash, block.state_root
        );
    }
}

fn hex_height(height: u64) -> String {
    format!("{height:#x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(height: u64) -> AgreedBlock {
        AgreedBlock {
            height,
            hash: B256::with_last_byte(height as u8),
            state_root: B256::with_last_byte(height as u8),
        }
    }

    #[tokio::test]
    async fn bisects_to_last_agreed_block() {
        let (last_agreed, first_divergent) = bisect_last_agreed(12, |height| async move {
            Ok((height < 9).then(|| block(height)))
        })
        .await
        .unwrap();

        assert_eq!(last_agreed, Some(block(8)));
        assert_eq!(first_divergent, 9);
    }

    #[tokio::test]
    async fn reports_divergence_at_genesis() {
        let (last_agreed, first_divergent) = bisect_last_agreed(12, |_| async { Ok(None) })
            .await
            .unwrap();

        assert_eq!(last_agreed, None);
        assert_eq!(first_divergent, 0);
    }

    #[test]
    fn complete_block_agreement_includes_state_root() {
        let first = RpcBlock {
            number: U64::from(8),
            hash: B256::with_last_byte(1),
            state_root: B256::with_last_byte(2),
        };
        let second = RpcBlock {
            state_root: B256::with_last_byte(3),
            ..first
        };

        assert!(agreed_block(&[first, second]).is_none());
    }
}
