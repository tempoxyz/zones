use alloy::{
    network::EthereumWallet,
    primitives::{Address, B256, Bytes, U256},
    providers::{Provider, ProviderBuilder},
};
use eyre::{WrapErr as _, eyre};
use tempo_alloy::TempoNetwork;
use tempo_contracts::precompiles::ITIP20 as TIP20Token;
use zone::abi::{ZONE_OUTBOX_ADDRESS, ZoneOutbox, ZonePortal};

use crate::{
    bridge_utils::{
        build_encrypted_deposit_payload_for_zone, build_encrypted_router_callback,
        parse_private_key, resolve_token_ref, resolve_zone_ref,
    },
    zone_utils::{
        EncryptedDepositResult, ROUTER_CALLBACK_GAS_LIMIT, check, normalize_http_rpc,
        wait_for_encrypted_deposit_result, wait_for_withdrawal_processed,
    },
};

const APPROVAL_GAS_LIMIT: u64 = 150_000;
const WITHDRAWAL_TX_GAS: u64 = 1_000_000;

#[derive(Debug, clap::Parser)]
pub(crate) struct SendRoutedWithdrawal {
    /// Source zone reference: name, zone ID, or portal address.
    #[arg(long)]
    source_zone: String,

    /// Source token reference: alias or address.
    #[arg(long)]
    source_token: String,

    /// Target zone reference: name, zone ID, or portal address.
    #[arg(long)]
    target_zone: String,

    /// Target token reference: alias or address.
    #[arg(long)]
    target_token: String,

    /// Withdrawal amount in token base units.
    #[arg(long)]
    amount: u128,

    /// Target recipient on the destination zone. Defaults to the PRIVATE_KEY signer address.
    #[arg(long)]
    recipient: Option<Address>,

    /// Memo for the downstream encrypted deposit.
    #[arg(long, default_value_t = B256::ZERO)]
    memo: B256,

    /// Minimum acceptable L1 swap output. Required when source and target tokens differ.
    #[arg(long)]
    min_amount_out: Option<u128>,

    /// Optional router override. Otherwise sourced from the source zone metadata.
    #[arg(long)]
    router: Option<Address>,

    /// Optional fallback recipient on the source zone if the router callback fails.
    #[arg(long)]
    fallback_recipient: Option<Address>,

    /// Tempo L1 RPC URL.
    #[arg(long, env = "L1_RPC_URL")]
    l1_rpc_url: String,

    /// Source zone L2 RPC URL used to submit the withdrawal.
    #[arg(long, env = "ZONE_RPC_URL", default_value = "http://localhost:8546")]
    source_rpc_url: String,

    /// Optional target zone L2 RPC URL used to verify the downstream encrypted deposit.
    #[arg(long)]
    target_rpc_url: Option<String>,

    /// Private key used to sign the source-zone withdrawal request.
    #[arg(long, env = "PRIVATE_KEY")]
    private_key: String,

    /// ZoneFactory contract address used to resolve zone IDs when local metadata is unavailable.
    #[arg(long)]
    zone_factory: Option<Address>,
}

impl SendRoutedWithdrawal {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let signer = parse_private_key(&self.private_key)?;
        let signer_address = signer.address();
        let recipient = self.recipient.unwrap_or(signer_address);
        let fallback_recipient = self.fallback_recipient.unwrap_or(signer_address);
        let http_rpc = normalize_http_rpc(&self.l1_rpc_url);

