//! Verifies the settled token-backing invariant for a ZonePortal.

use alloy::{
    primitives::{Address, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::BlockId,
};
use eyre::{WrapErr as _, ensure};
use tempo_alloy::TempoNetwork;
use tempo_contracts::precompiles::ITIP20 as TIP20Token;
use tempo_zone_contracts::{IZoneInbox, ZONE_INBOX_ADDRESS, ZonePortal};

use crate::zone_utils::normalize_http_rpc;

const LOG_QUERY_BLOCK_CHUNK: u64 = 5_000;

#[derive(Debug, clap::Parser)]
pub(crate) struct VerifyPortalBacking {
    /// Tempo L1 HTTP RPC URL.
    #[arg(long, env = "L1_RPC_URL")]
    l1_rpc_url: String,

    /// Zone HTTP RPC URL.
    #[arg(long, env = "ZONE_RPC_URL")]
    zone_rpc_url: String,

    /// ZonePortal address on Tempo L1.
    #[arg(long)]
    portal: Address,

    /// TIP-20 token address, shared by Tempo L1 and the Zone.
    #[arg(long)]
    token: Address,

    /// First L1 block to scan. Must include the Portal's complete event history.
    #[arg(long, default_value_t = 0)]
    l1_from_block: u64,

    /// First Zone block to scan. Must include the ZoneInbox's complete event history.
    #[arg(long, default_value_t = 0)]
    zone_from_block: u64,
}

impl VerifyPortalBacking {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let l1_rpc_url = normalize_http_rpc(&self.l1_rpc_url);
        let zone_rpc_url = normalize_http_rpc(&self.zone_rpc_url);
        let l1 = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&l1_rpc_url)
            .await
            .wrap_err("failed connecting to Tempo L1")?;
        let zone = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&zone_rpc_url)
            .await
            .wrap_err("failed connecting to Zone RPC")?;

        let (l1_snapshot, zone_snapshot) =
            tokio::try_join!(l1.get_block_number(), zone.get_block_number(),)
                .wrap_err("failed reading snapshot blocks")?;
        ensure!(
            self.l1_from_block <= l1_snapshot,
            "L1 scan start {} is after snapshot block {l1_snapshot}",
            self.l1_from_block
        );
        ensure!(
            self.zone_from_block <= zone_snapshot,
            "Zone scan start {} is after snapshot block {zone_snapshot}",
            self.zone_from_block
        );
        let l1_block = BlockId::number(l1_snapshot);
        let zone_block = BlockId::number(zone_snapshot);
        let portal = ZonePortal::new(self.portal, &l1);
        let l1_token = TIP20Token::new(self.token, &l1);
        let zone_token = TIP20Token::new(self.token, &zone);
        let inbox = IZoneInbox::new(ZONE_INBOX_ADDRESS, &zone);

        let l1_reads = l1
            .multicall()
            .block(l1_block)
            .add(l1_token.balanceOf(self.portal))
            .add(portal.withdrawalQueueHead())
            .add(portal.withdrawalQueueTail())
            .add(portal.depositCount())
            .add(portal.lastProcessedDepositNumber());
        let zone_reads = zone
            .multicall()
            .block(zone_block)
            .add(zone_token.totalSupply())
            .add(inbox.processedDepositNumber());
        let (
            (
                portal_balance,
                withdrawal_head,
                withdrawal_tail,
                deposit_count,
                l1_processed_deposits,
            ),
            (zone_supply, zone_processed_deposits),
        ) = tokio::try_join!(l1_reads.aggregate(), zone_reads.aggregate())
            .wrap_err("failed reading backing state")?;

        ensure!(
            withdrawal_head == withdrawal_tail,
            "withdrawal queue is not settled: head={withdrawal_head}, tail={withdrawal_tail}"
        );
        ensure!(
            deposit_count == l1_processed_deposits,
            "L1 deposit queue is not settled: depositCount={deposit_count}, \
             lastProcessedDepositNumber={l1_processed_deposits}"
        );
        ensure!(
            zone_processed_deposits == l1_processed_deposits,
            "Zone and L1 deposit counters disagree: ZoneInbox={zone_processed_deposits}, \
             ZonePortal={l1_processed_deposits}"
        );

        let portal_refunds = portal_refund_liability(
            &l1,
            self.portal,
            self.token,
            self.l1_from_block,
            l1_snapshot,
        )
        .await?;
        let inbox_refunds =
            inbox_refund_liability(&zone, self.token, self.zone_from_block, zone_snapshot).await?;

        let required_backing = zone_supply
            .checked_add(portal_refunds)
            .and_then(|total| total.checked_add(inbox_refunds))
            .ok_or_else(|| eyre::eyre!("required backing overflow"))?;

        println!("Portal backing audit");
        println!("  L1 snapshot block:       {l1_snapshot}");
        println!("  Zone snapshot block:     {zone_snapshot}");
        println!(
            "  L1 refund scan:          {}..={l1_snapshot}",
            self.l1_from_block
        );
        println!(
            "  Zone refund scan:        {}..={zone_snapshot}",
            self.zone_from_block
        );
        println!("  Portal:                  {}", self.portal);
        println!("  Token:                   {}", self.token);
        println!("  Portal balance:          {portal_balance}");
        println!("  Zone total supply:       {zone_supply}");
        println!("  Portal refund liability: {portal_refunds}");
        println!("  Inbox refund liability:  {inbox_refunds}");
        println!("  Required backing:        {required_backing}");

        if portal_balance >= required_backing {
            println!(
                "  PASS: backing surplus    {}",
                portal_balance - required_backing
            );
            Ok(())
        } else {
            let deficit = required_backing - portal_balance;
            println!("  FAIL: backing deficit    {deficit}");
            Err(eyre::eyre!("Portal is underbacked by {deficit} base units"))
        }
    }
}

