//! Chain-derived token-backing verification for a ZonePortal, including queued flows.
//!
//! The verifier does not predict protocol transitions. It reconstructs every
//! outstanding liability from pinned chain state and complete lifecycle event
//! histories, returning a serializable report suitable for CLIs, workload
//! generators, and failure artifacts.

use alloy::{
    primitives::{Address, B256, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::BlockId,
};
use eyre::{WrapErr as _, ensure};
use serde::{Deserialize, Serialize};
use tempo_alloy::TempoNetwork;
use tempo_contracts::precompiles::ITIP20 as TIP20Token;
use tempo_zone_contracts::{
    IZoneInbox, ZONE_FACTORY_ADDRESS, ZONE_INBOX_ADDRESS, ZoneFactory, ZonePortal,
};
use zone_primitives::constants::zone_chain_id;

const LOG_QUERY_BLOCK_CHUNK: u64 = 5_000;

#[derive(Clone, Copy)]
struct EventRanges {
    l1_from: u64,
    l1_to: u64,
    zone_from: u64,
    zone_to: u64,
}

/// Inputs that identify one Portal/token pair and its complete event ranges.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct PortalBackingRequest {
    /// ZonePortal address on Tempo L1.
    pub portal: Address,

    /// TIP-20 token address, shared by Tempo L1 and the Zone.
    pub token: Address,

    /// First L1 block to scan. Must include the Portal's complete event history.
    pub l1_from_block: u64,

    /// First Zone block to scan. Must include the ZoneInbox's complete event history.
    pub zone_from_block: u64,
}

/// Complete evidence used to decide the Portal backing invariant.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PortalBackingReport {
    pub portal: Address,
    pub token: Address,
    pub l1_snapshot_block: u64,
    pub zone_snapshot_block: u64,
    pub l1_snapshot_hash: B256,
    pub zone_snapshot_hash: B256,
    pub l1_from_block: u64,
    pub zone_from_block: u64,
    pub l1_chain_id: u64,
    pub zone_chain_id: u64,
    pub portal_zone_id: u32,
    pub portal_balance: U256,
    pub zone_total_supply: U256,
    pub deposit_count: u64,
    pub l1_processed_deposits: u64,
    pub zone_processed_deposits: u64,
    pub withdrawal_queue_head: U256,
    pub withdrawal_queue_tail: U256,
    pub pending_deposit_liability: U256,
    pub pending_withdrawal_liability: U256,
    pub portal_refund_liability: U256,
    pub inbox_refund_liability: U256,
    pub required_backing: U256,
    pub backing_surplus: U256,
    pub backing_deficit: U256,
}

impl PortalBackingReport {
    /// Whether the observed Portal balance covers every reconstructed liability.
    pub fn is_solvent(&self) -> bool {
        self.backing_deficit.is_zero()
    }

    /// Fail with the invariant violation while preserving the report for callers
    /// that want to serialize it before returning an error.
    pub fn ensure_solvent(&self) -> eyre::Result<()> {
        ensure!(
            self.is_solvent(),
            "Portal is underbacked by {} base units",
            self.backing_deficit
        );
        Ok(())
    }
}

