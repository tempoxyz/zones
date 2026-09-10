//! Exercise proof collection against a real pending engine payload and Tempo L1.

use alloy_consensus::BlockHeader as _;
use alloy_provider::Provider as _;
use std::time::Duration;
use zone_sequencer::StoredBlockProof;

use crate::utils::start_real_p2p_cluster_with_active_nodes;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn canonical_leader_block_has_durable_proof() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let cluster = start_real_p2p_cluster_with_active_nodes(120, 2).await?;
    let leader = &cluster.nodes[0];
    let mut canonical = leader.subscribe_to_canonical_state();
    let notification = tokio::time::timeout(Duration::from_secs(60), canonical.recv()).await??;
    let tip = notification.tip();
    let path = leader
        .proof_directory()
        .join(format!("{}-{:x}.json", tip.number(), tip.hash()));
    let proof: StoredBlockProof = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_eq!(proof.witness.block_hash, tip.hash());
    assert_eq!(proof.witness.parent_hash, tip.parent_hash());
    assert_eq!(proof.witness.block_number, tip.number());
    assert!(leader.provider().get_block_number().await? >= proof.witness.block_number);

    // Turn the spool path into a regular file to force the next atomic write to fail,
    // retaining the previously collected proofs in a sibling directory.
    let directory = leader.proof_directory();
    let retained = directory.with_file_name("proofs-retained");
    std::fs::rename(&directory, &retained)?;
    std::fs::File::create(&directory)?;
    let provider = leader.provider();
    let stalled_height = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let head = provider.get_block_number().await?;
            if leader
                .pending_block_number()?
                .is_some_and(|number| number > head)
            {
                break eyre::Ok(head);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await??;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(
        provider.get_block_number().await?,
        stalled_height,
        "proof persistence failure must not advance canonical forkchoice"
    );
    std::fs::remove_file(&directory)?;
    std::fs::rename(&retained, &directory)?;
    Ok(())
}
