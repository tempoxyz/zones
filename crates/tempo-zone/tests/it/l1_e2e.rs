//! Full L1+L2 end-to-end tests with a real in-process Tempo L1 node.
//!
//! Unlike the injection-based tests in `e2e.rs`, these tests start a real
//! Tempo L1 dev node and a Zone L2 node connected via WebSocket. The L1
//! subscriber naturally receives blocks and deposits — no synthetic injection.

use crate::utils::{L1TestNode, WithdrawalArgs, ZoneAccount, ZoneTestNode, spawn_sequencer};
use alloy::{
    primitives::{Address, B256, U256},
    providers::Provider,
};
use std::time::Duration;
use tempo_precompiles::PATH_USD_ADDRESS;
use zone::abi::{TEMPO_STATE_ADDRESS, TempoState, ZONE_TOKEN_ADDRESS};

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
    assert!(
        l1_block_1 > l1_block_0,
        "L1 should be producing blocks in dev mode"
    );

    // Start zone node connected to real L1 — genesis is patched from the L1's
    // current header so TempoState chain continuity works.
    let zone = ZoneTestNode::start_from_l1(l1.http_url(), l1.ws_url(), Address::ZERO).await?;

    // Wait for the zone to advance past block 0 (genesis anchor)
    let zone_tempo_number = zone.wait_for_l2_tempo_finalized(0, L1_TIMEOUT).await?;
    assert!(
        zone_tempo_number > 0,
        "zone should have advanced past genesis anchor"
    );

    // Zone should also have produced L2 blocks
    let zone_block = zone.provider().get_block_number().await?;
    assert!(zone_block > 0, "zone L2 should have blocks");

    // tempoBlockHash should be non-zero (real L1 headers)
    let tempo_state = TempoState::new(TEMPO_STATE_ADDRESS, zone.provider());
    let tempo_hash = tempo_state.tempoBlockHash().call().await?;
    assert_ne!(
        tempo_hash,
        B256::ZERO,
        "tempoBlockHash should be set from real L1 headers"
    );

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
    let balance_before = zone
        .balance_of(ZONE_TOKEN_ADDRESS, account.address())
        .await?;
    assert_eq!(
        balance_before,
        U256::ZERO,
        "recipient should start with zero on L2"
    );

    // Deposit on L1, wait for mint on L2
    let minted_balance = account.deposit(deposit_amount, L1_TIMEOUT, &zone).await?;
    assert_eq!(
        minted_balance,
        U256::from(deposit_amount),
        "minted balance should equal deposit amount (fee=0)"
    );

    // Spawn zone sequencer (batch submitter + withdrawal processor)
    let _sequencer_handle = spawn_sequencer(&l1, &zone, portal_address, l1.dev_signer()).await;

    // Request withdrawal on L2
    let withdrawal_amount: u128 = 500_000; // 0.5 pathUSD
    account.withdraw(withdrawal_amount).await?;

    // Wait for the withdrawal to be fully processed on L1
    let withdrawal_timeout = std::time::Duration::from_secs(60);
    l1.wait_for_withdrawal_on_l1(
        portal_address,
        account.address(),
        withdrawal_amount,
        withdrawal_timeout,
    )
    .await?;

    // Verify the L2 balance decreased by at least the withdrawal amount
    let l2_balance_after = zone
        .balance_of(ZONE_TOKEN_ADDRESS, account.address())
        .await?;
    assert!(
        l2_balance_after <= U256::from(deposit_amount - withdrawal_amount),
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
    let (portal_a, portal_b, router) = l1
        .deploy_two_zones_with_sequencers(seq_a_signer.address(), seq_b_signer.address())
        .await?;

    // --- Step 3: Start both zone nodes ---
    let zone_a = ZoneTestNode::start_from_l1(l1.http_url(), l1.ws_url(), portal_a).await?;
    let zone_b = ZoneTestNode::start_from_l1(l1.http_url(), l1.ws_url(), portal_b).await?;

    zone_a.wait_for_l2_tempo_finalized(0, L1_TIMEOUT).await?;
    zone_b.wait_for_l2_tempo_finalized(0, L1_TIMEOUT).await?;

    // --- Step 4: Deposit into zone_a ---
    let mut account_a = ZoneAccount::from_l1_and_zone(&l1, &zone_a, portal_a);
    let deposit_amount: u128 = 1_000_000; // 1 pathUSD
    l1.fund_user(account_a.address(), deposit_amount * 2)
        .await?;
    account_a
        .deposit(deposit_amount, L1_TIMEOUT, &zone_a)
        .await?;

    // Spawn sequencers for both zones
    let _seq_a = spawn_sequencer(&l1, &zone_a, portal_a, seq_a_signer.clone()).await;
    let _seq_b = spawn_sequencer(&l1, &zone_b, portal_b, seq_b_signer.clone()).await;

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
        .wait_for_balance(
            ZONE_TOKEN_ADDRESS,
            account_a.address(),
            U256::from(cross_amount),
            cross_timeout,
        )
        .await?;

    let zone_b_balance = zone_b
        .balance_of(ZONE_TOKEN_ADDRESS, account_a.address())
        .await?;
    assert_eq!(
        zone_b_balance,
        U256::from(cross_amount),
        "zone_b should have received the cross-zone deposit"
    );

    // zone_a balance should have decreased
    let zone_a_balance = zone_a
        .balance_of(ZONE_TOKEN_ADDRESS, account_a.address())
        .await?;
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
        .wait_for_balance(
            ZONE_TOKEN_ADDRESS,
            account_b.address(),
            zone_a_balance,
            cross_timeout,
        )
        .await?;

    let final_zone_a = zone_a
        .balance_of(ZONE_TOKEN_ADDRESS, account_b.address())
        .await?;
    assert!(
        final_zone_a > U256::ZERO,
        "zone_a should have received the reverse cross-zone deposit (got {final_zone_a})"
    );

    // zone_b balance should have decreased
    let final_zone_b = zone_b
        .balance_of(ZONE_TOKEN_ADDRESS, account_b.address())
        .await?;
    assert!(
        final_zone_b < U256::from(cross_amount),
        "zone_b balance should decrease by at least the reverse amount (got {final_zone_b})"
    );

    Ok(())
}

