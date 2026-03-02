//! E2E tests for the TIP-403 policy proxy precompile on the zone.
//!
//! These tests verify that the `ZoneTip403ProxyRegistry` precompile correctly
//! serves authorization queries from the `SharedPolicyCache` and rejects
//! mutating calls. The cache is populated directly in tests (no L1 listener).

use alloy::primitives::{U256, address};
use alloy_provider::ProviderBuilder;
use alloy_signer_local::{MnemonicBuilder, coins_bip39::English};
use tempo_chainspec::spec::TEMPO_T0_BASE_FEE;
use tempo_contracts::precompiles::{ITIP20, ITIP403Registry};
use tempo_precompiles::{PATH_USD_ADDRESS, TIP403_REGISTRY_ADDRESS};
use zone::l1_state::tip403::{CompoundData, PolicyEvent};

use crate::utils::{DEFAULT_TIMEOUT, TEST_MNEMONIC, start_local_zone_with_fixture};

/// Deposit pathUSD to Alice, then transfer a portion to Bob on the zone.
///
/// TIP-20 transfers use the default `transferPolicyId` of 1 (allow all),
/// so they always succeed regardless of the policy cache state.
#[tokio::test(flavor = "multi_thread")]
async fn test_tip20_transfer_on_zone() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    let alice_signer = MnemonicBuilder::<English>::default()
        .phrase(TEST_MNEMONIC)
        .build()?;
    let alice = alice_signer.address();

    let bob = address!("0x0000000000000000000000000000000000000B0B");
    let deposit_amount: u128 = 1_000_000; // 1 pathUSD (6 decimals)

    // Deposit pathUSD to Alice
    let deposit = fixture.make_deposit(PATH_USD_ADDRESS, alice, alice, deposit_amount);
    fixture.inject_deposits(zone.deposit_queue(), vec![deposit]);

    zone.wait_for_balance(
        PATH_USD_ADDRESS,
        alice,
        U256::from(deposit_amount),
        DEFAULT_TIMEOUT,
    )
    .await?;

    // Alice transfers 400,000 to Bob
    let transfer_amount: u128 = 400_000;
    let alice_provider = ProviderBuilder::new()
        .wallet(alice_signer)
        .connect_http(zone.http_url().clone());

    let tip20 = ITIP20::new(PATH_USD_ADDRESS, &alice_provider);
    let pending = tip20
        .transfer(bob, U256::from(transfer_amount))
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(100_000)
        .send()
        .await?;

    // Inject an empty L1 block to trigger block production including the pool tx
    fixture.inject_empty_block(zone.deposit_queue());

    let receipt = pending.get_receipt().await?;
    assert!(receipt.status(), "transfer should succeed");

    // Verify Bob received the transfer
    let bob_balance = zone
        .wait_for_balance(
            PATH_USD_ADDRESS,
            bob,
            U256::from(transfer_amount),
            DEFAULT_TIMEOUT,
        )
        .await?;
    assert_eq!(bob_balance, U256::from(transfer_amount));

    // Alice should have remaining balance minus gas
    let alice_balance = zone.balance_of(PATH_USD_ADDRESS, alice).await?;
    let expected_remaining = deposit_amount - transfer_amount;
    assert!(
        alice_balance <= U256::from(expected_remaining),
        "alice should have at most {expected_remaining} (got {alice_balance})"
    );

    Ok(())
}

