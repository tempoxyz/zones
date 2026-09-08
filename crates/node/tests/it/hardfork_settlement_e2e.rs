//! Upgrade an existing portal and prove the first T13 recovery batch before settling its output.

use std::{collections::BTreeMap, time::Duration};

use alloy_consensus::{BlockHeader, Transaction};
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rlp::Decodable;
use alloy_rpc_types_eth::{BlockId, BlockNumberOrTag};
use alloy_sol_types::{SolCall, SolError};
use tempo_alloy::TempoNetwork;
use tempo_contracts::precompiles::ITIP20;
use tempo_precompiles::PATH_USD_ADDRESS;
use tempo_primitives::TempoHeader;
use tempo_zone_contracts::{
    IZoneInbox, IZoneOutbox, TEMPO_STATE_ADDRESS, TempoState, ZONE_INBOX_ADDRESS, ZonePortal,
};
use tokio_util::sync::CancellationToken;
use zone_rpc::types::ZoneExecutionWitness;
use zone_sequencer::{BatchData, BatchSubmitter};
use zone_spf::{
    BatchOutput, BatchWitness, PublicInputs, SpfConfig, TempoImport, TempoStateWitness, ZoneBlock,
    ZoneStateWitness, prove_zone_batch,
};

use crate::utils::{
    DEFAULT_TIMEOUT, L1TestNode, TcpChaosProxy, ZoneAccount, ZoneTestNode, now_secs, poll_until,
};