/// Audit a Portal/token pair using pinned L1 and Zone snapshots.
///
/// Both scan starts must cover the complete corresponding event history. The
/// verifier fails closed when terminal event totals exceed their originating
/// events, which is the observable signal of an incomplete history.
pub async fn audit_portal_backing<L1, Zone>(
    l1: &L1,
    zone: &Zone,
    request: PortalBackingRequest,
) -> eyre::Result<PortalBackingReport>
where
    L1: Provider<TempoNetwork>,
    Zone: Provider<TempoNetwork>,
{
    let (l1_snapshot, zone_snapshot, l1_chain_id, actual_zone_chain_id) = tokio::try_join!(
        l1.get_block_number(),
        zone.get_block_number(),
        l1.get_chain_id(),
        zone.get_chain_id(),
    )
    .wrap_err("failed reading snapshot blocks and chain IDs")?;
    ensure!(
        request.l1_from_block <= l1_snapshot,
        "L1 scan start {} is after snapshot block {l1_snapshot}",
        request.l1_from_block
    );
    ensure!(
        request.zone_from_block <= zone_snapshot,
        "Zone scan start {} is after snapshot block {zone_snapshot}",
        request.zone_from_block
    );
    let l1_block = BlockId::number(l1_snapshot);
    let zone_block = BlockId::number(zone_snapshot);
    let (l1_snapshot_hash, zone_snapshot_hash) = tokio::try_join!(
        header_hash(l1, l1_snapshot, "L1"),
        header_hash(zone, zone_snapshot, "Zone"),
    )?;
    let portal = ZonePortal::new(request.portal, l1);
    let factory = ZoneFactory::new(ZONE_FACTORY_ADDRESS, l1);
    let l1_token = TIP20Token::new(request.token, l1);
    let zone_token = TIP20Token::new(request.token, zone);
    let inbox = IZoneInbox::new(ZONE_INBOX_ADDRESS, zone);

    let l1_reads = l1
        .multicall()
        .block(l1_block)
        .add(l1_token.balanceOf(request.portal))
        .add(portal.withdrawalQueueHead())
        .add(portal.withdrawalQueueTail())
        .add(portal.depositCount())
        .add(portal.lastProcessedDepositNumber())
        .add(portal.zoneId())
        .add(portal.isTokenEnabled(request.token))
        .add(factory.isZonePortal(request.portal));
    let zone_reads = zone
        .multicall()
        .block(zone_block)
        .add(zone_token.totalSupply())
        .add(inbox.processedDepositNumber())
        .add(inbox.tempoPortal());
    let (
        (
            portal_balance,
            withdrawal_head,
            withdrawal_tail,
            deposit_count,
            l1_processed_deposits,
            portal_zone_id,
            token_enabled,
            portal_registered,
        ),
        (zone_supply, zone_processed_deposits, inbox_portal),
    ) = tokio::try_join!(l1_reads.aggregate(), zone_reads.aggregate())
        .wrap_err("failed reading backing state")?;

    let factory_zone = factory
        .zones(portal_zone_id)
        .block(l1_block)
        .call()
        .await
        .wrap_err("failed reading ZoneFactory registration")?;
    let expected_zone_chain_id =
        zone_chain_id(l1_chain_id, portal_zone_id).wrap_err("failed deriving Zone chain ID")?;

    ensure!(portal_registered, "Portal is not registered in ZoneFactory");
    ensure!(
        factory_zone.portal == request.portal && factory_zone.zoneId == portal_zone_id,
        "Portal does not match ZoneFactory registration for zone {portal_zone_id}"
    );
    ensure!(
        inbox_portal == request.portal,
        "ZoneInbox Portal mismatch: expected {}, got {inbox_portal}",
        request.portal
    );
    ensure!(
        actual_zone_chain_id == expected_zone_chain_id,
        "Zone chain ID mismatch: expected {expected_zone_chain_id} for L1 chain {l1_chain_id} and zone {portal_zone_id}, got {actual_zone_chain_id}"
    );
    ensure!(token_enabled, "Token is not enabled on the Portal");

    ensure!(
        zone_processed_deposits <= deposit_count,
        "Zone processed more deposits than the Portal has recorded: ZoneInbox={zone_processed_deposits}, \
             ZonePortal={deposit_count}"
    );

    let portal_refunds = portal_refund_liability(
        l1,
        request.portal,
        request.token,
        request.l1_from_block,
        l1_snapshot,
    )
    .await?;
    let inbox_refunds =
        inbox_refund_liability(zone, request.token, request.zone_from_block, zone_snapshot).await?;
    let pending_deposits = pending_deposit_liability(
        l1,
        request.portal,
        request.token,
        zone_processed_deposits,
        request.l1_from_block,
        l1_snapshot,
    )
    .await?;
    let pending_withdrawals = withdrawal_liability(
        l1,
        zone,
        request.portal,
        request.token,
        EventRanges {
            l1_from: request.l1_from_block,
            l1_to: l1_snapshot,
            zone_from: request.zone_from_block,
            zone_to: zone_snapshot,
        },
    )
    .await?;

    let required_backing = zone_supply
        .checked_add(portal_refunds)
        .and_then(|total| total.checked_add(inbox_refunds))
        .and_then(|total| total.checked_add(pending_deposits))
        .and_then(|total| total.checked_add(pending_withdrawals))
        .ok_or_else(|| eyre::eyre!("required backing overflow"))?;

    let backing_surplus = portal_balance.saturating_sub(required_backing);
    let backing_deficit = required_backing.saturating_sub(portal_balance);

    let (final_l1_hash, final_zone_hash) = tokio::try_join!(
        header_hash(l1, l1_snapshot, "L1"),
        header_hash(zone, zone_snapshot, "Zone"),
    )?;
    ensure!(
        final_l1_hash == l1_snapshot_hash,
        "L1 snapshot block {l1_snapshot} reorged during the audit: started at {l1_snapshot_hash}, ended at {final_l1_hash}"
    );
    ensure!(
        final_zone_hash == zone_snapshot_hash,
        "Zone snapshot block {zone_snapshot} reorged during the audit: started at {zone_snapshot_hash}, ended at {final_zone_hash}"
    );

    Ok(PortalBackingReport {
        portal: request.portal,
        token: request.token,
        l1_snapshot_block: l1_snapshot,
        zone_snapshot_block: zone_snapshot,
        l1_snapshot_hash,
        zone_snapshot_hash,
        l1_from_block: request.l1_from_block,
        zone_from_block: request.zone_from_block,
        l1_chain_id,
        zone_chain_id: actual_zone_chain_id,
        portal_zone_id,
        portal_balance,
        zone_total_supply: zone_supply,
        deposit_count,
        l1_processed_deposits,
        zone_processed_deposits,
        withdrawal_queue_head: withdrawal_head,
        withdrawal_queue_tail: withdrawal_tail,
        pending_deposit_liability: pending_deposits,
        pending_withdrawal_liability: pending_withdrawals,
        portal_refund_liability: portal_refunds,
        inbox_refund_liability: inbox_refunds,
        required_backing,
        backing_surplus,
        backing_deficit,
    })
}

