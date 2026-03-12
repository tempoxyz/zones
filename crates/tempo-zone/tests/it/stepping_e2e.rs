//! Stepping mode E2E test: verifies batch submission after extended L1 gap.
//!
//! When a zone node goes down for longer than the EIP-2935 history window (8192 blocks),
//! the batch submitter must split the batch into multiple direct-mode submissions.
//! This test validates that the stepping logic correctly handles this scenario.

use crate::utils::{L1TestNode, ZoneAccount, ZoneTestNode, spawn_sequencer};
use alloy::providers::Provider;
use std::time::Duration;
use zone::abi::{ZONE_TOKEN_ADDRESS, ZonePortal};

/// Extended timeout for stepping tests — the L1 needs to mine >8200 blocks
/// and the zone must process them all.
const STEPPING_TIMEOUT: Duration = Duration::from_secs(180);

/// Timeout for L1 operations.
const L1_TIMEOUT: Duration = Duration::from_secs(30);

/// Test that batch submission works after the zone's `tempoBlockNumber` has
/// fallen outside the EIP-2935 history window (gap > 8192 blocks).
///
/// The stepping logic must split the batch into multiple direct-mode
/// submissions, each within the EIP-2935 effective window.
///
/// 1. Start L1 with 10ms block time to mine blocks quickly.
/// 2. Deploy zone portal on L1.
/// 3. Wait for L1 to advance >8200 blocks past genesis.
/// 4. Start zone node connected to L1.
/// 5. Wait for zone to process all L1 blocks.
/// 6. Fund user and deposit to create non-trivial state.
/// 7. Spawn sequencer (monitor + withdrawal processor).
/// 8. Assert multiple BatchSubmitted events (stepping produces >=2 submissions).
/// 9. Verify withdrawal works through stepped batches.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow: mines >8200 L1 blocks (~82s), run with --ignored or in nightly CI"]
async fn test_batch_submission_after_extended_l1_gap() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    // --- Step 1: Start L1 with fast block time ---
    let l1 = L1TestNode::start_with(|cfg| {
        cfg.dev.block_time = Some(Duration::from_millis(10));
    })
    .await?;

    // --- Step 2: Deploy zone portal ---
    let portal_address = l1.deploy_zone().await?;

    // --- Step 3: Wait for L1 to advance past the EIP-2935 window ---
    // The portal's genesisTempoBlockNumber is set to the current L1 block at
    // deployment time. We need the L1 to advance >8200 blocks beyond that.
    let genesis_block = l1.provider().get_block_number().await?;
    let target_block = genesis_block + 8200;

    tracing::info!(
        genesis_block,
        target_block,
        "Waiting for L1 to advance past EIP-2935 window"
    );

    // Poll until L1 reaches the target — at 10ms/block this takes ~82 seconds.
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
        if poll_start.elapsed() > Duration::from_secs(120) {
            return Err(eyre::eyre!(
                "Timed out waiting for L1 to reach block {target_block} (current: {current})"
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // --- Step 4: Start zone node connected to L1 ---
    let zone = ZoneTestNode::start_from_l1(l1.http_url(), l1.ws_url(), portal_address).await?;

    // --- Step 5: Wait for zone to catch up ---
    zone.wait_for_l2_tempo_finalized(0, STEPPING_TIMEOUT)
        .await?;

    // --- Step 6: Fund user and deposit ---
    let mut account = ZoneAccount::from_l1_and_zone(&l1, &zone, portal_address);
    l1.fund_user(account.address(), 2_000_000).await?;
    account.deposit(1_000_000, L1_TIMEOUT, &zone).await?;

    // --- Step 7: Spawn sequencer ---
    let _seq = spawn_sequencer(&l1, &zone, portal_address, l1.dev_signer()).await;

    // --- Step 8: Assert multiple BatchSubmitted events ---
    // The gap exceeds the EIP-2935 window, so stepping must produce >=2 submitBatch calls.
    let batch_timeout = Duration::from_secs(60);
    let batch_start = std::time::Instant::now();
    let portal = ZonePortal::new(portal_address, l1.provider());

    loop {
        let events = portal.BatchSubmitted_filter().from_block(0).query().await?;

        let batch_count = events.len();
        if batch_count >= 2 {
            tracing::info!(
                batch_count,
                elapsed_secs = batch_start.elapsed().as_secs(),
                "Stepping produced multiple batch submissions"
            );
            break;
        }

        if batch_start.elapsed() > batch_timeout {
            return Err(eyre::eyre!(
                "Expected >= 2 BatchSubmitted events from stepping, got {batch_count}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // --- Step 9: Verify withdrawal works through stepped batches ---
    let withdrawal_amount: u128 = 500_000;
    account.withdraw(withdrawal_amount).await?;

    l1.wait_for_withdrawal_on_l1(
        portal_address,
        account.address(),
        withdrawal_amount,
        STEPPING_TIMEOUT,
    )
    .await?;

    // Verify L2 balance decreased
    let l2_balance = zone
        .balance_of(ZONE_TOKEN_ADDRESS, account.address())
        .await?;
    assert!(
        l2_balance <= alloy::primitives::U256::from(1_000_000u128 - withdrawal_amount),
        "L2 balance should decrease by at least the withdrawal amount (got {l2_balance})"
    );

    Ok(())
}
