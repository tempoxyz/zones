//! Extended-gap batch submission E2E test.
//!
//! When a zone node goes down for long enough that even the first stepping
//! boundary is outside the EIP-2935 history window, the sequencer must still
//! submit batches successfully once it comes back. This exercises the
//! long-downtime ancestry path instead of the simpler direct-mode case.

use crate::utils::{
    L1TestNode, ZoneTestNode, poll_until, spawn_sequencer, spawn_sequencer_with_anchor_config,
};
use alloy::providers::Provider;
use alloy_sol_types::SolCall;
use std::time::Duration;
use zone::{BatchAnchorConfig, abi::ZonePortal};

const EIP2935_HISTORY_WINDOW: u64 = 8192;
const EIP2935_SAFETY_MARGIN: u64 = 360;
const EIP2935_EFFECTIVE_WINDOW: u64 = EIP2935_HISTORY_WINDOW - EIP2935_SAFETY_MARGIN;
const EXTENDED_GAP_BLOCKS: u64 = EIP2935_HISTORY_WINDOW + EIP2935_EFFECTIVE_WINDOW + 64;

const SHORT_EIP2935_HISTORY_WINDOW: u64 = 10;
const SHORT_EIP2935_SAFETY_MARGIN: u64 = 4;
const SHORT_EIP2935_EFFECTIVE_WINDOW: u64 =
    SHORT_EIP2935_HISTORY_WINDOW - SHORT_EIP2935_SAFETY_MARGIN;
const SHORT_EXTENDED_GAP_BLOCKS: u64 =
    SHORT_EIP2935_HISTORY_WINDOW + SHORT_EIP2935_EFFECTIVE_WINDOW + 1;
const SHORT_MULTI_STEP_BATCH_COUNT: u64 = 3;
const SHORT_MULTI_STEP_GAP_BLOCKS: u64 = SHORT_EIP2935_HISTORY_WINDOW
    + SHORT_EIP2935_EFFECTIVE_WINDOW * SHORT_MULTI_STEP_BATCH_COUNT
    + 1;

/// Extended timeout for stepping tests — the L1 needs to mine >16k blocks and
/// the zone must replay enough history to cross the first stepping boundary.
const STEPPING_TIMEOUT: Duration = Duration::from_secs(300);
const BATCH_TIMEOUT: Duration = Duration::from_secs(90);
const SHORT_STEPPING_TIMEOUT: Duration = Duration::from_secs(60);

async fn fetch_submit_batch_call(
    l1: &L1TestNode,
    tx_hash: alloy_primitives::B256,
) -> eyre::Result<(ZonePortal::submitBatchCall, u64)> {
    let response: serde_json::Value = reqwest::Client::new()
        .post(l1.http_url().clone())
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "eth_getTransactionByHash",
            "params": [format!("{tx_hash:#x}")],
        }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    if let Some(error) = response.get("error") {
        eyre::bail!("eth_getTransactionByHash failed for {tx_hash}: {error}");
    }

    let tx = response
        .get("result")
        .filter(|value| !value.is_null())
        .ok_or_else(|| eyre::eyre!("submitBatch tx {tx_hash} not found"))?;

    let input = tx
        .get("input")
        .and_then(|value| value.as_str())
        .filter(|input| *input != "0x")
        .or_else(|| {
            tx.get("calls")
                .and_then(|value| value.as_array())
                .and_then(|calls| {
                    calls
                        .iter()
                        .filter_map(|call| call.get("input").and_then(|value| value.as_str()))
                        .find(|input| *input != "0x")
                })
        })
        .ok_or_else(|| eyre::eyre!("submitBatch tx {tx_hash} has no calldata input"))?;

    let calldata = const_hex::decode(input.strip_prefix("0x").unwrap_or(input)).map_err(|err| {
        eyre::eyre!("failed to hex-decode submitBatch calldata for {tx_hash}: {err}")
    })?;
    let call = ZonePortal::submitBatchCall::abi_decode(&calldata)
        .map_err(|err| eyre::eyre!("failed to decode submitBatch calldata: {err}"))?;

    let block_number = tx
        .get("blockNumber")
        .and_then(|value| value.as_str())
        .ok_or_else(|| eyre::eyre!("submitBatch tx {tx_hash} is missing blockNumber"))?;
    let block_number =
        u64::from_str_radix(block_number.strip_prefix("0x").unwrap_or(block_number), 16)?;

    Ok((call, block_number))
}

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

