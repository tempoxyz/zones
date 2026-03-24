//! End-to-end tests for the private zone RPC server.
//!
//! These tests launch a zone node with a private RPC server and verify:
//! - Authentication enforcement (missing/invalid tokens, wrong chain ID)
//! - Public method access (both sequencer and regular users)
//! - Balance & state privacy (users only see their own data)
//! - Block redaction (logsBloom zeroed, transactions cleared for non-sequencers)
//! - Method tier enforcement (restricted/disabled/unknown methods)

use crate::utils::{
    DEFAULT_TIMEOUT, TEST_MNEMONIC, ZoneAccount, now_secs, start_zone_with_private_rpc,
    start_zone_with_private_rpc_l1, start_zone_with_private_rpc_l1_with_encryption,
};
use alloy::{
    primitives::{Address, B256, U256, address, hex},
    signers::local::PrivateKeySigner,
};
use alloy_provider::ProviderBuilder;
use alloy_signer_local::{MnemonicBuilder, coins_bip39::English};
use alloy_sol_types::SolCall;
use futures::{SinkExt, StreamExt};
use p256::ecdsa::SigningKey as P256SigningKey;
use rand::thread_rng;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};
use tempo_chainspec::spec::TEMPO_T0_BASE_FEE;
use tempo_contracts::precompiles::{
    ITIP20 as ContractTip20,
    account_keychain::IAccountKeychain::SignatureType as KeyInfoSignatureType,
};
use tempo_precompiles::{PATH_USD_ADDRESS, tip20::ITIP20 as PrecompileTip20};
use tokio::time::sleep;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};

fn corrupt_token_hex(token: &str) -> String {
    let mut bytes = hex::decode(token).expect("token hex should decode");
    let idx = usize::from(bytes.len() > 1);
    bytes[idx] ^= 0x01;
    hex::encode(bytes)
}

fn assert_filter_not_found_error(response: &serde_json::Value) {
    let error = response
        .get("error")
        .unwrap_or_else(|| panic!("expected filter-not-found error, got {response}"));
    assert_eq!(
        error["code"].as_i64().unwrap(),
        -32602,
        "filter-not-found should surface as invalid params",
    );
    assert_eq!(
        error["message"].as_str().unwrap(),
        "filter not found",
        "filter-not-found message should be stable",
    );
}

type PrivateRpcWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn private_rpc_ws_url(http_url: &url::Url) -> eyre::Result<url::Url> {
    let mut ws_url = http_url.clone();
    let target_scheme = if ws_url.scheme() == "https" {
        "wss"
    } else {
        "ws"
    };
    ws_url
        .set_scheme(target_scheme)
        .map_err(|_| eyre::eyre!("failed to derive websocket URL"))?;
    Ok(ws_url)
}

fn jsonrpc_with_params(method: &str, params: Value, id: u64) -> String {
    json!({"jsonrpc":"2.0","method":method,"params":params,"id":id}).to_string()
}

async fn connect_private_rpc_ws(url: &url::Url, auth_token: &str) -> eyre::Result<PrivateRpcWs> {
    let ws_url = private_rpc_ws_url(url)?;
    let mut req = ws_url.as_str().into_client_request()?;
    req.headers_mut().insert(
        "x-authorization-token",
        auth_token
            .parse()
            .expect("auth token header should be valid"),
    );
    let (ws, _) = connect_async(req).await?;
    Ok(ws)
}

async fn ws_next_json(ws: &mut PrivateRpcWs) -> eyre::Result<Value> {
    let Some(msg) = tokio::time::timeout(DEFAULT_TIMEOUT, ws.next())
        .await
        .map_err(|_| eyre::eyre!("timed out waiting for websocket message"))?
    else {
        eyre::bail!("websocket closed unexpectedly");
    };

    match msg? {
        Message::Text(text) => Ok(serde_json::from_str(&text)?),
        other => eyre::bail!("expected text websocket message, got {other:?}"),
    }
}

async fn ws_subscribe(ws: &mut PrivateRpcWs, params: Value) -> eyre::Result<String> {
    ws.send(Message::Text(
        jsonrpc_with_params("eth_subscribe", params, 1).into(),
    ))
    .await?;
    let response = ws_next_json(ws).await?;
    Ok(response["result"]
        .as_str()
        .expect("subscription response should include an id")
        .to_owned())
}

async fn ws_expect_no_message(ws: &mut PrivateRpcWs, duration: Duration) -> eyre::Result<()> {
    match tokio::time::timeout(duration, ws.next()).await {
        Err(_) => Ok(()),
        Ok(Some(Ok(Message::Close(_)))) | Ok(None) => Ok(()),
        Ok(Some(Ok(Message::Text(text)))) => {
            eyre::bail!("unexpected websocket message: {text}")
        }
        Ok(Some(Ok(other))) => eyre::bail!("unexpected websocket frame: {other:?}"),
        Ok(Some(Err(err))) => Err(err.into()),
    }
}

async fn ws_collect_messages_until_quiet(
    ws: &mut PrivateRpcWs,
    duration: Duration,
) -> eyre::Result<Vec<Value>> {
    let mut messages = Vec::new();

    loop {
        match tokio::time::timeout(duration, ws.next()).await {
            Err(_) => return Ok(messages),
            Ok(Some(Ok(Message::Close(_)))) | Ok(None) => return Ok(messages),
            Ok(Some(Ok(Message::Text(text)))) => messages.push(serde_json::from_str(&text)?),
            Ok(Some(Ok(other))) => eyre::bail!("unexpected websocket frame: {other:?}"),
            Ok(Some(Err(err))) => return Err(err.into()),
        }
    }
}

