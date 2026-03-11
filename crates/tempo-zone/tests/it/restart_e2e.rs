//! E2E tests for zone sequencer restart resilience.
//!
//! These tests verify that the zone monitor and withdrawal processor correctly
//! resume from on-chain portal state after a restart, covering:
//!
//! - Batch submission resumes from the portal's `blockHash()` (not block 0)
//! - Withdrawals continue to be processed after a sequencer restart
//! - Multiple restart cycles don't corrupt state

use crate::utils::{L1TestNode, ZoneAccount, ZoneTestNode, spawn_sequencer};
use alloy::primitives::{Address, U256};
use zone::abi::{ZONE_TOKEN_ADDRESS, ZonePortal};

/// Longer timeout for real L1 tests.
const L1_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Timeout for waiting on withdrawals (includes batch submission + processing).
const WITHDRAWAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Read the portal's `blockHash()` — the last submitted zone block hash.
async fn portal_block_hash(
    l1: &L1TestNode,
    portal_address: Address,
) -> eyre::Result<alloy_primitives::B256> {
    let portal = ZonePortal::new(portal_address, l1.provider());
    Ok(portal.blockHash().call().await?)
}

/// Read the portal's withdrawal queue head and tail.
async fn portal_queue_state(l1: &L1TestNode, portal_address: Address) -> eyre::Result<(u64, u64)> {
    let portal = ZonePortal::new(portal_address, l1.provider());
    let head: U256 = portal.withdrawalQueueHead().call().await?;
    let tail: U256 = portal.withdrawalQueueTail().call().await?;
    Ok((head.to::<u64>(), tail.to::<u64>()))
}

/// Count `BatchSubmitted` events on the portal.
async fn batch_submitted_count(l1: &L1TestNode, portal_address: Address) -> eyre::Result<usize> {
    let portal = ZonePortal::new(portal_address, l1.provider());
    let events = portal.BatchSubmitted_filter().from_block(0).query().await?;
    Ok(events.len())
}

/// Sequencer restart after a successful batch + withdrawal cycle.
///
/// 1. Start L1 + zone, deposit, spawn sequencer, withdraw, wait for L1 processing.
/// 2. Abort the sequencer (simulating a restart).
/// 3. Respawn the sequencer — it should resume from the portal's `blockHash()`.
/// 4. Perform another withdrawal and verify it completes on L1.
///
/// This proves the monitor correctly reads `blockHash()` on startup and doesn't
/// attempt to scan from block 0 (which would hit the max block range error).
#[tokio::test(flavor = "multi_thread")]
async fn test_sequencer_restart_resumes_batch_submission() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let l1 = L1TestNode::start().await?;
    let portal_address = l1.deploy_zone().await?;
    let zone = ZoneTestNode::start_from_l1(l1.http_url(), l1.ws_url(), portal_address).await?;
    zone.wait_for_l2_tempo_finalized(0, L1_TIMEOUT).await?;

    // --- Phase 1: Deposit + first withdrawal ---
    let mut account = ZoneAccount::from_l1_and_zone(&l1, &zone, portal_address);
    let deposit_amount: u128 = 2_000_000; // 2 pathUSD
    l1.fund_user(account.address(), deposit_amount).await?;
    account.deposit(deposit_amount, L1_TIMEOUT, &zone).await?;

    let seq_handle = spawn_sequencer(&l1, &zone, portal_address, l1.dev_signer()).await;

    let first_withdrawal: u128 = 500_000;
    account.withdraw(first_withdrawal).await?;
    l1.wait_for_withdrawal_on_l1(
        portal_address,
        account.address(),
        first_withdrawal,
        WITHDRAWAL_TIMEOUT,
    )
    .await?;

    // Verify portal state advanced
    let hash_after_first = portal_block_hash(&l1, portal_address).await?;
    assert!(
        !hash_after_first.is_zero(),
        "portal blockHash should be non-zero after first batch"
    );
    let batches_before_restart = batch_submitted_count(&l1, portal_address).await?;
    assert!(
        batches_before_restart > 0,
        "should have at least one batch submitted"
    );

    // --- Phase 2: Restart sequencer ---
    seq_handle.monitor_handle.abort();
    seq_handle.withdrawal_handle.abort();
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Respawn — should resume from portal's blockHash, not block 0
    let _seq_handle2 = spawn_sequencer(&l1, &zone, portal_address, l1.dev_signer()).await;

    // --- Phase 3: Second withdrawal after restart ---
    let second_withdrawal: u128 = 300_000;
    account.withdraw(second_withdrawal).await?;
    l1.wait_for_withdrawal_on_l1(
        portal_address,
        account.address(),
        second_withdrawal,
        WITHDRAWAL_TIMEOUT,
    )
    .await?;

    // More batches should have been submitted
    let batches_after_restart = batch_submitted_count(&l1, portal_address).await?;
    assert!(
        batches_after_restart > batches_before_restart,
        "new batches should be submitted after restart (before={batches_before_restart}, after={batches_after_restart})"
    );

    // Portal block hash should have advanced
    let hash_after_second = portal_block_hash(&l1, portal_address).await?;
    assert_ne!(
        hash_after_first, hash_after_second,
        "portal blockHash should advance after second batch"
    );

    Ok(())
}

