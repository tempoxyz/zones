//! Independent T14 native-runtime and roster-safety acceptance tests.

use alloy_primitives::{Address, B256, U256};
use tempo_contracts::precompiles::ITIP20;
use tempo_precompiles::PATH_USD_ADDRESS;
use tempo_zone_contracts::{FAST_TRANSFER_ADDRESS, IFastTransfer};
use zone_fast_transfer::EpochRoster;
use zone_node::fast_drain::{DrainPeer, FastDrainConfig, FastDrainError};
use zone_primitives::fast_transfer::ZoneDomain;

use crate::utils::{
    DEFAULT_TIMEOUT, TIP20_TX_GAS, local_dev_zone_account, start_local_zone_with_fixture,
};

const T14_PROTOCOL_VERSION: u16 = 1;

fn roster(zone_id: u32, portal_byte: u8, epoch: u64, members: [Address; 3]) -> EpochRoster {
    EpochRoster::from_finalized_registry(
        ZoneDomain {
            l1_chain_id: 14_014,
            zone_id,
            chain_id: 140_000 + u64::from(zone_id),
            portal: Address::repeat_byte(portal_byte),
            authority_epoch: epoch,
            roster_hash: B256::repeat_byte(portal_byte),
            protocol_version: T14_PROTOCOL_VERSION,
        },
        members,
    )
    .unwrap()
}

#[test]
fn t14_next_roster_must_not_overlap_the_retiring_authority() {
    let old_members = [
        Address::repeat_byte(0x01),
        Address::repeat_byte(0x02),
        Address::repeat_byte(0x03),
    ];
    let old = roster(14, 0x14, 14, old_members);
    let overlapping_next = roster(
        14,
        0x14,
        15,
        [
            old_members[2],
            Address::repeat_byte(0x04),
            Address::repeat_byte(0x05),
        ],
    );
    let peers = std::array::from_fn(|index| DrainPeer {
        zone_id: 100 + index as u32,
        roster: roster(
            100 + index as u32,
            0x20 + index as u8,
            14,
            [
                Address::repeat_byte(0x40 + index as u8),
                Address::repeat_byte(0x50 + index as u8),
                Address::repeat_byte(0x60 + index as u8),
            ],
        ),
        closure_hash: B256::repeat_byte(0x70 + index as u8),
    });
    let config = FastDrainConfig {
        local_roster: old,
        local_member: old_members[0],
        peers,
        next_roster: Some(overlapping_next),
    };

    assert!(
        matches!(config.validate(), Err(FastDrainError::InvalidConfiguration)),
        "retiring keys must not also form part of the successor production authority"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn public_runtime_rejects_fast_pool_funding_before_t14_without_token_effects()
-> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (zone, mut l1) = start_local_zone_with_fixture(14).await?;
    let (provider, operator) = local_dev_zone_account(&zone)?;
    let deposit = l1.make_deposit(PATH_USD_ADDRESS, operator, operator, 1_000_000);
    l1.inject_deposits(zone.deposit_queue(), vec![deposit]);
    zone.wait_for_balance(
        PATH_USD_ADDRESS,
        operator,
        U256::from(1_000_000),
        DEFAULT_TIMEOUT,
    )
    .await?;

    let token = ITIP20::new(PATH_USD_ADDRESS, &provider);
    let fast = IFastTransfer::new(FAST_TRANSFER_ADDRESS, &provider);
    let operator_before = token.balanceOf(operator).call().await?;
    let precompile_before = token.balanceOf(FAST_TRANSFER_ADDRESS).call().await?;
    let supply_before = token.totalSupply().call().await?;

    let pending = fast
        .fundPool(PATH_USD_ADDRESS, 100_000, 10_000)
        .gas(TIP20_TX_GAS)
        .send()
        .await?;
    l1.inject_empty_block(zone.deposit_queue());
    let receipt = pending.get_receipt().await?;
    assert!(
        !receipt.status(),
        "fast native dispatch must fail before T14 activation"
    );

    assert_eq!(token.balanceOf(operator).call().await?, operator_before);
    assert_eq!(
        token.balanceOf(FAST_TRANSFER_ADDRESS).call().await?,
        precompile_before
    );
    assert_eq!(token.totalSupply().call().await?, supply_before);
    Ok(())
}