/// Auth enforcement: missing header → 401, garbage token → 401/403, wrong chain ID → 403.
#[tokio::test(flavor = "multi_thread")]
async fn test_auth_rejection() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let ctx = start_zone_with_private_rpc().await?;

    // No auth header → 401
    let (status, _) = ctx
        .call_no_auth("eth_blockNumber", serde_json::json!([]))
        .await?;
    assert_eq!(status.as_u16(), 401, "missing auth should return 401");

    // Garbage token → 401 or 403
    let (status, _) = ctx
        .call_raw("eth_blockNumber", serde_json::json!([]), "deadbeef")
        .await?;
    assert!(
        status.as_u16() == 401 || status.as_u16() == 403,
        "invalid auth should return 401 or 403, got {status}"
    );

    // Valid signature but wrong chain ID → 403
    let bad_token = ctx.build_bad_token(
        &ctx.sequencer_signer,
        0,
        ctx.config.chain_id + 1,
        Address::ZERO,
    );
    let (status, _) = ctx
        .call_raw("eth_blockNumber", serde_json::json!([]), &bad_token)
        .await?;
    assert_eq!(status.as_u16(), 403, "wrong chain ID should return 403");

    Ok(())
}

/// Real P256 and WebAuthn auth tokens are accepted by the private RPC.
#[tokio::test(flavor = "multi_thread")]
async fn test_non_secp_auth_tokens() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let ctx = start_zone_with_private_rpc().await?;
    let p256_signer = P256SigningKey::random(&mut thread_rng());
    let webauthn_signer = P256SigningKey::random(&mut thread_rng());

    for token in [
        ctx.p256_token(&p256_signer),
        ctx.webauthn_token(&webauthn_signer),
    ] {
        let resp = ctx
            .call("eth_blockNumber", serde_json::json!([]), &token)
            .await?;
        assert!(
            resp.get("error").is_none(),
            "auth token should succeed: {resp}"
        );
        assert!(
            resp["result"].as_str().is_some(),
            "expected block number result"
        );
    }

    Ok(())
}

/// Invalid P256 signatures and WebAuthn challenge mismatches are rejected.
#[tokio::test(flavor = "multi_thread")]
async fn test_invalid_non_secp_auth_tokens_are_rejected() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let ctx = start_zone_with_private_rpc().await?;
    let p256_signer = P256SigningKey::random(&mut thread_rng());
    let webauthn_signer = P256SigningKey::random(&mut thread_rng());

    let bad_p256 = corrupt_token_hex(&ctx.p256_token(&p256_signer));
    let (status, _) = ctx
        .call_raw("eth_blockNumber", serde_json::json!([]), &bad_p256)
        .await?;
    assert_eq!(status.as_u16(), 403, "invalid P256 token should return 403");

    let bad_webauthn = ctx.webauthn_token_with_challenge(&webauthn_signer, B256::repeat_byte(0x77));
    let (status, _) = ctx
        .call_raw("eth_blockNumber", serde_json::json!([]), &bad_webauthn)
        .await?;
    assert_eq!(
        status.as_u16(),
        403,
        "WebAuthn token with wrong challenge should return 403",
    );

    Ok(())
}

/// Authorized P256 keychain tokens authenticate as the root account in both V1 and V2 encodings.
#[tokio::test(flavor = "multi_thread")]
async fn test_keychain_auth_tokens_v1_and_v2() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut ctx = start_zone_with_private_rpc().await?;
    let root_signer = PrivateKeySigner::random();
    let access_signer = P256SigningKey::random(&mut thread_rng());

    ctx.inject_deposit(
        PATH_USD_ADDRESS,
        address!("0x0000000000000000000000000000000000001111"),
        root_signer.address(),
        1_000_000,
    )
    .await?;

    let (_, key_id) = ctx.keychain_p256_token(root_signer.address(), &access_signer, 0x04);
    ctx.authorize_keychain_key(
        &root_signer,
        key_id,
        KeyInfoSignatureType::P256,
        now_secs() + 300,
    )
    .await?;

    for version in [0x03, 0x04] {
        let (token, _) = ctx.keychain_p256_token(root_signer.address(), &access_signer, version);
        let resp = ctx
            .call(
                "eth_call",
                serde_json::json!([
                    {
                        "from": format!("{:#x}", root_signer.address()),
                        "to": format!("{:#x}", root_signer.address()),
                        "input": "0x"
                    },
                    "latest"
                ]),
                &token,
            )
            .await?;
        assert_eq!(
            resp["result"].as_str().unwrap(),
            "0x",
            "keychain auth should allow calls from the root account",
        );
    }

    Ok(())
}

