use alloy::{
    network::EthereumWallet,
    primitives::{Address, B256, Bytes, U256, keccak256},
    providers::{Provider, ProviderBuilder},
    signers::{Signer, local::PrivateKeySigner},
    sol,
    sol_types::SolValue,
};
use eyre::{WrapErr as _, eyre};
use k256::{AffinePoint, ProjectivePoint, Scalar, elliptic_curve::sec1::ToEncodedPoint};
use std::{path::PathBuf, time::Duration};
use tempo_alloy::TempoNetwork;
use tempo_contracts::precompiles::{
    IRolesAuth, ITIP20 as TIP20Token, ITIP20Factory as TIP20Factory,
};
use tempo_precompiles::{PATH_USD_ADDRESS, TIP20_FACTORY_ADDRESS, tip20::ISSUER_ROLE};
use zone::{
    abi::{
        EncryptedDepositPayload, SwapAndDepositRouterEncryptedCallback, ZONE_OUTBOX_ADDRESS,
        ZoneOutbox, ZonePortal,
    },
    precompiles::ecies::encrypt_deposit,
};

use crate::zone_utils::{
    ROUTER_CALLBACK_GAS_LIMIT, STABLECOIN_DEX_ADDRESS, ZoneMetadata, check, fund_l1_wallet,
    normalize_http_rpc, token_balance, wait_for_balance, wait_for_deposit_processed,
    wait_for_token_enabled, wait_for_withdrawal_processed,
};

const DEMO_PATHUSD_GAS_NET: u128 = 5_000_000;
const DEX_LIQUIDITY_MULTIPLIER: u128 = 3;
const NONCE_CONFLICT_RETRIES: u32 = 5;
const PATHUSD_HEADROOM: u128 = 10_000_000;
const WITHDRAWAL_TX_GAS: u64 = 1_000_000;

sol! {
    #[sol(rpc)]
    contract StablecoinDEX {
        function createPair(address base) external returns (bytes32 key);
        function place(address token, uint128 amount, bool isBid, int16 tick) external returns (uint128 orderId);
        function quoteSwapExactAmountIn(address tokenIn, address tokenOut, uint128 amountIn) external view returns (uint128 amountOut);
    }
}

#[derive(Debug, clap::Parser)]
pub(crate) struct DemoSwapAndDeposit {
    /// Path to the zone directory containing zone.json.
    #[arg(long)]
    zone_dir: PathBuf,

    /// Tempo L1 RPC URL.
    #[arg(
        long,
        env = "L1_RPC_URL",
        default_value = "https://eng:bold-raman-silly-torvalds@rpc.moderato.tempo.xyz"
    )]
    l1_rpc_url: String,

    /// Zone L2 RPC URL.
    #[arg(long, env = "ZONE_RPC_URL", default_value = "http://localhost:8546")]
    zone_rpc_url: String,

    /// Private key (hex) for the operator / demo wallet.
    #[arg(long, env = "PRIVATE_KEY")]
    private_key: String,

    /// Sequencer private key (hex). Needed to enable tokens on the portal and
    /// to encrypt the routed deposit payload.
    #[arg(long, env = "SEQUENCER_KEY")]
    sequencer_key: Option<String>,

    /// SwapAndDepositRouter address. Falls back to zone.json.
    #[arg(long)]
    router: Option<Address>,

    /// Demo swap amount in token base units (6 decimals for the demo tokens).
    #[arg(long, default_value_t = 100_000_000)]
    amount: u128,

    /// Tick used for the seeded DEX liquidity.
    #[arg(long, default_value_t = 0)]
    tick: i16,
}

impl DemoSwapAndDeposit {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let zone_metadata = ZoneMetadata::load(&self.zone_dir)?;
        let portal = zone_metadata.get_required_address("portal")?;
        let router = self
            .router
            .or(zone_metadata.get_optional_address("swapAndDepositRouter")?)
            .ok_or_else(|| {
                eyre!(
                    "swapAndDepositRouter not found in {}. Run `just deploy-router <name>` or pass --router.",
                    self.zone_dir.join("zone.json").display()
                )
            })?;
        let sequencer_key = self
            .sequencer_key
            .or_else(|| zone_metadata.get_optional_string("sequencerKey"))
            .ok_or_else(|| {
                eyre!(
                    "sequencer key missing. Set SEQUENCER_KEY or store sequencerKey in {}.",
                    self.zone_dir.join("zone.json").display()
                )
            })?;