/// Whitelist policy: members are authorized, non-members are not (fail-closed).
#[tokio::test(flavor = "multi_thread")]
async fn test_policy_proxy_whitelist_authorization() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    // Inject a few empty L1 blocks so the zone is running
    fixture.inject_empty_blocks(zone.deposit_queue(), 3);
    zone.wait_for_tempo_block_number(3, DEFAULT_TIMEOUT).await?;

    let alice = address!("0x000000000000000000000000000000000000A11C");
    let bob = address!("0x0000000000000000000000000000000000000B0B");

    // Populate the cache: policy 5 = WHITELIST, Alice is a member
    {
        let cache = zone.policy_cache();
        let mut w = cache.write();
        w.set_policy_type(5, ITIP403Registry::PolicyType::WHITELIST);
        w.set_member(5, alice, 1, true);
    }

    let registry = ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, zone.provider());

    // Alice is whitelisted → authorized
    let alice_authorized = registry.isAuthorized(5, alice).call().await?;
    assert!(alice_authorized, "alice should be authorized (whitelisted)");

    // Bob is NOT in whitelist → not authorized (fail-closed)
    let bob_authorized = registry.isAuthorized(5, bob).call().await?;
    assert!(
        !bob_authorized,
        "bob should NOT be authorized (not in whitelist)"
    );

    // Policy 5 should exist
    let exists = registry.policyExists(5).call().await?;
    assert!(exists, "policy 5 should exist");

    // Policy data should return WHITELIST
    let data = registry.policyData(5).call().await?;
    assert_eq!(
        data.policyType,
        ITIP403Registry::PolicyType::WHITELIST,
        "policy 5 should be WHITELIST"
    );

    Ok(())
}

/// Blacklist policy: members are NOT authorized, non-members ARE authorized.
#[tokio::test(flavor = "multi_thread")]
async fn test_policy_proxy_blacklist_authorization() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    fixture.inject_empty_blocks(zone.deposit_queue(), 3);
    zone.wait_for_tempo_block_number(3, DEFAULT_TIMEOUT).await?;

    let alice = address!("0x000000000000000000000000000000000000A11C");
    let bob = address!("0x0000000000000000000000000000000000000B0B");

    // Populate the cache: policy 5 = BLACKLIST, Alice is blacklisted
    {
        let cache = zone.policy_cache();
        let mut w = cache.write();
        w.set_policy_type(5, ITIP403Registry::PolicyType::BLACKLIST);
        w.set_member(5, alice, 1, true);
    }

    let registry = ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, zone.provider());

    // Alice is in blacklist → NOT authorized
    let alice_authorized = registry.isAuthorized(5, alice).call().await?;
    assert!(
        !alice_authorized,
        "alice should NOT be authorized (blacklisted)"
    );

    // Bob is NOT in blacklist → authorized
    let bob_authorized = registry.isAuthorized(5, bob).call().await?;
    assert!(bob_authorized, "bob should be authorized (not blacklisted)");

    Ok(())
}

/// Compound policy: delegates to sub-policies based on role.
#[tokio::test(flavor = "multi_thread")]
async fn test_policy_proxy_compound_policy() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    fixture.inject_empty_blocks(zone.deposit_queue(), 3);
    zone.wait_for_tempo_block_number(3, DEFAULT_TIMEOUT).await?;

    let alice = address!("0x000000000000000000000000000000000000A11C");
    let bob = address!("0x0000000000000000000000000000000000000B0B");

    // Policy 5 = sender whitelist, policy 6 = recipient blacklist
    // Compound policy 10 references them
    {
        let cache = zone.policy_cache();
        let mut w = cache.write();
        w.set_policy_type(5, ITIP403Registry::PolicyType::WHITELIST);
        w.set_member(5, alice, 1, true); // Alice whitelisted as sender
        w.set_policy_type(6, ITIP403Registry::PolicyType::BLACKLIST);
        w.set_member(6, bob, 1, true); // Bob blacklisted as recipient
        w.set_compound(
            10,
            CompoundData {
                sender_policy_id: 5,
                recipient_policy_id: 6,
                mint_recipient_policy_id: 1, // builtin allow
            },
        );
    }

    let registry = ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, zone.provider());

    // Alice is in sender whitelist → authorized as sender
    let alice_sender = registry.isAuthorizedSender(10, alice).call().await?;
    assert!(alice_sender, "alice should be authorized as sender");

    // Bob is in recipient blacklist → NOT authorized as recipient
    let bob_recipient = registry.isAuthorizedRecipient(10, bob).call().await?;
    assert!(
        !bob_recipient,
        "bob should NOT be authorized as recipient (blacklisted)"
    );

    // compoundPolicyData should return the sub-policy IDs
    let compound = registry.compoundPolicyData(10).call().await?;
    assert_eq!(compound.senderPolicyId, 5);
    assert_eq!(compound.recipientPolicyId, 6);
    assert_eq!(compound.mintRecipientPolicyId, 1);

    Ok(())
}