/// Keychain auth rejects missing, revoked, expired, and signature-type-mismatched keys.
#[tokio::test(flavor = "multi_thread")]
async fn test_keychain_auth_rejection_cases() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut ctx = start_zone_with_private_rpc().await?;

    let missing_root = PrivateKeySigner::random();
    let missing_access = P256SigningKey::random(&mut thread_rng());
    let (missing_token, _) = ctx.keychain_p256_token(missing_root.address(), &missing_access, 0x04);
    let (status, _) = ctx
        .call_raw("eth_blockNumber", serde_json::json!([]), &missing_token)
        .await?;
    assert_eq!(
        status.as_u16(),
        403,
        "missing keychain auth should return 403"
    );

    let revoked_root = PrivateKeySigner::random();
    let revoked_access = P256SigningKey::random(&mut thread_rng());
    ctx.inject_deposit(
        PATH_USD_ADDRESS,
        address!("0x0000000000000000000000000000000000002222"),
        revoked_root.address(),
        1_000_000,
    )
    .await?;
    let (revoked_token, revoked_key_id) =
        ctx.keychain_p256_token(revoked_root.address(), &revoked_access, 0x04);
    ctx.authorize_keychain_key(
        &revoked_root,
        revoked_key_id,
        KeyInfoSignatureType::P256,
        now_secs() + 300,
    )
    .await?;
    ctx.revoke_keychain_key(&revoked_root, revoked_key_id)
        .await?;
    let (status, _) = ctx
        .call_raw("eth_blockNumber", serde_json::json!([]), &revoked_token)
        .await?;
    assert_eq!(status.as_u16(), 403, "revoked key should return 403");

    let expired_root = PrivateKeySigner::random();
    let expired_access = P256SigningKey::random(&mut thread_rng());
    ctx.inject_deposit(
        PATH_USD_ADDRESS,
        address!("0x0000000000000000000000000000000000003333"),
        expired_root.address(),
        1_000_000,
    )
    .await?;
    let (expired_token, expired_key_id) =
        ctx.keychain_p256_token(expired_root.address(), &expired_access, 0x04);
    ctx.authorize_keychain_key(
        &expired_root,
        expired_key_id,
        KeyInfoSignatureType::P256,
        now_secs() + 1,
    )
    .await?;
    sleep(std::time::Duration::from_secs(2)).await;
    let (status, _) = ctx
        .call_raw("eth_blockNumber", serde_json::json!([]), &expired_token)
        .await?;
    assert_eq!(status.as_u16(), 403, "expired key should return 403");

    let mismatch_root = PrivateKeySigner::random();
    let mismatch_access = P256SigningKey::random(&mut thread_rng());
    ctx.inject_deposit(
        PATH_USD_ADDRESS,
        address!("0x0000000000000000000000000000000000004444"),
        mismatch_root.address(),
        1_000_000,
    )
    .await?;
    let (mismatch_token, mismatch_key_id) =
        ctx.keychain_p256_token(mismatch_root.address(), &mismatch_access, 0x04);
    ctx.authorize_keychain_key(
        &mismatch_root,
        mismatch_key_id,
        KeyInfoSignatureType::Secp256k1,
        now_secs() + 300,
    )
    .await?;
    let (status, _) = ctx
        .call_raw("eth_blockNumber", serde_json::json!([]), &mismatch_token)
        .await?;
    assert_eq!(
        status.as_u16(),
        403,
        "signature-type mismatch should return 403",
    );

    Ok(())
}

/// Public methods (blockNumber, chainId, gasPrice) work for both sequencer and users.
#[tokio::test(flavor = "multi_thread")]
async fn test_public_methods() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let ctx = start_zone_with_private_rpc().await?;
    let user_signer = PrivateKeySigner::random();

    for method in ["eth_blockNumber", "eth_chainId", "eth_gasPrice"] {
        let seq_resp = ctx.call_as_sequencer(method, serde_json::json!([])).await?;
        assert!(
            seq_resp.get("result").is_some() && seq_resp.get("error").is_none(),
            "sequencer should succeed for {method}"
        );

        let user_resp = ctx
            .call_as_user(method, serde_json::json!([]), &user_signer)
            .await?;
        assert!(
            user_resp.get("result").is_some() && user_resp.get("error").is_none(),
            "user should succeed for {method}"
        );
    }

    Ok(())
}

/// Filter ownership is scoped to the creating account, and uninstall removes follow-up access.
#[tokio::test(flavor = "multi_thread")]
async fn test_filter_ownership_and_uninstall_cleanup() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut ctx = start_zone_with_private_rpc().await?;
    let owner_signer = PrivateKeySigner::random();
    let other_signer = PrivateKeySigner::random();

    let create_resp = ctx
        .call_as_user("eth_newBlockFilter", serde_json::json!([]), &owner_signer)
        .await?;
    assert!(
        create_resp.get("error").is_none(),
        "owner should be able to create a block filter: {create_resp}"
    );
    let filter_id = create_resp["result"].clone();

    ctx.inject_empty_block().await?;

    let owner_changes = ctx
        .call_as_user(
            "eth_getFilterChanges",
            serde_json::json!([filter_id.clone()]),
            &owner_signer,
        )
        .await?;
    assert!(
        owner_changes.get("error").is_none(),
        "owner should be able to read filter changes: {owner_changes}"
    );
    assert!(
        owner_changes["result"]
            .as_array()
            .is_some_and(|changes| !changes.is_empty()),
        "owner should observe at least one new block hash"
    );

    let other_changes = ctx
        .call_as_user(
            "eth_getFilterChanges",
            serde_json::json!([filter_id.clone()]),
            &other_signer,
        )
        .await?;
    assert_filter_not_found_error(&other_changes);

    let uninstall_resp = ctx
        .call_as_user(
            "eth_uninstallFilter",
            serde_json::json!([filter_id.clone()]),
            &owner_signer,
        )
        .await?;
    assert!(
        uninstall_resp["result"].as_bool().unwrap(),
        "owner uninstall should succeed",
    );

    let after_uninstall = ctx
        .call_as_user(
            "eth_getFilterChanges",
            serde_json::json!([filter_id]),
            &owner_signer,
        )
        .await?;
    assert_filter_not_found_error(&after_uninstall);

    Ok(())
}