        let operator_signer = parse_private_key(&self.private_key)?;
        let operator = operator_signer.address();
        let sequencer_signer = parse_private_key(&sequencer_key)?;
        let sequencer = sequencer_signer.address();

        let http_rpc = normalize_http_rpc(&self.l1_rpc_url);

        let faucet_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&http_rpc)
            .await?;
        let operator_wallet = EthereumWallet::from(operator_signer);
        let l1 = ProviderBuilder::new_with_network::<TempoNetwork>()
            .wallet(operator_wallet)
            .connect(&http_rpc)
            .await?;
        l1.client()
            .set_poll_interval(std::time::Duration::from_secs(1));

        let sequencer_wallet = EthereumWallet::from(sequencer_signer);
        let l1_seq = ProviderBuilder::new_with_network::<TempoNetwork>()
            .wallet(sequencer_wallet)
            .connect(&http_rpc)
            .await?;
        l1_seq
            .client()
            .set_poll_interval(std::time::Duration::from_secs(1));

        let l2 = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&self.zone_rpc_url)
            .await?;
        let l2_operator = ProviderBuilder::new_with_network::<TempoNetwork>()
            .wallet(EthereumWallet::from(parse_private_key(&self.private_key)?))
            .connect(&self.zone_rpc_url)
            .await?;

        let deposit_fee = ZonePortal::new(portal, &l1)
            .calculateDepositFee()
            .call()
            .await
            .wrap_err("failed to fetch portal deposit fee")?;
        let withdrawal_fee = ZoneOutbox::new(ZONE_OUTBOX_ADDRESS, &l2)
            .calculateWithdrawalFee(ROUTER_CALLBACK_GAS_LIMIT)
            .call()
            .await
            .wrap_err("failed to fetch outbox withdrawal fee")?;
        let dex_liquidity = self
            .amount
            .checked_mul(DEX_LIQUIDITY_MULTIPLIER)
            .ok_or_else(|| eyre!("dex liquidity amount overflow"))?;
        let alpha_gross_deposit = self
            .amount
            .checked_add(withdrawal_fee)
            .and_then(|value| value.checked_add(deposit_fee))
            .ok_or_else(|| eyre!("alpha deposit amount overflow"))?;
        let pathusd_gross_deposit = DEMO_PATHUSD_GAS_NET
            .checked_add(deposit_fee)
            .ok_or_else(|| eyre!("pathUSD deposit amount overflow"))?;

        println!("╔══════════════════════════════════════════════════════════════╗");
        println!("║       Same-Zone Router Swap + Deposit Demo                  ║");
        println!("╚══════════════════════════════════════════════════════════════╝");
        println!();
        println!("  Operator:         {operator}");
        println!("  Sequencer:        {sequencer}");
        println!("  Portal:           {portal}");
        println!("  Router:           {router}");
        println!("  L1 RPC:           {http_rpc}");
        println!("  Zone RPC:         {}", self.zone_rpc_url);
        println!("  Swap amount:      {}", self.amount);
        println!("  DEX tick:         {}", self.tick);
        println!("  Deposit fee:      {deposit_fee}");
        println!("  Withdrawal fee:   {withdrawal_fee}");
        println!();

        println!("Step 1: Fund the operator with pathUSD on L1");
        fund_l1_wallet(&faucet_provider, operator).await?;
        let required_pathusd = dex_liquidity
            .checked_add(pathusd_gross_deposit)
            .and_then(|value| value.checked_add(PATHUSD_HEADROOM))
            .ok_or_else(|| eyre!("required pathUSD amount overflow"))?;
        let l1_balance = wait_for_balance(
            &l1,
            PATH_USD_ADDRESS,
            operator,
            U256::from(required_pathusd),
            "L1 pathUSD",
        )
        .await
        .wrap_err(
            "operator pathUSD balance did not reach the minimum needed for the demo after tempo_fundAddress",
        )?;
        println!("  L1 pathUSD balance after faucet request: {l1_balance}");
        println!();

        println!("Step 2: Create fresh AlphaUSD and BetaUSD demo tokens");
        let alpha = create_demo_token(&l1, operator, "AlphaUSD", "aUSD", B256::random()).await?;
        let beta = create_demo_token(&l1, operator, "BetaUSD", "bUSD", B256::random()).await?;
        println!("  AlphaUSD: {alpha}");
        println!("  BetaUSD:  {beta}");
        println!();

        println!("Step 3: Configure and mint the demo tokens");
        let mint_amount = dex_liquidity
            .checked_add(self.amount)
            .ok_or_else(|| eyre!("mint amount overflow"))?;
        configure_and_mint_demo_token(&l1, operator, alpha, mint_amount).await?;
        configure_and_mint_demo_token(&l1, operator, beta, mint_amount).await?;
        println!("  Minted {mint_amount} units of each token to the operator");
        println!();

        println!("Step 4: Enable both tokens on the zone portal");
        let zone_inbox_from_block = l2.get_block_number().await.unwrap_or(0);
        enable_token_with_retry(&ZonePortal::new(portal, &l1_seq), alpha).await?;
        wait_for_token_enabled(&l2, zone_inbox_from_block, alpha).await?;
        let zone_inbox_from_block = l2.get_block_number().await.unwrap_or(0);
        enable_token_with_retry(&ZonePortal::new(portal, &l1_seq), beta).await?;
        wait_for_token_enabled(&l2, zone_inbox_from_block, beta).await?;
        println!("  Both demo tokens are now available on the zone");
        println!();

        println!("Step 5: Seed StablecoinDEX liquidity for AlphaUSD/BetaUSD swaps");
        seed_dex_liquidity(&l1, alpha, beta, dex_liquidity, self.tick).await?;
        let expected_beta = StablecoinDEX::new(STABLECOIN_DEX_ADDRESS, &l1)
            .quoteSwapExactAmountIn(alpha, beta, self.amount)
            .call()
            .await
            .wrap_err("failed to quote DEX swap")?;
        let expected_beta_net = expected_beta.checked_sub(deposit_fee).ok_or_else(|| {
            eyre!(
                "quoted swap output {expected_beta} does not cover the portal deposit fee {deposit_fee}"
            )
        })?;
        println!(
            "  Seeded liquidity: {dex_liquidity} units at tick {}",
            self.tick
        );
        println!(
            "  Quoted swap output for {} AlphaUSD: {expected_beta} BetaUSD",
            self.amount
        );
        println!("  Expected L2 BetaUSD after portal fee: {expected_beta_net}");
        println!();

        println!("Step 6: Deposit pathUSD for L2 gas and AlphaUSD for the swap");
        let portal_contract = ZonePortal::new(portal, &l1);

        let l2_from_block = l2.get_block_number().await.unwrap_or(0);
        TIP20Token::new(PATH_USD_ADDRESS, &l1)
            .approve(portal, U256::MAX)
            .send_sync()
            .await
            .wrap_err("failed to approve pathUSD for portal")?;
        let receipt = portal_contract
            .deposit(
                PATH_USD_ADDRESS,
                operator,
                pathusd_gross_deposit,
                B256::ZERO,
            )
            .send_sync()
            .await
            .wrap_err("failed to deposit pathUSD to the zone")?;
        check(&receipt, "deposit pathUSD")?;
        wait_for_deposit_processed(&l2, l2_from_block, operator, operator, PATH_USD_ADDRESS)
            .await?;

        let l2_from_block = l2.get_block_number().await.unwrap_or(0);
        TIP20Token::new(alpha, &l1)
            .approve(portal, U256::MAX)
            .send_sync()
            .await
            .wrap_err("failed to approve AlphaUSD for portal")?;
        let receipt = portal_contract
            .deposit(alpha, operator, alpha_gross_deposit, B256::ZERO)
            .send_sync()
            .await
            .wrap_err("failed to deposit AlphaUSD to the zone")?;
        check(&receipt, "deposit AlphaUSD")?;
        wait_for_deposit_processed(&l2, l2_from_block, operator, operator, alpha).await?;

        let alpha_before = token_balance(&l2_operator, alpha, operator)
            .await
            .unwrap_or_default();
        let beta_before = token_balance(&l2_operator, beta, operator)
            .await
            .unwrap_or_default();
        println!("  L2 AlphaUSD balance: {alpha_before}");
        println!("  L2 BetaUSD balance:  {beta_before}");
        println!();

        println!(
            "Step 7: Withdraw AlphaUSD to the router, swap on L1, and deposit BetaUSD back into the zone using an encrypted deposit"
        );
        let receipt = TIP20Token::new(alpha, &l2_operator)
            .approve(ZONE_OUTBOX_ADDRESS, U256::MAX)
            .gas(150_000)
            .send()
            .await
            .wrap_err("failed to submit AlphaUSD approval for the outbox")?
            .get_receipt()
            .await
            .wrap_err("failed to approve AlphaUSD for the outbox")?;
        check(&receipt, "approve AlphaUSD for outbox")?;
        println!(
            "  Outbox approved for AlphaUSD on L2  [tx: {}]",
            receipt.transaction_hash
        );

        let portal_contract_seq = ZonePortal::new(portal, &l1_seq);
        let callback_data = build_encrypted_router_callback(
            &portal_contract_seq,
            portal,
            beta,
            operator,
            B256::ZERO,
            expected_beta,
            &sequencer_key,
        )
        .await?;
        let l1_from_block = l1.get_block_number().await.unwrap_or(0);
        let receipt = ZoneOutbox::new(ZONE_OUTBOX_ADDRESS, &l2_operator)
            .requestWithdrawal(
                alpha,
                router,
                self.amount,
                B256::ZERO,
                ROUTER_CALLBACK_GAS_LIMIT,
                operator,
                callback_data,
                Bytes::new(),
            )
            .gas(WITHDRAWAL_TX_GAS)
            .send()
            .await
            .wrap_err("failed to submit routed withdrawal request on the zone")?
            .get_receipt()
            .await
            .wrap_err("failed to request routed withdrawal on the zone")?;
        check(&receipt, "request routed withdrawal")?;
        println!(
            "  Routed withdrawal requested on L2  [tx: {}]",
            receipt.transaction_hash
        );

        let l2_alpha_after_request = token_balance(&l2_operator, alpha, operator)
            .await
            .unwrap_or_default();
        println!("  AlphaUSD balance immediately after request: {l2_alpha_after_request}");

        let l1_block = wait_for_withdrawal_processed(
            &l1,
            l1_from_block,
            portal,
            router,
            alpha,
            self.amount,
            true,
        )
        .await?;
        println!("  Router callback processed on L1 (block {l1_block})");

        let expected_beta_balance = beta_before + U256::from(expected_beta_net);
        let final_beta = wait_for_balance(
            &l2_operator,
            beta,
            operator,
            expected_beta_balance,
            "BetaUSD",
        )
        .await?;
        let final_alpha = token_balance(&l2_operator, alpha, operator)
            .await
            .unwrap_or_default();
        println!("  Final L2 AlphaUSD balance: {final_alpha}");
        println!("  Final L2 BetaUSD balance:  {final_beta}");
        println!();

        println!("Demo complete!");
        println!("  AlphaUSD:       {alpha}");
        println!("  BetaUSD:        {beta}");
        println!("  Router:         {router}");
        println!("  Portal:         {portal}");
        println!("  L2 withdrawal request tx: {}", receipt.transaction_hash);
        println!("  Beta received:  {}", final_beta - beta_before);

        Ok(())
    }
}

