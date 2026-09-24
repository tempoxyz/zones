//! A test L1 starting at T13 must preserve this checkout's Solidity from genesis.

use alloy_provider::Provider;
use alloy_rpc_types_eth::BlockId;
use tempo_contracts::precompiles::{initial_zone_factory_state, t13_zone_factory_state};

use crate::utils::{L1TestNode, forge_runtime_bytecode, now_secs};

#[tokio::test(flavor = "multi_thread")]
async fn explicit_test_runtimes_survive_blocks() -> eyre::Result<()> {
    use alloy_primitives::Bytes;
    use reth_chainspec::EthChainSpec as _;
    use std::sync::Arc;
    use tempo_chainspec::TempoChainSpec;

    let runtimes = [
        Bytes::from_static(&[0x00, 0x01]),
        Bytes::from_static(&[0x00, 0x02]),
        Bytes::from_static(&[0x00, 0x03]),
    ];
    let [_, portal, verifier, messenger] = t13_zone_factory_state(Default::default());
    let accounts = [portal, verifier, messenger];
    let l1 = L1TestNode::start_with(|cfg| {
        let mut genesis = cfg.chain.genesis().clone();
        for (account, code) in accounts.iter().zip(&runtimes) {
            genesis.alloc.get_mut(&account.address).unwrap().code = Some(code.clone());
        }
        cfg.chain = Arc::new(TempoChainSpec::from_genesis(genesis));
    })
    .await?;
    // Fund via the native TIP20 precompile, without calling the sentinel contracts.
    for _ in 0..2 {
        l1.fund_user(l1.admin_address(), 1).await?;
        for (account, expected) in accounts.iter().zip(&runtimes) {
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
async fn t13_transition_still_installs_pinned_runtimes() -> eyre::Result<()> {
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
        for account in t13_zone_factory_state(l1.admin_address()) {
            assert_eq!(
                l1.provider().get_code_at(account.address).await?,
                account.code
            );
        }
    }
    l1.deploy_zone().await?;
    Ok(())
}