/// Balance & state privacy: users see `0x0` for other addresses (balance and nonce),
/// can see their own, and sequencer has full access.
#[tokio::test(flavor = "multi_thread")]
async fn test_balance_privacy() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut ctx = start_zone_with_private_rpc().await?;

    let depositor = address!("0x0000000000000000000000000000000000001111");
    let recipient = address!("0x0000000000000000000000000000000000005678");
    let deposit_amount: u128 = 1_000_000;

    ctx.inject_deposit(PATH_USD_ADDRESS, depositor, recipient, deposit_amount)
        .await?;

    let user_signer = PrivateKeySigner::random();

    // User querying another address's balance → 0x0
    let resp = ctx.get_balance_as_user(recipient, &user_signer).await?;
    assert_eq!(
        resp["result"].as_str().unwrap(),
        "0x0",
        "non-owner should see 0x0 balance for other addresses"
    );

    // User querying another address's tx count → 0x0
    let resp = ctx.get_tx_count_as_user(recipient, &user_signer).await?;
    assert_eq!(
        resp["result"].as_str().unwrap(),
        "0x0",
        "non-owner should see 0x0 for other address's tx count"
    );

    // User querying own balance → works (no error)
    let resp = ctx
        .get_balance_as_user(user_signer.address(), &user_signer)
        .await?;
    assert!(
        resp.get("result").is_some() && resp.get("error").is_none(),
        "user should be able to query own balance"
    );

    // Sequencer querying any address → full access
    let resp = ctx.get_balance_as_sequencer(recipient).await?;
    assert!(
        resp.get("result").is_some() && resp.get("error").is_none(),
        "sequencer should be able to query any address's balance"
    );

    Ok(())
}

/// `eth_call` against the zone TIP-20 enforces read privacy for `balanceOf`
/// and `allowance`, while the configured sequencer retains access.
#[tokio::test(flavor = "multi_thread")]
async fn test_tip20_eth_call_privacy() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut ctx = start_zone_with_private_rpc().await?;

    let owner_signer = MnemonicBuilder::<English>::default()
        .phrase(TEST_MNEMONIC)
        .build()?;
    let owner = owner_signer.address();
    let spender_signer = PrivateKeySigner::random();
    let spender = spender_signer.address();
    let outsider_signer = PrivateKeySigner::random();

    let deposit_amount: u128 = 1_000_000;
    let allowance_amount: u128 = 333_333;

    ctx.inject_deposit(PATH_USD_ADDRESS, owner, owner, deposit_amount)
        .await?;

    let owner_provider = ProviderBuilder::new()
        .wallet(owner_signer.clone())
        .connect_http(ctx.zone.http_url().clone());
    let approve_pending = ContractTip20::new(PATH_USD_ADDRESS, &owner_provider)
        .approve(spender, U256::from(allowance_amount))
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(150_000)
        .send()
        .await?;
    ctx.fixture.inject_empty_block(ctx.zone.deposit_queue());
    let approve_receipt = approve_pending.get_receipt().await?;
    assert!(approve_receipt.status(), "approve should succeed");
    let expected_owner_balance = ctx.zone.balance_of(PATH_USD_ADDRESS, owner).await?;

    let balance_call = PrecompileTip20::balanceOfCall { account: owner };
    let balance_data = format!("0x{}", hex::encode(balance_call.abi_encode()));
    let allowance_call = PrecompileTip20::allowanceCall { owner, spender };
    let allowance_data = format!("0x{}", hex::encode(allowance_call.abi_encode()));

    let outsider_balance = ctx
        .call_as_user(
            "eth_call",
            serde_json::json!([
                {
                    "to": format!("{PATH_USD_ADDRESS:#x}"),
                    "data": balance_data,
                },
                "latest"
            ]),
            &outsider_signer,
        )
        .await?;
    assert!(
        outsider_balance.get("error").is_some(),
        "non-owner balanceOf(other) should revert"
    );

    let outsider_allowance = ctx
        .call_as_user(
            "eth_call",
            serde_json::json!([
                {
                    "to": format!("{PATH_USD_ADDRESS:#x}"),
                    "data": allowance_data,
                },
                "latest"
            ]),
            &outsider_signer,
        )
        .await?;
    assert!(
        outsider_allowance.get("error").is_some(),
        "unrelated caller allowance(owner, spender) should revert"
    );

    let sequencer_balance = ctx
        .call_as_sequencer(
            "eth_call",
            serde_json::json!([
                {
                    "from": format!("{:#x}", ctx.config.sequencer),
                    "to": format!("{PATH_USD_ADDRESS:#x}"),
                    "data": format!("0x{}", hex::encode(balance_call.abi_encode())),
                },
                "latest"
            ]),
        )
        .await?;
    let sequencer_balance_bytes = hex::decode(
        sequencer_balance["result"]
            .as_str()
            .expect("sequencer balanceOf should return hex")
            .trim_start_matches("0x"),
    )?;
    assert_eq!(
        PrecompileTip20::balanceOfCall::abi_decode_returns(&sequencer_balance_bytes)?,
        expected_owner_balance
    );

    let sequencer_allowance = ctx
        .call_as_sequencer(
            "eth_call",
            serde_json::json!([
                {
                    "from": format!("{:#x}", ctx.config.sequencer),
                    "to": format!("{PATH_USD_ADDRESS:#x}"),
                    "data": format!("0x{}", hex::encode(allowance_call.abi_encode())),
                },
                "latest"
            ]),
        )
        .await?;
    let sequencer_allowance_bytes = hex::decode(
        sequencer_allowance["result"]
            .as_str()
            .expect("sequencer allowance should return hex")
            .trim_start_matches("0x"),
    )?;
    assert_eq!(
        PrecompileTip20::allowanceCall::abi_decode_returns(&sequencer_allowance_bytes)?,
        U256::from(allowance_amount)
    );

    Ok(())
}

