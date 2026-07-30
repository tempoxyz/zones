//! Zero-downtime leadership handoff e2e tests.
//!
//! A three-node cluster runs the real role controller on every node: the leader generation
//! (engine with the per-anchor production permit, broadcast, settlement, sequencer tasks)
//! and the follower generation (anchor-gated import, transaction forwarding). Tests publish
//! finalized leadership transitions directly into each node's `LeadershipSchedule`, standing
//! in for the receipt-authenticated `LeaderUpdated` observations of a real L1 subscriber,
//! and drive L1 consumption through deposit-queue injection.

use std::time::Duration;

use alloy::primitives::{U256, address};
use alloy_provider::Provider;
use tempo_chainspec::spec::TEMPO_T0_BASE_FEE;
use tempo_contracts::precompiles::ITIP20;
use tempo_precompiles::PATH_USD_ADDRESS;

use crate::utils::{
    DEFAULT_POLL, DEFAULT_TIMEOUT, L1Fixture, TIP20_TX_GAS, local_dev_zone_account, poll_until,
    start_local_p2p_cluster,
};

/// Ceiling for one leadership switch: covers the Commonware handshake, the promotion
/// barrier's tip-evidence round trip, and the generation swap.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a fenced or demoted node is watched to confirm it produces nothing.
const QUIESCENCE: Duration = Duration::from_secs(3);
/// Live-propagation ceiling during the observation→activation window: well below the
/// follower's 30-second inactivity backfill probe, so a routing regression that degrades
/// the window to backfill-paced replication fails fast instead of passing slowly.
const LIVE_PROPAGATION_TIMEOUT: Duration = Duration::from_secs(15);

