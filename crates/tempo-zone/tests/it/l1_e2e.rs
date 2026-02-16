//! Full L1+L2 end-to-end tests with a real in-process Tempo L1 node.
//!
//! Unlike the injection-based tests in `e2e.rs`, these tests start a real
//! Tempo L1 dev node and a Zone L2 node connected via WebSocket. The L1
//! subscriber naturally receives blocks and deposits — no synthetic injection.

use alloy::{
    primitives::{Address, B256, U256},
    providers::Provider,
};
use zone::abi::{TempoState, TEMPO_STATE_ADDRESS, ZONE_TOKEN_ADDRESS};

use tempo_precompiles::PATH_USD_ADDRESS;

use crate::utils::{L1TestNode, WithdrawalArgs, ZoneAccount, ZoneTestNode};

/// Longer timeout for real L1 tests — the L1 dev node produces blocks every
/// 500ms and the L1Subscriber needs to connect, backfill, and subscribe.
const L1_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Start a real L1 dev node and a zone node connected to it.
/// Verify the zone advances as L1 blocks arrive — proving the full
/// L1Subscriber → DepositQueue → ZoneEngine pipeline works end-to-end.
#[tokio::test(flavor = "multi_thread")]
async fn test_zone_advances_with_real_l1() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    // Start real Tempo L1 in dev mode (500ms block time)
    let l1 = L1TestNode::start().await?;

    // Verify L1 is producing blocks
    let l1_block_0 = l1.provider().get_block_number().await?;
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let l1_block_1 = l1.provider().get_block_number().await?;
    assert!(l1_block_1 > l1_block_0, "L1 should be producing blocks in dev mode");

    // Start zone node connected to real L1 — genesis is patched from the L1's
    // current header so TempoState chain continuity works.
    let zone = ZoneTestNode::start_from_l1(l1.http_url(), l1.ws_url(), Address::ZERO).await?;

    // Wait for the zone to advance past block 0 (genesis anchor)
    let zone_tempo_number = zone.wait_for_l2_tempo_finalized(0, L1_TIMEOUT).await?;
    assert!(zone_tempo_number > 0, "zone should have advanced past genesis anchor");

    // Zone should also have produced L2 blocks
    let zone_block = zone.provider().get_block_number().await?;
    assert!(zone_block > 0, "zone L2 should have blocks");

    // tempoBlockHash should be non-zero (real L1 headers)
    let tempo_state = TempoState::new(TEMPO_STATE_ADDRESS, zone.provider());
    let tempo_hash = tempo_state.tempoBlockHash().call().await?;
    assert_ne!(tempo_hash, B256::ZERO, "tempoBlockHash should be set from real L1 headers");

    Ok(())
}