/// Multi-asset deposit + withdrawal test:
///
///  1. Start L1 dev node.
///  2. Create a second TIP-20 token ("ZoneUSD") on L1.
///  3. Deploy ZoneFactory, create a zone with pathUSD as initial token.
///  4. Enable ZoneUSD on the portal.
///  5. Start zone node connected to L1 (ZoneUSD is auto-initialized via TokenEnabled event).
///  6. Deposit pathUSD and ZoneUSD into the zone.
///  7. Spawn sequencer, withdraw both tokens back to L1.
///  8. Verify withdrawals processed and L2 balances decreased.
///
/// ```text
///  L1 (pathUSD + ZoneUSD)          Zone L2
///    |--- deposit pathUSD -------->|  ✓ pathUSD minted
///    |--- deposit ZoneUSD -------->|  ✓ ZoneUSD minted
///    |<-- withdraw pathUSD --------|  ✓ pathUSD burned
///    |<-- withdraw ZoneUSD --------|  ✓ ZoneUSD burned
/// ```
///
/// NOTE: Requires `forge build` in `docs/specs/` for ZoneFactory artifact.
#[tokio::test(flavor = "multi_thread")]
async fn test_multiasset_deposit_withdrawal() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    // --- Step 1: Start L1 ---
    let l1 = L1TestNode::start().await?;

    // --- Step 2: Create a second TIP-20 token on L1 ---
    let zone_usd_salt = B256::new([
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 42,
    ]);
    let l1_zone_usd = l1.create_tip20("ZoneUSD", "zUSD", zone_usd_salt).await?;

    // Mint ZoneUSD to the dev account so we can fund the user
    let mint_amount: u128 = 100_000_000; // 100 ZoneUSD (6 decimals)
    l1.mint_tip20(l1_zone_usd, l1.dev_address(), mint_amount)
        .await?;

    // --- Step 3: Deploy L1 infrastructure and create a zone ---
    let portal_address = l1.deploy_zone().await?;

    // --- Step 4: Start zone node connected to L1 ---
    let zone = ZoneTestNode::start_from_l1(l1.http_url(), l1.ws_url(), portal_address).await?;

    // --- Step 5: Enable ZoneUSD on the portal ---
    // Must happen AFTER zone startup so the zone's L1 subscriber picks up the
    // TokenEnabled event from a live block.
    l1.enable_token_on_portal(portal_address, l1_zone_usd)
        .await?;
    let enable_block = l1.provider().get_block_number().await?;

    // Wait for the zone to finalize past the enableToken block
    zone.wait_for_l2_tempo_finalized(enable_block, L1_TIMEOUT)
        .await?;

    // L2 token address is the same as L1 by design (auto-initialized via TokenEnabled event)
    let l2_zone_usd = l1_zone_usd;

    // --- Step 6: Deposit both tokens (user account) ---
    let mut account = ZoneAccount::from_l1_and_zone(&l1, &zone, portal_address);
    let pathusd_amount: u128 = 1_000_000; // 1 pathUSD
    let zoneusd_amount: u128 = 2_000_000; // 2 ZoneUSD

    // Fund user with both tokens on L1
    l1.fund_user(account.address(), pathusd_amount * 2).await?;
    l1.fund_user_token(l1_zone_usd, account.address(), zoneusd_amount * 2)
        .await?;

    // Deposit pathUSD
    let pathusd_minted = account.deposit(pathusd_amount, L1_TIMEOUT, &zone).await?;
    assert_eq!(
        pathusd_minted,
        U256::from(pathusd_amount),
        "pathUSD minted balance should equal deposit amount"
    );

    // Deposit ZoneUSD
    let zoneusd_minted = account
        .deposit_token(l1_zone_usd, l2_zone_usd, zoneusd_amount, L1_TIMEOUT, &zone)
        .await?;
    assert_eq!(
        zoneusd_minted,
        U256::from(zoneusd_amount),
        "ZoneUSD minted balance should equal deposit amount"
    );

    // --- Step 7: Spawn sequencer and withdraw both tokens ---
    let _sequencer_handle = spawn_sequencer(&l1, &zone, portal_address, l1.dev_signer()).await;
    let withdrawal_timeout = std::time::Duration::from_secs(60);

    // Withdraw pathUSD
    let pathusd_withdrawal: u128 = 500_000; // 0.5 pathUSD
    account.withdraw(pathusd_withdrawal).await?;

    l1.wait_for_withdrawal_on_l1(
        portal_address,
        account.address(),
        pathusd_withdrawal,
        withdrawal_timeout,
    )
    .await?;

    // Withdraw ZoneUSD
    let zoneusd_withdrawal: u128 = 1_000_000; // 1 ZoneUSD
    account
        .withdraw_token(l2_zone_usd, zoneusd_withdrawal)
        .await?;

    l1.wait_for_withdrawal_on_l1_token(
        portal_address,
        l1_zone_usd,
        account.address(),
        zoneusd_withdrawal,
        withdrawal_timeout,
    )
    .await?;

    // --- Step 8: Verify L2 balances decreased ---
    let final_pathusd = zone
        .balance_of(ZONE_TOKEN_ADDRESS, account.address())
        .await?;
    assert!(
        final_pathusd < U256::from(pathusd_amount - pathusd_withdrawal),
        "L2 pathUSD balance should decrease by at least the withdrawal amount (got {final_pathusd})"
    );

    let final_zoneusd = zone.balance_of(l2_zone_usd, account.address()).await?;
    assert!(
        final_zoneusd <= U256::from(zoneusd_amount - zoneusd_withdrawal),
        "L2 ZoneUSD balance should decrease by at least the withdrawal amount (got {final_zoneusd})"
    );

    Ok(())
}