/// Builtin policies: policy 0 = reject all, policy 1 = allow all.
#[tokio::test(flavor = "multi_thread")]
async fn test_policy_proxy_builtin_policies() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    fixture.inject_empty_blocks(zone.deposit_queue(), 3);
    zone.wait_for_tempo_block_number(3, DEFAULT_TIMEOUT).await?;

    let alice = address!("0x000000000000000000000000000000000000A11C");

    let registry = ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, zone.provider());

    // Policy 0 = reject all
    let policy0_auth = registry.isAuthorized(0, alice).call().await?;
    assert!(!policy0_auth, "policy 0 should reject all");

    // Policy 1 = allow all
    let policy1_auth = registry.isAuthorized(1, alice).call().await?;
    assert!(policy1_auth, "policy 1 should allow all");

    // Both should exist
    let exists0 = registry.policyExists(0).call().await?;
    assert!(exists0, "policy 0 should exist (builtin)");
    let exists1 = registry.policyExists(1).call().await?;
    assert!(exists1, "policy 1 should exist (builtin)");

    // Policy 0 = WHITELIST semantics (empty whitelist = reject all)
    let data0 = registry.policyData(0).call().await?;
    assert_eq!(data0.policyType, ITIP403Registry::PolicyType::WHITELIST);

    // Policy 1 = BLACKLIST semantics (empty blacklist = allow all)
    let data1 = registry.policyData(1).call().await?;
    assert_eq!(data1.policyType, ITIP403Registry::PolicyType::BLACKLIST);

    Ok(())
}

/// Mutating calls (e.g. createPolicy) should revert with ReadOnlyRegistry.
#[tokio::test(flavor = "multi_thread")]
async fn test_policy_proxy_reverts_mutating_calls() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    fixture.inject_empty_blocks(zone.deposit_queue(), 3);
    zone.wait_for_tempo_block_number(3, DEFAULT_TIMEOUT).await?;

    let registry = ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, zone.provider());

    // createPolicy should revert
    let result = registry
        .createPolicy(
            address!("0x0000000000000000000000000000000000000001"),
            ITIP403Registry::PolicyType::WHITELIST,
        )
        .call()
        .await;

    assert!(result.is_err(), "createPolicy should revert on zone proxy");

    Ok(())
}

/// Compound policy `isAuthorized` checks BOTH sender AND recipient sub-policies (Transfer role).
#[tokio::test(flavor = "multi_thread")]
async fn test_compound_policy_transfer_role_authorization() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    fixture.inject_empty_blocks(zone.deposit_queue(), 3);
    zone.wait_for_tempo_block_number(3, DEFAULT_TIMEOUT).await?;

    let alice = address!("0x000000000000000000000000000000000000A11C");
    let bob = address!("0x0000000000000000000000000000000000000B0B");
    let carol = address!("0x000000000000000000000000000000000000CA01");

    // Policy 5 = sender whitelist, policy 6 = recipient blacklist
    // Compound policy 10 references them
    {
        let cache = zone.policy_cache();
        let mut w = cache.write();
        w.set_policy_type(5, ITIP403Registry::PolicyType::WHITELIST);
        w.set_member(5, alice, 1, true); // Alice whitelisted as sender
        w.set_policy_type(6, ITIP403Registry::PolicyType::BLACKLIST);
        w.set_member(6, bob, 1, true); // Bob blacklisted as recipient
        w.set_compound(
            10,
            CompoundData {
                sender_policy_id: 5,
                recipient_policy_id: 6,
                mint_recipient_policy_id: 1, // builtin allow
            },
        );
    }

    let registry = ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, zone.provider());

    // Alice: whitelisted as sender + NOT in recipient blacklist → true
    let alice_auth = registry.isAuthorized(10, alice).call().await?;
    assert!(
        alice_auth,
        "alice should be authorized (passes both sender and recipient checks)"
    );

    // Bob: NOT in sender whitelist → false (short-circuits before recipient check)
    let bob_auth = registry.isAuthorized(10, bob).call().await?;
    assert!(
        !bob_auth,
        "bob should NOT be authorized (not in sender whitelist)"
    );

    // Carol: whitelisted as sender AND in recipient blacklist
    {
        let cache = zone.policy_cache();
        let mut w = cache.write();
        w.set_member(5, carol, 1, true); // whitelisted as sender
        w.set_member(6, carol, 1, true); // blacklisted as recipient
    }

    // Carol passes sender check but fails recipient → false
    let carol_auth = registry.isAuthorized(10, carol).call().await?;
    assert!(
        !carol_auth,
        "carol should NOT be authorized (passes sender but fails recipient blacklist)"
    );

    Ok(())
}

