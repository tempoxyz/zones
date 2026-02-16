//! Self-contained e2e tests using deposit-queue injection.
//!
//! These tests launch a Zone L2 node without a real L1 connection and inject
//! synthetic L1 blocks + deposits directly into the [`DepositQueue`]. The L1
//! subscriber retries a dummy URL in the background, but L2 execution is fully
//! exercised via queue injection (with the L1 state cache seeded for precompile reads).

use alloy::primitives::{Address, B256, U256, address};
use tempo_contracts::precompiles::ITIP20;
use tempo_precompiles::PATH_USD_ADDRESS;
use zone::abi::{TempoState, ZoneInbox, ZoneOutbox, TEMPO_STATE_ADDRESS, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS};

use crate::utils::{
    L1Fixture, ZoneTestNode, poll_until, seed_fixture_for_zone,
    start_local_zone_with_fixture, DEFAULT_POLL, DEFAULT_TIMEOUT,
};

/// Self-contained test: inject a deposit via the queue and verify the zone
/// mints the corresponding pathUSD balance on L2.
///
/// Flow:
/// 1. Start a zone node with no real L1 (dummy URL).
/// 2. Inject an L1 block with a deposit into the deposit queue.
/// 3. Wait for the ZoneEngine to produce L2 blocks.
/// 4. Verify the recipient's pathUSD balance increased on L2.
#[tokio::test(flavor = "multi_thread")]
async fn test_deposit_via_queue_injection() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    let depositor = address!("0x0000000000000000000000000000000000001234");
    let recipient = address!("0x0000000000000000000000000000000000005678");
    let deposit_amount: u128 = 1_000_000; // 1 pathUSD (6 decimals)

    let deposit = fixture.make_deposit(depositor, recipient, deposit_amount);
    fixture.inject_deposits(zone.deposit_queue(), vec![deposit]);

    let balance = zone
        .wait_for_balance(PATH_USD_ADDRESS, recipient, U256::ZERO, DEFAULT_TIMEOUT)
        .await?;
    assert_eq!(balance, U256::from(deposit_amount), "minted amount should equal deposit amount");

    Ok(())
}

/// Self-contained test: inject multiple deposits across multiple L1 blocks
/// and verify all are minted on L2.
#[tokio::test(flavor = "multi_thread")]
async fn test_multiple_deposits_across_blocks() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    let alice = address!("0x000000000000000000000000000000000000A11C");
    let bob = address!("0x0000000000000000000000000000000000000B0B");
    let sender = address!("0x0000000000000000000000000000000000001111");

    // Block 1: deposit to Alice
    let d1 = fixture.make_deposit(sender, alice, 500_000);
    fixture.inject_deposits(zone.deposit_queue(), vec![d1]);

    // Block 2: empty block (no deposits)
    fixture.inject_empty_block(zone.deposit_queue());

    // Block 3: two deposits — one to Alice, one to Bob
    let d2 = fixture.make_deposit(sender, alice, 300_000);
    let d3 = fixture.make_deposit(sender, bob, 700_000);
    fixture.inject_deposits(zone.deposit_queue(), vec![d2, d3]);

    // Alice should have 500k + 300k = 800k
    let alice_balance = zone
        .wait_for_balance(PATH_USD_ADDRESS, alice, U256::ZERO, DEFAULT_TIMEOUT)
        .await?;
    assert_eq!(alice_balance, U256::from(800_000u128));

    // Bob should have 700k
    let bob_balance = zone
        .wait_for_balance(PATH_USD_ADDRESS, bob, U256::ZERO, DEFAULT_TIMEOUT)
        .await?;
    assert_eq!(bob_balance, U256::from(700_000u128));

    Ok(())
}

/// Self-contained test: verify the zone produces blocks even for empty L1
/// blocks (no deposits). The zone must advance its TempoState for every L1
/// block to maintain chain continuity.
#[tokio::test(flavor = "multi_thread")]
async fn test_empty_l1_blocks_advance_zone() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    // Inject several empty L1 blocks
    fixture.inject_empty_blocks(zone.deposit_queue(), 5);

    // Each L1 block advances tempoBlockNumber — wait for all 5
    zone.wait_for_tempo_block_number(5, DEFAULT_TIMEOUT).await?;

    Ok(())
}