async fn create_demo_token<P: Provider<TempoNetwork>>(
    l1: &P,
    admin: Address,
    name: &str,
    symbol: &str,
    salt: B256,
) -> eyre::Result<Address> {
    let factory = TIP20Factory::new(TIP20_FACTORY_ADDRESS, l1);
    let token = factory
        .getTokenAddress(admin, salt)
        .call()
        .await
        .wrap_err("failed to compute token address")?;
    let receipt = factory
        .createToken(
            name.to_string(),
            symbol.to_string(),
            "USD".to_string(),
            PATH_USD_ADDRESS,
            admin,
            salt,
        )
        .send_sync()
        .await
        .wrap_err("createToken failed")?;
    check(&receipt, "createToken")?;
    Ok(token)
}

async fn configure_and_mint_demo_token<P: Provider<TempoNetwork>>(
    l1: &P,
    admin: Address,
    token: Address,
    mint_amount: u128,
) -> eyre::Result<()> {
    let token_contract = TIP20Token::new(token, l1);
    let receipt = token_contract
        .setSupplyCap(U256::from(u128::MAX))
        .send_sync()
        .await
        .wrap_err("setSupplyCap failed")?;
    check(&receipt, "setSupplyCap")?;

    let receipt = IRolesAuth::new(token, l1)
        .grantRole(*ISSUER_ROLE, admin)
        .send_sync()
        .await
        .wrap_err("grantRole failed")?;
    check(&receipt, "grantRole")?;

    let receipt = token_contract
        .mint(admin, U256::from(mint_amount))
        .send_sync()
        .await
        .wrap_err("mint failed")?;
    check(&receipt, "mint")?;
    Ok(())
}

