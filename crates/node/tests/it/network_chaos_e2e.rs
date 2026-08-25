//! End-to-end tests for recovery from real TCP-level network failures.
//!
//! Unlike the restart tests, every node and background task remains alive. All nodes reach the real
//! Tempo L1 through one controlled TCP proxy, so disconnecting it closes every established RPC
//! socket and exercises the production reconnect and finalized backfill path.

use std::time::Duration;

use alloy::{primitives::U256, providers::Provider as _};
use alloy_network::ReceiptResponse as _;
use tempo_precompiles::PATH_USD_ADDRESS;
use tempo_primitives::transaction::calc_gas_balance_spending;
use tempo_zone_contracts::{IZoneOutbox, ZONE_OUTBOX_ADDRESS, ZONE_TOKEN_ADDRESS, ZonePortal};

use crate::utils::{
    RealP2pCluster, TcpChaosProxy, ZoneAccount, poll_until, start_real_p2p_cluster_with_l1_proxy,
    start_real_p2p_cluster_with_per_node_l1_proxies,
};

const NETWORK_TIMEOUT: Duration = Duration::from_secs(60);
const SUBMISSION_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const L1_BLOCK_TIME: Duration = Duration::from_millis(250);
const OUTAGE_BLOCK_GAP: u64 = 8;
const INITIAL_DEPOSIT: u128 = 10_000_000;
const OUTAGE_DEPOSIT: u128 = 1_000_000;
const OUTAGE_WITHDRAWAL: u128 = 250_000;

struct OutageAssetFlow {
    phase: &'static str,
    l2_balance_before: U256,
    l1_balance_before_withdrawal: U256,
    deposit_block: u64,
    withdrawal_hash: alloy::primitives::B256,
    withdrawal_fee: u128,
}

async fn batch_count(
    portal: &ZonePortal::ZonePortalInstance<alloy::providers::DynProvider>,
) -> eyre::Result<usize> {
    Ok(portal
        .BatchSubmitted_filter()
        .from_block(0)
        .query()
        .await?
        .len())
}

async fn wait_for_settled_height(
    portal: &ZonePortal::ZonePortalInstance<alloy::providers::DynProvider>,
    height: u64,
    description: &str,
) -> eyre::Result<U256> {
    poll_until(NETWORK_TIMEOUT, POLL_INTERVAL, description, || {
        async move {
            let settled_height = portal.zoneHeight().call().await?;
            Ok((settled_height >= U256::from(height)).then_some(settled_height))
        }
    })
    .await
}

async fn submit_outage_asset_flow(
    cluster: &RealP2pCluster,
    account: &mut ZoneAccount,
    phase: &'static str,
    outage_start: u64,
) -> eyre::Result<OutageAssetFlow> {
    let l2_balance_before = cluster.nodes[0]
        .balance_of(ZONE_TOKEN_ADDRESS, account.address())
        .await?;
    let deposit_block =
        tokio::time::timeout(SUBMISSION_TIMEOUT, account.submit_deposit(OUTAGE_DEPOSIT))
            .await
            .map_err(|_| eyre::eyre!("{phase} deposit submission timed out"))??;
    eyre::ensure!(
        deposit_block >= outage_start,
        "{phase} deposit landed before the L1 outage began"
    );

    let withdrawal_fee = IZoneOutbox::new(ZONE_OUTBOX_ADDRESS, cluster.nodes[0].provider())
        .calculateWithdrawalFee(0)
        .call()
        .await?;
    let l1_balance_before_withdrawal = cluster
        .l1
        .balance_of(PATH_USD_ADDRESS, account.address())
        .await?;
    let withdrawal_hash = tokio::time::timeout(
        SUBMISSION_TIMEOUT,
        account.submit_withdrawal(OUTAGE_WITHDRAWAL),
    )
    .await
    .map_err(|_| eyre::eyre!("{phase} withdrawal submission timed out"))??;

    Ok(OutageAssetFlow {
        phase,
        l2_balance_before,
        l1_balance_before_withdrawal,
        deposit_block,
        withdrawal_hash,
        withdrawal_fee,
    })
}

