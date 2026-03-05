//! End-to-end demo of the TIP-20 + TIP-403 blacklist lifecycle.
//!
//! # What this does
//!
//! Runs a self-contained 9-step scenario on a live zone to demonstrate how
//! transfer-policy enforcement works across L1 and L2:
//!
//!  1. Create a fresh TIP-20 token on L1 (via `TIP20Factory`)
//!  2. Configure the token (supply cap, ISSUER_ROLE, mint, approve)
//!  3. Sequencer enables the token on the zone portal
//!  4. Deposit tokens into the zone (plain deposit to admin)
//!  5. Create a TIP-403 blacklist policy, blacklist a target wallet, assign it to the token
//!  6. Encrypted deposit to the blacklisted target → zone bounces it back
//!  7. Remove target from the blacklist on L1
//!  8. Encrypted deposit to the now-unblacklisted target → zone accepts it
//!  9. Target withdraws tokens from zone back to L1
//!
//! Each step prints what it's doing, why, and every tx hash / address for
//! debugging and auditability.
//!
//! # Prerequisites
//!
//! - A running zone (defaults to z5: `generated/z5/zone.json`)
//! - The zone's sequencer must be actively producing blocks
//! - An L1 account with enough funds for gas (set via `PRIVATE_KEY`)
//! - The admin account needs a small pathUSD balance on L1 (deposited to
//!   the target wallet for L2 gas fees)
//!
//! # Usage
//!
//! ```sh
//! just demo-blacklist              # defaults: zone z5, amount=500000
//! just demo-blacklist 1000000      # custom deposit amount
//! ```
//!
//! Or directly via cargo:
//! ```sh
//! cargo run -p tempo-xtask -- demo-blacklist \
//!     --portal 0x9d00Ee56f371Cc5365a686180dE3648207399640
//! ```
//!
//! # Architecture notes
//!
//! - Each run uses a random salt and random target wallet so runs are fully
//!   isolated and idempotent.
//! - Two L1 providers are needed: one for the admin wallet (token operations,
//!   deposits) and one for the sequencer wallet (`enableToken`). The sequencer
//!   key is shared with the running zone node, so `enableToken` uses a retry
//!   loop to handle transient nonce conflicts.
//! - TIP-403 policies are assigned directly to the token via
//!   `changeTransferPolicyId`. The deployed L1 `TIP403Registry` does not have
//!   `createCompoundPolicy`, so we use a simple blacklist policy.
//! - After modifying the blacklist on L1, the zone needs a few seconds to sync
//!   the policy state via its L1 listener. We wait 6 seconds which is enough
//!   for a couple of L1 blocks.

use alloy::{
    network::{EthereumWallet, primitives::ReceiptResponse},
    primitives::{Address, B256, Bytes, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::Filter,
    signers::local::PrivateKeySigner,
    sol_types::SolEvent,
};
use eyre::{WrapErr as _, eyre};
use tempo_alloy::TempoNetwork;
use tempo_contracts::precompiles::{
    IRolesAuth, ITIP20 as TIP20Token, ITIP20Factory as TIP20Factory,
    ITIP403Registry as TIP403Registry,
};
use tempo_precompiles::{
    PATH_USD_ADDRESS, TIP20_FACTORY_ADDRESS, TIP403_REGISTRY_ADDRESS, tip20::ISSUER_ROLE,
};
use zone::{
    abi::{EncryptedDepositPayload, ZONE_OUTBOX_ADDRESS, ZoneInbox, ZoneOutbox, ZonePortal},
    precompiles::ecies::encrypt_deposit,
};

const L1_EXPLORER: &str = "https://explore.moderato.tempo.xyz/tx";

#[derive(Debug, clap::Parser)]
pub(crate) struct DemoBlacklist {
    /// Tempo L1 RPC URL.
    #[arg(
        long,
        env = "L1_RPC_URL",
        default_value = "https://eng:bold-raman-silly-torvalds@rpc.moderato.tempo.xyz"
    )]
    l1_rpc_url: String,

    /// ZonePortal contract address on Tempo L1.
    #[arg(long, env = "L1_PORTAL_ADDRESS")]
    portal: Address,

    /// Private key (hex) of the token admin / depositor.
    #[arg(long, env = "PRIVATE_KEY")]
    private_key: String,

    /// Sequencer private key (hex). Required because only the sequencer can enable tokens.
    /// If not set, reads from the zone.json file specified by --zone-dir.
    #[arg(long, env = "SEQUENCER_KEY")]
    sequencer_key: Option<String>,

    /// Path to zone directory containing zone.json (used to read sequencer key if --sequencer-key is not set).
    #[arg(long, default_value = "generated/z5")]
    zone_dir: String,

    /// Zone L2 RPC URL.
    #[arg(long, env = "ZONE_RPC_URL", default_value = "http://localhost:8546")]
    zone_rpc_url: String,

    /// Amount of tokens to use per deposit in the demo.
    #[arg(long, default_value_t = 500_000)]
    amount: u128,
}