async fn seed_dex_liquidity<P: Provider<TempoNetwork>>(
    l1: &P,
    alpha: Address,
    beta: Address,
    amount: u128,
    tick: i16,
) -> eyre::Result<()> {
    let dex = StablecoinDEX::new(STABLECOIN_DEX_ADDRESS, l1);

    let receipt = dex
        .createPair(alpha)
        .send_sync()
        .await
        .wrap_err("createPair(alpha) failed")?;
    check(&receipt, "createPair(alpha)")?;

    let receipt = dex
        .createPair(beta)
        .send_sync()
        .await
        .wrap_err("createPair(beta) failed")?;
    check(&receipt, "createPair(beta)")?;

    let receipt = TIP20Token::new(PATH_USD_ADDRESS, l1)
        .approve(STABLECOIN_DEX_ADDRESS, U256::MAX)
        .send_sync()
        .await
        .wrap_err("approve pathUSD for DEX failed")?;
    check(&receipt, "approve pathUSD for DEX")?;

    let receipt = dex
        .place(alpha, amount, true, tick)
        .send_sync()
        .await
        .wrap_err("place AlphaUSD bid failed")?;
    check(&receipt, "place AlphaUSD bid")?;

    let receipt = TIP20Token::new(beta, l1)
        .approve(STABLECOIN_DEX_ADDRESS, U256::MAX)
        .send_sync()
        .await
        .wrap_err("approve BetaUSD for DEX failed")?;
    check(&receipt, "approve BetaUSD for DEX")?;

    let receipt = dex
        .place(beta, amount, false, tick)
        .send_sync()
        .await
        .wrap_err("place BetaUSD ask failed")?;
    check(&receipt, "place BetaUSD ask")?;

    Ok(())
}