#[tokio::test(flavor = "multi_thread")]
async fn test_t13_migrates_and_settles_existing_portal() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let activation = now_secs() + 60;
    let l1 = L1TestNode::start_with_t13(activation).await?;
    let portal_address = l1.deploy_zone().await?;
    let portal = ZonePortal::new(portal_address, l1.provider());
    // Create metadata before the zone starts; only alpha is processed before the upgrade.
    let alpha = l1
        .create_tip20("Alpha", "ALP", B256::with_last_byte(1))
        .await?;
    let beta = l1
        .create_tip20("Beta", "BET", B256::with_last_byte(2))
        .await?;
    let gamma = l1
        .create_tip20("Gamma", "GAM", B256::with_last_byte(3))
        .await?;
    // HTTP polling retains pending block-filter changes while responses are paused, without
    // WebSocket keepalive timeouts obscuring the hardfork coordination under test.
    let proxy = TcpChaosProxy::start(TcpChaosProxy::upstream_addr(l1.http_url())?).await?;
    let (zone, spec) = ZoneTestNode::start_from_l1_with_t13(
        l1.http_url(),
        &proxy.proxy_url(l1.http_url())?,
        portal_address,
        activation,
    )
    .await?;
    let provider = zone.provider();
    let inbox = IZoneInbox::new(ZONE_INBOX_ADDRESS, &provider);
    let mut account = ZoneAccount::from_l1_and_zone(&l1, &zone, portal_address);
    l1.enable_token_on_portal(portal_address, alpha).await?;
    l1.fund_user(account.address(), 1_000_000).await?;
    account.deposit(100, DEFAULT_TIMEOUT, &zone).await?;
    zone.wait_for_tempo_block_number(l1.provider().get_block_number().await?, DEFAULT_TIMEOUT)
        .await?;
    // The fixture finalizes every eight Zone blocks. Reach exactly the first T12 boundary.
    while provider.get_block_number().await? < 8 {
        l1.fund_user(l1.admin_address(), 1).await?;
        zone.wait_for_tempo_block_number(l1.provider().get_block_number().await?, DEFAULT_TIMEOUT)
            .await?;
    }
    assert_eq!(provider.get_block_number().await?, 8);
    let parent = provider.get_block_by_number(8.into()).await?.unwrap();
    assert!(
        parent.header.timestamp() < activation,
        "T12 setup crossed activation"
    );
    assert_eq!(portal.enabledTokenCount().call().await?, U256::from(2));
    assert_eq!(ITIP20::new(alpha, &provider).name().call().await?, "Alpha");
    assert_eq!(inbox.processedDepositNumber().call().await?, 1);
    let migrated_hash = inbox.processedTokenEnablementHash().call().await?;
    assert_eq!(migrated_hash, portal.tokenEnablementHash().call().await?);
    assert_eq!(
        provider
            .get_storage_at(
                ZONE_INBOX_ADDRESS,
                zone_precompiles::inbox::slots::PROCESSED_ENABLED_TOKEN_COUNT
            )
            .await?,
        U256::ZERO
    );

    // Limit the pause so outstanding HTTP requests remain within their normal timeout.
    tokio::time::sleep(Duration::from_secs(
        activation.saturating_sub(now_secs() + 20),
    ))
    .await;
    proxy.pause_upstream_to_client(true);
    let l1_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
        .wallet(alloy_network::EthereumWallet::from(l1.dev_signer()))
        .connect_http(l1.http_url().clone())
        .erased();
    let submitter = BatchSubmitter::with_signer_and_anchor_config(
        portal_address,
        l1_provider.clone(),
        l1.dev_signer(),
        Default::default(),
    );
    let legacy = BatchData {
        zone_height: 8,
        tempo_block_number: zone.tempo_block_number().await?,
        prev_block_hash: B256::ZERO,
        next_block_hash: parent.header.hash,
        prev_processed_deposit_hash: B256::ZERO,
        next_processed_deposit_hash: inbox.processedDepositQueueHash().call().await?,
        prev_deposit_number: 0,
        next_deposit_number: 1,
        prev_processed_token_count: 0,
        next_processed_token_count: 0,
        withdrawal_queue_hash: B256::ZERO,
        withdrawal_batch_index: 1,
    };
    submitter
        .submit_batch(
            &submitter.prepare_batch(legacy).await?,
            None,
            &CancellationToken::new(),
        )
        .await
        .map_err(|err| eyre::eyre!("legacy settlement: {err:?}"))?;
    assert_eq!(portal.blockHash().call().await?, parent.header.hash);

    // Leave one enablement and one deposit outstanding on T12 while the zone cannot receive L1.
    l1.enable_token_on_portal(portal_address, beta).await?;
    account.submit_deposit(200).await?;
    let last_t12 = l1_provider
        .get_block_by_number(BlockNumberOrTag::Latest)
        .await?
        .unwrap();
    assert!(last_t12.header.timestamp() < activation);
    tokio::time::sleep(Duration::from_secs(activation.saturating_sub(now_secs()))).await;
    proxy.pause_upstream_to_client(false);
    poll_until(
        DEFAULT_TIMEOUT,
        Duration::from_millis(100),
        "T12 backlog visible",
        || async {
            Ok(zone
                .deposit_queue()
                .last_enqueued()
                .filter(|anchor| anchor.number >= last_t12.header.number())
                .map(|_| ()))
        },
    )
    .await?;
    // Wall-clock T13 alone must not execute the queued T12 work.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(provider.get_block_number().await?, 8);
    assert_eq!(
        inbox.processedTokenEnablementHash().call().await?,
        migrated_hash
    );

    // This transaction mines the first actual T13 L1 block and installs the new runtimes.
    l1.fund_user(l1.admin_address(), 1).await?;
    zone.wait_for_balance(
        PATH_USD_ADDRESS,
        account.address(),
        U256::from(300),
        DEFAULT_TIMEOUT,
    )
    .await?;
    let end = zone.stop_engine().await?;
    assert!(
        end >= 10,
        "recovery must contain a checkpoint prefix and a full block"
    );
    for number in 9..end {
        let block = BlockId::number(number);
        assert_eq!(
            inbox
                .processedEnabledTokenCount()
                .block(block)
                .call()
                .await?,
            0,
            "checkpoint {number} must not migrate the token cursor"
        );
        assert_eq!(
            inbox
                .processedTokenEnablementHash()
                .block(block)
                .call()
                .await?,
            migrated_hash,
            "checkpoint {number} must preserve the legacy token prefix"
        );
        assert_eq!(
            inbox.processedDepositNumber().block(block).call().await?,
            1,
            "checkpoint {number} must leave the pending deposit untouched"
        );
    }
    assert_eq!(inbox.processedEnabledTokenCount().call().await?, 3);
    assert_eq!(inbox.processedDepositNumber().call().await?, 2);
    assert_eq!(ITIP20::new(alpha, &provider).name().call().await?, "Alpha");
    assert_eq!(ITIP20::new(beta, &provider).name().call().await?, "Beta");
    assert!(!portal.tokenEnablementCursorInitialized().call().await?);
    assert_eq!(portal.lastProcessedEnabledTokenCount().call().await?, 0);
    let blocked = ZonePortal::new(portal_address, l1.admin_provider())
        .enableToken(gamma)
        .call()
        .await
        .expect_err("enablement must wait for migration settlement");
    assert_eq!(
        blocked.as_revert_data().unwrap(),
        ZonePortal::TokenEnablementCursorNotInitialized {}.abi_encode()
    );

    let witness = recovery_witness(&l1, &zone, portal_address, 9, end).await?;
    let config = SpfConfig::new(spec, portal_address);
    let output = prove_zone_batch(&config, witness.clone())?;
    let head = provider.get_block_by_number(end.into()).await?.unwrap();
    assert_eq!(output.block_transition.prevBlockHash, parent.header.hash);
    assert_eq!(
        output.block_transition.nextBlockHash, head.header.hash,
        "SPF and production execution must produce the same state and block hash"
    );
    assert_eq!(
        output.token_enablement_transition.prevProcessedTokenCount,
        0
    );
    assert_eq!(
        output.token_enablement_transition.nextProcessedTokenCount,
        3
    );
    assert_eq!(output.deposit_queue_transition.prevDepositNumber, 1);
    assert_eq!(output.deposit_queue_transition.nextDepositNumber, 2);
    assert_eq!(output.last_batch_commitment.withdrawal_batch_index, 2);
    assert_eq!(
        output.deposit_queue_transition.nextProcessedHash,
        inbox.processedDepositQueueHash().call().await?
    );

    let mut checkpoint_only = witness.clone();
    checkpoint_only.zone_blocks.pop();
    assert!(matches!(
        prove_zone_batch(&config, checkpoint_only),
        Err(zone_spf::Error::InvalidBatchShape)
    ));
    let mut omitted = witness;
    if let TempoImport::Full { enabled_tokens, .. } =
        &mut omitted.zone_blocks.last_mut().unwrap().tempo_import
    {
        enabled_tokens.clear();
    }
    assert!(
        prove_zone_batch(&config, omitted).is_err(),
        "the full token suffix is mandatory"
    );

    let batch = batch_from_output(end, zone.tempo_block_number().await?, &output);
    let prepared = submitter.prepare_batch(batch).await?;
    let settled = submitter
        .submit_batch(&prepared, None, &CancellationToken::new())
        .await
        .map_err(|err| eyre::eyre!("T13 settlement: {err:?}"))?;
    assert_eq!(settled.lastProcessedEnabledTokenCount, 3);
    assert_eq!(settled.withdrawalBatchIndex, 2);
    assert_eq!(
        settled.withdrawalQueueIndex,
        U256::MAX,
        "empty batch must not create a withdrawal queue"
    );
    assert!(portal.tokenEnablementCursorInitialized().call().await?);
    assert_eq!(portal.lastProcessedEnabledTokenCount().call().await?, 3);
    assert_eq!(portal.lastProcessedDepositNumber().call().await?, 2);
    assert_eq!(
        portal.blockHash().call().await?,
        output.block_transition.nextBlockHash
    );
    assert!(
        submitter
            .submit_batch(&prepared, None, &CancellationToken::new())
            .await
            .is_err(),
        "settlement cannot replay"
    );
    assert_eq!(portal.withdrawalBatchIndex().call().await?, 2);
    l1.enable_token_on_portal(portal_address, gamma).await?;
    assert_eq!(portal.enabledTokenCount().call().await?, U256::from(4));
    Ok(())
}