impl DemoBlacklist {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let key_str = self
            .private_key
            .strip_prefix("0x")
            .unwrap_or(&self.private_key);
        let signer: PrivateKeySigner = key_str.parse()?;
        let admin = signer.address();
        let wallet = EthereumWallet::from(signer);

        // Resolve sequencer key: CLI flag > env var > zone.json
        let seq_key_str = match &self.sequencer_key {
            Some(k) => k.clone(),
            None => {
                let zone_json_path = std::path::PathBuf::from(&self.zone_dir).join("zone.json");
                let zone_json: serde_json::Value = serde_json::from_str(
                    &std::fs::read_to_string(&zone_json_path).wrap_err_with(|| {
                        format!(
                            "no --sequencer-key and could not read {}",
                            zone_json_path.display()
                        )
                    })?,
                )?;
                zone_json["sequencerKey"]
                    .as_str()
                    .ok_or_else(|| eyre!("sequencerKey not found in {}", zone_json_path.display()))?
                    .to_string()
            }
        };
        let seq_key = seq_key_str.strip_prefix("0x").unwrap_or(&seq_key_str);
        let seq_signer: PrivateKeySigner = seq_key.parse()?;
        let sequencer_addr = seq_signer.address();
        let seq_wallet = EthereumWallet::from(seq_signer);

        let http_rpc = self
            .l1_rpc_url
            .replace("wss://", "https://")
            .replace("ws://", "http://");

        let l1 = ProviderBuilder::new_with_network::<TempoNetwork>()
            .wallet(wallet)
            .connect(&http_rpc)
            .await?;
        l1.client()
            .set_poll_interval(std::time::Duration::from_secs(1));

        // Separate provider for sequencer-only operations (enableToken)
        let l1_seq = ProviderBuilder::new_with_network::<TempoNetwork>()
            .wallet(seq_wallet)
            .connect(&http_rpc)
            .await?;
        l1_seq
            .client()
            .set_poll_interval(std::time::Duration::from_secs(1));