/// Planned A→B handoff at an exact activation boundary.
///
/// Asserts the plan's v1 success criterion: no missing or duplicate zone height, no
/// canonical rollback, the boundary block and everything after it produced by B and
/// everything before it by A, a pending transaction surviving the handoff, and all nodes
/// converging on identical block hashes — with A continuing as a follower of B.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_planned_handoff_moves_production_at_exact_activation_boundary() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut cluster = start_local_p2p_cluster(24).await?;
    // Commonware drops messages for offline peers; the bootstrap leader also needs tip
    // evidence from both followers before its first promotion.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // --- Blocks 1..=3 are produced by A (the manifest bootstrap leader). ---
    for _ in 0..3 {
        cluster.inject_block(vec![])?;
    }
    cluster.wait_all_at(3, HANDOFF_TIMEOUT).await?;

    // --- Block 4 funds a sender so a pending transaction can ride through the handoff. ---
    let (sender_wallet, sender) = local_dev_zone_account(&cluster.nodes[2])?;
    let amount = 1_000_000_u128;
    cluster.inject_block(vec![L1Fixture::make_deposit_for_block(
        PATH_USD_ADDRESS,
        sender,
        sender,
        amount,
    )])?;
    cluster.wait_all_at(4, DEFAULT_TIMEOUT).await?;
    cluster.nodes[2]
        .wait_for_balance(
            PATH_USD_ADDRESS,
            sender,
            U256::from(amount),
            DEFAULT_TIMEOUT,
        )
        .await?;

    // Submit a transfer to follower C. C admits it locally and forwards it to every quorum peer,
    // including active leader A and incoming leader B. Nobody includes it before the handoff
    // because no further anchor is injected under A's authorization.
    let recipient = address!("0x00000000000000000000000000000000000ffff1");
    cluster.fixture.seed_no_receive_policy(recipient)?;
    let transfer_amount = 123_456_u128;
    let pending = ITIP20::new(PATH_USD_ADDRESS, sender_wallet)
        .transfer(recipient, U256::from(transfer_amount))
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(TIP20_TX_GAS)
        .send()
        .await?;
    let transaction_hash = *pending.tx_hash();
    let leader_provider = cluster.nodes[0].provider();
    poll_until(
        DEFAULT_TIMEOUT,
        DEFAULT_POLL,
        "forwarded transaction in the outgoing leader's pool",
        || {
            let leader_provider = &leader_provider;
            async move {
                Ok(leader_provider
                    .get_transaction_by_hash(transaction_hash)
                    .await?)
            }
        },
    )
    .await?;
    let incoming_leader_provider = cluster.nodes[1].provider();
    poll_until(
        DEFAULT_TIMEOUT,
        DEFAULT_POLL,
        "forwarded transaction in the incoming leader's pool",
        || {
            let incoming_leader_provider = &incoming_leader_provider;
            async move {
                Ok(incoming_leader_provider
                    .get_transaction_by_hash(transaction_hash)
                    .await?)
            }
        },
    )
    .await?;

    // --- The handoff: leadership of every anchor >= H moves to B. ---
    let handoff_anchor = cluster.next_anchor_number();
    assert_eq!(
        handoff_anchor, 5,
        "test assumes 1:1 zone-height:anchor mapping"
    );
    cluster.publish_transition(1, 1, handoff_anchor)?;

    // Produce blocks under B until the pending transfer is included. B already retained the
    // transaction before the handoff, independently of A's post-demotion reconciliation.
    let receipt = tokio::time::timeout(HANDOFF_TIMEOUT, async {
        loop {
            cluster.inject_block(vec![])?;
            let height = cluster.next_anchor_number() - 1;
            cluster.nodes[1]
                .wait_for_block_number(height, HANDOFF_TIMEOUT)
                .await?;
            if let Some(receipt) = cluster.nodes[1]
                .provider()
                .get_transaction_receipt(transaction_hash)
                .await?
            {
                return Ok::<_, eyre::Report>(receipt);
            }
        }
    })
    .await
    .map_err(|_| eyre::eyre!("timed out waiting for the pending transfer under leader B"))??;
    assert!(
        receipt.status(),
        "the pending transfer must succeed under B"
    );
    let inclusion_block = receipt
        .block_number
        .ok_or_else(|| eyre::eyre!("receipt missing block number"))?;
    assert!(
        inclusion_block >= handoff_anchor,
        "the pending transaction must be included by B (block {inclusion_block}), not by the \
         demoted leader"
    );

    // --- Convergence: every node holds the identical chain; the producer switches exactly
    // at the activation boundary (block height == embedded anchor in this harness). ---
    let final_height = cluster.nodes[1].provider().get_block_number().await?;
    cluster.wait_all_at(final_height, HANDOFF_TIMEOUT).await?;
    let a_producer = cluster.sequencer_signers[0].address();
    let b_producer = cluster.sequencer_signers[1].address();
    for height in 1..=final_height {
        let header = cluster.assert_same_block(height).await?;
        let expected = if height < handoff_anchor {
            a_producer
        } else {
            b_producer
        };
        assert_eq!(
            header.beneficiary, expected,
            "block {height} has the wrong producer (boundary at {handoff_anchor})"
        );
    }

    // --- A remains a live follower of B: it imports the next B-produced block. ---
    cluster.inject_block(vec![])?;
    let next = final_height + 1;
    cluster.wait_all_at(next, HANDOFF_TIMEOUT).await?;
    let header = cluster.assert_same_block(next).await?;
    assert_eq!(header.beneficiary, b_producer);

    // Every recipient balance is identical everywhere.
    for node in &cluster.nodes {
        node.wait_for_balance(
            PATH_USD_ADDRESS,
            recipient,
            U256::from(transfer_amount),
            DEFAULT_TIMEOUT,
        )
        .await?;
    }
    Ok(())
}