/// Same stepping scenario as the ignored long-gap test, but with a 10-block
/// configured EIP-2935 window so it runs in regular integration test time.
#[tokio::test(flavor = "multi_thread")]
async fn test_batch_submission_after_configured_short_l1_gap() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let anchor_config =
        BatchAnchorConfig::new(SHORT_EIP2935_HISTORY_WINDOW, SHORT_EIP2935_SAFETY_MARGIN)?;

    let l1 = L1TestNode::start_with(|cfg| {
        cfg.dev.block_time = Some(Duration::from_millis(20));
    })
    .await?;

    let portal_address = l1.deploy_zone().await?;
    let portal = ZonePortal::new(portal_address, l1.provider());
    let genesis_block = portal.genesisTempoBlockNumber().call().await?;
    let target_block = genesis_block + SHORT_EXTENDED_GAP_BLOCKS;

    poll_until(
        SHORT_STEPPING_TIMEOUT,
        Duration::from_millis(50),
        "L1 advanced past configured short EIP-2935 window",
        || {
            let provider = l1.provider();
            async move {
                let current = provider.get_block_number().await?;
                if current >= target_block {
                    Ok(Some(current))
                } else {
                    Ok(None)
                }
            }
        },
    )
    .await?;

    let zone =
        ZoneTestNode::start_from_l1_portal_genesis(l1.http_url(), l1.ws_url(), portal_address)
            .await?;

    let first_step_tempo = genesis_block + SHORT_EIP2935_EFFECTIVE_WINDOW;
    zone.wait_for_tempo_block_number(first_step_tempo, SHORT_STEPPING_TIMEOUT)
        .await?;

    let l1_tip = l1.provider().get_block_number().await?;
    eyre::ensure!(
        l1_tip.saturating_sub(first_step_tempo) > SHORT_EIP2935_HISTORY_WINDOW,
        "test precondition not met: first configured step tempo {first_step_tempo} is only {} blocks behind L1 tip {l1_tip}",
        l1_tip.saturating_sub(first_step_tempo),
    );

    let seq = spawn_sequencer_with_anchor_config(
        &l1,
        &zone,
        portal_address,
        l1.dev_signer(),
        anchor_config,
    )
    .await;

    poll_until(
        SHORT_STEPPING_TIMEOUT,
        Duration::from_millis(250),
        "BatchSubmitted event after configured short L1 gap",
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
                if events.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(events.len()))
                }
            }
        },
    )
    .await?;

    Ok(())
}