async fn header_hash<P: Provider<TempoNetwork>>(
    provider: &P,
    number: u64,
    layer: &str,
) -> eyre::Result<B256> {
    provider
        .get_header_by_number(number.into())
        .await
        .wrap_err_with(|| format!("failed reading {layer} snapshot header {number}"))?
        .map(|header| header.inner.hash)
        .ok_or_else(|| eyre::eyre!("{layer} snapshot header {number} not found"))
}

/// Connect to Tempo L1 and the full operator Zone RPC, then run an audit.
///
/// This entry point lets external workload generators share the authoritative
/// verifier without depending on the same Alloy provider version.
pub async fn audit_portal_backing_rpc(
    l1_rpc_url: &str,
    zone_rpc_url: &str,
    request: PortalBackingRequest,
) -> eyre::Result<PortalBackingReport> {
    let l1 = ProviderBuilder::new_with_network::<TempoNetwork>()
        .connect(l1_rpc_url)
        .await
        .wrap_err("failed connecting to Tempo L1")?;
    let zone = ProviderBuilder::new_with_network::<TempoNetwork>()
        .connect(zone_rpc_url)
        .await
        .wrap_err("failed connecting to full operator Zone RPC")?;
    audit_portal_backing(&l1, &zone, request).await
}

/// Deposits after the Zone snapshot's processed-deposit watermark have reached the Portal but
/// have not yet been minted on the Zone. `WithdrawalBounceBack` entries are intentionally not
/// counted here: they remain represented by their original outstanding withdrawal until the
/// Inbox either re-mints them or turns them into an Inbox refund.
async fn pending_deposit_liability<P: Provider<TempoNetwork>>(
    provider: &P,
    portal_address: Address,
    token: Address,
    zone_processed_deposits: u64,
    from_block: u64,
    to_block: u64,
) -> eyre::Result<U256> {
    let portal = ZonePortal::new(portal_address, provider);
    let deposits = portal
        .DepositMade_filter()
        .from_block(from_block)
        .to_block(to_block)
        .chunked()
        .chunk_size(LOG_QUERY_BLOCK_CHUNK)
        .query()
        .await
        .wrap_err("failed scanning ZonePortal DepositMade events")?;

    deposits
        .into_iter()
        .filter(|(event, _)| event.token == token && event.depositNumber > zone_processed_deposits)
        .try_fold(U256::ZERO, |total, (event, _)| {
            total
                .checked_add(U256::from(event.netAmount))
                .ok_or_else(|| eyre::eyre!("pending deposit total overflow"))
        })
}