/// Full encrypted deposit + withdrawal flow:
///
///  1. Start L1 dev node, deploy ZoneFactory + create zone.
///  2. Generate sequencer encryption key, start zone with sequencer key.
///  3. Register encryption key on the portal via `setSequencerEncryptionKey`.
///  4. Fund depositor, call `depositEncrypted` on the portal — encrypting
///     (recipient, memo) to the sequencer's public key. The recipient is a
///     known key (mnemonic index 2) so we can withdraw later.
///     The zone processes this automatically: ECIES decrypt → CP proof →
///     AES-GCM verify → mint to recipient. `deposit_encrypted` waits for
///     the L2 balance to confirm the full pipeline succeeded.
///  5. Spawn sequencer tasks, recipient requests withdrawal on L2.
///  6. Wait for batch submission + withdrawal processing on L1.
///
/// ```text
///  L1                                       Zone L2
///   │                                         │
///   │  setSequencerEncryptionKey              │
///   │                                         │
///   │  depositEncrypted ─────────►    │
///   │                                         │
///   │               ECIES decrypt             │
///   │               + CP proof                │
///   │                   │                    │
///   │                   ▼                    │
///   │            advanceTempo                 │
///   │                   │                    │
///   │                   ▼                    │
///   │            CP ✓ + AES decrypt           │
///   │            → mint to recipient         │
///   │                                         │
///   │   ◄──── requestWithdrawal ───── │
///   │   ◄──── submitBatch ────────  │
///   │   processWithdrawal                     │
///   │            → tokens to L1              │
/// ```
///
/// NOTE: Requires `forge build` in `docs/specs/` for ZoneFactory artifact.
#[tokio::test(flavor = "multi_thread")]
async fn test_encrypted_deposit_and_withdrawal() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    // Generate sequencer encryption key (deterministic for test reproducibility)
    use sha2::{Digest, Sha256};
    let enc_key_bytes: [u8; 32] = Sha256::digest(b"test-sequencer-encryption-key-l1-e2e").into();
    let encryption_key = k256::SecretKey::from_slice(&enc_key_bytes).expect("valid key");

    // --- Step 1: Start L1 + deploy zone ---
    let l1 = L1TestNode::start().await?;
    let portal_address = l1.deploy_zone().await?;

    // --- Step 2: Start zone with sequencer key ---
    // Must start the zone BEFORE registering the encryption key, so the zone's
    // genesis anchor captures the current L1 block. The encryption key registration
    // and deposit happen in subsequent L1 blocks that the zone processes naturally.
    let zone = ZoneTestNode::start_from_l1_with_sequencer_key(
        l1.http_url(),
        l1.ws_url(),
        portal_address,
        encryption_key.clone(),
    )
    .await?;

    zone.wait_for_l2_tempo_finalized(0, L1_TIMEOUT).await?;

    // --- Step 3: Register encryption key on portal ---
    // This produces an L1 block that the zone will process via L1Subscriber.
    l1.set_sequencer_encryption_key(portal_address, &encryption_key)
        .await?;

    // --- Step 4: Encrypted deposit to a recipient we control ---
    // Use mnemonic index 2 as the recipient so we have keys for withdrawal.
    let recipient_signer = l1.signer_at(2);
    let recipient = recipient_signer.address();

    let mut depositor = ZoneAccount::from_l1_and_zone(&l1, &zone, portal_address);
    let deposit_amount: u128 = 1_000_000; // 1 pathUSD

    l1.fund_user(depositor.address(), deposit_amount).await?;

    // deposit_encrypted waits for `balance >= deposit_amount` on L2, so success
    // here proves the full ECIES pipeline worked (decrypt → CP verify → AES → mint).
    depositor
        .deposit_encrypted(deposit_amount, recipient, B256::ZERO, L1_TIMEOUT, &zone)
        .await?;

    // --- Step 5: Spawn sequencer + withdraw from the recipient's account on L2 ---
    // Spawn sequencer after deposit to avoid L1 nonce races — the dev signer
    // is used by both fund_user and the sequencer's batch submitter.
    let _sequencer_handle = spawn_sequencer(&l1, &zone, portal_address, l1.dev_signer()).await;

    let mut recipient_account =
        ZoneAccount::with_signer(recipient_signer, &l1, &zone, portal_address);

    let withdrawal_amount: u128 = 500_000; // 0.5 pathUSD
    recipient_account.withdraw(withdrawal_amount).await?;

    // --- Step 6: Wait for the withdrawal to be fully processed on L1 ---
    let withdrawal_timeout = std::time::Duration::from_secs(60);
    l1.wait_for_withdrawal_on_l1(
        portal_address,
        recipient,
        withdrawal_amount,
        withdrawal_timeout,
    )
    .await?;

    Ok(())
}

