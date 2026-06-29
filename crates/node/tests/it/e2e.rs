//! Self-contained e2e tests using deposit-queue injection.
//!
//! These tests launch a Zone L2 node without a real L1 connection and inject
//! synthetic L1 blocks + deposits directly into the [`DepositQueue`]. The L1
//! subscriber retries a dummy URL in the background, but L2 execution is fully
//! exercised via queue injection (with the L1 state cache seeded for precompile reads).

use alloy::primitives::{Address, B256, Bytes, U256, address};
use alloy_consensus::Transaction;
use alloy_eips::NumHash;
use alloy_provider::{DynProvider, Provider};
use alloy_sol_types::SolCall;
use tempo_chainspec::spec::TEMPO_T0_BASE_FEE;
use tempo_precompiles::PATH_USD_ADDRESS;
use tempo_zone_contracts::{
    TEMPO_STATE_ADDRESS, TempoState, Withdrawal, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS,
    ZoneInbox, ZoneOutbox,
};
use zone_l1::ChainTempoStateExt;

use crate::utils::{
    DEFAULT_POLL, DEFAULT_TIMEOUT, L1Fixture, WITHDRAWAL_TX_GAS, ZoneTestNode, approve_outbox,
    local_dev_zone_account, poll_until, seed_fixture_for_zone, start_local_zone_with_fixture,
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

    let deposit = fixture.make_deposit(PATH_USD_ADDRESS, depositor, recipient, deposit_amount);
    fixture.inject_deposits(zone.deposit_queue(), vec![deposit]);

    let balance = zone
        .wait_for_balance(
            PATH_USD_ADDRESS,
            recipient,
            U256::from(deposit_amount),
            DEFAULT_TIMEOUT,
        )
        .await?;
    assert_eq!(
        balance,
        U256::from(deposit_amount),
        "minted amount should equal deposit amount"
    );

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
    let d1 = fixture.make_deposit(PATH_USD_ADDRESS, sender, alice, 500_000);
    fixture.inject_deposits(zone.deposit_queue(), vec![d1]);

    // Block 2: empty block (no deposits)
    fixture.inject_empty_block(zone.deposit_queue());

    // Block 3: two deposits — one to Alice, one to Bob
    let d2 = fixture.make_deposit(PATH_USD_ADDRESS, sender, alice, 300_000);
    let d3 = fixture.make_deposit(PATH_USD_ADDRESS, sender, bob, 700_000);
    fixture.inject_deposits(zone.deposit_queue(), vec![d2, d3]);

    // Alice should have 500k + 300k = 800k
    let alice_balance = zone
        .wait_for_balance(
            PATH_USD_ADDRESS,
            alice,
            U256::from(800_000u128),
            DEFAULT_TIMEOUT,
        )
        .await?;
    assert_eq!(alice_balance, U256::from(800_000u128));

    // Bob should have 700k
    let bob_balance = zone
        .wait_for_balance(
            PATH_USD_ADDRESS,
            bob,
            U256::from(700_000u128),
            DEFAULT_TIMEOUT,
        )
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
    let d1 = L1Fixture::make_deposit_for_block(PATH_USD_ADDRESS, sender, alice, 500_000);
    fixture.enqueue(&b1, zone1.deposit_queue(), vec![d1]);
    fixture.enqueue(&b1, zone2.deposit_queue(), vec![]);

    // L1 block 2: empty on zone1, deposit to Bob on zone2
    let b2 = fixture.next_block();
    let d2 = L1Fixture::make_deposit_for_block(PATH_USD_ADDRESS, sender, bob, 700_000);
    fixture.enqueue(&b2, zone1.deposit_queue(), vec![]);
    fixture.enqueue(&b2, zone2.deposit_queue(), vec![d2]);

    // L1 block 3: deposits on both zones
    let b3 = fixture.next_block();
    let d3a = L1Fixture::make_deposit_for_block(PATH_USD_ADDRESS, sender, alice, 300_000);
    let d3b = L1Fixture::make_deposit_for_block(PATH_USD_ADDRESS, sender, bob, 200_000);
    fixture.enqueue(&b3, zone1.deposit_queue(), vec![d3a]);
    fixture.enqueue(&b3, zone2.deposit_queue(), vec![d3b]);

    // Zone1: Alice should have 500k + 300k = 800k, Bob should have 0
    let zone1_alice = zone1
        .wait_for_balance(
            PATH_USD_ADDRESS,
            alice,
            U256::from(800_000u128),
            DEFAULT_TIMEOUT,
        )
        .await?;
    assert_eq!(zone1_alice, U256::from(800_000u128));

    let zone1_bob = zone1.balance_of(PATH_USD_ADDRESS, bob).await?;
    assert_eq!(
        zone1_bob,
        U256::ZERO,
        "zone1: Bob should have zero — deposit was on zone2"
    );

    // Zone2: Bob should have 700k + 200k = 900k, Alice should have 0
    let zone2_bob = zone2
        .wait_for_balance(
            PATH_USD_ADDRESS,
            bob,
            U256::from(900_000u128),
            DEFAULT_TIMEOUT,
        )
        .await?;
    assert_eq!(zone2_bob, U256::from(900_000u128));

    let zone2_alice = zone2.balance_of(PATH_USD_ADDRESS, alice).await?;
    assert_eq!(
        zone2_alice,
        U256::ZERO,
        "zone2: Alice should have zero — deposit was on zone1"
    );

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
    assert_ne!(
        initial_hash,
        B256::ZERO,
        "initial tempoBlockHash should be non-zero (genesis hash)"
    );

    // Inject 3 empty L1 blocks
    for _ in 0..3 {
        fixture.inject_empty_block(zone.deposit_queue());
    }

    // Wait for tempoBlockNumber to reach 3
    let final_number = zone.wait_for_tempo_block_number(3, DEFAULT_TIMEOUT).await?;
    assert_eq!(
        final_number, 3,
        "tempoBlockNumber should be 3 after 3 L1 blocks"
    );

    // tempoBlockHash should have changed
    let final_hash = tempo_state.tempoBlockHash().call().await?;
    assert_ne!(
        final_hash, initial_hash,
        "tempoBlockHash should change after advancing"
    );
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

    let deposit = fixture.make_deposit(PATH_USD_ADDRESS, sender, recipient, deposit_amount);
    fixture.inject_deposits(zone.deposit_queue(), vec![deposit]);

    // Wait for the deposit to be processed
    zone.wait_for_balance(
        PATH_USD_ADDRESS,
        recipient,
        U256::from(deposit_amount),
        DEFAULT_TIMEOUT,
    )
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
    let deposit_event = tempo_advanced_events
        .iter()
        .find(|(e, _)| e.depositsProcessed == U256::from(1));
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
    assert_eq!(
        dp_event.to, recipient,
        "DepositProcessed recipient mismatch"
    );
    assert_eq!(
        dp_event.amount, deposit_amount,
        "DepositProcessed amount mismatch"
    );

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
        .map(|to| fixture.make_deposit(PATH_USD_ADDRESS, sender, *to, amount_each))
        .collect();

    fixture.inject_deposits(zone.deposit_queue(), deposits);

    // Wait for the last recipient to receive their deposit
    let last_recipient = *recipients.last().unwrap();
    zone.wait_for_balance(
        PATH_USD_ADDRESS,
        last_recipient,
        U256::from(amount_each),
        DEFAULT_TIMEOUT,
    )
    .await?;

    // Verify all recipients received the correct amount
    for recipient in &recipients {
        let balance = zone.balance_of(PATH_USD_ADDRESS, *recipient).await?;
        assert_eq!(
            balance,
            U256::from(amount_each),
            "recipient {recipient} should have {amount_each}"
        );
    }

    Ok(())
}