/// Verifies that a larger configured-window gap is split into multiple
/// `submitBatch` L1 transactions before the sequencer catches up.
#[tokio::test(flavor = "multi_thread")]
async fn test_configured_short_l1_gap_requires_multiple_stepping_batches() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let anchor_config =
        BatchAnchorConfig::new(SHORT_EIP2935_HISTORY_WINDOW, SHORT_EIP2935_SAFETY_MARGIN)?;

    // Start L1 with fast blocks so the configured test window ages out quickly.
    let l1 = L1TestNode::start_with(|cfg| {
        cfg.dev.block_time = Some(Duration::from_millis(20));
    })
    .await?;

    let portal_address = l1.deploy_zone().await?;
    let portal = ZonePortal::new(portal_address, l1.provider());
    let genesis_block = portal.genesisTempoBlockNumber().call().await?;
    let target_block = genesis_block + SHORT_MULTI_STEP_GAP_BLOCKS;

    // Mine enough L1 blocks that catching up requires several configured steps.
    poll_until(
        SHORT_STEPPING_TIMEOUT,
        Duration::from_millis(50),
        "L1 advanced far enough to require multiple configured stepping batches",
        || {
            let provider = l1.provider();
            async move {
                let current = provider.get_block_number().await?;
                if current >= target_block {
                    Ok(Some(current))
                } else {
                    Ok(None)
                }
            }
        },
    )
    .await?;

    let zone =
        ZoneTestNode::start_from_l1_portal_genesis(l1.http_url(), l1.ws_url(), portal_address)
            .await?;

    // Let the zone replay far enough to have multiple valid split boundaries.
    let multi_step_tempo =
        genesis_block + SHORT_EIP2935_EFFECTIVE_WINDOW * SHORT_MULTI_STEP_BATCH_COUNT;
    zone.wait_for_tempo_block_number(multi_step_tempo, SHORT_STEPPING_TIMEOUT)
        .await?;

    // Assert the first step is genuinely outside the configured history window.
    let first_step_tempo = genesis_block + SHORT_EIP2935_EFFECTIVE_WINDOW;
    let l1_tip = l1.provider().get_block_number().await?;
    eyre::ensure!(
        l1_tip.saturating_sub(first_step_tempo) > SHORT_EIP2935_HISTORY_WINDOW,
        "test precondition not met: first configured step tempo {first_step_tempo} is only {} blocks behind L1 tip {l1_tip}",
        l1_tip.saturating_sub(first_step_tempo),
    );
    eyre::ensure!(
        multi_step_tempo < l1_tip.saturating_sub(SHORT_EIP2935_SAFETY_MARGIN),
        "test precondition not met: multi-step tempo {multi_step_tempo} is not below the safe L1 anchor tip {l1_tip}",
    );

    // Start the sequencer only after the backlog has accumulated.
    let seq = spawn_sequencer_with_anchor_config(
        &l1,
        &zone,
        portal_address,
        l1.dev_signer(),
        anchor_config,
    )
    .await;

    // Wait for the stepping loop to submit the first configured catch-up batches.
    let calls = poll_until(
        SHORT_STEPPING_TIMEOUT,
        Duration::from_millis(250),
        "multiple stepping BatchSubmitted events",
        || {
            let portal = &portal;
            let seq = &seq;
            let l1 = &l1;
            async move {
                if seq.monitor_handle.is_finished() {
                    eyre::bail!("monitor task exited before submitting stepping batches");
                }

                if seq.withdrawal_handle.is_finished() {
                    eyre::bail!("withdrawal processor exited before stepping batches completed");
                }

                let events = portal.BatchSubmitted_filter().from_block(0).query().await?;
                if events.len() < SHORT_MULTI_STEP_BATCH_COUNT as usize {
                    return Ok(None);
                }

                let mut calls = Vec::with_capacity(SHORT_MULTI_STEP_BATCH_COUNT as usize);
                for (_, log) in events.iter().take(SHORT_MULTI_STEP_BATCH_COUNT as usize) {
                    let tx_hash = log.transaction_hash.ok_or_else(|| {
                        eyre::eyre!("BatchSubmitted log missing transaction hash")
                    })?;
                    let (call, _) = fetch_submit_batch_call(l1, tx_hash).await?;
                    calls.push(call);
                }

                Ok(Some(calls))
            }
        },
    )
    .await?;

    let tempo_block_numbers = calls
        .iter()
        .map(|call| call.tempoBlockNumber)
        .collect::<Vec<_>>();
    // Each stepping submission should move the portal anchor forward.
    eyre::ensure!(
        tempo_block_numbers
            .windows(2)
            .all(|window| window[0] < window[1]),
        "stepping submissions should advance tempoBlockNumber monotonically: {tempo_block_numbers:?}"
    );
    // Since these steps are still out of the short direct window, they use ancestry mode.
    eyre::ensure!(
        calls
            .iter()
            .all(|call| call.recentTempoBlockNumber > call.tempoBlockNumber),
        "stepping catch-up submissions should use ancestry anchors: {tempo_block_numbers:?}"
    );
    // Proof bytes stay empty until real proof generation is wired in.
    eyre::ensure!(
        calls.iter().all(|call| call.proof.is_empty()),
        "stepping catch-up submissions should keep proof bytes empty for now"
    );

    Ok(())
}