async fn portal_refund_liability<P: Provider<TempoNetwork>>(
    provider: &P,
    portal: Address,
    token: Address,
    from_block: u64,
    to_block: u64,
) -> eyre::Result<U256> {
    let portal = ZonePortal::new(portal, provider);
    let pending = portal
        .DepositBounceBackPending_filter()
        .from_block(from_block)
        .to_block(to_block)
        .chunked()
        .chunk_size(LOG_QUERY_BLOCK_CHUNK)
        .query()
        .await
        .wrap_err("failed scanning ZonePortal DepositBounceBackPending events")?;
    let claimed = portal
        .RefundClaimed_filter()
        .from_block(from_block)
        .to_block(to_block)
        .chunked()
        .chunk_size(LOG_QUERY_BLOCK_CHUNK)
        .query()
        .await
        .wrap_err("failed scanning ZonePortal RefundClaimed events")?;

    let pending_total = pending
        .into_iter()
        .filter(|(event, _)| event.token == token)
        .try_fold(U256::ZERO, |total, (event, _)| {
            total
                .checked_add(U256::from(event.amount))
                .ok_or_else(|| eyre::eyre!("Portal pending refund total overflow"))
        })?;
    let claimed_total = claimed
        .into_iter()
        .filter(|(event, _)| event.token == token)
        .try_fold(U256::ZERO, |total, (event, _)| {
            total
                .checked_add(U256::from(event.amount))
                .ok_or_else(|| eyre::eyre!("Portal claimed refund total overflow"))
        })?;

    outstanding_refunds("Portal", pending_total, claimed_total)
}

async fn inbox_refund_liability<P: Provider<TempoNetwork>>(
    provider: &P,
    token: Address,
    from_block: u64,
    to_block: u64,
) -> eyre::Result<U256> {
    let inbox = IZoneInbox::new(ZONE_INBOX_ADDRESS, provider);
    let pending = inbox
        .WithdrawalBounceBackPending_filter()
        .from_block(from_block)
        .to_block(to_block)
        .chunked()
        .chunk_size(LOG_QUERY_BLOCK_CHUNK)
        .query()
        .await
        .wrap_err("failed scanning ZoneInbox WithdrawalBounceBackPending events")?;
    let claimed = inbox
        .RefundClaimed_filter()
        .from_block(from_block)
        .to_block(to_block)
        .chunked()
        .chunk_size(LOG_QUERY_BLOCK_CHUNK)
        .query()
        .await
        .wrap_err("failed scanning ZoneInbox RefundClaimed events")?;

    let pending_total = pending
        .into_iter()
        .filter(|(event, _)| event.token == token)
        .try_fold(U256::ZERO, |total, (event, _)| {
            total
                .checked_add(U256::from(event.amount))
                .ok_or_else(|| eyre::eyre!("Inbox pending refund total overflow"))
        })?;
    let claimed_total = claimed
        .into_iter()
        .filter(|(event, _)| event.token == token)
        .try_fold(U256::ZERO, |total, (event, _)| {
            total
                .checked_add(U256::from(event.amount))
                .ok_or_else(|| eyre::eyre!("Inbox claimed refund total overflow"))
        })?;

    outstanding_refunds("Inbox", pending_total, claimed_total)
}

fn outstanding_refunds(scope: &str, pending: U256, claimed: U256) -> eyre::Result<U256> {
    pending.checked_sub(claimed).ok_or_else(|| {
        eyre::eyre!(
            "{scope} refund event history is incomplete: claimed {claimed}, pending {pending}"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outstanding_refunds_subtracts_claims() {
        assert_eq!(
            outstanding_refunds("test", U256::from(100), U256::from(35)).unwrap(),
            U256::from(65)
        );
    }

    #[test]
    fn outstanding_refunds_rejects_incomplete_history() {
        assert!(outstanding_refunds("test", U256::from(10), U256::from(11)).is_err());
    }
}