/// Applying a sequence of `PolicyEvent`s correctly updates the proxy's responses.
#[tokio::test(flavor = "multi_thread")]
async fn test_policy_type_change_via_events() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    fixture.inject_empty_blocks(zone.deposit_queue(), 3);
    zone.wait_for_tempo_block_number(3, DEFAULT_TIMEOUT).await?;

    let alice = address!("0x000000000000000000000000000000000000A11C");
    let registry = ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, zone.provider());

    // Step 1: Create policy 5 as WHITELIST via event, add Alice
    {
        let cache = zone.policy_cache();
        cache.write().apply_events(
            1,
            &[
                PolicyEvent::PolicyCreated {
                    policy_id: 5,
                    policy_type: ITIP403Registry::PolicyType::WHITELIST,
                },
                PolicyEvent::MembershipChanged {
                    policy_id: 5,
                    account: alice,
                    in_set: true,
                },
            ],
        );
    }

    // Alice should be authorized (whitelisted)
    let authorized = registry.isAuthorized(5, alice).call().await?;
    assert!(authorized, "alice should be authorized (whitelisted)");

    // Step 2: Remove Alice via event at block 2
    {
        let cache = zone.policy_cache();
        cache.write().apply_events(
            2,
            &[PolicyEvent::MembershipChanged {
                policy_id: 5,
                account: alice,
                in_set: false,
            }],
        );
    }

    // Alice should no longer be authorized
    let authorized = registry.isAuthorized(5, alice).call().await?;
    assert!(!authorized, "alice should NOT be authorized after removal");

    // Step 3: Create compound policy 10 via event
    {
        let cache = zone.policy_cache();
        cache.write().apply_events(
            3,
            &[PolicyEvent::CompoundPolicyCreated {
                policy_id: 10,
                sender_policy_id: 5,
                recipient_policy_id: 1, // allow all
                mint_recipient_policy_id: 1,
            }],
        );
    }

    // Compound data should be queryable
    let compound = registry.compoundPolicyData(10).call().await?;
    assert_eq!(compound.senderPolicyId, 5);
    assert_eq!(compound.recipientPolicyId, 1);

    // Policy 10 should exist
    let exists = registry.policyExists(10).call().await?;
    assert!(exists, "compound policy 10 should exist");

    Ok(())
}

/// `TokenPolicyChanged` event updates the token→policy mapping in the cache.
#[tokio::test(flavor = "multi_thread")]
async fn test_token_policy_change_via_events() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    fixture.inject_empty_blocks(zone.deposit_queue(), 3);
    zone.wait_for_tempo_block_number(3, DEFAULT_TIMEOUT).await?;

    let token = address!("0x0000000000000000000000000000000000BEEF01");

    // Apply a token policy change event
    {
        let cache = zone.policy_cache();
        cache.write().apply_events(
            1,
            &[PolicyEvent::TokenPolicyChanged {
                token,
                policy_id: 5,
            }],
        );
    }

    // Verify via cache read that the token policy was set
    {
        let cache = zone.policy_cache();
        let r = cache.read();
        let policy_id = r.get_token_policy(token, u64::MAX);
        assert_eq!(policy_id, Some(5), "token should map to policy 5");
    }

    // Update the token's policy to 7
    {
        let cache = zone.policy_cache();
        cache.write().apply_events(
            2,
            &[PolicyEvent::TokenPolicyChanged {
                token,
                policy_id: 7,
            }],
        );
    }

    // Verify the update took effect
    {
        let cache = zone.policy_cache();
        let r = cache.read();
        let policy_id = r.get_token_policy(token, u64::MAX);
        assert_eq!(policy_id, Some(7), "token should now map to policy 7");
    }

    Ok(())
}