/// Verify empty withdrawal batches finalize on the configured chain-time interval.
#[tokio::test(flavor = "multi_thread")]
async fn test_withdrawal_batch_finalization() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    let zone_outbox = ZoneOutbox::new(ZONE_OUTBOX_ADDRESS, zone.provider());

    let initial_batch_index = zone_outbox.withdrawalBatchIndex().call().await?;
    let initial_zone_block = zone.provider().get_block_number().await?;

    // The first fixture block has a large timestamp jump from genesis, so it
    // closes the first empty batch immediately.
    fixture.inject_empty_block(zone.deposit_queue());
    let first_boundary_block = initial_zone_block + 1;

    poll_until(
        DEFAULT_TIMEOUT,
        DEFAULT_POLL,
        "first empty withdrawal batch finalized",
        || {
            let zone_outbox = &zone_outbox;
            async move {
                let idx = zone_outbox.withdrawalBatchIndex().call().await?;
                if idx == initial_batch_index + 1 {
                    Ok(Some(idx))
                } else {
                    Ok(None)
                }
            }
        },
    )
    .await?;

    fixture.inject_empty_blocks(zone.deposit_queue(), 3);
    poll_until(
        DEFAULT_TIMEOUT,
        DEFAULT_POLL,
        "intermediate empty zone blocks produced",
        || {
            let provider = zone.provider();
            async move {
                let number = provider.get_block_number().await?;
                if number >= first_boundary_block + 3 {
                    Ok(Some(number))
                } else {
                    Ok(None)
                }
            }
        },
    )
    .await?;

    let intermediate_batch_index = zone_outbox.withdrawalBatchIndex().call().await?;
    assert_eq!(
        intermediate_batch_index,
        initial_batch_index + 1,
        "withdrawalBatchIndex should not advance before the configured interval elapses"
    );

    fixture.inject_empty_blocks(zone.deposit_queue(), 1);

    let final_batch_index = poll_until(
        DEFAULT_TIMEOUT,
        DEFAULT_POLL,
        "withdrawalBatchIndex advanced",
        || {
            let zone_outbox = &zone_outbox;
            async move {
                let idx = zone_outbox.withdrawalBatchIndex().call().await?;
                if idx == initial_batch_index + 2 {
                    Ok(Some(idx))
                } else {
                    Ok(None)
                }
            }
        },
    )
    .await?;

    assert_eq!(
        final_batch_index,
        initial_batch_index + 2,
        "withdrawalBatchIndex should advance when the configured interval elapses"
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

async fn submit_withdrawal(
    fixture: &mut L1Fixture,
    zone: &ZoneTestNode,
    provider: &DynProvider,
    dev_address: Address,
    amount: u128,
) -> eyre::Result<u64> {
    let outbox = ZoneOutbox::new(ZONE_OUTBOX_ADDRESS, provider.clone());
    let pending = outbox
        .requestWithdrawal(
            PATH_USD_ADDRESS,
            dev_address,
            amount,
            B256::ZERO,
            0,
            dev_address,
            Bytes::new(),
            Bytes::new(),
        )
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(WITHDRAWAL_TX_GAS)
        .send()
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    let receipt = pending.get_receipt().await?;
    assert!(receipt.status(), "withdrawal should succeed");
    receipt
        .block_number
        .ok_or_else(|| eyre::eyre!("withdrawal receipt missing block number"))
}

/// Verify a lone withdrawal in a current-only block is deferred to the next block.
#[tokio::test(flavor = "multi_thread")]
async fn test_withdrawal_requests_finalize_next_block() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;
    let (provider, dev_address) = local_dev_zone_account(&zone)?;
    let outbox = ZoneOutbox::new(ZONE_OUTBOX_ADDRESS, provider.clone());

    let deposit_amount: u128 = 2_000_000;
    let deposit = fixture.make_deposit(PATH_USD_ADDRESS, dev_address, dev_address, deposit_amount);
    fixture.inject_deposits(zone.deposit_queue(), vec![deposit]);
    zone.wait_for_balance(
        PATH_USD_ADDRESS,
        dev_address,
        U256::from(deposit_amount),
        DEFAULT_TIMEOUT,
    )
    .await?;
    approve_outbox(&mut fixture, &zone, &provider).await?;

    let batch_index_before = outbox.withdrawalBatchIndex().call().await?;
    let withdrawal_block =
        submit_withdrawal(&mut fixture, &zone, &provider, dev_address, 250_000).await?;

    // Current-only block defers finalization — no BatchFinalized yet.
    let deferred_logs = outbox
        .BatchFinalized_filter()
        .from_block(withdrawal_block)
        .to_block(withdrawal_block)
        .query()
        .await?;
    assert!(
        deferred_logs.is_empty(),
        "withdrawal block {withdrawal_block} should defer BatchFinalized"
    );
    assert_eq!(
        outbox.withdrawalBatchIndex().call().await?,
        batch_index_before,
        "withdrawalBatchIndex should not advance on deferred block"
    );
    assert!(
        outbox.pendingWithdrawalsCount().call().await? > U256::ZERO,
        "withdrawal should remain pending after deferred block"
    );

    // Next quiet block finalizes the deferred withdrawal via the prior path.
    fixture.inject_empty_block(zone.deposit_queue());
    let quiet_block = withdrawal_block + 1;
    let finalized_batch_index = poll_until(
        DEFAULT_TIMEOUT,
        DEFAULT_POLL,
        "withdrawal batch finalized on next block",
        || {
            let outbox = &outbox;
            async move {
                let index = outbox.withdrawalBatchIndex().call().await?;
                if index == batch_index_before + 1 {
                    Ok(Some(index))
                } else {
                    Ok(None)
                }
            }
        },
    )
    .await?;

    assert_eq!(
        outbox.pendingWithdrawalsCount().call().await?,
        U256::ZERO,
        "finalize block should sweep pending withdrawals"
    );
    assert!(
        outbox
            .WithdrawalRequested_filter()
            .from_block(quiet_block)
            .to_block(quiet_block)
            .query()
            .await?
            .is_empty(),
        "quiet boundary block should carry no WithdrawalRequested logs"
    );

    let requested_logs = outbox
        .WithdrawalRequested_filter()
        .from_block(withdrawal_block)
        .to_block(withdrawal_block)
        .query()
        .await?;
    assert_eq!(requested_logs.len(), 1);
    let (requested, requested_log) = &requested_logs[0];
    let withdrawal_tx_hash = requested_log
        .transaction_hash
        .ok_or_else(|| eyre::eyre!("WithdrawalRequested log missing transaction hash"))?;
    let withdrawal = Withdrawal::from_requested_event(requested, withdrawal_tx_hash, Bytes::new());
    let expected_hash = Withdrawal::queue_hash(&[withdrawal]);

    let finalized_logs = outbox
        .BatchFinalized_filter()
        .from_block(quiet_block)
        .to_block(quiet_block)
        .query()
        .await?;
    assert_eq!(
        finalized_logs.len(),
        1,
        "exactly one BatchFinalized should follow deferred withdrawal"
    );
    let (finalized, log) = &finalized_logs[0];
    assert_eq!(finalized.withdrawalQueueHash, expected_hash);
    assert_eq!(finalized.withdrawalBatchIndex, finalized_batch_index);

    let tx_hash = log
        .transaction_hash
        .ok_or_else(|| eyre::eyre!("BatchFinalized log missing transaction hash"))?;
    let finalize_tx = provider
        .get_transaction_by_hash(tx_hash)
        .await?
        .ok_or_else(|| eyre::eyre!("finalizeWithdrawalBatch tx {tx_hash} not found"))?;
    let finalize_call =
        ZoneOutbox::finalizeWithdrawalBatchCall::abi_decode(finalize_tx.input().as_ref())?;
    assert_eq!(
        finalize_call.count,
        U256::from(1),
        "builder should finalize exactly the deferred withdrawal"
    );
    assert_eq!(finalize_call.encryptedSenders.len(), 1);

    let last_batch = outbox.lastBatch().call().await?;
    assert_eq!(last_batch.withdrawalQueueHash, expected_hash);
    assert_eq!(last_batch.withdrawalBatchIndex, finalized_batch_index);

    Ok(())
}