async fn enable_token_with_retry<P: Provider<TempoNetwork>>(
    portal: &ZonePortal::ZonePortalInstance<&P, TempoNetwork>,
    token: Address,
) -> eyre::Result<()> {
    let mut last_err = None;

    for attempt in 0..NONCE_CONFLICT_RETRIES {
        match portal.enableToken(token).send().await {
            Ok(pending) => {
                let receipt = pending.get_receipt().await?;
                check(&receipt, "enableToken")?;
                println!(
                    "  Enabled token {token} on L1  [tx: {}]",
                    receipt.transaction_hash
                );
                return Ok(());
            }
            Err(err) => {
                let msg = err.to_string();
                if msg.contains("underpriced") || msg.contains("nonce") {
                    println!(
                        "  Retry {}/{} for enableToken due to nonce conflict...",
                        attempt + 1,
                        NONCE_CONFLICT_RETRIES
                    );
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    last_err = Some(err);
                    continue;
                }
                return Err(err)
                    .wrap_err("enableToken failed — check SEQUENCER_KEY and zone state");
            }
        }
    }

    Err(last_err
        .map(|err| eyre!(err))
        .unwrap_or_else(|| eyre!("enableToken failed after retries")))
}

fn parse_private_key(private_key: &str) -> eyre::Result<PrivateKeySigner> {
    private_key
        .strip_prefix("0x")
        .unwrap_or(private_key)
        .parse()
        .wrap_err("invalid private key")
}

async fn build_encrypted_router_callback<P: Provider<TempoNetwork>>(
    portal: &ZonePortal::ZonePortalInstance<&P, TempoNetwork>,
    portal_address: Address,
    token_out: Address,
    recipient: Address,
    memo: B256,
    min_amount_out: u128,
    sequencer_private_key: &str,
) -> eyre::Result<Bytes> {
    let (key, key_index) =
        ensure_sequencer_encryption_key(portal, portal_address, sequencer_private_key).await?;
    let y_parity = key.normalized_y_parity().ok_or_else(|| {
        eyre!(
            "unexpected yParity {:#x}, expected 0/1 or 0x02/0x03",
            key.yParity
        )
    })?;

    let encrypted =
        encrypt_deposit(&key.x, y_parity, recipient, memo, portal_address, key_index)
            .ok_or_else(|| eyre!("ECIES encryption failed — invalid sequencer public key?"))?;

    let callback = SwapAndDepositRouterEncryptedCallback {
        token_out,
        target_portal: portal_address,
        key_index,
        encrypted: EncryptedDepositPayload {
            ephemeralPubkeyX: encrypted.eph_pub_x,
            ephemeralPubkeyYParity: encrypted.eph_pub_y_parity,
            ciphertext: Bytes::from(encrypted.ciphertext),
            nonce: encrypted.nonce.into(),
            tag: encrypted.tag.into(),
        },
        min_amount_out,
    };

    Ok(Bytes::from(callback.abi_encode()))
}