        let l1 = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&http_rpc)
            .await?;
        l1.client()
            .set_poll_interval(std::time::Duration::from_secs(1));

        let source_zone = resolve_zone_ref(&self.source_zone, &l1, self.zone_factory).await?;
        let target_zone = resolve_zone_ref(&self.target_zone, &l1, self.zone_factory).await?;
        let source_token = resolve_token_ref(&self.source_token)?;
        let target_token = resolve_token_ref(&self.target_token)?;
        let router = self
            .router
            .or(source_zone.router)
            .ok_or_else(|| eyre!("router not provided and not found in source zone metadata"))?;
        let min_amount_out =
            effective_min_amount_out(source_token, target_token, self.min_amount_out)?;

        ensure_token_enabled(&l1, source_zone.portal, source_token, "source").await?;
        ensure_token_enabled(&l1, target_zone.portal, target_token, "target").await?;

        let payload =
            build_encrypted_deposit_payload_for_zone(&l1, &target_zone, recipient, self.memo)
                .await?;
        let callback_data = build_encrypted_router_callback(target_token, &payload, min_amount_out);

        let l2_wallet = EthereumWallet::from(parse_private_key(&self.private_key)?);
        let source_l2 = ProviderBuilder::new_with_network::<TempoNetwork>()
            .wallet(l2_wallet)
            .connect(&self.source_rpc_url)
            .await?;

        let effective_target_rpc_url = self.target_rpc_url.clone().or_else(|| {
            (source_zone.portal == target_zone.portal).then(|| self.source_rpc_url.clone())
        });
        let target_l2 = if let Some(ref target_rpc_url) = effective_target_rpc_url {
            Some(
                ProviderBuilder::new_with_network::<TempoNetwork>()
                    .connect(target_rpc_url)
                    .await?,
            )
        } else {
            None
        };
        let target_from_block = if let Some(ref target_l2) = target_l2 {
            Some(target_l2.get_block_number().await.unwrap_or(0))
        } else {
            None
        };

        println!("Requesting routed withdrawal");
        println!("  Signer:            {signer_address}");
        println!("  Source zone:       {}", source_zone.reference);
        println!("  Source portal:     {}", source_zone.portal);
        println!("  Source token:      {source_token}");
        println!("  Target zone:       {}", target_zone.reference);
        println!("  Target portal:     {}", target_zone.portal);
        println!("  Target token:      {target_token}");
        println!("  Router:            {router}");
        println!("  Recipient:         {recipient}");
        println!("  Fallback recipient:{fallback_recipient}");
        println!("  Amount:            {}", self.amount);
        println!("  Min amount out:    {min_amount_out}");

        let receipt = TIP20Token::new(source_token, &source_l2)
            .approve(ZONE_OUTBOX_ADDRESS, U256::MAX)
            .gas(APPROVAL_GAS_LIMIT)
            .send()
            .await
            .wrap_err("failed to submit source token approval for the outbox")?
            .get_receipt()
            .await
            .wrap_err("failed to approve source token for the outbox")?;
        check(&receipt, "approve source token for outbox")?;
        println!(
            "  Outbox approved on source zone  [tx: {}]",
            receipt.transaction_hash
        );

        let l1_from_block = l1.get_block_number().await.unwrap_or(0);
        let receipt = ZoneOutbox::new(ZONE_OUTBOX_ADDRESS, &source_l2)
            .requestWithdrawal(
                source_token,
                router,
                self.amount,
                self.memo,
                ROUTER_CALLBACK_GAS_LIMIT,
                fallback_recipient,
                callback_data,
                Bytes::new(),
            )
            .gas(WITHDRAWAL_TX_GAS)
            .send()
            .await
            .wrap_err("failed to submit routed withdrawal request on the source zone")?
            .get_receipt()
            .await
            .wrap_err("failed to request routed withdrawal on the source zone")?;
        check(&receipt, "request routed withdrawal")?;
        println!(
            "  Routed withdrawal requested on source zone  [tx: {}]",
            receipt.transaction_hash
        );

        let l1_block = wait_for_withdrawal_processed(
            &l1,
            l1_from_block,
            source_zone.portal,
            router,
            source_token,
            self.amount,
            true,
        )
        .await?;
        println!("  Router callback processed on L1 (block {l1_block})");

        if let (Some(target_l2), Some(from_block)) = (target_l2.as_ref(), target_from_block) {
            match wait_for_encrypted_deposit_result(
                target_l2,
                from_block,
                router,
                Some(recipient),
                Some(target_token),
                Some(self.memo),
            )
            .await?
            {
                EncryptedDepositResult::Processed {
                    block,
                    token,
                    amount,
                    memo,
                    ..
                } => {
                    if token != target_token {
                        return Err(eyre!(
                            "unexpected target token in encrypted deposit result: expected {target_token}, got {token}"
                        ));
                    }
                    if memo != self.memo {
                        return Err(eyre!(
                            "unexpected memo in encrypted deposit result: expected {}, got {memo}",
                            self.memo
                        ));
                    }
                    println!("  Encrypted deposit processed on target zone (block {block})");
                    println!("  Target amount received: {amount}");
                }
                EncryptedDepositResult::Failed {
                    block,
                    token,
                    amount,
                    ..
                } => {
                    return Err(eyre!(
                        "encrypted deposit failed on target zone (block {block}, token {token}, amount {amount})"
                    ));
                }
            }
        } else {
            println!("  Skipped target zone verification because target-rpc-url was not provided");
        }

        Ok(())
    }
}

fn effective_min_amount_out(
    source_token: Address,
    target_token: Address,
    min_amount_out: Option<u128>,
) -> eyre::Result<u128> {
    if source_token == target_token {
        return Ok(min_amount_out.unwrap_or(0));
    }

    min_amount_out.ok_or_else(|| {
        eyre!("min-amount-out is required when source-token and target-token differ")
    })
}

async fn ensure_token_enabled<P: Provider<TempoNetwork>>(
    l1: &P,
    portal: Address,
    token: Address,
    label: &str,
) -> eyre::Result<()> {
    let enabled = ZonePortal::new(portal, l1)
        .isTokenEnabled(token)
        .call()
        .await
        .wrap_err_with(|| format!("failed to query {label} portal token enablement"))?;
    if !enabled {
        return Err(eyre!(
            "{label} token {token} is not enabled on portal {portal}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_min_amount_out_for_swaps() -> eyre::Result<()> {
        let source = resolve_token_ref("alphausd")?;
        let target = resolve_token_ref("betausd")?;
        let err = effective_min_amount_out(source, target, None).unwrap_err();
        assert!(err.to_string().contains("min-amount-out is required"));
        Ok(())
    }

    #[test]
    fn defaults_min_amount_out_to_zero_for_same_token_routing() -> eyre::Result<()> {
        let token = resolve_token_ref("pathusd")?;
        assert_eq!(effective_min_amount_out(token, token, None)?, 0);
        assert_eq!(effective_min_amount_out(token, token, Some(42))?, 42);
        Ok(())
    }
}