/// Two consecutive withdrawal blocks are joined into a single batch at block N+1.
#[tokio::test(flavor = "multi_thread")]
async fn test_consecutive_withdrawal_blocks_joined_into_one_batch() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;
    let (provider, dev_address) = local_dev_zone_account(&zone)?;
    let outbox = ZoneOutbox::new(ZONE_OUTBOX_ADDRESS, provider.clone());

    let deposit = fixture.make_deposit(PATH_USD_ADDRESS, dev_address, dev_address, 2_000_000);
    fixture.inject_deposits(zone.deposit_queue(), vec![deposit]);
    zone.wait_for_balance(
        PATH_USD_ADDRESS,
        dev_address,
        U256::from(2_000_000u128),
        DEFAULT_TIMEOUT,
    )
    .await?;
    approve_outbox(&mut fixture, &zone, &provider).await?;

    let batch_index_before = outbox.withdrawalBatchIndex().call().await?;
    let block_n = submit_withdrawal(&mut fixture, &zone, &provider, dev_address, 250_000).await?;
    let deferred_logs = outbox
        .BatchFinalized_filter()
        .from_block(block_n)
        .to_block(block_n)
        .query()
        .await?;
    assert!(
        deferred_logs.is_empty(),
        "block N should defer finalization"
    );

    let block_n_plus_1 =
        submit_withdrawal(&mut fixture, &zone, &provider, dev_address, 350_000).await?;

    poll_until(
        DEFAULT_TIMEOUT,
        DEFAULT_POLL,
        "joined withdrawal batch finalized",
        || {
            let outbox = &outbox;
            async move {
                let index = outbox.withdrawalBatchIndex().call().await?;
                if index == batch_index_before + 1 {
                    Ok(Some(index))
                } else {
                    Ok(None)
                }
            }
        },
    )
    .await?;

    let finalized_logs = outbox
        .BatchFinalized_filter()
        .from_block(block_n_plus_1)
        .to_block(block_n_plus_1)
        .query()
        .await?;
    assert_eq!(
        finalized_logs.len(),
        1,
        "block N+1 should emit exactly one BatchFinalized covering both withdrawals"
    );

    let requested_n = outbox
        .WithdrawalRequested_filter()
        .from_block(block_n)
        .to_block(block_n)
        .query()
        .await?;
    let requested_n1 = outbox
        .WithdrawalRequested_filter()
        .from_block(block_n_plus_1)
        .to_block(block_n_plus_1)
        .query()
        .await?;
    assert_eq!(requested_n.len(), 1);
    assert_eq!(requested_n1.len(), 1);

    let mut withdrawals = Vec::new();
    for (requested, log) in requested_n.iter().chain(requested_n1.iter()) {
        let tx_hash = log
            .transaction_hash
            .ok_or_else(|| eyre::eyre!("WithdrawalRequested log missing transaction hash"))?;
        withdrawals.push(Withdrawal::from_requested_event(
            requested,
            tx_hash,
            Bytes::new(),
        ));
    }
    let expected_hash = Withdrawal::queue_hash(&withdrawals);
    let (finalized, log) = &finalized_logs[0];
    assert_eq!(finalized.withdrawalQueueHash, expected_hash);

    let tx_hash = log
        .transaction_hash
        .ok_or_else(|| eyre::eyre!("BatchFinalized log missing transaction hash"))?;
    let finalize_tx = provider
        .get_transaction_by_hash(tx_hash)
        .await?
        .ok_or_else(|| eyre::eyre!("finalizeWithdrawalBatch tx {tx_hash} not found"))?;
    let finalize_call =
        ZoneOutbox::finalizeWithdrawalBatchCall::abi_decode(finalize_tx.input().as_ref())?;
    assert_eq!(
        finalize_call.count,
        U256::from(2),
        "joined batch should cover both withdrawal blocks"
    );
    assert_eq!(finalize_call.encryptedSenders.len(), 2);

    Ok(())
}

