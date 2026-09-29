//! Future timestamps must defer production without consuming the queued L1 block.

use alloy_provider::Provider as _;
use std::time::Duration;

use crate::utils::{DEFAULT_TIMEOUT, now_secs, start_local_zone_with_fixture};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retries_future_block_without_advancing_head_or_consuming_l1() -> eyre::Result<()> {
    let (zone, mut fixture) = start_local_zone_with_fixture(3).await?;
    let future = fixture.next_block_at(now_secs() + 3);
    fixture.enqueue(&future, zone.deposit_queue(), vec![]);

    assert!(
        zone.wait_for_block_number(1, Duration::from_millis(300))
            .await
            .is_err()
    );
    assert_eq!(zone.provider().get_block_number().await?, 0);
    assert_eq!(
        zone.deposit_queue().peek().unwrap().header.inner.number,
        future.header.inner.number
    );

    // The same queued L1 block becomes admissible once the clock catches up.
    zone.wait_for_block_number(1, DEFAULT_TIMEOUT).await?;
    fixture.inject_empty_block(zone.deposit_queue());
    zone.wait_for_block_number(2, DEFAULT_TIMEOUT).await?;
    Ok(())
}