/// Test that TIP-403 policy operations on L1 work correctly and the zone
/// continues to advance normally after policy changes.
///
///  1. Start L1 dev node, deploy zone.
///  2. Create a blacklist policy on L1.
///  3. Assign it to pathUSD.
///  4. Blacklist a user address.
///  5. Start zone node, verify it advances past the policy blocks.
///  6. Verify the policy state on L1 via the helpers.
///
/// NOTE: Full on-chain TIP-403 enforcement on the zone (blocking transfers)
/// requires the TIP403Registry shim precompile, which is not yet wired.
/// This test validates the L1 infrastructure and that policy changes don't
/// break zone block production.
#[tokio::test(flavor = "multi_thread")]
async fn test_l1_policy_operations_and_zone_advancement() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let l1 = L1TestNode::start().await?;
    let portal_address = l1.deploy_zone().await?;

    // --- Create policy infrastructure on L1 ---
    let policy_id = l1.create_blacklist_policy().await?;

    // Assign the blacklist to pathUSD
    l1.change_transfer_policy_id(PATH_USD_ADDRESS, policy_id)
        .await?;

    // Blacklist the user account
    let blacklisted_user = l1.user_signer().address();
    l1.blacklist_address(policy_id, blacklisted_user).await?;

    // Verify policy state on L1
    let auth_result = l1.is_authorized(policy_id, blacklisted_user).await?;
    assert!(
        !auth_result,
        "blacklisted user should NOT be authorized on L1"
    );

    // Non-blacklisted address should be authorized
    let clean_user = l1.signer_at(3).address();
    let clean_auth = l1.is_authorized(policy_id, clean_user).await?;
    assert!(
        clean_auth,
        "non-blacklisted user should be authorized on L1"
    );

    // --- Start zone and verify it advances past the policy blocks ---
    let zone = ZoneTestNode::start_from_l1(l1.http_url(), l1.ws_url(), portal_address).await?;
    zone.wait_for_l2_tempo_finalized(0, L1_TIMEOUT).await?;

    // Zone should have produced blocks — policy changes on L1 don't break zone
    let zone_block = zone.provider().get_block_number().await?;
    assert!(
        zone_block > 0,
        "zone should have produced blocks after L1 policy changes"
    );

    // Deposit to a non-blacklisted user should still work.
    // Use signer_at(3) — the same `clean_user` verified above — because the default
    // user_signer (index 1) was blacklisted earlier in this test.
    let clean_signer = l1.signer_at(3);
    let mut account = ZoneAccount::with_signer(clean_signer, &l1, &zone, portal_address);
    let deposit_amount: u128 = 1_000_000;
    l1.fund_user(account.address(), deposit_amount).await?;
    let minted = account.deposit(deposit_amount, L1_TIMEOUT, &zone).await?;
    assert_eq!(
        minted,
        U256::from(deposit_amount),
        "deposit should succeed for non-blacklisted user"
    );

    Ok(())
}