/// Current-only block with interval elapsed finalizes that block's withdrawals.
#[tokio::test(flavor = "multi_thread")]
async fn test_current_only_block_finalizes_when_interval_elapsed() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;
    let (provider, dev_address) = local_dev_zone_account(&zone)?;
    let outbox = ZoneOutbox::new(ZONE_OUTBOX_ADDRESS, provider.clone());

    let batch_index = outbox.withdrawalBatchIndex().call().await?;
    fixture.inject_empty_block(zone.deposit_queue());
    poll_until(
        DEFAULT_TIMEOUT,
        DEFAULT_POLL,
        "genesis empty batch closed",
        || {
            let outbox = &outbox;
            async move {
                let idx = outbox.withdrawalBatchIndex().call().await?;
                if idx == batch_index + 1 {
                    Ok(Some(idx))
                } else {
                    Ok(None)
                }
            }
        },
    )
    .await?;

    let deposit = fixture.make_deposit(PATH_USD_ADDRESS, dev_address, dev_address, 1_000_000);
    fixture.inject_deposits(zone.deposit_queue(), vec![deposit]);
    zone.wait_for_balance(
        PATH_USD_ADDRESS,
        dev_address,
        U256::from(1_000_000u128),
        DEFAULT_TIMEOUT,
    )
    .await?;
    approve_outbox(&mut fixture, &zone, &provider).await?;

    // Advance 3 quiet blocks (3s < 4s interval) after the boundary.
    fixture.inject_empty_blocks(zone.deposit_queue(), 3);

    let batch_index_before = outbox.withdrawalBatchIndex().call().await?;
    // 4th block after the boundary: interval elapsed, current-only block should finalize.
    let withdrawal_block =
        submit_withdrawal(&mut fixture, &zone, &provider, dev_address, 250_000).await?;

    poll_until(
        DEFAULT_TIMEOUT,
        DEFAULT_POLL,
        "interval-elapsed current-only block finalized",
        || {
            let outbox = &outbox;
            async move {
                let index = outbox.withdrawalBatchIndex().call().await?;
                if index == batch_index_before + 1 {
                    Ok(Some(index))
                } else {
                    Ok(None)
                }
            }
        },
    )
    .await?;

    let finalized_logs = outbox
        .BatchFinalized_filter()
        .from_block(withdrawal_block)
        .to_block(withdrawal_block)
        .query()
        .await?;
    assert_eq!(
        finalized_logs.len(),
        1,
        "interval-elapsed current-only block should finalize in the same block"
    );

    let requested_logs = outbox
        .WithdrawalRequested_filter()
        .from_block(withdrawal_block)
        .to_block(withdrawal_block)
        .query()
        .await?;
    let (requested, requested_log) = &requested_logs[0];
    let withdrawal_tx_hash = requested_log
        .transaction_hash
        .ok_or_else(|| eyre::eyre!("WithdrawalRequested log missing transaction hash"))?;
    let withdrawal = Withdrawal::from_requested_event(requested, withdrawal_tx_hash, Bytes::new());
    assert_eq!(
        finalized_logs[0].0.withdrawalQueueHash,
        Withdrawal::queue_hash(&[withdrawal])
    );

    let tx_hash = finalized_logs[0]
        .1
        .transaction_hash
        .ok_or_else(|| eyre::eyre!("BatchFinalized log missing transaction hash"))?;
    let finalize_tx = provider
        .get_transaction_by_hash(tx_hash)
        .await?
        .ok_or_else(|| eyre::eyre!("finalizeWithdrawalBatch tx {tx_hash} not found"))?;
    let finalize_call =
        ZoneOutbox::finalizeWithdrawalBatchCall::abi_decode(finalize_tx.input().as_ref())?;
    assert_eq!(finalize_call.count, U256::from(1));

    Ok(())
}

