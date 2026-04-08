//! Tests for zone-specific precompile availability.

use tempo_precompiles::PATH_USD_ADDRESS;

use crate::utils::{
    DEFAULT_TIMEOUT, TestStablecoinDEX, start_local_zone_with_fixture, STABLECOIN_DEX_ADDRESS,
};

/// The StablecoinDEX precompile should be disabled on zones — any call to
/// it must revert.
#[tokio::test(flavor = "multi_thread")]
async fn test_dex_disabled_on_zone() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (zone, mut fixture) = start_local_zone_with_fixture(10).await?;

    // Inject an empty block so the zone is alive and processing.
    fixture.inject_empty_block(zone.deposit_queue());
    zone.wait_for_tempo_block_number(1, DEFAULT_TIMEOUT).await?;

    // Attempt to call createPair on the DEX — should revert because the
    // precompile is not registered on the zone.
    let dex = TestStablecoinDEX::new(STABLECOIN_DEX_ADDRESS, zone.provider());
    let result = dex.createPair(PATH_USD_ADDRESS).call().await;

    assert!(
        result.is_err(),
        "StablecoinDEX should be disabled on zones — createPair must revert"
    );

    Ok(())
}
