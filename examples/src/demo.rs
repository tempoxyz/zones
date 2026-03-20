use std::time::{Duration, Instant};

use alloy::{
    network::EthereumWallet,
    primitives::{Address, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    chain::{
        DEFAULT_L1_RPC_URL, DEFAULT_TOKEN_ADDRESS, DEFAULT_ZONE_RPC_URL, EncryptedDepositPayload,
        FIXED_DEPOSIT_GAS, TIP20Token, ZonePortal, detect_local_zone_config, normalize_http_rpc,
        parse_private_key,
    },
    crypto::{
        build_recipient_cert, generate_signing_key, now_unix, random_id, route_leaf_hash,
        signing_key_verifying_hex, verify_recipient_cert, verify_resolve_token,
        verify_route_intent,
    },
    model::{
        CompleteRegistrationRequest, EncryptedPayloadResponse, ErrorResponse, LinkRecipientRequest,
        MetaResponse, MintRouteRequest, MintRouteResponse, ResolveRequest, ResolveResponse,
        RouteProof, SETTLEMENT_SERVICE_ID, StartRegistrationRequest, StartRegistrationResponse,
        StatusResponse,
    },
    server::{ServerConfig, SpawnedServer, spawn_server},
};

#[derive(Debug, Clone)]
pub struct HandoffDemoOptions {
    pub base_url: Option<String>,
    pub email: String,
    pub amount: u128,
    pub portal_address: Option<String>,
    pub l1_rpc_url: Option<String>,
    pub zone_rpc_url: String,
    pub token_address: String,
    pub sender_private_key: Option<String>,
    pub skip_faucet: bool,
    pub wait_timeout_secs: u64,
}

impl Default for HandoffDemoOptions {
    fn default() -> Self {
        Self {
            base_url: None,
            email: "user@example.com".to_string(),
            amount: 5_000_000,
            portal_address: None,
            l1_rpc_url: None,
            zone_rpc_url: DEFAULT_ZONE_RPC_URL.to_string(),
            token_address: format!("{DEFAULT_TOKEN_ADDRESS:#x}"),
            sender_private_key: None,
            skip_faucet: false,
            wait_timeout_secs: 45,
        }
    }
}

pub async fn run_handoff_demo(options: HandoffDemoOptions) -> Result<()> {
    let detected = detect_local_zone_config(&options.zone_rpc_url)?;
    let l1_rpc_url = options
        .l1_rpc_url
        .or_else(|| {
            detected
                .as_ref()
                .and_then(|config| config.l1_rpc_url.clone())
        })
        .map(|value| normalize_http_rpc(&value))
        .unwrap_or_else(|| DEFAULT_L1_RPC_URL.to_string());
    let zone_rpc_url = options.zone_rpc_url;
    let token_address: Address = options
        .token_address
        .parse()
        .context("token address is not a valid 0x-prefixed address")?;
    let asset = format!("{token_address:#x}");
    let amount = options.amount;
    let amount_str = amount.to_string();
    let email = options.email;
    let timeout = Duration::from_secs(options.wait_timeout_secs);

    let read_l1 = ProviderBuilder::new().connect_http(l1_rpc_url.parse()?);
    let zone = ProviderBuilder::new().connect_http(zone_rpc_url.parse()?);

    let sender_signer = match options.sender_private_key {
        Some(key) => parse_private_key(&key)?,
        None => PrivateKeySigner::random(),
    };
    let sender_address = sender_signer.address();
    let sender_wallet = EthereumWallet::from(sender_signer);
    let funded_l1 = ProviderBuilder::new()
        .wallet(sender_wallet)
        .connect_http(l1_rpc_url.parse()?);

    // Each run creates a fresh receiver identity plus a fresh hidden zone destination.
    // The email handle only points to a signed commitment, never directly to this address.
    let receiver_identity_key = generate_signing_key();
    let receiver_zone_signer = PrivateKeySigner::random();
    let receiver_zone_address = receiver_zone_signer.address();
    let route_secret = random_id("routeleaf");
    let route_root = route_leaf_hash(&format!("{receiver_zone_address:#x}"), &route_secret);

    let token = TIP20Token::new(token_address, &read_l1);
    let symbol = token
        .symbol()
        .call()
        .await
        .unwrap_or_else(|_| "pathUSD".to_string());

    let (base_url, spawned_server) = connect_handoff_server(
        options.base_url,
        options.portal_address,
        &l1_rpc_url,
        &zone_rpc_url,
        token_address,
        detected.as_ref(),
    )
    .await?;
    let client = reqwest::Client::new();
    let meta: MetaResponse = get_json(&client, &format!("{base_url}/meta")).await?;

    println!("Handoff example");
    println!("Server:    {base_url}");
    println!("L1 RPC:    {l1_rpc_url}");
    println!("Zone RPC:  {zone_rpc_url}");
    println!("Token:     {symbol} ({asset})");
    if let Some(config) = &detected
        && let Some(zone_id) = &config.zone_id
    {
        println!("Zone ID:   {zone_id}");
    }
    println!("Amount:    {amount} {symbol}");

    println!("\n1. Receiver chooses a hidden zone destination");
    println!("   Hidden recipient address: {receiver_zone_address}");

    println!("\n2. Receiver registers the public identifier with Handoff Identity");
    let start_registration: StartRegistrationResponse = post_json(
        &client,
        &format!("{base_url}/identity/register/start"),
        &StartRegistrationRequest {
            email: email.clone(),
            recipient_verifying_key: signing_key_verifying_hex(&receiver_identity_key),
        },
    )
    .await?;
    println!(
        "   Handoff issued recipient id {} and dev verification code {}.",
        start_registration.recipient_id, start_registration.verification_code
    );

    println!("\n3. Receiver signs the route commitment and completes verification");
    let recipient_cert = build_recipient_cert(
        &email,
        &route_root,
        &receiver_identity_key,
        now_unix() + 3600,
        1,
    );
    verify_recipient_cert(&recipient_cert, &email)?;
    let complete_status: StatusResponse = post_json(
        &client,
        &format!("{base_url}/identity/register/complete"),
        &CompleteRegistrationRequest {
            recipient_id: start_registration.recipient_id.clone(),
            verification_code: start_registration.verification_code.clone(),
            cert: recipient_cert,
        },
    )
    .await?;
    println!("   Registration status: {}.", complete_status.status);

    println!("\n4. Receiver links the hidden zone destination with Handoff Settlement");
    let link_status: StatusResponse = post_json(
        &client,
        &format!("{base_url}/settlement/link-recipient"),
        &LinkRecipientRequest {
            recipient_id: start_registration.recipient_id.clone(),
            zone_address: format!("{receiver_zone_address:#x}"),
            route_secret: route_secret.clone(),
        },
    )
    .await?;
    println!("   Link status: {}.", link_status.status);

    println!("\n5. Sender resolves the identifier through Handoff Identity");
    let resolve_response: ResolveResponse = post_json(
        &client,
        &format!("{base_url}/identity/resolve"),
        &ResolveRequest {
            email: email.clone(),
            asset: asset.clone(),
            amount: amount_str.clone(),
        },
    )
    .await?;
    verify_recipient_cert(&resolve_response.cert, &email)?;
    verify_resolve_token(
        &resolve_response.resolve_token,
        &meta.identity_verifying_key,
        &resolve_response.recipient_id,
        &amount_str,
        &asset,
    )?;
    println!("   Sender got a signed recipient cert and resolve token.");

    println!("\n6. Sender asks Handoff Settlement to mint a one-time route");
    let mint_route_response: MintRouteResponse = post_json(
        &client,
        &format!("{base_url}/settlement/mint-route"),
        &MintRouteRequest {
            resolve_token: resolve_response.resolve_token,
        },
    )
    .await?;
    verify_route_response(
        &mint_route_response,
        &meta.settlement_verifying_key,
        &route_root,
        &amount_str,
        &asset,
    )?;
    let portal_address: Address = mint_route_response
        .portal_address
        .parse()
        .context("route response portal address is not valid")?;
    let minted_token_address: Address = mint_route_response
        .token_address
        .parse()
        .context("route response token address is not valid")?;
    ensure!(
        minted_token_address == token_address,
        "route response token {} did not match expected {}",
        minted_token_address,
        token_address
    );
    println!(
        "   Sender received a route intent plus a real encrypted deposit payload for portal {portal_address}."
    );

    println!("\n7. Sender funds an L1 account and approves the portal");
    if options.skip_faucet {
        println!("   Skipping tempo_fundAddress because --skip-faucet was set.");
    } else {
        fund_l1_wallet(&read_l1, sender_address).await?;
        println!("   Faucet requested for sender address {sender_address}.");
    }
    wait_for_tip20_balance(
        &read_l1,
        token_address,
        sender_address,
        U256::from(amount),
        timeout,
    )
    .await?;
    approve_portal(&funded_l1, token_address, portal_address).await?;
    println!("   Sender is ready to submit the encrypted deposit.");

    println!("\n8. Sender submits ZonePortal.depositEncrypted(...)");
    let receiver_balance_before =
        tip20_balance(&zone, token_address, receiver_zone_address).await?;
    let net_amount = expected_zone_credit(&read_l1, portal_address, amount).await?;
    let receipt = pay_route_on_l1(
        &funded_l1,
        portal_address,
        token_address,
        amount,
        mint_route_response
            .key_index
            .parse()
            .context("invalid route key index")?,
        payload_from_response(&mint_route_response.encrypted_payload)?,
    )
    .await?;
    println!(
        "   L1 tx succeeded in block {} with hash {}.",
        receipt.block_number.unwrap_or_default(),
        receipt.transaction_hash
    );

    println!("\n9. Receiver waits for the hidden zone balance to update");
    let receiver_balance = wait_for_tip20_balance(
        &zone,
        token_address,
        receiver_zone_address,
        receiver_balance_before + U256::from(net_amount),
        timeout,
    )
    .await?;
    println!(
        "   Hidden recipient balance is now {} {}.",
        receiver_balance, symbol
    );

    println!(
        "\nSuccess: sender paid {amount} {symbol} to {email} through Handoff's HTTP control plane and a real encrypted ZonePortal deposit without learning the hidden zone destination."
    );

    if let Some(server) = spawned_server {
        server.join_handle.abort();
    }

    Ok(())
}

async fn connect_handoff_server(
    base_url: Option<String>,
    portal_address: Option<String>,
    l1_rpc_url: &str,
    zone_rpc_url: &str,
    token_address: Address,
    detected: Option<&crate::chain::DetectedZoneConfig>,
) -> Result<(String, Option<SpawnedServer>)> {
    if let Some(base_url) = base_url {
        return Ok((base_url.trim_end_matches('/').to_string(), None));
    }

    let portal_address: Address = portal_address
        .or_else(|| detected.map(|config| config.portal_address.clone()))
        .context(
            "missing portal address; pass --portal-address, point demo at --base-url, or run against a detectable local tempo-zone process",
        )?
        .parse()
        .context("portal address is not a valid 0x-prefixed address")?;

    let spawned_server = spawn_server(ServerConfig {
        addr: "127.0.0.1:0"
            .parse()
            .expect("local server bind address is valid"),
        l1_rpc_url: l1_rpc_url.to_string(),
        portal_address,
        token_address,
    })
    .await?;
    println!(
        "   Spawned local Handoff server at {} for zone RPC {}.",
        spawned_server.base_url, zone_rpc_url
    );

    Ok((spawned_server.base_url.clone(), Some(spawned_server)))
}

fn verify_route_response(
    response: &MintRouteResponse,
    settlement_verifying_key: &str,
    route_root: &str,
    amount: &str,
    asset: &str,
) -> Result<()> {
    verify_route_proof(&response.route_proof, route_root)?;
    verify_route_intent(
        &response.route_intent,
        settlement_verifying_key,
        route_root,
        amount,
        asset,
    )?;
    ensure!(
        response.route_intent.settlement_service == SETTLEMENT_SERVICE_ID,
        "unexpected settlement service {}",
        response.route_intent.settlement_service
    );
    Ok(())
}

fn verify_route_proof(route_proof: &RouteProof, route_root: &str) -> Result<()> {
    ensure!(
        route_proof.leaf_hash == route_root,
        "route proof leaf hash does not match the recipient commitment"
    );
    ensure!(
        route_proof.merkle_proof.is_empty(),
        "unexpected merkle proof entries in the single-leaf example"
    );
    Ok(())
}

fn payload_from_response(response: &EncryptedPayloadResponse) -> Result<EncryptedDepositPayload> {
    let nonce: [u8; 12] = decode_fixed_hex(&response.nonce)
        .context("encrypted payload nonce is not a 12-byte hex value")?;
    let tag: [u8; 16] = decode_fixed_hex(&response.tag)
        .context("encrypted payload tag is not a 16-byte hex value")?;

    Ok(EncryptedDepositPayload {
        ephemeralPubkeyX: response
            .ephemeral_pubkey_x
            .parse()
            .context("encrypted payload pubkey is not a valid bytes32")?,
        ephemeralPubkeyYParity: response.ephemeral_pubkey_y_parity,
        ciphertext: decode_hex(&response.ciphertext)
            .context("encrypted payload ciphertext is not valid hex")?
            .into(),
        nonce: nonce.into(),
        tag: tag.into(),
    })
}

async fn get_json<T>(client: &reqwest::Client, url: &str) -> Result<T>
where
    T: DeserializeOwned,
{
    decode_response(client.get(url).send().await?).await
}

async fn post_json<B, T>(client: &reqwest::Client, url: &str, body: &B) -> Result<T>
where
    B: Serialize + ?Sized,
    T: DeserializeOwned,
{
    decode_response(client.post(url).json(body).send().await?).await
}

async fn decode_response<T>(response: reqwest::Response) -> Result<T>
where
    T: DeserializeOwned,
{
    let status = response.status();
    if status.is_success() {
        return response
            .json()
            .await
            .context("response body was not valid JSON");
    }

    let body = response
        .json::<ErrorResponse>()
        .await
        .map(|error| error.error)
        .unwrap_or_else(|_| format!("request failed with status {status}"));
    bail!("{body}");
}

async fn fund_l1_wallet<P: Provider>(provider: &P, address: Address) -> Result<()> {
    let _: Vec<alloy::primitives::B256> = provider
        .raw_request("tempo_fundAddress".into(), (address,))
        .await
        .context("tempo_fundAddress RPC request failed")?;
    Ok(())
}

async fn approve_portal<P: Provider>(
    provider: &P,
    token_address: Address,
    portal_address: Address,
) -> Result<()> {
    let token = TIP20Token::new(token_address, provider);
    let receipt = token
        .approve(portal_address, U256::MAX)
        .send()
        .await
        .context("failed to submit the token approval transaction")?
        .get_receipt()
        .await
        .context("failed to fetch the approval receipt")?;
    ensure!(receipt.status(), "portal approval transaction reverted");
    Ok(())
}

async fn pay_route_on_l1<P: Provider>(
    provider: &P,
    portal_address: Address,
    token_address: Address,
    amount: u128,
    key_index: U256,
    payload: EncryptedDepositPayload,
) -> Result<alloy::rpc::types::TransactionReceipt> {
    let portal = ZonePortal::new(portal_address, provider);
    let receipt = portal
        .depositEncrypted(token_address, amount, key_index, payload)
        .send()
        .await
        .context("failed to submit the encrypted deposit transaction")?
        .get_receipt()
        .await
        .context("failed to fetch the encrypted deposit receipt")?;
    ensure!(receipt.status(), "depositEncrypted transaction reverted");
    Ok(receipt)
}

async fn expected_zone_credit<P: Provider>(
    l1_provider: &P,
    portal_address: Address,
    amount: u128,
) -> Result<u128> {
    let portal = ZonePortal::new(portal_address, l1_provider);
    let zone_gas_rate = portal
        .zoneGasRate()
        .call()
        .await
        .context("failed to read the portal zone gas rate")?;
    let deposit_fee = zone_gas_rate.saturating_mul(FIXED_DEPOSIT_GAS);
    ensure!(
        amount >= deposit_fee,
        "deposit amount {amount} is smaller than the configured deposit fee {deposit_fee}"
    );
    Ok(amount - deposit_fee)
}

async fn tip20_balance<P: Provider>(
    provider: &P,
    token_address: Address,
    account: Address,
) -> Result<U256> {
    TIP20Token::new(token_address, provider)
        .balanceOf(account)
        .call()
        .await
        .context("balanceOf failed")
}

async fn wait_for_tip20_balance<P: Provider>(
    provider: &P,
    token_address: Address,
    account: Address,
    min_balance: U256,
    timeout: Duration,
) -> Result<U256> {
    let started = Instant::now();
    loop {
        let balance = tip20_balance(provider, token_address, account).await?;
        if balance >= min_balance {
            return Ok(balance);
        }
        if started.elapsed() >= timeout {
            bail!(
                "timed out waiting for {} balance to reach at least {}",
                account,
                min_balance
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn decode_hex(value: &str) -> Result<Vec<u8>> {
    let trimmed = value.trim().strip_prefix("0x").unwrap_or(value.trim());
    hex::decode(trimmed).context("value is not valid hex")
}

fn decode_fixed_hex<const N: usize>(value: &str) -> Result<[u8; N]> {
    let bytes = decode_hex(value)?;
    let len = bytes.len();
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected {N} bytes but decoded {len} bytes"))
}
