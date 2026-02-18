//! End-to-end tests for the private zone RPC server.
//!
//! These tests launch a zone node with a private RPC server and verify:
//! - Authentication enforcement (missing/invalid tokens, wrong chain ID)
//! - Public method access (both sequencer and regular users)
//! - Balance & state privacy (users only see their own data)
//! - Block redaction (logsBloom zeroed, transactions cleared for non-sequencers)
//! - Method tier enforcement (restricted/disabled/unknown methods)

use alloy::{
    primitives::{Address, address},
    signers::local::PrivateKeySigner,
};
use tempo_precompiles::PATH_USD_ADDRESS;

use crate::utils::start_zone_with_private_rpc;

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