/// A transition finalizes while the selected node is several blocks behind.
///
/// The alive outgoing leader keeps producing through `H - 1`; the lagging node produces
/// nothing until its own consumption reaches the boundary (the next-anchor rule), then
/// catches up, satisfies the promotion barrier, and produces `H` exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_lagged_follower_promotes_only_after_catching_up() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut cluster = start_local_p2p_cluster(24).await?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Blocks 1..=2 are observed by everyone.
    for _ in 0..2 {
        cluster.inject_block(vec![])?;
    }
    cluster.wait_all_at(2, HANDOFF_TIMEOUT).await?;

    // Blocks 3..=5: node B (index 1) does not observe the anchors, so it cannot import the
    // corresponding zone blocks — it lags at height 2 while A and C advance.
    let mut withheld = Vec::new();
    for _ in 0..3 {
        withheld.push(cluster.inject_block_observed_by(vec![], &[0, 2])?);
    }
    cluster.nodes[0]
        .wait_for_block_number(5, DEFAULT_TIMEOUT)
        .await?;
    cluster.nodes[2]
        .wait_for_block_number(5, DEFAULT_TIMEOUT)
        .await?;
    assert_eq!(cluster.nodes[1].provider().get_block_number().await?, 2);

    // Leadership of anchors >= 6 moves to the lagging B.
    let handoff_anchor = cluster.next_anchor_number();
    assert_eq!(handoff_anchor, 6);
    cluster.publish_transition(1, 1, handoff_anchor)?;

    // Anchor 6 arrives. A's permit denies it (leader_for(6) = B), so A halts exactly at the
    // boundary; B cannot produce it either because its consumption is still before the
    // boundary (next anchor 3 is governed by A).
    let boundary = cluster.inject_block_observed_by(vec![], &[0, 2])?;
    tokio::time::sleep(QUIESCENCE).await;
    assert_eq!(
        cluster.nodes[0].provider().get_block_number().await?,
        5,
        "the demoted leader must not produce the boundary anchor"
    );
    assert_eq!(
        cluster.nodes[1].provider().get_block_number().await?,
        2,
        "the lagging node must not produce before catching up"
    );
    assert_eq!(
        cluster.nodes[2].provider().get_block_number().await?,
        5,
        "a follower must not accept a block nobody may produce"
    );

    // Deliver the withheld observations: B imports 3..=5, satisfies the promotion barrier
    // against the outgoing leader's tip evidence, and produces the boundary block itself.
    for anchor in withheld {
        cluster.record_anchor(1, anchor, vec![])?;
    }
    cluster.record_anchor(1, boundary, vec![])?;
    cluster.wait_all_at(6, HANDOFF_TIMEOUT).await?;

    let a_producer = cluster.sequencer_signers[0].address();
    let b_producer = cluster.sequencer_signers[1].address();
    for height in 1..=6 {
        let header = cluster.assert_same_block(height).await?;
        let expected = if height < handoff_anchor {
            a_producer
        } else {
            b_producer
        };
        assert_eq!(
            header.beneficiary, expected,
            "block {height} has the wrong producer (boundary at {handoff_anchor})"
        );
    }

    // B keeps producing; everyone follows.
    cluster.inject_block(vec![])?;
    cluster.wait_all_at(7, HANDOFF_TIMEOUT).await?;
    assert_eq!(cluster.assert_same_block(7).await?.beneficiary, b_producer);
    Ok(())
}

