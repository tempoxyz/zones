//! Extended-gap batch submission E2E test.
//!
//! When a zone node goes down for long enough that even the first stepping
//! boundary is outside the EIP-2935 history window, the sequencer must still
//! submit batches successfully once it comes back. This exercises the
//! long-downtime ancestry path instead of the simpler direct-mode case.

use crate::utils::{L1TestNode, ZoneTestNode, poll_until, spawn_sequencer};
use alloy::providers::Provider;
use std::time::Duration;
use zone::abi::ZonePortal;

const EIP2935_HISTORY_WINDOW: u64 = 8192;
const EIP2935_SAFETY_MARGIN: u64 = 360;
const EIP2935_EFFECTIVE_WINDOW: u64 = EIP2935_HISTORY_WINDOW - EIP2935_SAFETY_MARGIN;
const EXTENDED_GAP_BLOCKS: u64 = EIP2935_HISTORY_WINDOW + EIP2935_EFFECTIVE_WINDOW + 64;

/// Extended timeout for stepping tests — the L1 needs to mine >16k blocks and
/// the zone must replay enough history to cross the first stepping boundary.
const STEPPING_TIMEOUT: Duration = Duration::from_secs(300);
const BATCH_TIMEOUT: Duration = Duration::from_secs(90);

/// Test that batch submission works after the zone's `tempoBlockNumber` has
/// fallen far enough behind L1 that the first stepped sub-batch still lands
/// outside the EIP-2935 history window.
///
/// 1. Start L1 with 10ms block time to mine blocks quickly.
/// 2. Deploy zone portal on L1.
/// 3. Wait for L1 to advance past `history + effective window`, so the first
///    stepping boundary is still outside the history window.
/// 4. Start zone node connected to L1, anchored at the portal genesis.
/// 5. Wait for the zone to replay up to the first stepping boundary.
/// 6. Spawn sequencer while the zone is still far behind L1.
/// 7. Assert a `BatchSubmitted` event appears.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow: mines >16k L1 blocks and replays zone history, run with --ignored or in nightly CI"]
async fn test_batch_submission_after_extended_l1_gap() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    // --- Step 1: Start L1 with fast block time ---
    let l1 = L1TestNode::start_with(|cfg| {
        cfg.dev.block_time = Some(Duration::from_millis(10));
    })
    .await?;

    // --- Step 2: Deploy zone portal ---
    let portal_address = l1.deploy_zone().await?;

    let portal = ZonePortal::new(portal_address, l1.provider());
    let genesis_block = portal.genesisTempoBlockNumber().call().await?;
    let target_block = genesis_block + EXTENDED_GAP_BLOCKS;

    tracing::info!(
        genesis_block,
        target_block,
        "Waiting for L1 to advance past the extended ancestry threshold"
    );

    // Poll until L1 reaches the target — at 10ms/block this takes ~160 seconds.
    let poll_start = std::time::Instant::now();
    loop {
        let current = l1.provider().get_block_number().await?;
        if current >= target_block {
            tracing::info!(
                current,
                target_block,
                elapsed_secs = poll_start.elapsed().as_secs(),
                "L1 advanced past EIP-2935 window"
            );
            break;
        }
        if poll_start.elapsed() > STEPPING_TIMEOUT {
            return Err(eyre::eyre!(
                "Timed out waiting for L1 to reach block {target_block} (current: {current})"
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // --- Step 4: Start zone node connected to L1, anchored at the portal genesis ---
    let zone =
        ZoneTestNode::start_from_l1_portal_genesis(l1.http_url(), l1.ws_url(), portal_address)
            .await?;

    // --- Step 5: Wait for the zone to replay to the first stepping boundary ---
    let first_step_tempo = genesis_block + EIP2935_EFFECTIVE_WINDOW;
    zone.wait_for_tempo_block_number(first_step_tempo, STEPPING_TIMEOUT)
        .await?;

    let l1_tip = l1.provider().get_block_number().await?;
    eyre::ensure!(
        l1_tip.saturating_sub(first_step_tempo) > EIP2935_HISTORY_WINDOW,
        "test precondition not met: first step tempo {first_step_tempo} is only {} blocks behind L1 tip {l1_tip}",
        l1_tip.saturating_sub(first_step_tempo),
    );

    // --- Step 6: Spawn sequencer while the zone still has a large backlog ---
    let seq = spawn_sequencer(&l1, &zone, portal_address, l1.dev_signer()).await;

    // --- Step 7: Assert batch submission succeeds after the long gap ---
    let batch_count = poll_until(
        BATCH_TIMEOUT,
        Duration::from_millis(500),
        "BatchSubmitted event after extended L1 gap",
        || {
            let portal = &portal;
            let seq = &seq;
            async move {
                if seq.monitor_handle.is_finished() {
                    eyre::bail!("monitor task exited before submitting a batch");
                }

                if seq.withdrawal_handle.is_finished() {
                    eyre::bail!("withdrawal processor exited before batch submission completed");
                }

                let events = portal.BatchSubmitted_filter().from_block(0).query().await?;
                let batch_count = events.len();
                if batch_count >= 1 {
                    Ok(Some(batch_count))
                } else {
                    Ok(None)
                }
            }
        },
    )
    .await?;

    tracing::info!(
        batch_count,
        l1_tip,
        first_step_tempo,
        first_step_gap = l1_tip.saturating_sub(first_step_tempo),
        "Batch submission succeeded after extended L1 gap"
    );

    Ok(())
}
