//! End-to-end coverage for P2P link failures and latency.
//!
//! Every peer-to-peer connection crosses a controllable TCP proxy. Unlike restart tests, the
//! nodes, their transaction pools, and their role generations stay alive while links fail.

use std::{collections::HashSet, time::Duration};

use alloy::{
    consensus::BlockHeader as _, eips::BlockNumberOrTag, primitives::U256, providers::Provider as _,
};
use alloy_network::ReceiptResponse as _;
use tempo_zone_contracts::{ZONE_TOKEN_ADDRESS, ZonePortal};

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

#[derive(Clone, Copy)]
struct LeadershipDisconnect {
    l1: bool,
    p2p: bool,
}

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

async fn rotate_leadership(cluster: &RealP2pCluster, target_index: usize) -> eyre::Result<u64> {
    let portal = ZonePortal::new(cluster.portal_address, cluster.l1.provider());
    let previous_epoch = portal.leaderEpoch().call().await?;
    let target = cluster
        .attestation_signers
        .get(target_index)
        .ok_or_else(|| eyre::eyre!("leadership target node {target_index} does not exist"))?
        .address();

    // Relay through A's operator RPC so this follows the same guarded, finalized-L1 path used by
    // an operator handoff. The target's connectivity is irrelevant to submitting the rotation.
    let response: serde_json::Value = cluster.nodes[0]
        .provider()
        .raw_request("zone_setLeader".into(), [target])
        .await?;
    eyre::ensure!(
        response.get("status").and_then(serde_json::Value::as_str) == Some("submitted"),
        "leadership rotation to node {target_index} was not submitted: {response}"
    );
    Ok(previous_epoch + 1)
}

async fn wait_for_leadership_epoch(
    cluster: &RealP2pCluster,
    nodes: &[usize],
    expected_epoch: u64,
    description: &str,
) -> eyre::Result<()> {
    poll_until(NETWORK_TIMEOUT, POLL_INTERVAL, description, || async {
        Ok(nodes
            .iter()
            .all(|&index| {
                cluster.nodes[index]
                    .leadership()
                    .latest()
                    .is_some_and(|state| state.epoch() >= expected_epoch)
            })
            .then_some(()))
    })
    .await
}

async fn wait_for_producer_block(
    cluster: &RealP2pCluster,
    producer_index: usize,
    after_height: u64,
    description: &str,
) -> eyre::Result<u64> {
    let producer = cluster
        .attestation_signers
        .get(producer_index)
        .ok_or_else(|| eyre::eyre!("producer node {producer_index} does not exist"))?
        .address();
    poll_until(NETWORK_TIMEOUT, POLL_INTERVAL, description, || async {
        let height = cluster.nodes[producer_index]
            .provider()
            .get_block_number()
            .await?;
        if height <= after_height {
            return Ok(None);
        }
        let block = cluster.nodes[producer_index]
            .provider()
            .get_block_by_number(BlockNumberOrTag::Number(height))
            .await?;
        Ok(block
            .filter(|block| block.header.beneficiary() == producer)
            .map(|_| height))
    })
    .await
}

async fn batch_count(
    portal: &ZonePortal::ZonePortalInstance<alloy::providers::DynProvider>,
) -> eyre::Result<usize> {
    Ok(portal
        .BatchSubmitted_1_filter()
        .from_block(0)
        .query()
        .await?
        .len())
}

async fn wait_for_settlement_after(
    portal: &ZonePortal::ZonePortalInstance<alloy::providers::DynProvider>,
    height: u64,
) -> eyre::Result<u64> {
    let settled: U256 = poll_until(
        NETWORK_TIMEOUT,
        POLL_INTERVAL,
        &format!("a batch beyond zone height {height} to settle"),
        || async {
            let settled = portal.zoneHeight().call().await?;
            Ok((settled > U256::from(height)).then_some(settled))
        },
    )
    .await?;
    settled
        .try_into()
        .map_err(|_| eyre::eyre!("settled zone height does not fit in u64"))
}