async fn assert_outage_asset_flow(
    cluster: &RealP2pCluster,
    account: &ZoneAccount,
    flow: OutageAssetFlow,
) -> eyre::Result<()> {
    let receipt = poll_until(
        NETWORK_TIMEOUT,
        POLL_INTERVAL,
        &format!(
            "{} withdrawal to be included after reconnection",
            flow.phase
        ),
        || {
            let provider = cluster.nodes[0].provider();
            async move {
                Ok(provider
                    .get_transaction_receipt(flow.withdrawal_hash)
                    .await?)
            }
        },
    )
    .await?;
    eyre::ensure!(receipt.status(), "{} withdrawal reverted", flow.phase);

    cluster
        .l1
        .wait_for_withdrawal_on_l1(
            cluster.portal_address,
            account.address(),
            flow.l1_balance_before_withdrawal,
            OUTAGE_WITHDRAWAL,
            NETWORK_TIMEOUT,
        )
        .await?;
    let l1_balance_after = cluster
        .l1
        .balance_of(PATH_USD_ADDRESS, account.address())
        .await?;
    assert_eq!(
        l1_balance_after,
        flow.l1_balance_before_withdrawal + U256::from(OUTAGE_WITHDRAWAL),
        "{} produced the wrong L1 balance after withdrawal",
        flow.phase
    );

    for node in &cluster.nodes {
        node.wait_for_tempo_block_number(flow.deposit_block, NETWORK_TIMEOUT)
            .await?;
    }
    let inclusion_block = receipt
        .block_number
        .ok_or_else(|| eyre::eyre!("{} withdrawal receipt has no block", flow.phase))?;
    cluster
        .wait_all_at(inclusion_block, NETWORK_TIMEOUT)
        .await?;

    let transaction_fee = calc_gas_balance_spending(receipt.gas_used, receipt.effective_gas_price);
    let expected_l2_balance = flow.l2_balance_before + U256::from(OUTAGE_DEPOSIT)
        - U256::from(OUTAGE_WITHDRAWAL)
        - U256::from(flow.withdrawal_fee)
        - transaction_fee;
    for (index, node) in cluster.nodes.iter().enumerate() {
        let balance = node
            .balance_of(ZONE_TOKEN_ADDRESS, account.address())
            .await?;
        assert_eq!(
            balance, expected_l2_balance,
            "{} produced the wrong L2 balance on node {index}",
            flow.phase
        );
    }
    Ok(())
}

async fn recover_both_followers(
    cluster: &RealP2pCluster,
    account: &mut ZoneAccount,
    follower_one_proxy: &TcpChaosProxy,
    follower_two_proxy: &TcpChaosProxy,
    baseline_height: u64,
) -> eyre::Result<()> {
    let accepted_before_outage = [
        follower_one_proxy.accepted_connections(),
        follower_two_proxy.accepted_connections(),
    ];
    for proxy in [follower_one_proxy, follower_two_proxy] {
        proxy.disconnect();
    }
    for proxy in [follower_one_proxy, follower_two_proxy] {
        proxy.wait_for_no_connections(NETWORK_TIMEOUT).await?;
    }

    let outage_start = cluster.l1.provider().get_block_number().await?;
    let assets =
        submit_outage_asset_flow(cluster, account, "both-follower outage", outage_start).await?;
    let target = (outage_start + OUTAGE_BLOCK_GAP).max(assets.deposit_block + 1);
    cluster.nodes[0]
        .wait_for_tempo_block_number(target, NETWORK_TIMEOUT)
        .await?;
    for (index, follower) in cluster.nodes[1..].iter().enumerate() {
        eyre::ensure!(
            follower.tempo_block_number().await? < target,
            "follower {} reached outage target {target} without an L1 connection",
            index + 1
        );
    }
    for proxy in [follower_one_proxy, follower_two_proxy] {
        proxy.resume();
    }
    for (proxy, accepted) in [follower_one_proxy, follower_two_proxy]
        .into_iter()
        .zip(accepted_before_outage)
    {
        proxy
            .wait_for_connections_after(accepted, 1, NETWORK_TIMEOUT)
            .await?;
    }
    for follower in &cluster.nodes[1..] {
        follower
            .wait_for_tempo_block_number(target, NETWORK_TIMEOUT)
            .await?;
    }

    let recovered_height = cluster.nodes[0].provider().get_block_number().await?;
    eyre::ensure!(
        recovered_height > baseline_height,
        "leader did not continue producing during the follower outage"
    );
    cluster
        .wait_all_at(recovered_height, NETWORK_TIMEOUT)
        .await?;
    cluster.assert_same_block(recovered_height).await?;
    let portal = ZonePortal::new(cluster.portal_address, cluster.l1.provider());
    wait_for_settled_height(
        &portal,
        recovered_height,
        "quorum settlement to resume after both followers catch up",
    )
    .await?;
    assert_outage_asset_flow(cluster, account, assets).await
}

