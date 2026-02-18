//! Multi-asset zone deposit and withdrawal.

use alloy::primitives::{B256, U256};
use crate::utils::{L1TestNode, ZoneAccount, ZoneTestNode};

/// Longer timeout for real L1 tests.
const L1_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Multi-asset deposit + withdrawal:
///
/// 1. Start L1 dev node.
/// 2. Create AlphaUSD and BetaUSD on L1.
/// 3. Deploy zone, enable both tokens.
/// 4. Start zone node, create both tokens on L2.
/// 5. Alice deposits AlphaUSD and BetaUSD.
/// 6. Spawn sequencer, withdraw both tokens.
/// 7. Verify L1 withdrawals and L2 balance decreases.
///
/// ```text
///  L1 (AlphaUSD + BetaUSD)            Zone L2
///   |--- deposit AlphaUSD ----------->|  ✓ AlphaUSD minted
///   |--- deposit BetaUSD ------------>|  ✓ BetaUSD minted
///   |<-- withdraw AlphaUSD -----------|  ✓ AlphaUSD burned
///   |<-- withdraw BetaUSD ------------|  ✓ BetaUSD burned
/// ```
///
/// NOTE: Requires `forge build` in `docs/specs/` for ZoneFactory artifact.
#[tokio::test(flavor = "multi_thread")]
async fn test_multiasset_deposit_and_withdraw() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    // --- Step 1: Start L1 ---
    let l1 = L1TestNode::start().await?;

    // --- Step 2: Create two TIP-20 tokens on L1 ---
    let alpha_salt = B256::with_last_byte(100);
    let beta_salt = B256::with_last_byte(101);

    let l1_alpha = l1.create_tip20("AlphaUSD", "aUSD", alpha_salt).await?;
    let l1_beta = l1.create_tip20("BetaUSD", "bUSD", beta_salt).await?;

    let mint_amount: u128 = 100_000_000; // 100 tokens (6 decimals)
    l1.mint_tip20(l1_alpha, l1.dev_address(), mint_amount)
        .await?;
    l1.mint_tip20(l1_beta, l1.dev_address(), mint_amount)
        .await?;

    // --- Step 3: Deploy zone, enable both tokens ---
    let portal_address = l1.deploy_zone().await?;
    l1.enable_token_on_portal(portal_address, l1_alpha).await?;
    l1.enable_token_on_portal(portal_address, l1_beta).await?;

    // --- Step 4: Start zone node, create tokens on L2 ---
    let zone = ZoneTestNode::start_from_l1(l1.http_url(), l1.ws_url(), portal_address).await?;
    zone.wait_for_l2_tempo_finalized(0, L1_TIMEOUT).await?;

    // Fund dev account on L2 for token creation gas
    let dev_l2_gas: u128 = 10_000_000;
    l1.fund_dev_l2_gas(portal_address, &zone, dev_l2_gas, L1_TIMEOUT)
        .await?;

    let l2_alpha = zone.create_l2_token("AlphaUSD", "aUSD", alpha_salt).await?;
    let l2_beta = zone.create_l2_token("BetaUSD", "bUSD", beta_salt).await?;
    assert_eq!(l1_alpha, l2_alpha, "AlphaUSD L1/L2 address mismatch");
    assert_eq!(l1_beta, l2_beta, "BetaUSD L1/L2 address mismatch");

    // --- Step 5: Alice deposits both tokens ---
    let mut account = ZoneAccount::from_l1_and_zone(&l1, &zone, portal_address);
    let alpha_deposit: u128 = 1_000_000; // 1 AlphaUSD
    let beta_deposit: u128 = 2_000_000; // 2 BetaUSD

    // Fund Alice on L1
    l1.fund_user(account.address(), 10_000_000).await?; // pathUSD for gas
    l1.fund_user_token(l1_alpha, account.address(), alpha_deposit * 2)
        .await?;
    l1.fund_user_token(l1_beta, account.address(), beta_deposit * 2)
        .await?;

    // Deposit pathUSD for L2 gas
    account.deposit(5_000_000, L1_TIMEOUT, &zone).await?;

    // Deposit AlphaUSD
    let alpha_minted = account
        .deposit_token(l1_alpha, l2_alpha, alpha_deposit, L1_TIMEOUT, &zone)
        .await?;
    assert_eq!(
        alpha_minted,
        U256::from(alpha_deposit),
        "AlphaUSD minted should equal deposit"
    );

    // Deposit BetaUSD
    let beta_minted = account
        .deposit_token(l1_beta, l2_beta, beta_deposit, L1_TIMEOUT, &zone)
        .await?;
    assert_eq!(
        beta_minted,
        U256::from(beta_deposit),
        "BetaUSD minted should equal deposit"
    );

    // --- Step 6: Spawn zone sequencer (batch submitter + withdrawal processor) ---
    let _sequencer_handle = account.spawn_sequencer(&l1, &zone, l1.dev_signer()).await;
    let withdrawal_timeout = std::time::Duration::from_secs(60);

    // Withdraw AlphaUSD
    let alpha_withdrawal: u128 = 500_000; // 0.5 AlphaUSD
    account.withdraw_token(l2_alpha, alpha_withdrawal).await?;

    l1.wait_for_withdrawal_on_l1_token(
        portal_address,
        l1_alpha,
        account.address(),
        alpha_withdrawal,
        withdrawal_timeout,
    )
    .await?;

    // Withdraw BetaUSD
    let beta_withdrawal: u128 = 1_000_000; // 1 BetaUSD
    account.withdraw_token(l2_beta, beta_withdrawal).await?;

    l1.wait_for_withdrawal_on_l1_token(
        portal_address,
        l1_beta,
        account.address(),
        beta_withdrawal,
        withdrawal_timeout,
    )
    .await?;

    // --- Step 7: Verify L2 balances decreased ---
    let final_alpha = zone.balance_of(l2_alpha, account.address()).await?;
    assert!(
        final_alpha <= U256::from(alpha_deposit - alpha_withdrawal),
        "L2 AlphaUSD should decrease after withdrawal (got {final_alpha})"
    );

    let final_beta = zone.balance_of(l2_beta, account.address()).await?;
    assert!(
        final_beta <= U256::from(beta_deposit - beta_withdrawal),
        "L2 BetaUSD should decrease after withdrawal (got {final_beta})"
    );

    Ok(())
}
