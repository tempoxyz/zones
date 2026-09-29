//! The running node must reject future payloads before they can poison its canonical clock.

use alloy_consensus::BlockHeader as _;
use alloy_provider::Provider as _;
use alloy_rpc_types_engine::PayloadStatusEnum;
use reth_node_api::PayloadTypes as _;
use reth_primitives_traits::SealedBlock;
use zone_payload::ZonePayloadTypes;

use crate::utils::{DEFAULT_TIMEOUT, start_local_zone_with_fixture};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejects_future_payload_without_poisoning_next_height() -> eyre::Result<()> {
    let (zone, mut fixture) = start_local_zone_with_fixture(3).await?;
    let mut canonical = zone.subscribe_to_canonical_state();
    fixture.inject_empty_block(zone.deposit_queue());
    let first = tokio::time::timeout(DEFAULT_TIMEOUT, canonical.recv()).await??;
    let parent = first.tip();
    let provider = zone.provider();
    let height = parent.number();
    zone.wait_for_block_number(height, DEFAULT_TIMEOUT).await?;

    // Exercise the same newPayload validation used for live and backfilled peer imports.
    // Other block contents are immaterial: the clock gate must reject before execution.
    let mut future = parent.sealed_block().clone().into_block();
    future.header.inner.number = height + 1;
    future.header.inner.parent_hash = parent.hash();
    future.header.inner.timestamp += 3_600;
    let status = zone
        .engine
        .new_payload(ZonePayloadTypes::block_to_payload(
            SealedBlock::seal_slow(future),
            None,
        ))
        .await?;
    let PayloadStatusEnum::Invalid { validation_error } = status.status else {
        panic!("future payload was not rejected: {status:?}");
    };
    assert!(
        validation_error.contains("is in the future compared to our clock time"),
        "{validation_error}"
    );
    assert_eq!(provider.get_block_number().await?, height);

    // A normal block at the rejected height must still be executable and canonicalizable.
    fixture.inject_empty_block(zone.deposit_queue());
    zone.wait_for_block_number(height + 1, DEFAULT_TIMEOUT)
        .await?;
    Ok(())
}