/// Block access control: user gets redacted blocks (full=false), rejected for full=true;
/// sequencer gets full blocks.
#[tokio::test(flavor = "multi_thread")]
async fn test_block_access_control() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut ctx = start_zone_with_private_rpc().await?;
    ctx.inject_empty_block().await?;

    let user_signer = PrivateKeySigner::random();

    // User full=true → rejected with -32005
    let resp = ctx
        .call_as_user(
            "eth_getBlockByNumber",
            serde_json::json!(["latest", true]),
            &user_signer,
        )
        .await?;
    let error = resp
        .get("error")
        .expect("full=true should be rejected for user");
    assert_eq!(
        error["code"].as_i64().unwrap(),
        -32005,
        "full=true for non-sequencer should return -32005"
    );

    // User full=false → redacted block (empty txs, zeroed logsBloom)
    let resp = ctx
        .call_as_user(
            "eth_getBlockByNumber",
            serde_json::json!(["latest", false]),
            &user_signer,
        )
        .await?;
    let block = resp.get("result").expect("should have result");
    assert!(!block.is_null(), "block should not be null");

    let txs = block
        .get("transactions")
        .expect("block should have transactions field");
    assert!(
        txs.as_array().is_some_and(|a| a.is_empty()),
        "non-sequencer block transactions should be empty (redacted)"
    );
    if let Some(bloom) = block.get("logsBloom").and_then(|b| b.as_str()) {
        let bloom_trimmed = bloom.strip_prefix("0x").unwrap_or(bloom);
        assert!(
            bloom_trimmed.chars().all(|c| c == '0'),
            "non-sequencer block logsBloom should be all zeros"
        );
    }

    // Sequencer full=true → allowed
    let resp = ctx
        .call_as_sequencer("eth_getBlockByNumber", serde_json::json!(["latest", true]))
        .await?;
    assert!(
        resp.get("result").is_some() && resp.get("error").is_none(),
        "sequencer should get full block without error"
    );

    Ok(())
}

/// Method tier enforcement: restricted → -32005 for users (allowed for sequencer),
/// disabled → -32006 for everyone, unknown → -32601.
#[tokio::test(flavor = "multi_thread")]
async fn test_method_tiers() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let ctx = start_zone_with_private_rpc().await?;
    let user_signer = PrivateKeySigner::random();

    // Restricted methods → -32005 for non-sequencer
    for method in [
        "eth_getCode",
        "eth_getStorageAt",
        "eth_getBlockReceipts",
        "debug_traceTransaction",
        "txpool_content",
    ] {
        let resp = ctx
            .call_as_user(method, serde_json::json!([]), &user_signer)
            .await?;
        let error = resp
            .get("error")
            .unwrap_or_else(|| panic!("{method} should return error for non-sequencer"));
        assert_eq!(
            error["code"].as_i64().unwrap(),
            -32005,
            "{method} should return -32005 (Sequencer only)"
        );
    }

    // Restricted methods → allowed for sequencer (no -32005)
    let resp = ctx
        .call_as_sequencer(
            "eth_getCode",
            serde_json::json!([format!("{:#x}", ctx.config.sequencer), "latest"]),
        )
        .await?;
    if let Some(error) = resp.get("error") {
        assert_ne!(
            error["code"].as_i64().unwrap(),
            -32005,
            "sequencer should not get 'Sequencer only' error for restricted methods"
        );
    }

    // Disabled methods → -32006 for everyone (including sequencer)
    for method in ["eth_subscribe", "eth_mining", "eth_hashrate"] {
        let resp = ctx.call_as_sequencer(method, serde_json::json!([])).await?;
        let error = resp
            .get("error")
            .unwrap_or_else(|| panic!("{method} should return error even for sequencer"));
        assert_eq!(
            error["code"].as_i64().unwrap(),
            -32006,
            "{method} should return -32006 (Method disabled)"
        );
    }

    // Unknown method → -32601
    let resp = ctx
        .call_as_sequencer("eth_someNonexistentMethod", serde_json::json!([]))
        .await?;
    let error = resp
        .get("error")
        .expect("unknown method should return error");
    assert_eq!(
        error["code"].as_i64().unwrap(),
        -32601,
        "unknown method should return -32601"
    );

    Ok(())
}