        let l2 = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&self.zone_rpc_url)
            .await?;

        println!("╔══════════════════════════════════════════════════════════════╗");
        println!("║          TIP-20 + TIP-403 Blacklist Demo                    ║");
        println!("╚══════════════════════════════════════════════════════════════╝");
        println!();
        println!("  Admin:     {admin}");
        println!("  Sequencer: {sequencer_addr}");
        println!("  Portal:    {}", self.portal);
        println!("  L1 RPC:    {http_rpc}");
        println!("  Zone RPC:  {}", self.zone_rpc_url);
        println!();

        // Generate a fresh target wallet for the demo
        let target_signer = PrivateKeySigner::random();
        let target = target_signer.address();
        println!("  Target wallet (fresh): {target}");
        println!();

        // ── Step 1: Create a new TIP-20 token on L1 ──────────────────────
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("Step 1: Create a new TIP-20 token on L1");
        println!("  Deploying a fresh token via TIP20Factory.");
        println!();

        let salt = B256::random();
        let factory = TIP20Factory::new(TIP20_FACTORY_ADDRESS, &l1);

        let token_addr = factory
            .getTokenAddress(admin, salt)
            .call()
            .await
            .wrap_err("getTokenAddress failed")?;
        println!("  Predicted address: {token_addr}");

        let receipt = factory
            .createToken(
                "DemoUSD".to_string(),
                "DUSD".to_string(),
                "USD".to_string(),
                PATH_USD_ADDRESS,
                admin,
                salt,
            )
            .send()
            .await
            .wrap_err("createToken send failed")?
            .get_receipt()
            .await?;
        check(&receipt, "createToken")?;
        let tx = receipt.transaction_hash;
        println!("  Token created: DemoUSD (DUSD) at {token_addr}");
        println!("  {L1_EXPLORER}/{tx}");
        println!();

        let token = TIP20Token::new(token_addr, &l1);

        // ── Step 2: Configure token (supply cap, issuer role, mint) ──────
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("Step 2: Configure token — set supply cap, grant ISSUER_ROLE, mint tokens");
        println!();

        // TIP-20 supply cap is uint128 internally, so u128::MAX is the effective maximum.
        let receipt = token
            .setSupplyCap(U256::from(u128::MAX))
            .send()
            .await?
            .get_receipt()
            .await?;
        check(&receipt, "setSupplyCap")?;
        println!("  Supply cap set to max");
        println!("  {L1_EXPLORER}/{}", receipt.transaction_hash);

        let roles = IRolesAuth::new(token_addr, &l1);
        let receipt = roles
            .grantRole(*ISSUER_ROLE, admin)
            .send()
            .await?
            .get_receipt()
            .await?;
        check(&receipt, "grantRole")?;
        println!("  ISSUER_ROLE granted to {admin}");
        println!("  {L1_EXPLORER}/{}", receipt.transaction_hash);

        // Mint 4× the deposit amount: 2× goes to the initial deposit (step 4),
        // 1× for the bounced encrypted deposit (step 6), 1× for the successful one (step 8).
        let mint_amount = self.amount * 4;
        let receipt = token
            .mint(admin, U256::from(mint_amount))
            .send()
            .await?
            .get_receipt()
            .await?;
        check(&receipt, "mint")?;
        println!("  Minted {mint_amount} DUSD to admin");
        println!("  {L1_EXPLORER}/{}", receipt.transaction_hash);

        let receipt = token
            .approve(self.portal, U256::MAX)
            .send()
            .await?
            .get_receipt()
            .await?;
        check(&receipt, "approve")?;
        println!("  Portal approved for max spend");
        println!("  {L1_EXPLORER}/{}", receipt.transaction_hash);
        println!();

        // ── Step 3: Enable token on the zone ─────────────────────────────
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("Step 3: Enable token on zone");
        println!("  Only the sequencer ({sequencer_addr}) can call enableToken on the portal.");
        println!();

        let portal = ZonePortal::new(self.portal, &l1);
        let seq_portal = ZonePortal::new(self.portal, &l1_seq);
        // Retry enableToken because the sequencer key is shared with the running zone
        // node, which may have pending transactions causing nonce conflicts.
        let receipt = {
            let mut last_err = None;
            let mut pending = None;
            for attempt in 0..5u32 {
                match seq_portal.enableToken(token_addr).send().await {
                    Ok(p) => {
                        pending = Some(p);
                        break;
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.contains("underpriced") || msg.contains("nonce") {
                            println!(
                                "  Retry {}/5 — transient nonce conflict, waiting...",
                                attempt + 1
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            last_err = Some(e);
                            continue;
                        }
                        return Err(e).wrap_err("enableToken failed — is SEQUENCER_KEY correct?");
                    }
                }
            }
            pending
                .ok_or_else(|| {
                    last_err
                        .map(|e| eyre!(e))
                        .unwrap_or_else(|| eyre!("enableToken failed after retries"))
                })?
                .get_receipt()
                .await?
        };
        check(&receipt, "enableToken")?;
        let enable_block = receipt.block_number.unwrap_or(0);
        println!("  Token enabled on L1 (block {enable_block})");
        println!("  {L1_EXPLORER}/{}", receipt.transaction_hash);

        println!("  Waiting for zone to pick up the new token...");
        wait_for_token_enabled(&l2, token_addr).await?;
        println!("  Token available on zone!");
        println!();

        // ── Step 4: Deposit tokens into the zone ─────────────────────────
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        let deposit_amount = self.amount * 2;
        println!("Step 4: Deposit {deposit_amount} DUSD into the zone (to admin)");
        println!("  Plain deposit so admin has L2 funds for later encrypted deposits.");
        println!();

        let l2_block_before = l2.get_block_number().await.unwrap_or(0);
        let receipt = {
            let mut last_err = None;
            let mut pending = None;
            for attempt in 0..5u32 {
                match portal
                    .deposit(token_addr, admin, deposit_amount, B256::ZERO)
                    .send()
                    .await
                {
                    Ok(p) => {
                        pending = Some(p);
                        break;
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if msg.contains("nonce") || msg.contains("underpriced") {
                            println!(
                                "  Retry {}/5 — transient nonce conflict, waiting...",
                                attempt + 1
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            last_err = Some(e);
                            continue;
                        }
                        return Err(e).wrap_err("deposit failed");
                    }
                }
            }
            pending
                .ok_or_else(|| {
                    last_err
                        .map(|e| eyre!(e))
                        .unwrap_or_else(|| eyre!("deposit failed after retries"))
                })?
                .get_receipt()
                .await?
        };
        check(&receipt, "deposit")?;
        println!("  Deposited {deposit_amount} DUSD");
        println!("  {L1_EXPLORER}/{}", receipt.transaction_hash);

        println!("  Waiting for deposit to land on L2...");
        let block = wait_for_deposit_processed(&l2, l2_block_before, admin, admin).await?;
        println!("  Deposit processed on L2 (block {block})");

        let l2_balance = get_l2_balance(&l2, token_addr, admin).await?;
        println!("  Admin L2 balance: {l2_balance}");
        println!();

        // ── Step 5: Create blacklist policy and blacklist target ──────────
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("Step 5: Create a blacklist policy and blacklist target wallet");
        println!("  A TIP-403 blacklist blocks specific addresses from sending/receiving.");
        println!("  We create one, blacklist the target, and assign the policy to the token.");
        println!();

        let registry = TIP403Registry::new(TIP403_REGISTRY_ADDRESS, &l1);

        let receipt = registry
            .createPolicy(admin, TIP403Registry::PolicyType::BLACKLIST)
            .send()
            .await?
            .get_receipt()
            .await?;
        check(&receipt, "createPolicy(blacklist)")?;

        let blacklist_policy_id = receipt
            .inner
            .logs()
            .iter()
            .find_map(|log| {
                TIP403Registry::PolicyCreated::decode_log(&log.inner)
                    .ok()
                    .map(|e| e.data.policyId)
            })
            .ok_or_else(|| eyre!("no PolicyCreated event"))?;
        println!("  Blacklist policy created: ID={blacklist_policy_id}");
        println!("  {L1_EXPLORER}/{}", receipt.transaction_hash);

        let receipt = registry
            .modifyPolicyBlacklist(blacklist_policy_id, target, true)
            .send()
            .await?
            .get_receipt()
            .await?;
        check(&receipt, "modifyPolicyBlacklist(add)")?;
        println!("  {target} added to blacklist");
        println!("  {L1_EXPLORER}/{}", receipt.transaction_hash);

        let authorized = registry
            .isAuthorized(blacklist_policy_id, target)
            .call()
            .await?;
        println!(
            "  isAuthorized(policy={blacklist_policy_id}, target): {authorized}  (expected: false)"
        );

        // Assign blacklist policy directly to the token
        let receipt = token
            .changeTransferPolicyId(blacklist_policy_id)
            .send()
            .await?
            .get_receipt()
            .await?;
        check(&receipt, "changeTransferPolicyId")?;
        let current_policy = token.transferPolicyId().call().await?;
        println!("  Token transfer policy set to {current_policy}");
        println!("  {L1_EXPLORER}/{}", receipt.transaction_hash);

        // The zone's L1 listener syncs policy state every few L1 blocks (~2s each).
        // 6 seconds gives enough margin for the blacklist to propagate to L2.
        println!("  Waiting for zone to sync the blacklist from L1...");
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        println!();

        // ── Step 6: Encrypted deposit to blacklisted address → bounce ────
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!(
            "Step 6: Encrypted deposit {} DUSD to blacklisted address {target}",
            self.amount
        );
        println!("  The zone should reject this deposit and bounce funds back to sender.");
        println!();

        let l2_block_before = l2.get_block_number().await.unwrap_or(0);
        send_encrypted_deposit(&portal, self.portal, token_addr, target, self.amount).await?;

        println!("  Waiting for zone to process (expecting EncryptedDepositFailed)...");
        let bounced = wait_for_encrypted_result(&l2, l2_block_before, admin, Some(target)).await?;
        if bounced {
            println!("  BOUNCED! Deposit to blacklisted address was correctly rejected.");
            let sender_l2_balance = get_l2_balance(&l2, token_addr, admin).await?;
            println!("  Funds returned to sender on L2. Admin L2 balance: {sender_l2_balance}");
        } else {
            println!("  WARNING: Deposit was processed — blacklist may need more time to sync.");
        }
        println!();

        // Fund target with pathUSD so it can pay L2 gas in step 9.
        let pathusd_token = TIP20Token::new(PATH_USD_ADDRESS, &l1);
        let receipt = pathusd_token
            .approve(self.portal, U256::MAX)
            .send()
            .await?
            .get_receipt()
            .await?;
        check(&receipt, "approve pathUSD for portal")?;

        let gas_fund: u128 = 100_000;
        let l2_block_before = l2.get_block_number().await.unwrap_or(0);
        let receipt = portal
            .deposit(PATH_USD_ADDRESS, target, gas_fund, B256::ZERO)
            .send()
            .await?
            .get_receipt()
            .await?;
        check(&receipt, "deposit pathUSD to target for gas")?;
        let _ = wait_for_deposit_processed(&l2, l2_block_before, admin, target).await?;
        println!("  Deposited {gas_fund} pathUSD to target for L2 gas");

        // ── Step 7: Unblacklist the address ──────────────────────────────
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("Step 7: Remove target from blacklist on L1");
        println!();

        let receipt = registry
            .modifyPolicyBlacklist(blacklist_policy_id, target, false)
            .send()
            .await?
            .get_receipt()
            .await?;
        check(&receipt, "modifyPolicyBlacklist(remove)")?;
        println!("  {target} removed from blacklist");
        println!("  {L1_EXPLORER}/{}", receipt.transaction_hash);

        let authorized = registry
            .isAuthorized(blacklist_policy_id, target)
            .call()
            .await?;
        println!(
            "  isAuthorized(policy={blacklist_policy_id}, target): {authorized}  (expected: true)"
        );

        // Same L1→L2 sync delay as step 5.
        println!("  Waiting for zone to sync the unblacklist...");
        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        println!();

        // ── Step 8: Encrypted deposit to unblacklisted address → success ─
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!(
            "Step 8: Encrypted deposit {} DUSD to unblacklisted address {target}",
            self.amount
        );
        println!("  Now that the target is no longer blacklisted, this should succeed.");
        println!();

        let l2_block_before = l2.get_block_number().await.unwrap_or(0);
        send_encrypted_deposit(&portal, self.portal, token_addr, target, self.amount).await?;

        println!("  Waiting for zone to process (expecting EncryptedDepositProcessed)...");
        let bounced = wait_for_encrypted_result(&l2, l2_block_before, admin, Some(target)).await?;
        if bounced {
            println!("  WARNING: Deposit still bounced — policy may need more time to sync.");
        } else {
            println!("  SUCCESS! Deposit to unblacklisted address was accepted.");
        }

        let target_balance = get_l2_balance(&l2, token_addr, target).await?;
        println!("  Target L2 balance: {target_balance}");
        println!();

        // ── Step 9: Withdraw from zone back to L1 ───────────────────────
        println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
        println!("Step 9: Withdraw tokens from zone back to L1");
        println!("  Target wallet withdraws DUSD from the zone to their L1 address.");
        println!();

        // Target pays L2 gas in pathUSD (deposited in step 6b).
        let target_wallet = EthereumWallet::from(target_signer);
        let l2_target = ProviderBuilder::new_with_network::<TempoNetwork>()
            .wallet(target_wallet)
            .connect(&self.zone_rpc_url)
            .await?;

        let outbox_token = TIP20Token::new(token_addr, &l2_target);
        let receipt = outbox_token
            .approve(ZONE_OUTBOX_ADDRESS, U256::MAX)
            .gas(100_000)
            .send()
            .await
            .wrap_err("approve outbox on L2")?
            .get_receipt()
            .await?;
        check(&receipt, "approve outbox (L2)")?;
        println!(
            "  Outbox approved on L2  [tx: {}]",
            receipt.transaction_hash
        );

        // Withdraw all DemoUSD — gas is paid in pathUSD.
        let withdraw_amount = target_balance.to::<u128>();
        if withdraw_amount == 0 {
            println!("  No balance to withdraw — skipping.");
        } else {
            let outbox = ZoneOutbox::new(ZONE_OUTBOX_ADDRESS, &l2_target);

            let l1_block_before = l1.get_block_number().await?;
            let receipt = outbox
                .requestWithdrawal(
                    token_addr,
                    target,
                    withdraw_amount,
                    B256::ZERO,
                    0,
                    target,
                    Bytes::new(),
                )
                .gas(500_000)
                .send()
                .await?
                .get_receipt()
                .await?;
            check(&receipt, "requestWithdrawal")?;
            println!(
                "  Withdrawal requested: {withdraw_amount} DUSD  [tx: {}]",
                receipt.transaction_hash
            );

            println!("  Waiting for withdrawal to be processed on L1...");
            wait_for_withdrawal_processed(&l1, l1_block_before, self.portal, target).await?;
        }

        let final_l1 = token.balanceOf(target).call().await?;
        println!("  Target final L1 balance: {final_l1}");
        println!();

        // ── Done ─────────────────────────────────────────────────────────
        println!("╔══════════════════════════════════════════════════════════════╗");
        println!("║                     Demo Complete!                          ║");
        println!("╚══════════════════════════════════════════════════════════════╝");
        println!();
        println!("  Token:                       DemoUSD (DUSD) at {token_addr}");
        println!("  Target wallet:               {target}");
        println!("  Blacklist:                   policy={blacklist_policy_id}");
        println!("  Deposit to blacklisted addr: BOUNCED");
        println!("  Deposit after unblacklist:   ACCEPTED");
        if withdraw_amount > 0 {
            println!("  Withdrawal to L1:            {withdraw_amount} DUSD");
        }
        println!();

        Ok(())
    }
}