async fn assert_batch_history_is_canonical(
    cluster: &RealP2pCluster,
    portal: &ZonePortal::ZonePortalInstance<alloy::providers::DynProvider>,
) -> eyre::Result<u64> {
    let events = portal
        .BatchSubmitted_1_filter()
        .from_block(0)
        .query()
        .await?;
    eyre::ensure!(!events.is_empty(), "no batches were submitted");

    let mut indices = HashSet::with_capacity(events.len());
    let mut hashes = HashSet::with_capacity(events.len());
    for (offset, (event, _)) in events.iter().enumerate() {
        eyre::ensure!(
            indices.insert(event.withdrawalBatchIndex),
            "duplicate settlement batch index {}",
            event.withdrawalBatchIndex
        );
        eyre::ensure!(
            hashes.insert(event.nextBlockHash),
            "conflicting settlement reused zone block hash {}",
            event.nextBlockHash
        );
        if let Some((previous, _)) = offset.checked_sub(1).map(|index| &events[index]) {
            eyre::ensure!(
                event.withdrawalBatchIndex == previous.withdrawalBatchIndex + 1,
                "settlement batch indices are not contiguous: {} followed {}",
                event.withdrawalBatchIndex,
                previous.withdrawalBatchIndex
            );
        }
    }

    let settled_height: u64 = portal
        .zoneHeight()
        .call()
        .await?
        .try_into()
        .map_err(|_| eyre::eyre!("settled zone height does not fit in u64"))?;
    cluster.wait_all_at(settled_height, NETWORK_TIMEOUT).await?;
    let canonical = cluster.assert_same_block(settled_height).await?;
    eyre::ensure!(
        portal.blockHash().call().await? == canonical.hash,
        "Portal settled hash is not canonical at zone height {settled_height}"
    );
    Ok(settled_height)
}

async fn assert_leadership_rotation_recovers(
    cluster: &RealP2pCluster,
    network: &P2pChaosNetwork,
    b_l1: &crate::utils::TcpChaosProxy,
    disconnect: LeadershipDisconnect,
) -> eyre::Result<()> {
    let baseline = synchronized_head(cluster).await?;
    let accepted_before_outage = b_l1.accepted_connections();

    if disconnect.l1 {
        b_l1.disconnect();
        b_l1.wait_for_no_connections(NETWORK_TIMEOUT).await?;
    }
    if disconnect.p2p {
        network.disconnect_nodes(&[1]);
        network
            .wait_for_nodes_disconnected(&[1], NETWORK_TIMEOUT)
            .await?;
    }

    let expected_epoch = rotate_leadership(cluster, 1).await?;

    // A and C see the finalized transition and fence A at its activation boundary. B either has
    // not seen the transition (L1 outage), or cannot satisfy its P2P promotion barrier (P2P
    // outage), so no member may keep extending the chain before the requested links return.
    wait_for_leadership_epoch(
        cluster,
        &[0, 2],
        expected_epoch,
        "healthy nodes to observe the A→B leadership transition",
    )
    .await?;
    let a_fenced_height = cluster.nodes[0].provider().get_block_number().await?;
    let b_height_before_fence = cluster.nodes[1].provider().get_block_number().await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    eyre::ensure!(
        cluster.nodes[0].provider().get_block_number().await? == a_fenced_height,
        "outgoing leader A continued producing after finalized leadership moved to disconnected B"
    );
    let b_height_after_fence = cluster.nodes[1].provider().get_block_number().await?;
    let b_producer = cluster.attestation_signers[1].address();
    for height in b_height_before_fence + 1..=b_height_after_fence {
        let block = cluster.nodes[1]
            .provider()
            .get_block_by_number(BlockNumberOrTag::Number(height))
            .await?
            .ok_or_else(|| eyre::eyre!("B is missing block {height} from its local chain"))?;
        eyre::ensure!(
            block.header.beneficiary() != b_producer,
            "incoming leader B produced block {height} before its requested links were restored"
        );
    }

    if disconnect.l1 {
        b_l1.resume();
        b_l1.wait_for_connections_after(accepted_before_outage, 1, NETWORK_TIMEOUT)
            .await?;
    }
    if disconnect.p2p {
        network.resume_nodes(&[1]);
    }

    wait_for_leadership_epoch(
        cluster,
        &[1],
        expected_epoch,
        "reconnected B to observe its finalized leadership epoch",
    )
    .await?;

    let produced_height = wait_for_producer_block(
        cluster,
        1,
        baseline,
        "reconnected B to recover and produce a canonical block",
    )
    .await?;
    cluster
        .wait_all_at(produced_height, NETWORK_TIMEOUT)
        .await?;
    let header = cluster.assert_same_block(produced_height).await?;
    eyre::ensure!(
        header.beneficiary() == b_producer,
        "canonical recovery block {produced_height} was not produced by B"
    );

    // One further B-produced block proves this was a stable promotion, rather than a transient
    // block observed while the cluster was still reconciling the handoff.
    let next_height = produced_height + 1;
    cluster.nodes[1]
        .wait_for_block_number(next_height, NETWORK_TIMEOUT)
        .await?;
    cluster.wait_all_at(next_height, NETWORK_TIMEOUT).await?;
    let header = cluster.assert_same_block(next_height).await?;
    eyre::ensure!(
        header.beneficiary() == b_producer,
        "B did not retain leadership after recovery at block {next_height}"
    );
    Ok(())
}