async fn recover_leader(
    cluster: &RealP2pCluster,
    account: &mut ZoneAccount,
    leader_proxy: &TcpChaosProxy,
    follower_one_proxy: &TcpChaosProxy,
    follower_two_proxy: &TcpChaosProxy,
) -> eyre::Result<()> {
    let baseline_height = cluster.nodes[0].provider().get_block_number().await?;
    cluster
        .wait_all_at(baseline_height, NETWORK_TIMEOUT)
        .await?;
    cluster.assert_same_block(baseline_height).await?;
    leader_proxy
        .wait_for_connections_after(0, 1, NETWORK_TIMEOUT)
        .await?;
    let accepted_before_outage = leader_proxy.accepted_connections();
    leader_proxy.disconnect();
    leader_proxy
        .wait_for_no_connections(NETWORK_TIMEOUT)
        .await?;

    let outage_start = cluster.l1.provider().get_block_number().await?;
    let assets = submit_outage_asset_flow(cluster, account, "leader outage", outage_start).await?;
    let target = (outage_start + OUTAGE_BLOCK_GAP).max(assets.deposit_block + 1);
    for (index, follower) in cluster.nodes[1..].iter().enumerate() {
        poll_until(
            NETWORK_TIMEOUT,
            POLL_INTERVAL,
            &format!(
                "follower {} to observe L1 during the leader outage",
                index + 1
            ),
            || async {
                Ok(follower
                    .l1_block_tracker()
                    .latest()
                    .filter(|block| block.number >= target)
                    .map(|block| block.number))
            },
        )
        .await?;
    }
    eyre::ensure!(
        follower_one_proxy.active_connections() > 0 && follower_two_proxy.active_connections() > 0,
        "a follower L1 proxy lost connectivity during the leader-only outage"
    );
    for (index, node) in cluster.nodes.iter().enumerate() {
        eyre::ensure!(
            node.tempo_block_number().await? < target,
            "node {index} advanced zone state without a connected leader"
        );
    }
    leader_proxy.resume();
    leader_proxy
        .wait_for_connections_after(accepted_before_outage, 1, NETWORK_TIMEOUT)
        .await?;
    for node in &cluster.nodes {
        node.wait_for_tempo_block_number(target, NETWORK_TIMEOUT)
            .await?;
    }

    let recovered_height = cluster.nodes[0].provider().get_block_number().await?;
    eyre::ensure!(
        recovered_height > baseline_height,
        "leader did not produce the anchors missed during its L1 outage"
    );
    cluster
        .wait_all_at(recovered_height, NETWORK_TIMEOUT)
        .await?;
    cluster.assert_same_block(recovered_height).await?;
    let portal = ZonePortal::new(cluster.portal_address, cluster.l1.provider());
    wait_for_settled_height(
        &portal,
        recovered_height,
        "quorum settlement to resume after the leader catches up",
    )
    .await?;
    assert_outage_asset_flow(cluster, account, assets).await
}