/// Two independent zones processing deposits from a shared L1 timeline.
///
/// Verifies that:
/// - Two zone nodes can run concurrently with different chain IDs.
/// - Each zone independently processes only the deposits injected into its queue.
/// - Cross-zone isolation: deposits on zone1 don't appear on zone2 and vice versa.
#[tokio::test(flavor = "multi_thread")]
async fn test_two_zones_independent_deposits() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    // Start two zones with different chain IDs
    let zone1 = ZoneTestNode::start_local_with_chain_id(71001).await?;
    let zone2 = ZoneTestNode::start_local_with_chain_id(71002).await?;

    // Shared L1 fixture — same header timeline for both zones
    let mut fixture = L1Fixture::new();
    seed_fixture_for_zone(&fixture, &zone1, 20);
    seed_fixture_for_zone(&fixture, &zone2, 20);

    let sender = address!("0x0000000000000000000000000000000000001111");
    let alice = address!("0x000000000000000000000000000000000000A11C");
    let bob = address!("0x0000000000000000000000000000000000000B0B");

    // L1 block 1: deposit to Alice on zone1, empty on zone2
    let b1 = fixture.next_block();
    let d1 = L1Fixture::make_deposit_for_block(b1.header.inner.number, sender, alice, 500_000);
    fixture.enqueue(&b1, zone1.deposit_queue(), vec![d1]);
    fixture.enqueue(&b1, zone2.deposit_queue(), vec![]);

    // L1 block 2: empty on zone1, deposit to Bob on zone2
    let b2 = fixture.next_block();
    let d2 = L1Fixture::make_deposit_for_block(b2.header.inner.number, sender, bob, 700_000);
    fixture.enqueue(&b2, zone1.deposit_queue(), vec![]);
    fixture.enqueue(&b2, zone2.deposit_queue(), vec![d2]);

    // L1 block 3: deposits on both zones
    let b3 = fixture.next_block();
    let d3a = L1Fixture::make_deposit_for_block(b3.header.inner.number, sender, alice, 300_000);
    let d3b = L1Fixture::make_deposit_for_block(b3.header.inner.number, sender, bob, 200_000);
    fixture.enqueue(&b3, zone1.deposit_queue(), vec![d3a]);
    fixture.enqueue(&b3, zone2.deposit_queue(), vec![d3b]);

    // Zone1: Alice should have 500k + 300k = 800k, Bob should have 0
    let zone1_alice = zone1
        .wait_for_balance(PATH_USD_ADDRESS, alice, U256::ZERO, DEFAULT_TIMEOUT)
        .await?;
    assert_eq!(zone1_alice, U256::from(800_000u128));

    let zone1_bob = zone1.balance_of(PATH_USD_ADDRESS, bob).await?;
    assert_eq!(zone1_bob, U256::ZERO, "zone1: Bob should have zero — deposit was on zone2");

    // Zone2: Bob should have 700k + 200k = 900k, Alice should have 0
    let zone2_bob = zone2
        .wait_for_balance(PATH_USD_ADDRESS, bob, U256::ZERO, DEFAULT_TIMEOUT)
        .await?;
    assert_eq!(zone2_bob, U256::from(900_000u128));

    let zone2_alice = zone2.balance_of(PATH_USD_ADDRESS, alice).await?;
    assert_eq!(zone2_alice, U256::ZERO, "zone2: Alice should have zero — deposit was on zone1");

    Ok(())
}

/// Verify that TempoState on the zone advances its tempoBlockNumber and
/// tempoBlockHash correctly as L1 blocks are injected.
#[tokio::test(flavor = "multi_thread")]
async fn test_tempo_state_advances_with_l1_blocks() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    let tempo_state = TempoState::new(TEMPO_STATE_ADDRESS, zone.provider());

    // Before injecting any blocks, tempoBlockNumber should be 0 (genesis)
    let initial_number = tempo_state.tempoBlockNumber().call().await?;
    assert_eq!(initial_number, 0, "initial tempoBlockNumber should be 0");

    let initial_hash = tempo_state.tempoBlockHash().call().await?;
    assert_ne!(initial_hash, B256::ZERO, "initial tempoBlockHash should be non-zero (genesis hash)");

    // Inject 3 empty L1 blocks
    for _ in 0..3 {
        fixture.inject_empty_block(zone.deposit_queue());
    }

    // Wait for tempoBlockNumber to reach 3
    let final_number = zone.wait_for_tempo_block_number(3, DEFAULT_TIMEOUT).await?;
    assert_eq!(final_number, 3, "tempoBlockNumber should be 3 after 3 L1 blocks");

    // tempoBlockHash should have changed
    let final_hash = tempo_state.tempoBlockHash().call().await?;
    assert_ne!(final_hash, initial_hash, "tempoBlockHash should change after advancing");
    assert_ne!(final_hash, B256::ZERO, "tempoBlockHash should be non-zero");

    Ok(())
}

