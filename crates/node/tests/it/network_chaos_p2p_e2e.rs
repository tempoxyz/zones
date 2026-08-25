//! End-to-end coverage for P2P link failures and latency.
//!
//! Every peer-to-peer connection crosses a controllable TCP proxy. Unlike restart tests, the
//! nodes, their transaction pools, and their role generations stay alive while links fail.

use std::time::Duration;

use alloy::{primitives::U256, providers::Provider as _};
use alloy_network::ReceiptResponse as _;
use tempo_zone_contracts::ZONE_TOKEN_ADDRESS;

use crate::utils::{
    P2pChaosNetwork, RealP2pCluster, ZoneAccount, poll_until, start_real_p2p_network_chaos_cluster,
};

const NETWORK_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const L1_BLOCK_TIME: Duration = Duration::from_millis(250);
const P2P_LATENCY: Duration = Duration::from_millis(400);
const L1_LATENCY: Duration = Duration::from_millis(500);
const RECOVERY_GAP: u64 = 6;
const INITIAL_DEPOSIT: u128 = 10_000_000;
const WITHDRAWAL_AMOUNT: u128 = 250_000;

async fn start_cluster() -> eyre::Result<(
    RealP2pCluster,
    P2pChaosNetwork,
    [crate::utils::TcpChaosProxy; 3],
)> {
    let fixture = start_real_p2p_network_chaos_cluster(4, L1_BLOCK_TIME).await?;
    let cluster = &fixture.0;
    eyre::ensure!(
        cluster.nodes.len() == 3,
        "P2P network-chaos fixture must start three nodes"
    );

    // The bootstrap leader waits for peer tip evidence before promotion. Reaching a common block
    // proves the proxy mesh carries authenticated traffic before a fault is introduced.
    cluster.nodes[0]
        .wait_for_block_number(1, NETWORK_TIMEOUT)
        .await?;
    let head = cluster.nodes[0].provider().get_block_number().await?;
    cluster.wait_all_at(head, NETWORK_TIMEOUT).await?;
    cluster.assert_same_block(head).await?;
    Ok(fixture)
}

async fn synchronized_head(cluster: &RealP2pCluster) -> eyre::Result<u64> {
    let head = cluster.nodes[0].provider().get_block_number().await?;
    cluster.wait_all_at(head, NETWORK_TIMEOUT).await?;
    cluster.assert_same_block(head).await?;
    Ok(head)
}

async fn assert_isolation_recovers(
    cluster: &RealP2pCluster,
    network: &P2pChaosNetwork,
    isolated: &[usize],
) -> eyre::Result<()> {
    synchronized_head(cluster).await?;
    network.disconnect_nodes(isolated);
    network
        .wait_for_nodes_disconnected(isolated, NETWORK_TIMEOUT)
        .await?;

    let outage_start = cluster.nodes[0].provider().get_block_number().await?;
    let target = outage_start + RECOVERY_GAP;
    cluster.nodes[0]
        .wait_for_block_number(target, NETWORK_TIMEOUT)
        .await?;

    // The leader produces independently of P2P. When it is isolated, both followers fall behind;
    // when the followers are isolated, those followers fall behind instead.
    let lagging: &[usize] = if isolated.contains(&0) {
        &[1, 2]
    } else {
        isolated
    };
    for index in lagging {
        eyre::ensure!(
            cluster.nodes[*index].provider().get_block_number().await? < target,
            "isolated node {index} reached block {target} without a P2P connection"
        );
    }

    network.resume_nodes(isolated);
    cluster.wait_all_at(target, NETWORK_TIMEOUT).await?;
    cluster.assert_same_block(target).await?;
    Ok(())
}

/// Disconnect every P2P path involving the leader. The leader continues producing from L1 while
/// both followers stop, then the followers backfill the missed blocks after reconnection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_followers_catch_up_after_leader_p2p_disconnect() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (cluster, network, _l1_proxies) = start_cluster().await?;
    assert_isolation_recovers(&cluster, &network, &[0]).await
}

/// Disconnect both followers from P2P while the leader continues producing, then verify both
/// followers backfill and converge on the leader's canonical chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_followers_catch_up_after_their_p2p_disconnect() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (cluster, network, _l1_proxies) = start_cluster().await?;
    assert_isolation_recovers(&cluster, &network, &[1, 2]).await
}

