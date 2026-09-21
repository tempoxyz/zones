//! The contract test L1 must execute this checkout's Solidity, including after T13.

use alloy_provider::Provider;
use alloy_rpc_types_eth::BlockId;
use tempo_contracts::precompiles::{initial_zone_factory_state, t13_zone_factory_state};

use crate::utils::{L1TestNode, forge_runtime_bytecode, now_secs};

#[tokio::test(flavor = "multi_thread")]
async fn explicit_test_runtimes_survive_blocks() -> eyre::Result<()> {
    use alloy_primitives::Bytes;
    let runtimes = tempo_evm::T13ZoneRuntimes {
        portal: Bytes::from_static(&[0x00, 0x01]),
        verifier: Bytes::from_static(&[0x00, 0x02]),
        messenger: Bytes::from_static(&[0x00, 0x03]),
    };
    let l1 = L1TestNode::start_with_runtimes(runtimes.clone(), |_| {}).await?;
    let [_, portal, verifier, messenger] = t13_zone_factory_state(l1.admin_address());
    // Fund via the native TIP20 precompile, without calling the sentinel contracts.
    for _ in 0..2 {
        l1.fund_user(l1.admin_address(), 1).await?;
        for (account, expected) in [
            (&portal, &runtimes.portal),
            (&verifier, &runtimes.verifier),
            (&messenger, &runtimes.messenger),
        ] {
            assert_ne!(&account.code, expected);
            assert_eq!(&l1.provider().get_code_at(account.address).await?, expected);
        }
    }
    Ok(())
}

async fn assert_local_runtimes(l1: &L1TestNode, block: BlockId) -> eyre::Result<()> {
    let [_, portal, verifier, messenger] = t13_zone_factory_state(l1.admin_address());
    for (account, contract) in [
        (portal, "ZonePortal"),
        (verifier, "Verifier"),
        (messenger, "ZoneMessenger"),
    ] {
        assert_eq!(
            l1.provider()
                .get_code_at(account.address)
                .block_id(block)
                .await?,
            forge_runtime_bytecode(contract)?,
            "{contract} must use the locally compiled runtime"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn local_runtimes_survive_t13_blocks() -> eyre::Result<()> {
    let l1 = L1TestNode::start().await?;
    assert_local_runtimes(&l1, BlockId::number(0)).await?;
    for _ in 0..2 {
        l1.fund_user(l1.admin_address(), 1).await?;
        assert_local_runtimes(&l1, BlockId::latest()).await?;
    }
    // Exercise native factory deployment with the local shared implementation.
    l1.deploy_zone().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn t13_upgrade_installs_local_runtimes() -> eyre::Result<()> {
    let activation = now_secs() + 30;
    let l1 = L1TestNode::start_with_t13(activation).await?;
    for account in initial_zone_factory_state(l1.admin_address()) {
        assert_eq!(
            l1.provider()
                .get_code_at(account.address)
                .block_id(BlockId::number(0))
                .await?,
            account.code,
            "migration fixture must retain the pinned pre-T13 runtimes"
        );
    }
    tokio::time::sleep(std::time::Duration::from_secs(
        activation.saturating_sub(now_secs()) + 1,
    ))
    .await;
    for _ in 0..2 {
        l1.fund_user(l1.admin_address(), 1).await?;
        assert_local_runtimes(&l1, BlockId::latest()).await?;
    }
    l1.deploy_zone().await?;
    Ok(())
}