/// Verifies that the fast configured-window stepping path submits ancestry-mode
/// calldata, not a direct `tempoBlockNumber` lookup, while proof bytes remain
/// empty until real proof generation is implemented.
#[tokio::test(flavor = "multi_thread")]
async fn test_stepping_ancestry_submission_uses_recent_anchor() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let anchor_config =
        BatchAnchorConfig::new(SHORT_EIP2935_HISTORY_WINDOW, SHORT_EIP2935_SAFETY_MARGIN)?;

    let l1 = L1TestNode::start_with(|cfg| {
        cfg.dev.block_time = Some(Duration::from_millis(20));
    })
    .await?;

    let portal_address = l1.deploy_zone().await?;
    let portal = ZonePortal::new(portal_address, l1.provider());
    let genesis_block = portal.genesisTempoBlockNumber().call().await?;
    let target_block = genesis_block + SHORT_EXTENDED_GAP_BLOCKS;

    poll_until(
        SHORT_STEPPING_TIMEOUT,
        Duration::from_millis(50),
        "L1 advanced past configured short EIP-2935 window",
        || {
            let provider = l1.provider();
            async move {
                let current = provider.get_block_number().await?;
                if current >= target_block {
                    Ok(Some(current))
                } else {
                    Ok(None)
                }
            }
        },
    )
    .await?;

    let zone =
        ZoneTestNode::start_from_l1_portal_genesis(l1.http_url(), l1.ws_url(), portal_address)
            .await?;

    let first_step_tempo = genesis_block + SHORT_EIP2935_EFFECTIVE_WINDOW;
    zone.wait_for_tempo_block_number(first_step_tempo, SHORT_STEPPING_TIMEOUT)
        .await?;

    let l1_tip = l1.provider().get_block_number().await?;
    eyre::ensure!(
        l1_tip.saturating_sub(first_step_tempo) > SHORT_EIP2935_HISTORY_WINDOW,
        "test precondition not met: first configured step tempo {first_step_tempo} is only {} blocks behind L1 tip {l1_tip}",
        l1_tip.saturating_sub(first_step_tempo),
    );

    let seq = spawn_sequencer_with_anchor_config(
        &l1,
        &zone,
        portal_address,
        l1.dev_signer(),
        anchor_config,
    )
    .await;

    let (call, inclusion_block) = poll_until(
        SHORT_STEPPING_TIMEOUT,
        Duration::from_millis(250),
        "ancestry submitBatch calldata using recent anchor",
        || {
            let portal = &portal;
            let seq = &seq;
            let l1 = &l1;
            async move {
                if seq.monitor_handle.is_finished() {
                    eyre::bail!("monitor task exited before submitting a batch");
                }

                if seq.withdrawal_handle.is_finished() {
                    eyre::bail!("withdrawal processor exited before batch submission completed");
                }

                let events = portal.BatchSubmitted_filter().from_block(0).query().await?;
                for (_, log) in events {
                    let tx_hash = log.transaction_hash.ok_or_else(|| {
                        eyre::eyre!("BatchSubmitted log missing transaction hash")
                    })?;
                    let (call, inclusion_block) = fetch_submit_batch_call(l1, tx_hash).await?;

                    if call.recentTempoBlockNumber != 0 {
                        return Ok(Some((call, inclusion_block)));
                    }
                }

                Ok(None)
            }
        },
    )
    .await?;

    eyre::ensure!(
        call.recentTempoBlockNumber > call.tempoBlockNumber,
        "ancestry submission should use a recent anchor greater than tempoBlockNumber"
    );
    eyre::ensure!(
        inclusion_block.saturating_sub(call.tempoBlockNumber) > SHORT_EIP2935_HISTORY_WINDOW,
        "test did not submit an out-of-config-window tempo block: tempo={}, included_at={inclusion_block}",
        call.tempoBlockNumber,
    );
    eyre::ensure!(
        call.proof.is_empty(),
        "ancestry submission should keep proof bytes empty until proof generation is implemented"
    );

    Ok(())
}