/// Sequencer restart with unprocessed withdrawal queue slots.
///
/// 1. Deposit, spawn sequencer, request withdrawal.
/// 2. Wait for the batch to be submitted (withdrawal enqueued on L1 portal).
/// 3. Abort the sequencer BEFORE the withdrawal is processed.
/// 4. Respawn the sequencer — `fetch_pending_withdrawals` restores the data.
/// 5. The OLD withdrawal from before the restart is processed on L1.
/// 6. A NEW withdrawal after restart also works.
#[tokio::test(flavor = "multi_thread")]
async fn test_sequencer_restart_with_pending_withdrawal_queue() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let l1 = L1TestNode::start().await?;
    let portal_address = l1.deploy_zone().await?;
    let zone = ZoneTestNode::start_from_l1(l1.http_url(), l1.ws_url(), portal_address).await?;
    zone.wait_for_l2_tempo_finalized(0, L1_TIMEOUT).await?;

    let mut account = ZoneAccount::from_l1_and_zone(&l1, &zone, portal_address);
    let deposit_amount: u128 = 3_000_000;
    l1.fund_user(account.address(), deposit_amount).await?;
    account.deposit(deposit_amount, L1_TIMEOUT, &zone).await?;

    let seq_handle = spawn_sequencer(&l1, &zone, portal_address, l1.dev_signer()).await;

    // Request withdrawal — wait for the batch to be submitted to L1
    let withdrawal_amount: u128 = 500_000;
    account.withdraw(withdrawal_amount).await?;

    // Wait for the batch to land on L1 (portal tail advances)
    crate::utils::poll_until(
        WITHDRAWAL_TIMEOUT,
        std::time::Duration::from_millis(200),
        "portal withdrawal queue tail > 0",
        || {
            let l1 = &l1;
            async move {
                let (_, tail) = portal_queue_state(l1, portal_address).await?;
                if tail > 0 { Ok(Some(tail)) } else { Ok(None) }
            }
        },
    )
    .await?;

    let (head_before, tail_before) = portal_queue_state(&l1, portal_address).await?;

    // --- Abort sequencer BEFORE the withdrawal is processed ---
    seq_handle.monitor_handle.abort();
    seq_handle.withdrawal_handle.abort();
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // --- Respawn sequencer (fetch_pending_withdrawals runs during init) ---
    let _seq_handle2 = spawn_sequencer(&l1, &zone, portal_address, l1.dev_signer()).await;

    // The OLD withdrawal from before the restart should be processed via
    // restored data (or was already processed before abort — either way this succeeds).
    l1.wait_for_withdrawal_on_l1(
        portal_address,
        account.address(),
        withdrawal_amount,
        WITHDRAWAL_TIMEOUT,
    )
    .await?;

    // Request a NEW withdrawal after restart to verify normal operation continues.
    let second_withdrawal: u128 = 400_000;
    account.withdraw(second_withdrawal).await?;
    l1.wait_for_withdrawal_on_l1(
        portal_address,
        account.address(),
        second_withdrawal,
        WITHDRAWAL_TIMEOUT,
    )
    .await?;

    // Portal queue should have advanced
    let (head_after, tail_after) = portal_queue_state(&l1, portal_address).await?;
    assert!(
        tail_after > tail_before,
        "portal tail should advance after second withdrawal (before={tail_before}, after={tail_after})"
    );
    assert!(
        head_after > head_before,
        "portal head should advance as withdrawals are processed (before={head_before}, after={head_after})"
    );

    Ok(())
}