/// Submit a signed L2 withdrawal request with an over-cap callback gas limit.
///
/// This exercises the RPC transaction path: the transaction is accepted into a
/// zone block, reverts in `ZoneOutbox`, and does not enter the pending
/// withdrawal queue.
#[tokio::test(flavor = "multi_thread")]
async fn test_withdrawal_request_rejects_over_max_callback_gas() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    let (provider, dev_address) = local_dev_zone_account(&zone)?;
    let outbox = ZoneOutbox::new(ZONE_OUTBOX_ADDRESS, &provider);

    let deposit_amount: u128 = 1_000_000;
    let deposit = fixture.make_deposit(PATH_USD_ADDRESS, dev_address, dev_address, deposit_amount);
    fixture.inject_deposits(zone.deposit_queue(), vec![deposit]);

    zone.wait_for_balance(
        PATH_USD_ADDRESS,
        dev_address,
        U256::from(deposit_amount),
        DEFAULT_TIMEOUT,
    )
    .await?;

    approve_outbox(&mut fixture, &zone, &provider).await?;

    let pending_before = outbox.pendingWithdrawalsCount().call().await?;
    let max_callback_gas = outbox.MAX_WITHDRAWAL_GAS_LIMIT().call().await?;

    let withdrawal_pending = outbox
        .requestWithdrawal(
            PATH_USD_ADDRESS,
            dev_address,
            250_000,
            B256::ZERO,
            max_callback_gas + 1,
            dev_address,
            Bytes::from_static(b"callback"),
            Bytes::new(),
        )
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(WITHDRAWAL_TX_GAS)
        .send()
        .await?;

    fixture.inject_empty_block(zone.deposit_queue());
    let withdrawal_receipt = withdrawal_pending.get_receipt().await?;
    assert!(
        !withdrawal_receipt.status(),
        "over-cap withdrawal request should revert on L2"
    );

    let pending_after = outbox.pendingWithdrawalsCount().call().await?;
    assert_eq!(
        pending_after, pending_before,
        "reverted withdrawal must not enter the pending queue"
    );

    Ok(())
}