async fn ensure_sequencer_encryption_key<P: Provider<TempoNetwork>>(
    portal: &ZonePortal::ZonePortalInstance<&P, TempoNetwork>,
    portal_address: Address,
    sequencer_private_key: &str,
) -> eyre::Result<(ZonePortal::sequencerEncryptionKeyReturn, U256)> {
    let (expected_x, expected_y_parity) = derive_encryption_public_key(sequencer_private_key)
        .wrap_err("failed to derive the sequencer encryption public key from SEQUENCER_KEY")?;
    let key_count = portal
        .encryptionKeyCount()
        .call()
        .await
        .wrap_err("failed to read portal encryption key count")?;

    let needs_registration = if key_count == U256::ZERO {
        println!("  Registering the sequencer encryption key on the portal");
        true
    } else {
        let current_key = portal
            .sequencerEncryptionKey()
            .call()
            .await
            .wrap_err("failed to read the active sequencer encryption key")?;
        let current_y_parity = current_key.normalized_y_parity().ok_or_else(|| {
            eyre!(
                "unexpected portal yParity {:#x}, expected 0/1 or 0x02/0x03",
                current_key.yParity
            )
        })?;
        if current_key.x == expected_x && current_y_parity == expected_y_parity {
            false
        } else {
            println!(
                "  Portal encryption key does not match SEQUENCER_KEY; registering the current sequencer key"
            );
            true
        }
    };

    if needs_registration {
        register_sequencer_encryption_key(portal, portal_address, sequencer_private_key).await?;
    }

    portal
        .encryption_key()
        .await
        .wrap_err("failed to fetch the active sequencer encryption key")
}

async fn register_sequencer_encryption_key<P: Provider<TempoNetwork>>(
    portal: &ZonePortal::ZonePortalInstance<&P, TempoNetwork>,
    portal_address: Address,
    sequencer_private_key: &str,
) -> eyre::Result<()> {
    let (x, y_parity) = derive_encryption_public_key(sequencer_private_key)
        .wrap_err("failed to derive the sequencer encryption public key")?;
    let signer = parse_private_key(sequencer_private_key)?;
    let message = keccak256((portal_address, x, U256::from(y_parity)).abi_encode());
    let sig = signer
        .sign_hash(&message)
        .await
        .wrap_err("failed to sign the encryption key proof-of-possession")?;
    let pop_v = sig.v() as u8 + 27;
    let pop_r = B256::from(sig.r().to_be_bytes::<32>());
    let pop_s = B256::from(sig.s().to_be_bytes::<32>());

    let receipt = portal
        .setSequencerEncryptionKey(x, y_parity, pop_v, pop_r, pop_s)
        .send_sync()
        .await
        .wrap_err("failed to send setSequencerEncryptionKey")?;
    check(&receipt, "setSequencerEncryptionKey")?;
    println!(
        "  Sequencer encryption key registered on L1  [tx: {}]",
        receipt.transaction_hash
    );
    Ok(())
}

fn derive_encryption_public_key(sequencer_private_key: &str) -> eyre::Result<(B256, u8)> {
    let key_str = sequencer_private_key
        .strip_prefix("0x")
        .unwrap_or(sequencer_private_key);
    let enc_key = k256::SecretKey::from_slice(&const_hex::decode(key_str)?)?;
    let scalar: Scalar = *enc_key.to_nonzero_scalar();
    let pub_point = AffinePoint::from(ProjectivePoint::GENERATOR * scalar);
    let encoded = pub_point.to_encoded_point(true);
    let x = B256::from_slice(encoded.x().unwrap().as_slice());
    let y_parity = encoded.as_bytes()[0];
    Ok((x, y_parity))
}