async fn recover_single_follower(
    cluster: &RealP2pCluster,
    account: &mut ZoneAccount,
    leader_proxy: &TcpChaosProxy,
    follower_one_proxy: &TcpChaosProxy,
    follower_two_proxy: &TcpChaosProxy,
) -> eyre::Result<()> {
    let baseline_height = cluster.nodes[0].provider().get_block_number().await?;
    cluster
        .wait_all_at(baseline_height, NETWORK_TIMEOUT)
        .await?;
    cluster.assert_same_block(baseline_height).await?;
    let accepted_before_outage = follower_one_proxy.accepted_connections();
    follower_one_proxy.disconnect();
    follower_one_proxy
        .wait_for_no_connections(NETWORK_TIMEOUT)
        .await?;

    let outage_start = cluster.l1.provider().get_block_number().await?;
    let assets =
        submit_outage_asset_flow(cluster, account, "single-follower outage", outage_start).await?;
    let target = (outage_start + OUTAGE_BLOCK_GAP).max(assets.deposit_block + 1);
    cluster.nodes[0]
        .wait_for_tempo_block_number(target, NETWORK_TIMEOUT)
        .await?;
    cluster.nodes[2]
        .wait_for_tempo_block_number(target, NETWORK_TIMEOUT)
        .await?;
    eyre::ensure!(
        cluster.nodes[1].tempo_block_number().await? < target,
        "isolated follower reached outage target without an L1 connection"
    );
    eyre::ensure!(
        leader_proxy.active_connections() > 0 && follower_two_proxy.active_connections() > 0,
        "healthy quorum member lost L1 connectivity during the single-follower outage"
    );

    // Capture a leader-produced height after the target before checking the portal. A prior batch
    // may have already settled past the phase baseline, which would not prove 2-of-3 settlement.
    let leader_height_during_outage = cluster.nodes[0].provider().get_block_number().await?;
    let portal = ZonePortal::new(cluster.portal_address, cluster.l1.provider());
    wait_for_settled_height(
        &portal,
        leader_height_during_outage,
        "2-of-3 settlement to reach the leader height during the single-follower outage",
    )
    .await?;

    follower_one_proxy.resume();
    follower_one_proxy
        .wait_for_connections_after(accepted_before_outage, 1, NETWORK_TIMEOUT)
        .await?;
    cluster.nodes[1]
        .wait_for_tempo_block_number(target, NETWORK_TIMEOUT)
        .await?;

    let recovered_height = cluster.nodes[0].provider().get_block_number().await?;
    eyre::ensure!(
        recovered_height > baseline_height,
        "healthy quorum did not advance during the single-follower outage"
    );
    cluster
        .wait_all_at(recovered_height, NETWORK_TIMEOUT)
        .await?;
    cluster.assert_same_block(recovered_height).await?;
    assert_outage_asset_flow(cluster, account, assets).await
}

/// All three sequencers lose their established L1 connections, remain running while L1 advances,
/// reconnect through the same endpoint, backfill the finalized gap, and converge again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_three_sequencers_reconnect_and_catch_up_after_l1_network_outage() -> eyre::Result<()>
{
    reth_tracing::init_test_tracing();

    let (cluster, proxy) = start_real_p2p_cluster_with_l1_proxy(4, L1_BLOCK_TIME).await?;
    eyre::ensure!(
        cluster.nodes.len() == 3,
        "network-chaos fixture must start three nodes"
    );

    // Establish a healthy baseline: nodes have connected through the shared proxy and all hold
    // the same leader-produced chain, and the quorum has submitted at least one batch to L1.
    proxy
        .wait_for_connections_after(0, 3, NETWORK_TIMEOUT)
        .await?;
    let portal = ZonePortal::new(cluster.portal_address, cluster.l1.provider());
    poll_until(
        NETWORK_TIMEOUT,
        POLL_INTERVAL,
        "an initial settled batch before the network outage",
        || {
            let portal = &portal;
            async move {
                let count = batch_count(portal).await?;
                Ok((count > 0).then_some(count))
            }
        },
    )
    .await?;
    let baseline_height = cluster.nodes[0].provider().get_block_number().await?;
    cluster
        .wait_all_at(baseline_height, NETWORK_TIMEOUT)
        .await?;
    cluster.assert_same_block(baseline_height).await?;

    let accepted_before_outage = proxy.accepted_connections();
    proxy.disconnect();
    proxy.wait_for_no_connections(NETWORK_TIMEOUT).await?;
    eyre::ensure!(
        proxy.active_connections() == 0,
        "disconnect did not close every established L1 socket through {}",
        proxy.listen_addr()
    );

    // Build a gap that cannot have been prefetched before the sockets were closed.
    let outage_start = cluster.l1.provider().get_block_number().await?;
    let outage_target = outage_start + OUTAGE_BLOCK_GAP;
    poll_until(
        NETWORK_TIMEOUT,
        POLL_INTERVAL,
        "L1 to advance while every sequencer link is disconnected",
        || {
            let provider = cluster.l1.provider();
            async move {
                let height = provider.get_block_number().await?;
                Ok((height >= outage_target).then_some(height))
            }
        },
    )
    .await?;
    for (index, node) in cluster.nodes.iter().enumerate() {
        eyre::ensure!(
            node.tempo_block_number().await? < outage_target,
            "node {index} reached outage target {outage_target} without an L1 connection"
        );
    }

    // Restoring the listeners must result in new TCP connections, not reuse of the sockets that
    // were alive before the fault. Catch-up then proves those connections carry valid RPC traffic.
    proxy.resume();
    proxy
        .wait_for_connections_after(accepted_before_outage, 3, NETWORK_TIMEOUT)
        .await?;
    for node in &cluster.nodes {
        node.wait_for_tempo_block_number(outage_target, NETWORK_TIMEOUT)
            .await?;
    }

    // Compare a fixed recovered height so continued L1 production cannot move the assertion's
    // target while nodes are being sampled.
    let mut recovered_height = u64::MAX;
    for node in &cluster.nodes {
        recovered_height = recovered_height.min(node.provider().get_block_number().await?);
    }
    eyre::ensure!(
        recovered_height > baseline_height,
        "nodes did not advance beyond their pre-outage zone height"
    );
    cluster
        .wait_all_at(recovered_height, NETWORK_TIMEOUT)
        .await?;
    cluster.assert_same_block(recovered_height).await?;

    poll_until(
        NETWORK_TIMEOUT,
        POLL_INTERVAL,
        "batch settlement to resume after L1 reconnection",
        || {
            let portal = &portal;
            async move {
                let settled_height = portal.zoneHeight().call().await?;
                Ok((settled_height >= U256::from(recovered_height)).then_some(settled_height))
            }
        },
    )
    .await?;

    Ok(())
}

