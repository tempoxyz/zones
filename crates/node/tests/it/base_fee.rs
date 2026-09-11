//! End-to-end coverage for the Zone base-fee transition.

use alloy::{
    consensus::BlockHeader as _,
    network::ReceiptResponse as _,
    primitives::{TxKind, U256},
    providers::Provider as _,
    signers::local::PrivateKeySigner,
};
use alloy_eips::eip2718::Encodable2718;
use alloy_signer::SignerSync as _;
use tempo_chainspec::spec::{TEMPO_T7_BASE_FEE_CAP, tempo_t7_next_block_base_fee};
use tempo_precompiles::PATH_USD_ADDRESS;
use tempo_primitives::{
    TempoTxEnvelope,
    transaction::{
        AASigned, Call, PrimitiveSignature, TempoSignature, TempoTransaction,
        calc_gas_balance_spending,
    },
};
use zone_precompiles::ZONE_FEE_MANAGER_ADDRESS;

use crate::utils::{DEFAULT_TIMEOUT, start_local_zone_with_fixture_and_withdrawal_batch_interval};

const ZONE_ID: u32 = 1;
const FEE_BALANCE: u128 = 1_000_000;

fn sponsored_transaction(
    sender: &PrivateKeySigner,
    fee_payer: &PrivateKeySigner,
    chain_id: u64,
) -> eyre::Result<TempoTxEnvelope> {
    let mut transaction = TempoTransaction {
        chain_id,
        max_fee_per_gas: TEMPO_T7_BASE_FEE_CAP as u128,
        gas_limit: 500_000,
        calls: vec![Call {
            to: TxKind::Call(sender.address()),
            value: U256::ZERO,
            input: Default::default(),
        }],
        fee_token: Some(PATH_USD_ADDRESS),
        ..Default::default()
    };

    let fee_payer_hash = transaction.fee_payer_signature_hash(sender.address());
    transaction.fee_payer_signature = Some(fee_payer.sign_hash_sync(&fee_payer_hash)?);
    let sender_signature = sender.sign_hash_sync(&transaction.signature_hash())?;
    let signed = AASigned::new_unhashed(
        transaction,
        TempoSignature::Primitive(PrimitiveSignature::Secp256k1(sender_signature)),
    );
    Ok(signed.into())
}

fn z1_genesis() -> eyre::Result<alloy::genesis::Genesis> {
    let mut genesis = zone_node::genesis::genesis_template()?;
    genesis
        .config
        .extra_fields
        .insert_value("z1Time".into(), 1)?;
    Ok(genesis)
}

#[tokio::test(flavor = "multi_thread")]
async fn z1_activates_dynamic_base_fee() -> eyre::Result<()> {
    let (zone, mut fixture) =
        start_local_zone_with_fixture_and_withdrawal_batch_interval(ZONE_ID, 2, 2, z1_genesis()?)
            .await?;

    let genesis = zone
        .provider()
        .get_block_by_number(0.into())
        .await?
        .expect("Zone genesis block");
    fixture.inject_empty_block(zone.deposit_queue());
    zone.wait_for_block_number(1, DEFAULT_TIMEOUT).await?;
    let first = zone
        .provider()
        .get_block_by_number(1.into())
        .await?
        .expect("first Zone block");
    assert_eq!(
        first.header.base_fee_per_gas(),
        Some(tempo_t7_next_block_base_fee(
            genesis.header.base_fee_per_gas().expect("genesis base fee"),
            genesis.header.gas_used(),
        )),
        "the Z1 activation block must adjust from its parent"
    );
    assert_eq!(
        zone.provider().get_gas_price().await?,
        u128::from(
            first
                .header
                .base_fee_per_gas()
                .expect("first block base fee")
        ),
        "eth_gasPrice must follow the active Zone base fee"
    );

    fixture.inject_empty_block(zone.deposit_queue());
    zone.wait_for_block_number(2, DEFAULT_TIMEOUT).await?;
    let second = zone
        .provider()
        .get_block_by_number(2.into())
        .await?
        .expect("second Zone block");
    assert_eq!(
        second.header.base_fee_per_gas(),
        Some(tempo_t7_next_block_base_fee(
            first
                .header
                .base_fee_per_gas()
                .expect("first block base fee"),
            first.header.gas_used(),
        )),
        "post-activation blocks must adjust from their parent's fee and gas usage"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn sponsored_transaction_settles_nonzero_base_fee() -> eyre::Result<()> {
    let (zone, mut fixture) =
        start_local_zone_with_fixture_and_withdrawal_batch_interval(ZONE_ID, 2, 2, z1_genesis()?)
            .await?;
    let sender = PrivateKeySigner::random();
    let fee_payer = PrivateKeySigner::random();
    let deposits = [sender.address(), fee_payer.address()]
        .map(|account| fixture.make_deposit(PATH_USD_ADDRESS, account, account, FEE_BALANCE));
    fixture.inject_deposits(zone.deposit_queue(), deposits.into());
    zone.wait_for_balance(
        PATH_USD_ADDRESS,
        fee_payer.address(),
        U256::from(FEE_BALANCE),
        DEFAULT_TIMEOUT,
    )
    .await?;

    let provider = zone.provider();
    let beneficiary = provider
        .get_block_by_number(1.into())
        .await?
        .expect("deposit block")
        .header
        .beneficiary();
    let sender_before = zone.balance_of(PATH_USD_ADDRESS, sender.address()).await?;
    let fee_payer_before = zone
        .balance_of(PATH_USD_ADDRESS, fee_payer.address())
        .await?;
    let beneficiary_before = zone.balance_of(PATH_USD_ADDRESS, beneficiary).await?;
    let escrow_before = zone
        .balance_of(PATH_USD_ADDRESS, ZONE_FEE_MANAGER_ADDRESS)
        .await?;

    let transaction = sponsored_transaction(&sender, &fee_payer, provider.get_chain_id().await?)?;
    let pending = provider
        .send_raw_transaction(&transaction.encoded_2718())
        .await?;
    fixture.inject_empty_block(zone.deposit_queue());
    let receipt = pending.get_receipt().await?;
    assert!(receipt.status(), "sponsored transaction must succeed");
    assert!(
        receipt.effective_gas_price > 0,
        "Z1 transaction must pay a nonzero base fee"
    );

    let actual_fee = calc_gas_balance_spending(receipt.gas_used, receipt.effective_gas_price);
    assert_eq!(
        zone.balance_of(PATH_USD_ADDRESS, sender.address()).await?,
        sender_before,
        "the sponsored sender must not pay the execution fee"
    );
    assert_eq!(
        zone.balance_of(PATH_USD_ADDRESS, fee_payer.address())
            .await?,
        fee_payer_before - actual_fee,
        "the fee payer must pay exactly the receipt fee"
    );
    assert_eq!(
        zone.balance_of(PATH_USD_ADDRESS, beneficiary).await?,
        beneficiary_before + actual_fee,
        "the producing sequencer must receive the execution fee"
    );
    assert_eq!(
        zone.balance_of(PATH_USD_ADDRESS, ZONE_FEE_MANAGER_ADDRESS)
            .await?,
        escrow_before,
        "unused maximum-fee escrow must be refunded"
    );

    Ok(())
}