/// A transition is observed several anchors before its activation boundary.
///
/// Between observation and activation the outgoing leader must keep operating at full
/// service: its blocks reach every node live (not via the 30-second backfill probe, and
/// including the incoming leader), a transaction submitted to a follower is forwarded to it
/// and included before the boundary — and production still switches to B exactly at the
/// boundary. Guards the routing regression where P2P commands follow the most recently
/// observed leadership record instead of the leader of the relevant anchor.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_advance_scheduled_handoff_keeps_outgoing_leader_live() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut cluster = start_local_p2p_cluster(24).await?;
    tokio::time::sleep(Duration::from_secs(1)).await;

    // Blocks 1..=3 are produced by A (the manifest bootstrap leader).
    for _ in 0..3 {
        cluster.inject_block(vec![])?;
    }
    cluster.wait_all_at(3, HANDOFF_TIMEOUT).await?;

    // Block 4 funds a sender used during the window.
    let (sender_wallet, sender) = local_dev_zone_account(&cluster.nodes[2])?;
    let amount = 1_000_000_u128;
    cluster.inject_block(vec![L1Fixture::make_deposit_for_block(
        PATH_USD_ADDRESS,
        sender,
        sender,
        amount,
    )])?;
    cluster.wait_all_at(4, DEFAULT_TIMEOUT).await?;
    cluster.nodes[2]
        .wait_for_balance(
            PATH_USD_ADDRESS,
            sender,
            U256::from(amount),
            DEFAULT_TIMEOUT,
        )
        .await?;

    // Publish A→B three anchors ahead of the boundary: every node observes B's upcoming
    // leadership while A still rightfully produces anchors 5..=7.
    let next_anchor = cluster.next_anchor_number();
    assert_eq!(
        next_anchor, 5,
        "test assumes 1:1 zone-height:anchor mapping"
    );
    let handoff_anchor = next_anchor + 3;
    cluster.publish_transition(1, 1, handoff_anchor)?;

    // A transfer submitted to follower C during the window is forwarded to every quorum peer,
    // including both still-active leader A and not-yet-active leader B.
    let recipient = address!("0x00000000000000000000000000000000000ffff2");
    cluster.fixture.seed_no_receive_policy(recipient)?;
    let transfer_amount = 123_456_u128;
    let pending = ITIP20::new(PATH_USD_ADDRESS, sender_wallet)
        .transfer(recipient, U256::from(transfer_amount))
        .gas_price(TEMPO_T0_BASE_FEE as u128)
        .gas(TIP20_TX_GAS)
        .send()
        .await?;
    let transaction_hash = *pending.tx_hash();
    let outgoing_leader_provider = cluster.nodes[0].provider();
    poll_until(
        LIVE_PROPAGATION_TIMEOUT,
        DEFAULT_POLL,
        "forwarded transaction in the outgoing leader's pool during the window",
        || {
            let provider = &outgoing_leader_provider;
            async move { Ok(provider.get_transaction_by_hash(transaction_hash).await?) }
        },
    )
    .await?;

    // A produces 5..=7 through the window; each block reaches every node live.
    for height in 5..=7 {
        cluster.inject_block(vec![])?;
        cluster
            .wait_all_at(height, LIVE_PROPAGATION_TIMEOUT)
            .await?;
    }

    // The forwarded transfer was included by A before the boundary.
    let receipt = cluster.nodes[0]
        .provider()
        .get_transaction_receipt(transaction_hash)
        .await?
        .ok_or_else(|| eyre::eyre!("transfer was not included during the window"))?;
    assert!(receipt.status(), "the window transfer must succeed under A");
    let inclusion_block = receipt
        .block_number
        .ok_or_else(|| eyre::eyre!("receipt missing block number"))?;
    assert!(
        inclusion_block < handoff_anchor,
        "the transfer must be included by the outgoing leader (block {inclusion_block}), \
         before the boundary at {handoff_anchor}"
    );

    // The boundary block and everything after it is produced by B; A follows.
    cluster.inject_block(vec![])?;
    cluster.wait_all_at(handoff_anchor, HANDOFF_TIMEOUT).await?;
    cluster.inject_block(vec![])?;
    cluster
        .wait_all_at(handoff_anchor + 1, HANDOFF_TIMEOUT)
        .await?;

    let a_producer = cluster.sequencer_signers[0].address();
    let b_producer = cluster.sequencer_signers[1].address();
    for height in 1..=handoff_anchor + 1 {
        let header = cluster.assert_same_block(height).await?;
        let expected = if height < handoff_anchor {
            a_producer
        } else {
            b_producer
        };
        assert_eq!(
            header.beneficiary, expected,
            "block {height} has the wrong producer (boundary at {handoff_anchor})"
        );
    }

    // Every recipient balance is identical everywhere.
    for node in &cluster.nodes {
        node.wait_for_balance(
            PATH_USD_ADDRESS,
            recipient,
            U256::from(transfer_amount),
            DEFAULT_TIMEOUT,
        )
        .await?;
    }
    Ok(())
}