/// Exercise three asymmetric L1 outages: both followers, the leader, then one follower. Each
/// restored side must backfill its missed anchors, process a deposit and withdrawal submitted
/// during the outage, and converge before the next phase. The final phase also proves that one
/// healthy follower preserves 2-of-3 settlement.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_asymmetric_l1_outages_recover_followers_then_leader() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (cluster, [leader_proxy, follower_one_proxy, follower_two_proxy]) =
        start_real_p2p_cluster_with_per_node_l1_proxies(4, L1_BLOCK_TIME).await?;
    eyre::ensure!(
        cluster.nodes.len() == 3,
        "network-chaos fixture must start one leader and two followers"
    );
    for proxy in [&follower_one_proxy, &follower_two_proxy] {
        proxy
            .wait_for_connections_after(0, 1, NETWORK_TIMEOUT)
            .await?;
    }

    let portal = ZonePortal::new(cluster.portal_address, cluster.l1.provider());
    poll_until(
        NETWORK_TIMEOUT,
        POLL_INTERVAL,
        "an initial settled batch before disconnecting the followers",
        || {
            let portal = &portal;
            async move {
                let count = batch_count(portal).await?;
                Ok((count > 0).then_some(count))
            }
        },
    )
    .await?;

    // Seed one account before introducing faults so every outage can carry both a new L1 deposit
    // and an L2 withdrawal. Approval is also completed up front: during the leader outage the
    // withdrawal request itself may enter the pool, but no approval block can be produced.
    let mut account =
        ZoneAccount::from_l1_and_zone(&cluster.l1, &cluster.nodes[0], cluster.portal_address);
    cluster
        .l1
        .fund_user(
            account.address(),
            INITIAL_DEPOSIT + 3 * OUTAGE_DEPOSIT + 10_000_000,
        )
        .await?;
    let initial_balance = account
        .deposit(INITIAL_DEPOSIT, NETWORK_TIMEOUT, &cluster.nodes[0])
        .await?;
    assert_eq!(initial_balance, U256::from(INITIAL_DEPOSIT));
    for node in &cluster.nodes {
        node.wait_for_balance(
            ZONE_TOKEN_ADDRESS,
            account.address(),
            initial_balance,
            NETWORK_TIMEOUT,
        )
        .await?;
    }
    account.approve_outbox(ZONE_TOKEN_ADDRESS).await?;

    let baseline_height = cluster.nodes[0].provider().get_block_number().await?;
    cluster
        .wait_all_at(baseline_height, NETWORK_TIMEOUT)
        .await?;
    cluster.assert_same_block(baseline_height).await?;

    recover_both_followers(
        &cluster,
        &mut account,
        &follower_one_proxy,
        &follower_two_proxy,
        baseline_height,
    )
    .await?;

    // Phase 2 reverses the fault: followers observe L1, but zone state cannot move without the
    // disconnected leader.
    recover_leader(
        &cluster,
        &mut account,
        &leader_proxy,
        &follower_one_proxy,
        &follower_two_proxy,
    )
    .await?;

    // Phase 3 leaves a healthy 2-of-3 quorum while one follower catches up after reconnection.
    recover_single_follower(
        &cluster,
        &mut account,
        &leader_proxy,
        &follower_one_proxy,
        &follower_two_proxy,
    )
    .await?;

    Ok(())
}