/// Full deposit + withdrawal flow with a real L1:
/// 1. Start L1 dev node.
/// 2. Deploy ZoneFactory on L1 and create a zone (deploys ZonePortal).
/// 3. Start zone node connected to L1 with the portal address.
/// 4. Deposit pathUSD on the ZonePortal to the dev account.
/// 5. Verify the zone mints the corresponding pathUSD balance on L2.
/// 6. Spawn zone sequencer background tasks (batch submitter + withdrawal processor).
/// 7. Request a withdrawal on L2 (approve + requestWithdrawal on ZoneOutbox).
/// 8. Wait for the batch to be submitted and the withdrawal to be processed on L1.
///
/// NOTE: This test requires the Foundry-compiled ZoneFactory artifact
/// at `docs/specs/out/ZoneFactory.sol/ZoneFactory.json`.
/// Run `forge build` in `docs/specs/` first.
#[tokio::test(flavor = "multi_thread")]
async fn test_deposit_via_real_l1() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    // Start real Tempo L1 in dev mode (500ms block time)
    let l1 = L1TestNode::start().await?;

    // Deploy L1 infrastructure and create a zone
    let portal_address = l1.deploy_zone().await?;

    // Start zone node connected to L1 with the real portal
    let zone = ZoneTestNode::start_from_l1(l1.http_url(), l1.ws_url(), portal_address).await?;

    // Wait for the zone to advance past genesis
    zone.wait_for_l2_tempo_finalized(0, L1_TIMEOUT).await?;

    // --- Deposit + withdrawal via ZoneAccount ---

    let mut account = ZoneAccount::from_l1_and_zone(&l1, &zone, portal_address);
    let deposit_amount: u128 = 1_000_000; // 1 pathUSD (6 decimals)

    // Fund the user account on L1 (separate from the sequencer/dev account)
    l1.fund_user(account.address(), deposit_amount * 2).await?;

    // Verify recipient starts with zero on L2
    let balance_before = zone.balance_of(ZONE_TOKEN_ADDRESS, account.address()).await?;
    assert_eq!(balance_before, U256::ZERO, "recipient should start with zero on L2");

    // Deposit on L1, wait for mint on L2
    let minted_balance = account.deposit(deposit_amount, L1_TIMEOUT, &zone).await?;
    assert_eq!(
        minted_balance,
        U256::from(deposit_amount),
        "minted balance should equal deposit amount (fee=0)"
    );

    // Spawn zone sequencer (batch submitter + withdrawal processor)
    let _sequencer_handle = account.spawn_sequencer(&l1, &zone, l1.dev_signer()).await;

    // Request withdrawal on L2
    let withdrawal_amount: u128 = 500_000; // 0.5 pathUSD
    account.withdraw(withdrawal_amount).await?;

    // Wait for the withdrawal to be fully processed on L1
    let withdrawal_timeout = std::time::Duration::from_secs(60);
    l1.wait_for_withdrawal_on_l1(portal_address, account.address(), withdrawal_amount, withdrawal_timeout)
        .await?;

    // Verify the L2 balance decreased by at least the withdrawal amount
    // (the ZoneOutbox also deducts a small fee on L2)
    let l2_balance_after = zone.balance_of(ZONE_TOKEN_ADDRESS, account.address()).await?;
    assert!(
        l2_balance_after < U256::from(deposit_amount - withdrawal_amount),
        "L2 balance should decrease by at least the withdrawal amount (got {l2_balance_after})"
    );

    Ok(())
}