/// Tracks every burned withdrawal until it is paid on L1, re-minted on the Zone, or moved into a
/// Portal or Inbox refund registry. This covers both finalized Portal queue entries and
/// withdrawals still waiting in the Zone outbox.
async fn withdrawal_liability<L1, Zone>(
    l1: &L1,
    zone: &Zone,
    portal_address: Address,
    token: Address,
    ranges: EventRanges,
) -> eyre::Result<U256>
where
    L1: Provider<TempoNetwork>,
    Zone: Provider<TempoNetwork>,
{
    let portal = ZonePortal::new(portal_address, l1);
    let inbox = IZoneInbox::new(ZONE_INBOX_ADDRESS, zone);
    let outbox =
        tempo_zone_contracts::IZoneOutbox::new(tempo_zone_contracts::ZONE_OUTBOX_ADDRESS, zone);

    let requested_filter = outbox
        .WithdrawalRequested_filter()
        .from_block(ranges.zone_from)
        .to_block(ranges.zone_to)
        .chunked()
        .chunk_size(LOG_QUERY_BLOCK_CHUNK);
    let paid_filter = portal
        .WithdrawalProcessed_filter()
        .from_block(ranges.l1_from)
        .to_block(ranges.l1_to)
        .chunked()
        .chunk_size(LOG_QUERY_BLOCK_CHUNK);
    let reminted_filter = inbox
        .WithdrawalBounceBackProcessed_filter()
        .from_block(ranges.zone_from)
        .to_block(ranges.zone_to)
        .chunked()
        .chunk_size(LOG_QUERY_BLOCK_CHUNK);
    let refunded_filter = inbox
        .WithdrawalBounceBackPending_filter()
        .from_block(ranges.zone_from)
        .to_block(ranges.zone_to)
        .chunked()
        .chunk_size(LOG_QUERY_BLOCK_CHUNK);
    let deposit_bounce_back_filter = portal
        .DepositBounceBack_filter()
        .from_block(ranges.l1_from)
        .to_block(ranges.l1_to)
        .chunked()
        .chunk_size(LOG_QUERY_BLOCK_CHUNK);
    let portal_refund_filter = portal
        .DepositBounceBackPending_filter()
        .from_block(ranges.l1_from)
        .to_block(ranges.l1_to)
        .chunked()
        .chunk_size(LOG_QUERY_BLOCK_CHUNK);
    let (requested, paid, deposit_bounce_backs, portal_refunds, reminted, refunded) =
        tokio::try_join!(
            requested_filter.query(),
            paid_filter.query(),
            deposit_bounce_back_filter.query(),
            portal_refund_filter.query(),
            reminted_filter.query(),
            refunded_filter.query(),
        )
        .wrap_err("failed scanning withdrawal lifecycle events")?;

    let requested = requested
        .into_iter()
        .filter(|(event, _)| event.token == token)
        .try_fold(U256::ZERO, |total, (event, _)| {
            total
                .checked_add(U256::from(event.amount))
                .ok_or_else(|| eyre::eyre!("requested withdrawal total overflow"))
        })?;
    let paid = paid
        .into_iter()
        .filter(|(event, _)| event.token == token && event.callbackSuccess)
        .try_fold(U256::ZERO, |total, (event, _)| {
            total
                .checked_add(U256::from(event.amount))
                .ok_or_else(|| eyre::eyre!("paid withdrawal total overflow"))
        })?;
    let deposit_bounce_backs = deposit_bounce_backs
        .into_iter()
        .filter(|(event, _)| event.token == token)
        .try_fold(U256::ZERO, |total, (event, _)| {
            total
                .checked_add(deposit_bounce_back_retired_amount(
                    event.amount,
                    event.bouncebackFee,
                ))
                .ok_or_else(|| eyre::eyre!("paid deposit bounce-back total overflow"))
        })?;
    let paid = paid
        .checked_add(deposit_bounce_backs)
        .ok_or_else(|| eyre::eyre!("paid withdrawal total overflow"))?;
    let reminted = reminted
        .into_iter()
        .filter(|(event, _)| event.token == token)
        .try_fold(U256::ZERO, |total, (event, _)| {
            total
                .checked_add(U256::from(event.amount))
                .ok_or_else(|| eyre::eyre!("re-minted withdrawal bounce-back total overflow"))
        })?;
    let portal_refunds = portal_refunds
        .into_iter()
        .filter(|(event, _)| event.token == token)
        .try_fold(U256::ZERO, |total, (event, _)| {
            total
                .checked_add(deposit_bounce_back_retired_amount(
                    event.amount,
                    event.bouncebackFee,
                ))
                .ok_or_else(|| eyre::eyre!("Portal refund transition total overflow"))
        })?;
    let refunded = refunded
        .into_iter()
        .filter(|(event, _)| event.token == token)
        .try_fold(U256::ZERO, |total, (event, _)| {
            total
                .checked_add(U256::from(event.amount))
                .ok_or_else(|| eyre::eyre!("refunded withdrawal bounce-back total overflow"))
        })?
        .checked_add(portal_refunds)
        .ok_or_else(|| eyre::eyre!("refunded withdrawal total overflow"))?;

    outstanding_withdrawals(requested, paid, reminted, refunded)
}

