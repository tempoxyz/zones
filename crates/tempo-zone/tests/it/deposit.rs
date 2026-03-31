use alloy::{
    primitives::{Address, B256, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::{MnemonicBuilder, PrivateKeySigner},
    sol_types::SolEvent,
};
use tempo_chainspec::spec::TEMPO_T0_BASE_FEE;
use tempo_contracts::precompiles::{IRolesAuth, ITIP20, ITIP20Factory};
use tempo_precompiles::{PATH_USD_ADDRESS, TIP20_FACTORY_ADDRESS, tip20::ISSUER_ROLE};
use zone::abi::ZonePortal;

use crate::utils::{self, DEFAULT_POLL, ZoneTestNode, poll_until};

const L1_WS_RPC_URL: &str = "wss://rpc.testnet.tempo.xyz";
const L1_HTTP_RPC_URL: &str = "https://rpc.testnet.tempo.xyz";

/// Fund an address on L1 via the testnet faucet (`tempo_fundAddress`).
async fn fund_l1_wallet(address: Address) -> eyre::Result<()> {
    let provider = ProviderBuilder::new().connect_http(L1_HTTP_RPC_URL.parse()?);
    let _: Vec<B256> = provider
        .raw_request("tempo_fundAddress".into(), (address,))
        .await?;
    Ok(())
}

/// End-to-end: deposit on an existing L1 ZonePortal, verify mint on zone.
///
/// Uses an existing ZonePortal deployed on testnet. The zone node starts
/// locally with the L1 subscriber pointing at testnet to pick up deposit
/// events, then mints on its local chain.
///
/// Requires env var:
/// - `L1_PORTAL_ADDRESS`: existing ZonePortal contract address on testnet
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires testnet: L1_PORTAL_ADDRESS"]
async fn test_l1_deposit_mints_on_zone() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let portal_address: Address = std::env::var("L1_PORTAL_ADDRESS")?.parse()?;

    // Fresh L1 wallet, funded via faucet
    let l1_signer = PrivateKeySigner::random();
    let depositor = l1_signer.address();
    fund_l1_wallet(depositor).await?;

    // Deterministic zone token address from test mnemonic + fixed salt
    let zone_wallet = MnemonicBuilder::from_phrase(utils::TEST_MNEMONIC).build()?;
    let zone_admin = zone_wallet.address();
    let zone_token_address = utils::compute_tip20_address(zone_admin, utils::ZONE_TEST_TOKEN_SALT);

    // Start the zone node pointing at the existing portal on testnet
    let zone = ZoneTestNode::start(L1_WS_RPC_URL.to_string(), portal_address).await?;

    // --- Zone setup: create the mint target token, grant system sender ISSUER_ROLE ---

    let zone_provider = ProviderBuilder::new()
        .wallet(zone_wallet)
        .connect_http(zone.http_url().clone());

    let factory = ITIP20Factory::new(TIP20_FACTORY_ADDRESS, zone_provider.clone());
    let receipt = factory
        .createToken(
            "ZoneTest".to_string(),
            "ZTEST".to_string(),
            "USD".to_string(),
            PATH_USD_ADDRESS,
            zone_admin,
            utils::ZONE_TEST_TOKEN_SALT,
        )
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(500_000)
        .send()
        .await?
        .get_receipt()
        .await?;

    let event = ITIP20Factory::TokenCreated::decode_log(&receipt.logs()[1].inner)?;
    assert_eq!(event.token, zone_token_address, "token address mismatch");

    let zone_token = ITIP20::new(zone_token_address, zone_provider.clone());
    let roles = IRolesAuth::new(zone_token_address, zone_provider.clone());

    // System tx sender (Address::ZERO) needs ISSUER_ROLE to mint deposits
    roles
        .grantRole(*ISSUER_ROLE, Address::ZERO)
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(300_000)
        .send()
        .await?
        .get_receipt()
        .await?;

    // --- L1: deposit on the existing portal ---

    let l1_provider = ProviderBuilder::new()
        .wallet(l1_signer)
        .connect_http(L1_HTTP_RPC_URL.parse()?);

    let portal = ZonePortal::new(portal_address, &l1_provider);
    let l1_token_address = PATH_USD_ADDRESS;
    let fee = portal.calculateDepositFee().call().await?;

    let deposit_amount: u128 = fee + 1_000_000;
    let expected_net = deposit_amount - fee;

    // Approve portal to transfer our L1 tokens
    let l1_token = ITIP20::new(l1_token_address, &l1_provider);
    l1_token
        .approve(portal_address, U256::from(deposit_amount))
        .send()
        .await?
        .get_receipt()
        .await?;

    let recipient = depositor;

    // Zone balance before deposit
    let balance_before = zone_token
        .balanceOf(recipient)
        .call()
        .await
        .unwrap_or(U256::ZERO);

    // Execute deposit on L1
    let deposit_receipt = portal
        .deposit(l1_token_address, recipient, deposit_amount, B256::ZERO)
        .send()
        .await?
        .get_receipt()
        .await?;
    assert!(deposit_receipt.status(), "L1 deposit tx failed");

    // --- Poll zone for the minted balance ---

    let minted = poll_until(
        std::time::Duration::from_secs(5),
        DEFAULT_POLL,
        "deposit mint on zone",
        || {
            let zone_token = &zone_token;
            async move {
                let balance_now = zone_token
                    .balanceOf(recipient)
                    .call()
                    .await
                    .unwrap_or(U256::ZERO);
                if balance_now > balance_before {
                    Ok(Some(balance_now - balance_before))
                } else {
                    Ok(None)
                }
            }
        },
    )
    .await?;

    assert_eq!(
        minted,
        U256::from(expected_net),
        "minted amount should equal net deposit (deposit {deposit_amount} - fee {fee})",
    );

    Ok(())
}