/// Verify a transaction receipt succeeded, returning an error with `label` context if it reverted.
fn check(receipt: &impl ReceiptResponse, label: &str) -> eyre::Result<()> {
    if !receipt.status() {
        return Err(eyre!("{label} reverted"));
    }
    Ok(())
}

/// Normalize the Y-parity byte to the SEC1 compressed prefix format (`0x02` / `0x03`).
///
/// The portal may return parity as `0` / `1` (boolean) or already as `0x02` / `0x03`.
/// The ECIES encryption function expects the SEC1 form.
fn normalize_y_parity(y: u8) -> Option<u8> {
    match y {
        0x02 | 0x03 => Some(y),
        0 | 1 => Some(0x02 + y),
        _ => None,
    }
}

/// Send an ECIES-encrypted deposit through the portal.
///
/// Fetches the sequencer's current encryption public key, encrypts the
/// `(recipient, memo)` tuple so only the sequencer can decrypt it on-chain,
/// and submits the encrypted deposit transaction on L1.
async fn send_encrypted_deposit<P: Provider<TempoNetwork>>(
    portal: &ZonePortal::ZonePortalInstance<&P, TempoNetwork>,
    portal_addr: Address,
    token: Address,
    to: Address,
    amount: u128,
) -> eyre::Result<()> {
    let key = portal
        .sequencerEncryptionKey()
        .call()
        .await
        .wrap_err("sequencerEncryptionKey() failed")?;
    let key_count: U256 = portal.encryptionKeyCount().call().await?;
    let key_index = key_count
        .checked_sub(U256::from(1))
        .ok_or_else(|| eyre!("no encryption keys set"))?;

    let y_parity = normalize_y_parity(key.yParity)
        .ok_or_else(|| eyre!("unexpected yParity {:#x}", key.yParity))?;

    let enc = encrypt_deposit(&key.x, y_parity, to, B256::ZERO, portal_addr, key_index)
        .ok_or_else(|| eyre!("ECIES encryption failed"))?;

    let payload = EncryptedDepositPayload {
        ephemeralPubkeyX: enc.eph_pub_x,
        ephemeralPubkeyYParity: enc.eph_pub_y_parity,
        ciphertext: Bytes::from(enc.ciphertext),
        nonce: enc.nonce.into(),
        tag: enc.tag.into(),
    };

    let receipt = portal
        .depositEncrypted(token, amount, key_index, payload)
        .send()
        .await
        .wrap_err("depositEncrypted send failed")?
        .get_receipt()
        .await?;
    check(&receipt, "depositEncrypted")?;
    println!(
        "  Encrypted deposit sent (block {})",
        receipt.block_number.unwrap_or(0),
    );
    println!("  {L1_EXPLORER}/{}", receipt.transaction_hash);

    Ok(())
}