/// Cross-zone withdrawal via the SwapAndDepositRouter:
///
///  1. Start L1 dev node.
///  2. Deploy ZoneFactory, create zone_a and zone_b, deploy SwapAndDepositRouter.
///  3. Start both zone nodes connected to L1.
///  4. Deposit pathUSD into zone_a.
///  5. Withdraw from zone_a with a callback that deposits into zone_b via the router.
///  6. Verify the deposit arrives on zone_b.
///  7. Withdraw from zone_b with a callback that deposits into zone_a via the router.
///  8. Verify the deposit arrives on zone_a.
///
/// ```text
///  Zone A          L1 (Router)          Zone B
///    |--- withdraw 0.4 -->|                |
///    |                    |-- deposit 0.4 ->|
///    |                    |                 |
///    |                    |<- withdraw 0.2 -|
///    |<-- deposit 0.2 ----|                 |
/// ```
///
/// NOTE: Requires `forge build` in `docs/specs/` for ZoneFactory + SwapAndDepositRouter artifacts.
#[tokio::test(flavor = "multi_thread")]
async fn test_cross_zone_withdrawal() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    // --- Step 1: Start L1 ---
    let l1 = L1TestNode::start().await?;

    // Separate sequencer keys for each zone to avoid L1 nonce conflicts
    let seq_a_signer = l1.signer_at(2);
    let seq_b_signer = l1.signer_at(3);

    // --- Step 2: Deploy L1 infrastructure (factory, two portals, router) ---
    let (portal_a, portal_b, router) = l1.deploy_two_zones_with_sequencers(
        seq_a_signer.address(),
        seq_b_signer.address(),
    ).await?;

    // --- Step 3: Start both zone nodes ---
    let zone_a = ZoneTestNode::start_from_l1(l1.http_url(), l1.ws_url(), portal_a).await?;
    let zone_b = ZoneTestNode::start_from_l1(l1.http_url(), l1.ws_url(), portal_b).await?;

    zone_a.wait_for_l2_tempo_finalized(0, L1_TIMEOUT).await?;
    zone_b.wait_for_l2_tempo_finalized(0, L1_TIMEOUT).await?;

    // --- Step 4: Deposit into zone_a ---
    let mut account_a = ZoneAccount::from_l1_and_zone(&l1, &zone_a, portal_a);
    let deposit_amount: u128 = 1_000_000; // 1 pathUSD
    l1.fund_user(account_a.address(), deposit_amount * 2).await?;
    account_a.deposit(deposit_amount, L1_TIMEOUT, &zone_a).await?;

    // Spawn sequencers for both zones
    let _seq_a = account_a.spawn_sequencer(&l1, &zone_a, seq_a_signer.clone()).await;
    let account_b = ZoneAccount::from_l1_and_zone(&l1, &zone_b, portal_b);
    let _seq_b = account_b.spawn_sequencer(&l1, &zone_b, seq_b_signer.clone()).await;

    // --- Step 5: Cross-zone withdrawal: zone_a → router → zone_b ---
    let cross_amount: u128 = 400_000; // 0.4 pathUSD
    let args_a_to_b = WithdrawalArgs::cross_zone_via_router(
        cross_amount,
        router,
        portal_b,
        PATH_USD_ADDRESS,
        account_a.address(),
    );
    account_a.withdraw_with(args_a_to_b).await?;

    // --- Step 6: Verify deposit arrives on zone_b ---
    let cross_timeout = std::time::Duration::from_secs(60);
    zone_b
        .wait_for_balance(ZONE_TOKEN_ADDRESS, account_a.address(), U256::ZERO, cross_timeout)
        .await?;

    let zone_b_balance = zone_b.balance_of(ZONE_TOKEN_ADDRESS, account_a.address()).await?;
    assert_eq!(
        zone_b_balance,
        U256::from(cross_amount),
        "zone_b should have received the cross-zone deposit"
    );

    // zone_a balance should have decreased
    let zone_a_balance = zone_a.balance_of(ZONE_TOKEN_ADDRESS, account_a.address()).await?;
    assert!(
        zone_a_balance <= U256::from(deposit_amount - cross_amount),
        "zone_a balance should decrease by at least the cross-zone amount (got {zone_a_balance})"
    );

    // --- Step 7: Cross-zone withdrawal: zone_b → router → zone_a ---
    let mut account_b = ZoneAccount::from_l1_and_zone(&l1, &zone_b, portal_b);
    let reverse_amount: u128 = 200_000; // 0.2 pathUSD
    let args_b_to_a = WithdrawalArgs::cross_zone_via_router(
        reverse_amount,
        router,
        portal_a,
        PATH_USD_ADDRESS,
        account_b.address(),
    );
    account_b.withdraw_with(args_b_to_a).await?;

    // --- Step 8: Verify deposit arrives on zone_a ---
    zone_a
        .wait_for_balance(ZONE_TOKEN_ADDRESS, account_b.address(), zone_a_balance, cross_timeout)
        .await?;

    let final_zone_a = zone_a.balance_of(ZONE_TOKEN_ADDRESS, account_b.address()).await?;
    assert!(
        final_zone_a > U256::ZERO,
        "zone_a should have received the reverse cross-zone deposit (got {final_zone_a})"
    );

    // zone_b balance should have decreased
    let final_zone_b = zone_b.balance_of(ZONE_TOKEN_ADDRESS, account_b.address()).await?;
    assert!(
        final_zone_b < U256::from(cross_amount),
        "zone_b balance should decrease by at least the reverse amount (got {final_zone_b})"
    );

    Ok(())
}