/// Test that an encrypted deposit whose decrypted recipient is blacklisted
/// gets redirected to the sender (refund) instead of minting to the recipient.
///
///  1. Start L1 dev node, deploy zone, register encryption key.
///  2. Create a blacklist policy, assign to pathUSD, blacklist the recipient.
///  3. Fund the policy cache so the zone knows about the blacklist.
///  4. Make an encrypted deposit targeting the blacklisted recipient.
///  5. Verify the deposit is refunded to the sender (minted to sender's L2 address).
///
/// NOTE: This test validates the builder-level policy check in `build_encrypted_deposit`.
/// The zone's policy cache must be pre-populated for the check to trigger.
#[tokio::test(flavor = "multi_thread")]
async fn test_encrypted_deposit_blacklisted_recipient() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    // Generate sequencer encryption key
    use sha2::{Digest, Sha256};
    let enc_key_bytes: [u8; 32] =
        Sha256::digest(b"test-sequencer-encryption-key-blacklist-e2e").into();
    let encryption_key = k256::SecretKey::from_slice(&enc_key_bytes).expect("valid key");

    // --- Step 1: Start L1 + deploy zone ---
    let l1 = L1TestNode::start().await?;
    let portal_address = l1.deploy_zone().await?;

    // --- Step 2: Create blacklist policy and blacklist the intended recipient ---
    let policy_id = l1.create_blacklist_policy().await?;
    l1.change_transfer_policy_id(PATH_USD_ADDRESS, policy_id)
        .await?;

    let blacklisted_recipient = l1.signer_at(2).address();
    l1.blacklist_address(policy_id, blacklisted_recipient)
        .await?;

    // Verify on L1
    assert!(
        !l1.is_authorized(policy_id, blacklisted_recipient).await?,
        "recipient should be blacklisted"
    );

    // --- Step 3: Start zone with sequencer key ---
    let zone = ZoneTestNode::start_from_l1_with_sequencer_key(
        l1.http_url(),
        l1.ws_url(),
        portal_address,
        encryption_key.clone(),
    )
    .await?;
    zone.wait_for_l2_tempo_finalized(0, L1_TIMEOUT).await?;

    // --- Step 4: Register encryption key ---
    l1.set_sequencer_encryption_key(portal_address, &encryption_key)
        .await?;

    // --- Step 5: Pre-populate zone's policy cache ---
    // The builder checks PolicyCache during encrypted deposit processing.
    // Since the policy listener may not have caught up yet, seed it manually.
    {
        use tempo_contracts::precompiles::ITIP403Registry::PolicyType;

        let policy_cache = zone.policy_cache();
        // Fetch L1 block number before acquiring the write lock to avoid
        // holding a parking_lot guard across an await point.
        let l1_block = l1.provider().get_block_number().await?;
        let mut cache = policy_cache.write();
        cache.set_token_policy(PATH_USD_ADDRESS, 0, policy_id);
        cache.set_policy_type(policy_id, PolicyType::BLACKLIST);
        cache.set_member(policy_id, blacklisted_recipient, 0, true);
        // Also seed for current and future blocks
        cache.set_token_policy(PATH_USD_ADDRESS, l1_block, policy_id);
        cache.set_member(policy_id, blacklisted_recipient, l1_block, true);
    }

    // --- Step 6: Make an encrypted deposit targeting the blacklisted recipient ---
    let mut depositor = ZoneAccount::from_l1_and_zone(&l1, &zone, portal_address);
    let deposit_amount: u128 = 1_000_000;
    l1.fund_user(depositor.address(), deposit_amount).await?;

    // The encrypted deposit targets `blacklisted_recipient`, but the builder should
    // redirect it to the sender (depositor) because the recipient is blacklisted.
    depositor
        .deposit_encrypted(
            deposit_amount,
            blacklisted_recipient,
            B256::ZERO,
            Duration::from_secs(45),
            &zone,
        )
        .await
        .ok(); // May fail if refund goes to sender instead of recipient

    // Wait a bit for processing
    tokio::time::sleep(Duration::from_secs(5)).await;

    // The blacklisted recipient should NOT have received the deposit
    let recipient_balance = zone
        .balance_of(ZONE_TOKEN_ADDRESS, blacklisted_recipient)
        .await?;

    // The depositor (sender) should have received the refund instead
    let sender_balance = zone
        .balance_of(ZONE_TOKEN_ADDRESS, depositor.address())
        .await?;

    assert_eq!(
        recipient_balance,
        U256::ZERO,
        "Blacklisted recipient should not have received the deposit"
    );
    assert!(
        sender_balance >= U256::from(deposit_amount),
        "Sender should have received the refund (balance: {sender_balance})"
    );

    Ok(())
}