/// B stays connected to the quorum but cannot observe L1 when the finalized A→B transition
/// occurs. Once its L1 link returns, it replays the missed transition and assumes leadership.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_incoming_leader_recovers_after_l1_disconnect() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (cluster, network, [_a_l1, b_l1, _c_l1]) = start_cluster().await?;
    assert_leadership_rotation_recovers(
        &cluster,
        &network,
        &b_l1,
        LeadershipDisconnect {
            l1: true,
            p2p: false,
        },
    )
    .await
}

/// B observes the finalized A→B transition on L1 while isolated from the quorum. Restoring its
/// P2P links lets it backfill, satisfy the promotion barrier, and assume leadership.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_incoming_leader_recovers_after_p2p_disconnect() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (cluster, network, [_a_l1, b_l1, _c_l1]) = start_cluster().await?;
    assert_leadership_rotation_recovers(
        &cluster,
        &network,
        &b_l1,
        LeadershipDisconnect {
            l1: false,
            p2p: true,
        },
    )
    .await
}

/// B is isolated from both L1 and the P2P quorum during the finalized A→B transition. Restoring
/// both planes lets it replay L1, recover the canonical P2P tip, and assume leadership.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_incoming_leader_recovers_after_l1_and_p2p_disconnect() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (cluster, network, [_a_l1, b_l1, _c_l1]) = start_cluster().await?;
    assert_leadership_rotation_recovers(
        &cluster,
        &network,
        &b_l1,
        LeadershipDisconnect {
            l1: true,
            p2p: true,
        },
    )
    .await
}

/// C loses both network planes while healthy A and B rotate leadership. The remaining 2-of-3
/// quorum must hand off and settle under B without waiting for C; C then reconnects, replays the
/// transition, and converges on the settled B-produced chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_handoff_and_settlement_continue_while_follower_is_disconnected() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (cluster, network, [_a_l1, _b_l1, c_l1]) = start_cluster().await?;
    let portal = ZonePortal::new(cluster.portal_address, cluster.l1.provider());
    eyre::ensure!(
        portal.sequencerThreshold().call().await? == 2,
        "test requires a 2-of-3 settlement quorum"
    );
    let baseline = synchronized_head(&cluster).await?;
    let c_accepted_before_outage = c_l1.accepted_connections();

    c_l1.disconnect();
    c_l1.wait_for_no_connections(NETWORK_TIMEOUT).await?;
    network.disconnect_nodes(&[2]);
    network
        .wait_for_nodes_disconnected(&[2], NETWORK_TIMEOUT)
        .await?;

    let expected_epoch = rotate_leadership(&cluster, 1).await?;
    wait_for_leadership_epoch(
        &cluster,
        &[0, 1],
        expected_epoch,
        "healthy A and B to observe the leadership transition",
    )
    .await?;
    let b_height = wait_for_producer_block(
        &cluster,
        1,
        baseline,
        "B to assume leadership while C is disconnected",
    )
    .await?;

    // Require a settlement strictly beyond the first observed B block. This rules out an
    // in-flight A-era submission and proves A+B alone formed a new certificate under leader B.
    let settled_height = wait_for_settlement_after(&portal, b_height).await?;
    for index in [0, 1] {
        cluster.nodes[index]
            .wait_for_block_number(settled_height, NETWORK_TIMEOUT)
            .await?;
    }
    let a_block = cluster.nodes[0]
        .provider()
        .get_block_by_number(BlockNumberOrTag::Number(settled_height))
        .await?
        .ok_or_else(|| eyre::eyre!("A is missing settled block {settled_height}"))?;
    let b_block = cluster.nodes[1]
        .provider()
        .get_block_by_number(BlockNumberOrTag::Number(settled_height))
        .await?
        .ok_or_else(|| eyre::eyre!("B is missing settled block {settled_height}"))?;
    eyre::ensure!(
        a_block.header.hash == b_block.header.hash,
        "healthy A and B diverged at settled height {settled_height}"
    );
    eyre::ensure!(
        b_block.header.beneficiary() == cluster.attestation_signers[1].address(),
        "settled block {settled_height} was not produced under leader B"
    );
    eyre::ensure!(
        cluster.nodes[2].provider().get_block_number().await? < settled_height,
        "isolated C reached the B-era settlement height without either network plane"
    );

    c_l1.resume();
    c_l1.wait_for_connections_after(c_accepted_before_outage, 1, NETWORK_TIMEOUT)
        .await?;
    network.resume_nodes(&[2]);
    wait_for_leadership_epoch(
        &cluster,
        &[2],
        expected_epoch,
        "reconnected C to replay the A→B leadership transition",
    )
    .await?;
    cluster.wait_all_at(settled_height, NETWORK_TIMEOUT).await?;
    cluster.assert_same_block(settled_height).await?;
    Ok(())
}