/// Double restart: verify the sequencer survives two consecutive restart cycles.
///
/// Each cycle: deposit → withdraw → restart. After the final restart, a new
/// withdrawal should still complete successfully.
#[tokio::test(flavor = "multi_thread")]
async fn test_double_sequencer_restart() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let l1 = L1TestNode::start().await?;
    let portal_address = l1.deploy_zone().await?;
    let zone = ZoneTestNode::start_from_l1(l1.http_url(), l1.ws_url(), portal_address).await?;
    zone.wait_for_l2_tempo_finalized(0, L1_TIMEOUT).await?;

    let mut account = ZoneAccount::from_l1_and_zone(&l1, &zone, portal_address);
    let deposit_amount: u128 = 5_000_000;
    l1.fund_user(account.address(), deposit_amount).await?;
    account.deposit(deposit_amount, L1_TIMEOUT, &zone).await?;

    // --- Cycle 1 ---
    let seq1 = spawn_sequencer(&l1, &zone, portal_address, l1.dev_signer()).await;

    account.withdraw(200_000).await?;
    l1.wait_for_withdrawal_on_l1(
        portal_address,
        account.address(),
        200_000,
        WITHDRAWAL_TIMEOUT,
    )
    .await?;

    seq1.monitor_handle.abort();
    seq1.withdrawal_handle.abort();
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // --- Cycle 2 ---
    let seq2 = spawn_sequencer(&l1, &zone, portal_address, l1.dev_signer()).await;

    account.withdraw(300_000).await?;
    l1.wait_for_withdrawal_on_l1(
        portal_address,
        account.address(),
        300_000,
        WITHDRAWAL_TIMEOUT,
    )
    .await?;

    seq2.monitor_handle.abort();
    seq2.withdrawal_handle.abort();
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // --- Cycle 3 (final) ---
    let _seq3 = spawn_sequencer(&l1, &zone, portal_address, l1.dev_signer()).await;

    account.withdraw(400_000).await?;
    l1.wait_for_withdrawal_on_l1(
        portal_address,
        account.address(),
        400_000,
        WITHDRAWAL_TIMEOUT,
    )
    .await?;

    // Verify all batches are on L1
    let total_batches = batch_submitted_count(&l1, portal_address).await?;
    assert!(
        total_batches >= 3,
        "should have at least 3 batches across 3 restart cycles (got {total_batches})"
    );

    // Verify L2 balance decreased by total withdrawals (200k + 300k + 400k = 900k)
    let l2_balance = zone
        .balance_of(ZONE_TOKEN_ADDRESS, account.address())
        .await?;
    assert!(
        l2_balance <= U256::from(deposit_amount - 900_000u128),
        "L2 balance should reflect all withdrawals (got {l2_balance})"
    );

    Ok(())
}

/// Batch-only restart (no withdrawals).
///
/// Verifies the monitor resumes batch submission after restart even when no
/// withdrawals are involved — the portal's `blockHash()` is the critical
/// anchor, not the withdrawal queue.
#[tokio::test(flavor = "multi_thread")]
async fn test_batch_only_restart_no_withdrawals() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let l1 = L1TestNode::start().await?;
    let portal_address = l1.deploy_zone().await?;
    let zone = ZoneTestNode::start_from_l1(l1.http_url(), l1.ws_url(), portal_address).await?;
    zone.wait_for_l2_tempo_finalized(0, L1_TIMEOUT).await?;

    // Deposit to generate L2 activity but don't withdraw
    let mut account = ZoneAccount::from_l1_and_zone(&l1, &zone, portal_address);
    l1.fund_user(account.address(), 1_000_000).await?;
    account.deposit(1_000_000, L1_TIMEOUT, &zone).await?;

    // Spawn sequencer, wait for at least one batch
    let seq1 = spawn_sequencer(&l1, &zone, portal_address, l1.dev_signer()).await;

    crate::utils::poll_until(
        WITHDRAWAL_TIMEOUT,
        std::time::Duration::from_millis(200),
        "at least one BatchSubmitted",
        || {
            let l1 = &l1;
            async move {
                let count = batch_submitted_count(l1, portal_address).await?;
                if count > 0 { Ok(Some(count)) } else { Ok(None) }
            }
        },
    )
    .await?;

    let hash_before = portal_block_hash(&l1, portal_address).await?;
    let batches_before = batch_submitted_count(&l1, portal_address).await?;

    // Restart
    seq1.monitor_handle.abort();
    seq1.withdrawal_handle.abort();
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    let _seq2 = spawn_sequencer(&l1, &zone, portal_address, l1.dev_signer()).await;

    // Wait for more zone blocks to be produced and batched
    // The zone produces blocks as L1 blocks arrive (every 500ms in dev mode),
    // so just waiting a bit should produce new blocks to batch.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let batches_after = batch_submitted_count(&l1, portal_address).await?;
    assert!(
        batches_after > batches_before,
        "new batches should be submitted after restart (before={batches_before}, after={batches_after})"
    );

    // Portal block hash should have advanced
    let hash_after = portal_block_hash(&l1, portal_address).await?;
    assert_ne!(
        hash_before, hash_after,
        "portal blockHash should advance after restart"
    );

    Ok(())
}