fn batch_from_output(height: u64, tempo_number: u64, output: &BatchOutput) -> BatchData {
    BatchData {
        zone_height: height,
        tempo_block_number: tempo_number,
        prev_block_hash: output.block_transition.prevBlockHash,
        next_block_hash: output.block_transition.nextBlockHash,
        prev_processed_deposit_hash: output.deposit_queue_transition.prevProcessedHash,
        next_processed_deposit_hash: output.deposit_queue_transition.nextProcessedHash,
        prev_deposit_number: output.deposit_queue_transition.prevDepositNumber,
        next_deposit_number: output.deposit_queue_transition.nextDepositNumber,
        prev_processed_token_count: output.token_enablement_transition.prevProcessedTokenCount,
        next_processed_token_count: output.token_enablement_transition.nextProcessedTokenCount,
        withdrawal_queue_hash: output.withdrawal_queue_hash,
        withdrawal_batch_index: output.last_batch_commitment.withdrawal_batch_index,
    }
}

/// Build a witness from actual execution and L1 Merkle proofs, without substituting synthetic state.
async fn recovery_witness(
    l1: &L1TestNode,
    zone: &ZoneTestNode,
    portal: Address,
    from: u64,
    to: u64,
) -> eyre::Result<BatchWitness> {
    let provider = zone.provider();
    let l1_provider =
        ProviderBuilder::new_with_network::<TempoNetwork>().connect_http(l1.http_url().clone());
    let parent = provider
        .get_block_by_number((from - 1).into())
        .await?
        .unwrap();
    let initial_number = TempoState::new(TEMPO_STATE_ADDRESS, &provider)
        .tempoBlockNumber()
        .block(BlockId::number(from - 1))
        .call()
        .await?;
    let initial = l1_provider
        .get_block_by_number(initial_number.into())
        .await?
        .unwrap();
    let mut zone_nodes = BTreeMap::new();
    let mut codes = BTreeMap::new();
    let mut tempo_nodes = BTreeMap::new();
    let mut blocks = Vec::new();
    let mut final_header = TempoHeader::default();
    for number in from..=to {
        let block = provider
            .get_block_by_number(number.into())
            .full()
            .await?
            .unwrap();
        let txs = block.transactions.as_transactions().unwrap();
        let generated: ZoneExecutionWitness = provider
            .raw_request(
                "debug_zoneExecutionWitness".into(),
                (BlockNumberOrTag::Number(number),),
            )
            .await?;
        for node in generated.execution_witness.state {
            zone_nodes.insert(keccak256(&node), node);
        }
        for code in generated.execution_witness.codes {
            codes.insert(keccak256(&code), code);
        }
        let is_full = txs[0]
            .input()
            .starts_with(&IZoneInbox::advanceTempoCall::SELECTOR);
        let (import, header, finalization) = if is_full {
            assert_eq!(number, to);
            assert_eq!(txs.len(), 2);
            let call = IZoneInbox::advanceTempoCall::abi_decode(txs[0].input())?;
            let header = TempoHeader::decode(&mut call.header.as_ref())?;
            let finalize = IZoneOutbox::finalizeWithdrawalBatchCall::abi_decode(txs[1].input())?;
            (
                TempoImport::Full {
                    header_rlp: call.header,
                    deposits: call.deposits,
                    decryptions: call.decryptions,
                    enabled_tokens: call.enabledTokens,
                },
                header,
                Some(finalize.count),
            )
        } else {
            assert_eq!(txs.len(), 1);
            assert!(
                generated.tempo_reads.is_empty(),
                "checkpoint must not read L1 state"
            );
            let call = IZoneInbox::advanceTempoHeadersCall::abi_decode(txs[0].input())?;
            let header = TempoHeader::decode(&mut call.headers.last().unwrap().as_ref())?;
            (
                TempoImport::CheckpointOnly {
                    headers_rlp: call.headers,
                },
                header,
                None,
            )
        };
        for read in generated.tempo_reads {
            let proof = l1_provider
                .get_proof(read.account, vec![read.slot])
                .block_id(BlockId::number(header.number()))
                .await?;
            for node in proof.account_proof {
                tempo_nodes.insert(keccak256(&node), node);
            }
            for storage in proof.storage_proof {
                for node in storage.proof {
                    tempo_nodes.insert(keccak256(&node), node);
                }
            }
        }
        blocks.push(ZoneBlock {
            number,
            parent_hash: block.header.parent_hash(),
            timestamp: block.header.timestamp(),
            timestamp_millis_part: block.header.timestamp_millis_part,
            beneficiary: block.header.beneficiary(),
            tempo_import: import,
            finalize_withdrawal_batch_count: finalization,
            finalize_withdrawal_batch_encrypted_senders: vec![],
            transactions: vec![],
        });
        final_header = header;
    }
    let zone_id = ZonePortal::new(portal, l1.provider())
        .zoneId()
        .call()
        .await?;
    Ok(BatchWitness {
        public_inputs: PublicInputs {
            parent_chain_id: 1_337,
            zone_id,
            portal,
            tempo_block_number: final_header.number(),
            anchor_block_number: final_header.number(),
            anchor_block_hash: alloy_consensus::Sealable::hash_slow(&final_header),
            expected_withdrawal_batch_index: 2,
        },
        parent_header: parent.header.as_ref().clone(),
        zone_blocks: blocks,
        zone_state_witness: ZoneStateWitness {
            node_pool: zone_nodes.into_values().collect(),
            bytecodes: codes.into_values().collect(),
        },
        tempo_state_witness: TempoStateWitness {
            initial_tempo_header_rlp: Bytes::from(alloy_rlp::encode(initial.header.as_ref())),
            node_pool: tempo_nodes.into_values().collect(),
        },
        tempo_ancestry_headers: vec![],
    })
}