fn deposit_bounce_back_retired_amount(amount: u128, bounceback_fee: u128) -> U256 {
    U256::from(amount) + U256::from(bounceback_fee)
}

fn outstanding_withdrawals(
    requested: U256,
    paid: U256,
    reminted: U256,
    refunded: U256,
) -> eyre::Result<U256> {
    requested
        .checked_sub(paid)
        .and_then(|total| total.checked_sub(reminted))
        .and_then(|total| total.checked_sub(refunded))
        .ok_or_else(|| {
            eyre::eyre!(
                "withdrawal event history is incomplete: requested={requested}, paid={paid}, \
                 reminted={reminted}, refunded={refunded}"
            )
        })
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

    #[test]
    fn outstanding_withdrawals_tracks_all_terminal_paths() {
        assert_eq!(
            outstanding_withdrawals(
                U256::from(100),
                U256::from(25),
                U256::from(30),
                U256::from(20),
            )
            .unwrap(),
            U256::from(25)
        );
    }

    #[test]
    fn outstanding_withdrawals_rejects_incomplete_history() {
        assert!(
            outstanding_withdrawals(U256::from(10), U256::from(11), U256::ZERO, U256::ZERO,)
                .is_err()
        );
    }

    #[test]
    fn successful_deposit_bounce_back_retires_refund_and_fee() {
        let retired = deposit_bounce_back_retired_amount(990, 10);

        assert_eq!(
            outstanding_withdrawals(U256::from(1_000), retired, U256::ZERO, U256::ZERO).unwrap(),
            U256::ZERO
        );
    }

    #[test]
    fn pending_deposit_bounce_back_retires_fee_and_tracks_refund() {
        let retired = deposit_bounce_back_retired_amount(990, 10);

        assert_eq!(
            outstanding_withdrawals(U256::from(1_000), U256::ZERO, U256::ZERO, retired).unwrap(),
            U256::ZERO
        );
        assert_eq!(
            outstanding_refunds("Portal", U256::from(990), U256::ZERO).unwrap(),
            U256::from(990)
        );
    }
}