/// Verify that TempoAdvanced and DepositProcessed events are emitted on
/// the ZoneInbox when processing deposits.
#[tokio::test(flavor = "multi_thread")]
async fn test_zone_inbox_events_on_deposit() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    let sender = address!("0x0000000000000000000000000000000000001111");
    let recipient = address!("0x0000000000000000000000000000000000002222");
    let deposit_amount: u128 = 5_000_000;

    let deposit = fixture.make_deposit(sender, recipient, deposit_amount);
    fixture.inject_deposits(zone.deposit_queue(), vec![deposit]);

    // Wait for the deposit to be processed
    zone.wait_for_balance(PATH_USD_ADDRESS, recipient, U256::ZERO, DEFAULT_TIMEOUT)
        .await?;

    // Query TempoAdvanced events from ZoneInbox
    let zone_inbox = ZoneInbox::new(ZONE_INBOX_ADDRESS, zone.provider());
    let tempo_advanced_filter = zone_inbox.TempoAdvanced_filter().from_block(0);
    let tempo_advanced_events = tempo_advanced_filter.query().await?;

    assert!(
        !tempo_advanced_events.is_empty(),
        "should have at least one TempoAdvanced event"
    );

    // Find the event for our deposit block (should have depositsProcessed == 1)
    let deposit_event = tempo_advanced_events.iter().find(|(e, _)| e.depositsProcessed == U256::from(1));
    assert!(
        deposit_event.is_some(),
        "should have a TempoAdvanced event with depositsProcessed == 1"
    );

    // Query DepositProcessed events
    let deposit_processed_filter = zone_inbox.DepositProcessed_filter().from_block(0);
    let deposit_processed_events = deposit_processed_filter.query().await?;

    assert!(
        !deposit_processed_events.is_empty(),
        "should have at least one DepositProcessed event"
    );

    // Verify the deposit event details
    let (dp_event, _) = &deposit_processed_events[0];
    assert_eq!(dp_event.sender, sender, "DepositProcessed sender mismatch");
    assert_eq!(dp_event.to, recipient, "DepositProcessed recipient mismatch");
    assert_eq!(dp_event.amount, deposit_amount, "DepositProcessed amount mismatch");

    Ok(())
}

/// Verify a large batch of deposits in a single L1 block is processed correctly.
#[tokio::test(flavor = "multi_thread")]
async fn test_large_deposit_batch() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    let sender = address!("0x0000000000000000000000000000000000001111");
    let num_deposits = 10u128;
    let amount_each: u128 = 100_000;

    // Build 10 deposits to different recipients in one L1 block
    let recipients: Vec<Address> = (0..num_deposits)
        .map(|i| {
            let mut addr_bytes = [0u8; 20];
            addr_bytes[19] = (i + 1) as u8;
            Address::from(addr_bytes)
        })
        .collect();
    let deposits: Vec<_> = recipients
        .iter()
        .map(|to| fixture.make_deposit(sender, *to, amount_each))
        .collect();

    fixture.inject_deposits(zone.deposit_queue(), deposits);

    // Wait for the last recipient to receive their deposit
    let last_recipient = *recipients.last().unwrap();
    zone.wait_for_balance(PATH_USD_ADDRESS, last_recipient, U256::ZERO, DEFAULT_TIMEOUT)
        .await?;

    // Verify all recipients received the correct amount
    let zone_token = ITIP20::new(PATH_USD_ADDRESS, zone.provider());
    for recipient in &recipients {
        let balance = zone_token.balanceOf(*recipient).call().await?;
        assert_eq!(
            balance,
            U256::from(amount_each),
            "recipient {recipient} should have {amount_each}"
        );
    }

    Ok(())
}

/// Verify the ZoneOutbox withdrawal batch finalization occurs in every block.
///
/// After injecting L1 blocks, each zone block should call finalizeWithdrawalBatch,
/// incrementing the withdrawalBatchIndex even when no withdrawals are pending.
#[tokio::test(flavor = "multi_thread")]
async fn test_withdrawal_batch_finalization() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    let zone_outbox = ZoneOutbox::new(ZONE_OUTBOX_ADDRESS, zone.provider());

    // Check initial batch index
    let initial_batch_index = zone_outbox.withdrawalBatchIndex().call().await?;

    // Inject 3 empty L1 blocks — each should trigger a finalizeWithdrawalBatch
    fixture.inject_empty_blocks(zone.deposit_queue(), 3);

    // Wait for batch index to advance by at least 3
    let final_batch_index = poll_until(
        DEFAULT_TIMEOUT,
        DEFAULT_POLL,
        "withdrawalBatchIndex advanced",
        || {
            let zone_outbox = &zone_outbox;
            async move {
                let idx = zone_outbox.withdrawalBatchIndex().call().await?;
                if idx >= initial_batch_index + 3 {
                    Ok(Some(idx))
                } else {
                    Ok(None)
                }
            }
        },
    )
    .await?;

    assert!(
        final_batch_index >= initial_batch_index + 3,
        "withdrawalBatchIndex should advance by at least 3 (got {initial_batch_index} -> {final_batch_index})"
    );

    // lastBatch should have zero withdrawalQueueHash (no withdrawals requested)
    let last_batch = zone_outbox.lastBatch().call().await?;
    assert_eq!(
        last_batch.withdrawalQueueHash,
        B256::ZERO,
        "lastBatch.withdrawalQueueHash should be zero with no withdrawals"
    );

    Ok(())
}