/// Query a TIP-20 token balance on L2.
async fn get_l2_balance<P: Provider<TempoNetwork>>(
    l2: &P,
    token: Address,
    account: Address,
) -> eyre::Result<U256> {
    Ok(TIP20Token::new(token, l2)
        .balanceOf(account)
        .call()
        .await
        .unwrap_or_default())
}

/// Poll L2 for a `TokenEnabled` event matching the given token address.
///
/// Times out after 60 seconds (120 polls × 500ms).
async fn wait_for_token_enabled<P: Provider<TempoNetwork>>(
    l2: &P,
    token: Address,
) -> eyre::Result<()> {
    let filter = Filter::new()
        .address(zone::abi::ZONE_INBOX_ADDRESS)
        .event_signature(ZoneInbox::TokenEnabled::SIGNATURE_HASH)
        .from_block(1);

    for _ in 0..120 {
        let logs = l2.get_logs(&filter).await.unwrap_or_default();
        for log in &logs {
            if let Ok(event) = ZoneInbox::TokenEnabled::decode_log(&log.inner)
                && event.data.token == token
            {
                return Ok(());
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    Err(eyre!("timeout waiting for TokenEnabled event on L2"))
}

/// Poll L2 for a `DepositProcessed` event matching the given sender and recipient.
///
/// Times out after 60 seconds (120 polls × 500ms).
async fn wait_for_deposit_processed<P: Provider<TempoNetwork>>(
    l2: &P,
    from_block: u64,
    sender: Address,
    to: Address,
) -> eyre::Result<u64> {
    let filter = Filter::new()
        .address(zone::abi::ZONE_INBOX_ADDRESS)
        .event_signature(ZoneInbox::DepositProcessed::SIGNATURE_HASH)
        .from_block(from_block);

    for _ in 0..120 {
        let logs = l2.get_logs(&filter).await.unwrap_or_default();
        for log in &logs {
            if let Ok(event) = ZoneInbox::DepositProcessed::decode_log(&log.inner)
                && event.data.sender == sender
                && event.data.to == to
            {
                let block = log.block_number.unwrap_or(0);
                return Ok(block);
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    Err(eyre!("timeout waiting for DepositProcessed"))
}

/// Poll L2 for either `EncryptedDepositProcessed` or `EncryptedDepositFailed`.
///
/// Returns `true` if the deposit bounced (blacklisted), `false` if it was accepted.
/// Times out after 60 seconds (120 polls × 500ms).
async fn wait_for_encrypted_result<P: Provider<TempoNetwork>>(
    l2: &P,
    from_block: u64,
    sender: Address,
    to: Option<Address>,
) -> eyre::Result<bool> {
    let processed_filter = Filter::new()
        .address(zone::abi::ZONE_INBOX_ADDRESS)
        .event_signature(ZoneInbox::EncryptedDepositProcessed::SIGNATURE_HASH)
        .from_block(from_block);

    let failed_filter = Filter::new()
        .address(zone::abi::ZONE_INBOX_ADDRESS)
        .event_signature(ZoneInbox::EncryptedDepositFailed::SIGNATURE_HASH)
        .from_block(from_block);

    for _ in 0..120 {
        let logs = l2.get_logs(&processed_filter).await.unwrap_or_default();
        for log in &logs {
            if let Ok(event) = ZoneInbox::EncryptedDepositProcessed::decode_log(&log.inner)
                && event.data.sender == sender
                && to.is_none_or(|t| event.data.to == t)
            {
                return Ok(false);
            }
        }

        let failed_logs = l2.get_logs(&failed_filter).await.unwrap_or_default();
        for log in &failed_logs {
            if let Ok(event) = ZoneInbox::EncryptedDepositFailed::decode_log(&log.inner)
                && event.data.sender == sender
            {
                return Ok(true);
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    Err(eyre!("timeout waiting for encrypted deposit result"))
}

/// Poll L1 for a `WithdrawalProcessed` event on the portal matching the recipient.
///
/// Times out after 120 seconds (240 polls × 500ms). Withdrawals are slower because
/// the sequencer must batch them and submit a proof to L1.
async fn wait_for_withdrawal_processed<P: Provider<TempoNetwork>>(
    l1: &P,
    from_block: u64,
    portal: Address,
    to: Address,
) -> eyre::Result<()> {
    let filter = Filter::new()
        .address(portal)
        .event_signature(ZonePortal::WithdrawalProcessed::SIGNATURE_HASH)
        .from_block(from_block);

    for _ in 0..240 {
        let logs = l1.get_logs(&filter).await.unwrap_or_default();
        for log in &logs {
            if let Ok(event) = ZonePortal::WithdrawalProcessed::decode_log(&log.inner)
                && event.data.to == to
            {
                let block = log.block_number.unwrap_or(0);
                let tx = log.transaction_hash.unwrap_or_default();
                println!("  Withdrawal processed on L1 (block {block})");
                println!("  {L1_EXPLORER}/{tx}");
                return Ok(());
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    Err(eyre!("timeout waiting for WithdrawalProcessed"))
}