/// Blacklisted sender cannot transfer on the zone.
///
///  1. Start L1 dev node, deploy zone.
///  2. Create a blacklist policy for senders, wrap it in a compound policy
///     (sender=blacklist, recipient=allow-all, mintRecipient=allow-all).
///  3. Assign the compound policy to pathUSD's `transferPolicyId`.
///  4. Blacklist Alice in the sender sub-policy.
///  5. Start zone connected to L1, wait for it to process the policy blocks.
///  6. Deposit pathUSD to Alice (succeeds — mint recipient is allow-all).
///  7. Alice attempts a transfer → rejected at pool level (blacklisted sender).
///
/// NOTE: The T2 hardfork must be active on L1 for compound policies and
/// directional authorization roles to work.
#[tokio::test(flavor = "multi_thread")]
async fn test_blacklisted_sender_transfer_rejected() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    // --- Step 1: Start L1 ---
    let l1 = L1TestNode::start().await?;
    let portal_address = l1.deploy_zone().await?;

    // --- Step 2: Create compound policy and blacklist Alice as sender ---
    let alice_signer = l1.user_signer();
    let alice = alice_signer.address();

    let sender_policy_id = l1.create_blacklist_policy().await?;
    let compound_policy_id = l1.create_compound_policy(sender_policy_id, 1, 1).await?;
    l1.change_transfer_policy_id(PATH_USD_ADDRESS, compound_policy_id)
        .await?;
    l1.blacklist_address(sender_policy_id, alice).await?;

    // Verify on L1: Alice is NOT authorized as sender
    {
        use tempo_contracts::precompiles::ITIP403Registry;
        use tempo_precompiles::TIP403_REGISTRY_ADDRESS;
        let registry = ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, l1.provider());
        let authorized = registry
            .isAuthorizedSender(compound_policy_id, alice)
            .call()
            .await?;
        assert!(
            !authorized,
            "alice should NOT be authorized as sender on L1"
        );
    }

    // --- Step 3: Start zone connected to L1 ---
    let zone = ZoneTestNode::start_from_l1(l1.http_url(), l1.ws_url(), portal_address).await?;
    zone.wait_for_l2_tempo_finalized(0, L1_TIMEOUT).await?;

    // Seed the policy cache so ZoneTip20Token knows pathUSD's policy.
    // The PolicyListener may not have caught up yet, so we seed manually.
    {
        use tempo_contracts::precompiles::ITIP403Registry::PolicyType;

        let l1_block = l1.provider().get_block_number().await?;
        let mut cache = zone.policy_cache().write();
        cache.set_token_policy(PATH_USD_ADDRESS, 0, compound_policy_id);
        cache.set_token_policy(PATH_USD_ADDRESS, l1_block, compound_policy_id);
        cache.set_policy_type(compound_policy_id, PolicyType::COMPOUND);
        cache.set_compound(
            compound_policy_id,
            zone::l1_state::tip403::CompoundData {
                sender_policy_id,
                recipient_policy_id: 1,
                mint_recipient_policy_id: 1,
            },
        );
        cache.set_policy_type(sender_policy_id, PolicyType::BLACKLIST);
        cache.set_member(sender_policy_id, alice, 0, true);
        cache.set_member(sender_policy_id, alice, l1_block, true);
    }

    // --- Step 4: Deposit to Alice via the dev account ---
    // Alice is blacklisted as a sender, so she can't transfer pathUSD on L1
    // herself. The dev account deposits on her behalf (recipient = allow-all).
    let deposit_amount: u128 = 1_000_000; // 1 pathUSD
    {
        use tempo_contracts::precompiles::ITIP20;
        use zone::abi::ZonePortal;

        let dev_provider = l1.dev_provider();
        ITIP20::new(PATH_USD_ADDRESS, &dev_provider)
            .approve(portal_address, U256::MAX)
            .send()
            .await?
            .get_receipt()
            .await?;

        let portal = ZonePortal::new(portal_address, &dev_provider);
        let receipt = portal
            .deposit(PATH_USD_ADDRESS, alice, deposit_amount, B256::ZERO)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "L1 deposit tx failed");
    }

    // Wait for the deposit to be minted on L2
    zone.wait_for_balance(
        ZONE_TOKEN_ADDRESS,
        alice,
        U256::from(deposit_amount),
        L1_TIMEOUT,
    )
    .await?;

    // --- Step 5: Alice attempts a transfer → should be rejected ---
    // The transfer may be rejected at the pool level (send() returns Err) or
    // accepted into a block but reverted (receipt.status() == false). Either
    // outcome proves the blacklist is enforced.
    let bob = Address::with_last_byte(0xBB);

    let alice_provider = alloy::providers::ProviderBuilder::new()
        .wallet(alice_signer)
        .connect_http(zone.http_url().clone());

    let tip20 = tempo_contracts::precompiles::ITIP20::new(ZONE_TOKEN_ADDRESS, &alice_provider);
    let send_result = tip20
        .transfer(bob, U256::from(200_000u128))
        .gas_price(tempo_chainspec::spec::TEMPO_T1_BASE_FEE as u128)
        .gas(500_000)
        .send()
        .await;

    match send_result {
        Err(_) => {
            // Pool-level rejection — blacklist enforced before inclusion.
        }
        Ok(pending) => {
            // Tx was accepted into the pool — wait for it to be included and
            // verify it reverted.
            tokio::time::sleep(Duration::from_secs(3)).await;
            let receipt = pending.get_receipt().await?;
            assert!(
                !receipt.status(),
                "transfer from blacklisted sender should revert, but succeeded"
            );
        }
    }

    // Bob should have zero balance
    let bob_balance = zone.balance_of(ZONE_TOKEN_ADDRESS, bob).await?;
    assert_eq!(bob_balance, U256::ZERO, "bob should have received nothing");

    Ok(())
}