/// B is P2P-isolated during an A→B handoff and retains a withdrawal in its local pool. After
/// reconnection, B must include the withdrawal, making that block a batch boundary, and settle it
/// on the canonical chain without duplicate indices or conflicting block hashes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_handoff_recovers_across_settlement_boundary() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (cluster, network, _l1_proxies) = start_cluster().await?;
    let portal = ZonePortal::new(cluster.portal_address, cluster.l1.provider());
    poll_until(
        NETWORK_TIMEOUT,
        POLL_INTERVAL,
        "an initial A-era settlement",
        || {
            let portal = &portal;
            async move {
                let count = batch_count(portal).await?;
                Ok((count > 0).then_some(count))
            }
        },
    )
    .await?;
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
    let expected_epoch = rotate_leadership(&cluster, 1).await?;
    wait_for_leadership_epoch(
        &cluster,
        &[0, 1, 2],
        expected_epoch,
        "all nodes to observe B's leadership at the settlement boundary",
    )
    .await?;

    // The transaction remains only in B's local pool while every P2P path involving B is down.
    // Its eventual inclusion under B creates the settlement boundary this test follows.
    let a_fenced_height = cluster.nodes[0].provider().get_block_number().await?;
    let b_fenced_height = cluster.nodes[1].provider().get_block_number().await?;
    let withdrawal_hash = account.submit_withdrawal(WITHDRAWAL_AMOUNT).await?;
    eyre::ensure!(
        cluster.nodes[0]
            .provider()
            .get_transaction_by_hash(withdrawal_hash)
            .await?
            .is_none(),
        "A received B's withdrawal while their P2P links were disconnected"
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    eyre::ensure!(
        cluster.nodes[0].provider().get_block_number().await? == a_fenced_height,
        "outgoing leader A produced while incoming leader B was P2P-isolated"
    );
    eyre::ensure!(
        cluster.nodes[1].provider().get_block_number().await? == b_fenced_height,
        "incoming leader B produced a private block while P2P-isolated"
    );
    eyre::ensure!(
        cluster.nodes[1]
            .provider()
            .get_transaction_receipt(withdrawal_hash)
            .await?
            .is_none(),
        "B included the withdrawal before its P2P links were restored"
    );
    let batches_at_fence = batch_count(&portal).await?;

    network.resume_nodes(&[1]);
    let receipt = poll_until(
        NETWORK_TIMEOUT,
        POLL_INTERVAL,
        "B to include the withdrawal after assuming leadership",
        || {
            let provider = cluster.nodes[1].provider();
            async move { Ok(provider.get_transaction_receipt(withdrawal_hash).await?) }
        },
    )
    .await?;
    eyre::ensure!(receipt.status(), "B-era withdrawal reverted");
    let boundary_height = receipt
        .block_number
        .ok_or_else(|| eyre::eyre!("B-era withdrawal receipt has no block number"))?;
    eyre::ensure!(
        boundary_height > baseline,
        "withdrawal was included before the leadership handoff"
    );
    cluster
        .wait_all_at(boundary_height, NETWORK_TIMEOUT)
        .await?;
    let boundary = cluster.assert_same_block(boundary_height).await?;
    eyre::ensure!(
        boundary.beneficiary() == cluster.attestation_signers[1].address(),
        "withdrawal boundary block {boundary_height} was not produced under leader B"
    );
    let boundary_hash = boundary.hash;

    poll_until(
        NETWORK_TIMEOUT,
        POLL_INTERVAL,
        "B's withdrawal boundary block to settle",
        || {
            let portal = &portal;
            async move {
                let events = portal
                    .BatchSubmitted_1_filter()
                    .from_block(0)
                    .query()
                    .await?;
                Ok(events
                    .iter()
                    .any(|(event, _)| event.nextBlockHash == boundary_hash)
                    .then_some(events.len()))
            }
        },
    )
    .await?;
    eyre::ensure!(
        batch_count(&portal).await? > batches_at_fence,
        "withdrawal boundary did not create a post-handoff batch"
    );

    let settled_height = assert_batch_history_is_canonical(&cluster, &portal).await?;
    eyre::ensure!(
        settled_height >= boundary_height,
        "settlement stopped before B's withdrawal boundary: {settled_height} < {boundary_height}"
    );
    let header = cluster.assert_same_block(settled_height).await?;
    eyre::ensure!(
        header.beneficiary() == cluster.attestation_signers[1].address(),
        "post-handoff settlement at {settled_height} was not produced under leader B"
    );
    Ok(())
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