/// Verify that `ChainTempoStateExt` on the committed `Chain` from a canon state
/// notification returns the correct L1 block NumHash after zone blocks are mined.
#[tokio::test(flavor = "multi_thread")]
async fn test_chain_tempo_state_ext_from_canon_notification() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;
    let mut canon_rx = zone.subscribe_to_canonical_state();

    // Inject 3 empty L1 blocks — each produces a zone block.
    fixture.inject_empty_blocks(zone.deposit_queue(), 3);

    // Wait for tempoBlockNumber to reach 3 via RPC (ensures blocks are mined).
    zone.wait_for_tempo_block_number(3, DEFAULT_TIMEOUT).await?;

    // Drain canon notifications and collect the L1 NumHash from each committed chain.
    let mut num_hashes: Vec<NumHash> = Vec::new();
    while let Ok(notification) = canon_rx.try_recv() {
        let chain = notification.committed();
        let nh = chain.tempo_num_hash();
        if nh.number > 0 {
            num_hashes.push(nh);
        }
    }

    // We should have received notifications for blocks 1, 2, 3.
    assert!(
        num_hashes.len() >= 3,
        "expected at least 3 canon notifications with non-zero tempoBlockNumber, got {}",
        num_hashes.len()
    );

    // Verify the L1 block numbers are monotonically increasing.
    for window in num_hashes.windows(2) {
        assert!(
            window[1].number > window[0].number,
            "L1 block numbers should be increasing: {} -> {}",
            window[0].number,
            window[1].number
        );
    }

    // The last notification should match the final L1 block number (3).
    let last = num_hashes.last().unwrap();
    assert_eq!(
        last.number, 3,
        "last canon notification should have tempoBlockNumber == 3"
    );
    assert_ne!(last.hash, B256::ZERO, "tempoBlockHash should be non-zero");

    // Cross-check against the on-chain TempoState.
    let tempo_state = TempoState::new(TEMPO_STATE_ADDRESS, zone.provider());
    let on_chain_hash = tempo_state.tempoBlockHash().call().await?;
    assert_eq!(
        last.hash, on_chain_hash,
        "Chain ext hash should match on-chain tempoBlockHash"
    );

    Ok(())
}