/// WebSocket log subscriptions apply the same caller-scoped filtering as
/// polling log APIs, while the sequencer sees the full stream.
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_logs_subscription_scopes_non_sequencer_and_allows_sequencer() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut ctx = start_zone_with_private_rpc().await?;
    let owner_signer = MnemonicBuilder::<English>::default()
        .phrase(TEST_MNEMONIC)
        .build()?;
    let outsider_signer = PrivateKeySigner::random();
    let spender = PrivateKeySigner::random().address();

    ctx.inject_deposit(
        PATH_USD_ADDRESS,
        owner_signer.address(),
        owner_signer.address(),
        1_000_000,
    )
    .await?;
    ctx.inject_deposit(
        PATH_USD_ADDRESS,
        outsider_signer.address(),
        outsider_signer.address(),
        1_000_000,
    )
    .await?;

    let owner_token = ctx.user_token(&owner_signer);
    let sequencer_token = ctx.sequencer_token();
    let mut owner_ws = connect_private_rpc_ws(&ctx.private_rpc_url, &owner_token).await?;
    let mut sequencer_ws = connect_private_rpc_ws(&ctx.private_rpc_url, &sequencer_token).await?;

    let owner_subscription = ws_subscribe(
        &mut owner_ws,
        json!(["logs", {"address": format!("{PATH_USD_ADDRESS:#x}")}]),
    )
    .await?;
    let sequencer_subscription = ws_subscribe(
        &mut sequencer_ws,
        json!(["logs", {"address": format!("{PATH_USD_ADDRESS:#x}")}]),
    )
    .await?;

    let owner_provider = ProviderBuilder::new()
        .wallet(owner_signer.clone())
        .connect_http(ctx.zone.http_url().clone());
    let outsider_provider = ProviderBuilder::new()
        .wallet(outsider_signer.clone())
        .connect_http(ctx.zone.http_url().clone());

    let owner_pending = ContractTip20::new(PATH_USD_ADDRESS, &owner_provider)
        .approve(spender, U256::from(111u64))
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(150_000)
        .send()
        .await?;
    let outsider_pending = ContractTip20::new(PATH_USD_ADDRESS, &outsider_provider)
        .approve(spender, U256::from(222u64))
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(150_000)
        .send()
        .await?;

    let owner_hash = *owner_pending.tx_hash();
    let outsider_hash = *outsider_pending.tx_hash();

    ctx.fixture.inject_empty_block(ctx.zone.deposit_queue());

    let owner_receipt = owner_pending.get_receipt().await?;
    let outsider_receipt = outsider_pending.get_receipt().await?;
    assert!(owner_receipt.status(), "owner approve should succeed");
    assert!(outsider_receipt.status(), "outsider approve should succeed");

    let mut owner_notifications = vec![ws_next_json(&mut owner_ws).await?];
    owner_notifications
        .extend(ws_collect_messages_until_quiet(&mut owner_ws, Duration::from_millis(500)).await?);
    let owner_hashes = owner_notifications
        .into_iter()
        .map(|notification| {
            assert_eq!(
                notification["params"]["subscription"].as_str().unwrap(),
                owner_subscription
            );
            notification["params"]["result"]["transactionHash"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<HashSet<_>>();
    assert_eq!(owner_hashes, HashSet::from([format!("{owner_hash:#x}")]));

    let mut sequencer_notifications = vec![ws_next_json(&mut sequencer_ws).await?];
    sequencer_notifications.extend(
        ws_collect_messages_until_quiet(&mut sequencer_ws, Duration::from_millis(500)).await?,
    );
    let sequencer_hashes = sequencer_notifications
        .into_iter()
        .map(|notification| {
            assert_eq!(
                notification["params"]["subscription"].as_str().unwrap(),
                sequencer_subscription
            );
            notification["params"]["result"]["transactionHash"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<HashSet<_>>();
    assert!(sequencer_hashes.contains(&format!("{owner_hash:#x}")));
    assert!(sequencer_hashes.contains(&format!("{outsider_hash:#x}")));

    Ok(())
}

/// Pending transaction subscriptions are sender-scoped for non-sequencers,
/// while the sequencer sees the unfiltered full transaction stream.
#[tokio::test(flavor = "multi_thread")]
async fn test_ws_pending_transactions_are_sender_scoped_for_users() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut ctx = start_zone_with_private_rpc().await?;
    let owner_signer = MnemonicBuilder::<English>::default()
        .phrase(TEST_MNEMONIC)
        .build()?;
    let outsider_signer = PrivateKeySigner::random();
    let spender = PrivateKeySigner::random().address();

    ctx.inject_deposit(
        PATH_USD_ADDRESS,
        owner_signer.address(),
        owner_signer.address(),
        1_000_000,
    )
    .await?;
    ctx.inject_deposit(
        PATH_USD_ADDRESS,
        outsider_signer.address(),
        outsider_signer.address(),
        1_000_000,
    )
    .await?;

    let owner_token = ctx.user_token(&owner_signer);
    let sequencer_token = ctx.sequencer_token();
    let mut owner_ws = connect_private_rpc_ws(&ctx.private_rpc_url, &owner_token).await?;
    let mut sequencer_ws = connect_private_rpc_ws(&ctx.private_rpc_url, &sequencer_token).await?;

    let owner_subscription = ws_subscribe(&mut owner_ws, json!(["newPendingTransactions"])).await?;
    let sequencer_subscription =
        ws_subscribe(&mut sequencer_ws, json!(["newPendingTransactions", true])).await?;

    let owner_provider = ProviderBuilder::new()
        .wallet(owner_signer.clone())
        .connect_http(ctx.zone.http_url().clone());
    let outsider_provider = ProviderBuilder::new()
        .wallet(outsider_signer.clone())
        .connect_http(ctx.zone.http_url().clone());

    let owner_pending = ContractTip20::new(PATH_USD_ADDRESS, &owner_provider)
        .approve(spender, U256::from(333u64))
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(150_000)
        .send()
        .await?;
    let outsider_pending = ContractTip20::new(PATH_USD_ADDRESS, &outsider_provider)
        .approve(spender, U256::from(444u64))
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(150_000)
        .send()
        .await?;

    let owner_hash = *owner_pending.tx_hash();
    let outsider_hash = *outsider_pending.tx_hash();

    let owner_notification = ws_next_json(&mut owner_ws).await?;
    assert_eq!(
        owner_notification["params"]["subscription"]
            .as_str()
            .unwrap(),
        owner_subscription
    );
    assert_eq!(
        owner_notification["params"]["result"].as_str().unwrap(),
        format!("{owner_hash:#x}")
    );
    ws_expect_no_message(&mut owner_ws, Duration::from_millis(500)).await?;

    let mut sequencer_pending = HashMap::new();
    for _ in 0..2 {
        let notification = ws_next_json(&mut sequencer_ws).await?;
        assert_eq!(
            notification["params"]["subscription"].as_str().unwrap(),
            sequencer_subscription
        );
        let tx = notification["params"]["result"]
            .as_object()
            .expect("sequencer should receive full pending tx objects");
        sequencer_pending.insert(
            tx["hash"].as_str().unwrap().to_owned(),
            tx["from"].as_str().unwrap().to_owned(),
        );
    }
    assert_eq!(
        sequencer_pending,
        HashMap::from([
            (
                format!("{owner_hash:#x}"),
                format!("{:#x}", owner_signer.address()),
            ),
            (
                format!("{outsider_hash:#x}"),
                format!("{:#x}", outsider_signer.address()),
            ),
        ])
    );

    ctx.fixture.inject_empty_block(ctx.zone.deposit_queue());
    let owner_receipt = owner_pending.get_receipt().await?;
    let outsider_receipt = outsider_pending.get_receipt().await?;
    assert!(owner_receipt.status(), "owner approve should succeed");
    assert!(outsider_receipt.status(), "outsider approve should succeed");

    Ok(())
}

/// Zone-specific metadata methods return the authenticated account/token expiry
/// and the configured zone metadata.
#[tokio::test(flavor = "multi_thread")]
async fn test_zone_metadata_methods() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let ctx = start_zone_with_private_rpc().await?;
    let user_signer = PrivateKeySigner::random();

    let auth_info = ctx
        .call_as_user(
            "zone_getAuthorizationTokenInfo",
            serde_json::json!([]),
            &user_signer,
        )
        .await?;
    assert_eq!(
        auth_info["result"]["account"].as_str().unwrap(),
        format!("{:#x}", user_signer.address()),
    );
    assert!(
        auth_info["result"]["expiresAt"].as_str().is_some(),
        "expiresAt should be a quantity string",
    );

    let zone_info = ctx
        .call_as_user("zone_getZoneInfo", serde_json::json!([]), &user_signer)
        .await?;
    assert_eq!(
        zone_info["result"]["zoneId"].as_str().unwrap(),
        format!("0x{:x}", ctx.config.zone_id),
    );
    assert_eq!(
        zone_info["result"]["zoneTokens"]
            .as_array()
            .unwrap()
            .iter()
            .map(|token| token.as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["0x20c0000000000000000000000000000000000000"],
    );
    assert_eq!(
        zone_info["result"]["sequencer"].as_str().unwrap(),
        format!("{:#x}", ctx.config.sequencer),
    );
    assert_eq!(
        zone_info["result"]["chainId"].as_str().unwrap(),
        format!("0x{:x}", ctx.config.chain_id),
    );

    Ok(())
}

/// `zone_getZoneInfo` returns every token currently enabled on the portal.
#[tokio::test(flavor = "multi_thread")]
async fn test_zone_get_zone_info_returns_all_enabled_tokens() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let ctx = start_zone_with_private_rpc_l1().await?;
    let user_signer = PrivateKeySigner::random();
    let alpha_salt = B256::with_last_byte(0x44);
    let alpha_token = ctx
        .l1()
        .create_tip20("AlphaUSD", "aUSD", alpha_salt)
        .await?;

    ctx.l1()
        .enable_token_on_portal(ctx.portal_address(), alpha_token)
        .await?;

    let zone_info = ctx
        .call_as_user("zone_getZoneInfo", serde_json::json!([]), &user_signer)
        .await?;
    let zone_tokens = zone_info["result"]["zoneTokens"]
        .as_array()
        .unwrap()
        .iter()
        .map(|token| token.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();

    assert_eq!(
        zone_tokens,
        vec![
            format!("{PATH_USD_ADDRESS:#x}"),
            format!("{alpha_token:#x}")
        ],
    );

    Ok(())
}

/// `zone_getDepositStatus` returns relevant regular deposits for both the sender
/// and the plaintext recipient, and returns an empty list for unrelated callers.
#[tokio::test(flavor = "multi_thread")]
async fn test_zone_get_deposit_status_regular_and_empty() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let ctx = start_zone_with_private_rpc_l1().await?;
    let l1 = ctx.l1();
    let portal_address = ctx.portal_address();

    let depositor_signer = l1.user_signer();
    let recipient_signer = l1.signer_at(2);
    let recipient = recipient_signer.address();

    let mut depositor =
        ZoneAccount::with_signer(depositor_signer.clone(), l1, &ctx.zone, portal_address);
    let deposit_amount: u128 = 1_000_000;
    l1.fund_user(depositor.address(), deposit_amount).await?;

    let (tempo_block_number, _) = depositor
        .deposit_to_with_block(recipient, deposit_amount, DEFAULT_TIMEOUT, &ctx.zone)
        .await?;

    let sender_status = ctx
        .get_deposit_status_as_user(tempo_block_number, &depositor_signer)
        .await?;
    let sender_deposits = sender_status["result"]["deposits"]
        .as_array()
        .expect("sender deposits should be an array");
    assert_eq!(sender_status["result"]["processed"], true);
    assert_eq!(sender_deposits.len(), 1);
    assert_eq!(sender_deposits[0]["kind"], "regular");
    assert_eq!(sender_deposits[0]["status"], "processed");
    assert_eq!(
        sender_deposits[0]["sender"].as_str().unwrap(),
        format!("{:#x}", depositor.address()),
    );
    assert_eq!(
        sender_deposits[0]["recipient"].as_str().unwrap(),
        format!("{recipient:#x}"),
    );
    assert_eq!(sender_deposits[0]["amount"], "0xf4240");

    let recipient_status = ctx
        .get_deposit_status_as_user(tempo_block_number, &recipient_signer)
        .await?;
    let recipient_deposits = recipient_status["result"]["deposits"]
        .as_array()
        .expect("recipient deposits should be an array");
    assert_eq!(recipient_status["result"]["processed"], true);
    assert_eq!(recipient_deposits.len(), 1);
    assert_eq!(
        recipient_deposits[0]["recipient"].as_str().unwrap(),
        format!("{recipient:#x}"),
    );

    let unrelated_signer = PrivateKeySigner::random();
    let unrelated_status = ctx
        .get_deposit_status_as_user(tempo_block_number, &unrelated_signer)
        .await?;
    let unrelated_deposits = unrelated_status["result"]["deposits"]
        .as_array()
        .expect("unrelated deposits should be an array");
    assert_eq!(unrelated_status["result"]["processed"], true);
    assert!(unrelated_deposits.is_empty());

    Ok(())
}

/// `zone_getDepositStatus` reveals encrypted deposits to the sender immediately,
/// and to the recipient once the L2 processed event has revealed the recipient.
#[tokio::test(flavor = "multi_thread")]
async fn test_zone_get_deposit_status_encrypted() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let ctx = start_zone_with_private_rpc_l1_with_encryption().await?;
    let l1 = ctx.l1();
    let portal_address = ctx.portal_address();

    let depositor_signer = l1.user_signer();
    let recipient_signer = l1.signer_at(2);
    let recipient = recipient_signer.address();

    let mut depositor =
        ZoneAccount::with_signer(depositor_signer.clone(), l1, &ctx.zone, portal_address);
    let deposit_amount: u128 = 1_000_000;
    let memo = B256::from([0x11; 32]);
    l1.fund_user(depositor.address(), deposit_amount).await?;

    let (tempo_block_number, _) = depositor
        .deposit_encrypted_with_block(deposit_amount, recipient, memo, DEFAULT_TIMEOUT, &ctx.zone)
        .await?;

    let sender_status = ctx
        .get_deposit_status_as_user(tempo_block_number, &depositor_signer)
        .await?;
    let sender_deposits = sender_status["result"]["deposits"]
        .as_array()
        .expect("sender deposits should be an array");
    assert_eq!(sender_status["result"]["processed"], true);
    assert_eq!(sender_deposits.len(), 1);
    assert_eq!(sender_deposits[0]["kind"], "encrypted");
    assert_eq!(sender_deposits[0]["status"], "processed");
    assert_eq!(
        sender_deposits[0]["recipient"].as_str().unwrap(),
        format!("{recipient:#x}"),
    );
    assert_eq!(
        sender_deposits[0]["memo"].as_str().unwrap(),
        format!("{memo:#x}"),
    );

    let recipient_status = ctx
        .get_deposit_status_as_user(tempo_block_number, &recipient_signer)
        .await?;
    let recipient_deposits = recipient_status["result"]["deposits"]
        .as_array()
        .expect("recipient deposits should be an array");
    assert_eq!(recipient_status["result"]["processed"], true);
    assert_eq!(recipient_deposits.len(), 1);
    assert_eq!(
        recipient_deposits[0]["recipient"].as_str().unwrap(),
        format!("{recipient:#x}"),
    );
    assert_eq!(
        recipient_deposits[0]["sender"].as_str().unwrap(),
        format!("{:#x}", depositor.address()),
    );

    Ok(())
}