/// A transaction admitted to an isolated follower's pool remains pending locally. Once P2P is
/// restored, reconciliation forwards it to the leader, which includes it in a canonical block.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_follower_mempool_transaction_propagates_after_reconnect() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (cluster, network, _l1_proxies) = start_cluster().await?;
    let mut account =
        ZoneAccount::from_l1_and_zone(&cluster.l1, &cluster.nodes[1], cluster.portal_address);
    cluster
        .l1
        .fund_user(account.address(), INITIAL_DEPOSIT + 10_000_000)
        .await?;
    account
        .deposit(INITIAL_DEPOSIT, NETWORK_TIMEOUT, &cluster.nodes[1])
        .await?;
    for node in &cluster.nodes {
        node.wait_for_balance(
            ZONE_TOKEN_ADDRESS,
            account.address(),
            U256::from(INITIAL_DEPOSIT),
            NETWORK_TIMEOUT,
        )
        .await?;
    }
    account.approve_outbox(ZONE_TOKEN_ADDRESS).await?;

    let baseline = synchronized_head(&cluster).await?;
    network.disconnect_nodes(&[1]);
    network
        .wait_for_nodes_disconnected(&[1], NETWORK_TIMEOUT)
        .await?;
    cluster.nodes[0]
        .wait_for_block_number(baseline + 2, NETWORK_TIMEOUT)
        .await?;

    let transaction_hash = account.submit_withdrawal(WITHDRAWAL_AMOUNT).await?;
    eyre::ensure!(
        cluster.nodes[0]
            .provider()
            .get_transaction_by_hash(transaction_hash)
            .await?
            .is_none(),
        "leader received the follower transaction while all P2P links were disconnected"
    );

    network.resume_nodes(&[1]);
    poll_until(
        NETWORK_TIMEOUT,
        POLL_INTERVAL,
        "reconnected follower transaction to reach the leader",
        || {
            let provider = cluster.nodes[0].provider();
            async move { Ok(provider.get_transaction_by_hash(transaction_hash).await?) }
        },
    )
    .await?;
    let receipt = poll_until(
        NETWORK_TIMEOUT,
        POLL_INTERVAL,
        "forwarded follower transaction to be included by the leader",
        || {
            let provider = cluster.nodes[0].provider();
            async move { Ok(provider.get_transaction_receipt(transaction_hash).await?) }
        },
    )
    .await?;
    eyre::ensure!(receipt.status(), "forwarded withdrawal reverted");
    let inclusion_block = receipt
        .block_number
        .ok_or_else(|| eyre::eyre!("forwarded withdrawal receipt has no block number"))?;
    cluster
        .wait_all_at(inclusion_block, NETWORK_TIMEOUT)
        .await?;
    cluster.assert_same_block(inclusion_block).await?;
    Ok(())
}

/// A follower remains able to import and validate the leader's chain while every P2P stream
/// involving that follower carries substantial latency.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_follower_keeps_up_with_high_p2p_latency() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (cluster, network, _l1_proxies) = start_cluster().await?;
    synchronized_head(&cluster).await?;
    network.set_nodes_latency(&[1], P2P_LATENCY);

    let target = cluster.nodes[0].provider().get_block_number().await? + RECOVERY_GAP;
    cluster.nodes[0]
        .wait_for_block_number(target, NETWORK_TIMEOUT)
        .await?;
    cluster.wait_all_at(target, NETWORK_TIMEOUT).await?;
    cluster.assert_same_block(target).await?;
    Ok(())
}

/// A follower does not import leader blocks while their L1 anchors are withheld. Once responses
/// resume with substantial latency, it observes the anchors and converges with the healthy nodes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_follower_keeps_up_with_high_l1_latency() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (cluster, _network, [_leader_l1, follower_l1, _other_follower_l1]) =
        start_cluster().await?;
    synchronized_head(&cluster).await?;
    let follower_anchor = cluster.nodes[1].tempo_block_number().await?;
    let highest_observed_anchor = cluster.nodes[1]
        .l1_block_tracker()
        .latest()
        .map_or(follower_anchor, |anchor| anchor.number);
    let target_anchor = highest_observed_anchor + RECOVERY_GAP;

    follower_l1.set_client_to_upstream_latency(L1_LATENCY);
    follower_l1.set_upstream_to_client_latency(L1_LATENCY);
    follower_l1.pause_upstream_to_client(true);

    cluster.nodes[0]
        .wait_for_tempo_block_number(target_anchor, NETWORK_TIMEOUT)
        .await?;
    let target_head = cluster.nodes[0].provider().get_block_number().await?;
    eyre::ensure!(
        cluster.nodes[1].tempo_block_number().await? < target_anchor,
        "follower imported anchor {target_anchor} while its L1 responses were paused"
    );
    eyre::ensure!(
        cluster.nodes[1]
            .l1_block_tracker()
            .latest()
            .is_none_or(|anchor| anchor.number < target_anchor),
        "follower observed anchor {target_anchor} while its L1 responses were paused"
    );

    follower_l1.pause_upstream_to_client(false);
    cluster.nodes[1]
        .wait_for_tempo_block_number(target_anchor, NETWORK_TIMEOUT)
        .await?;
    cluster.wait_all_at(target_head, NETWORK_TIMEOUT).await?;
    cluster.assert_same_block(target_head).await?;
    Ok(())
}
