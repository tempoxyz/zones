//! Infrastructure gate for the full instant-transfer acceptance suite.
//!
//! This test intentionally fails at the first unsupported black-box seam. It still creates ten
//! actual factory Zones first, proving that the blocker is the missing shared-L1, three-replica
//! fast fixture rather than Zone creation. It must be replaced by the complete flows recorded in
//! `instant-tests-status.md` once that seam exists; it is never evidence that E2E passed.

use std::collections::HashSet;

use tempo_zone_contracts::ZonePortal;

use crate::utils::{L1TestNode, ZoneCreationConfig};

const ZONE_COUNT: usize = 10;
const REPLICAS_PER_ZONE: usize = 3;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "Ten-Zone E2E explicitly waived by the requester; all other T14 tests remain required"]
async fn ten_actual_factory_zones_require_three_replica_fast_fixture() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let l1 = L1TestNode::start().await?;
    let factory = l1.native_zone_factory().await?;
    let mut portals = Vec::with_capacity(ZONE_COUNT);
    let mut zone_ids = HashSet::with_capacity(ZONE_COUNT);

    for zone_index in 0..ZONE_COUNT {
        let first_signer_index = 100 + (zone_index * REPLICAS_PER_ZONE) as u32;
        let signers = (0..REPLICAS_PER_ZONE)
            .map(|replica| l1.signer_at(first_signer_index + replica as u32))
            .collect::<Vec<_>>();
        let portal_address = l1
            .create_zone_with_admin_sequencers_and_config(
                factory,
                l1.admin_address(),
                signers.iter().map(|signer| signer.address()).collect(),
                2,
                ZoneCreationConfig::open(),
            )
            .await?;
        let portal = ZonePortal::new(portal_address, l1.provider());
        let zone_id = portal.zoneId().call().await?;
        eyre::ensure!(zone_ids.insert(zone_id), "factory reused Zone ID {zone_id}");
        portals.push(portal_address);
    }

    eyre::ensure!(
        portals.len() == ZONE_COUNT,
        "factory did not create ten Portals"
    );
    eyre::ensure!(
        portals.iter().copied().collect::<HashSet<_>>().len() == ZONE_COUNT,
        "factory reused a Portal address"
    );

    eyre::bail!(
        "UNSUPPORTED FAST ACCEPTANCE INFRASTRUCTURE: created {ZONE_COUNT} real factory Zones on one native Tempo L1, but the public baseline fixture cannot start {REPLICAS_PER_ZONE} independently restartable fast replicas per existing Portal on that shared L1. Required next seam: start replicas with unique chain IDs/state directories and finalized fast rosters, preserving their volumes for crash recovery. Until then A->B->C, real pool deposits, authenticated balances, both L1 replenishment legs, 90 routes/ring, load trials, chaos, and proof-required settlement cannot run and are not accepted."
    )
}
