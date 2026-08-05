//! End-to-end coverage for the Zone stateless proof function.
//!
//! These tests intentionally cover only two primary paths:
//!
//! 1. A two-block transition that carries user state between blocks.
//! 2. Execution-root parity for a non-empty production-builder block.

use std::{collections::BTreeMap, sync::Arc};

use alloy::{
    consensus::{BlockHeader as _, Sealable as _},
    genesis::{Genesis, GenesisAccount},
    primitives::{Address, B256, Bytes, U256, address, keccak256},
    providers::Provider as _,
    rpc::types::TransactionRequest,
};
use reth_chainspec::EthChainSpec as _;
use reth_trie_common::{EMPTY_ROOT_HASH, HashBuilder, Nibbles, TrieAccount, proof::ProofRetainer};
use tempo_chainspec::spec::{DEV, TEMPO_T0_BASE_FEE};
use tempo_precompiles::{PATH_USD_ADDRESS, storage::StorageKey as _, tip20::tip20_slots};
use tempo_primitives::TempoHeader;
use zone_chainspec::ZoneChainSpec;
use zone_spf::{
    BatchWitness, PublicInputs, SpfConfig, TempoStateWitness, ZoneBlock, ZoneStateWitness,
    prove_zone_batch,
};

use crate::utils::{
    DEFAULT_TIMEOUT, TIP20_TX_GAS, start_local_zone_with_fixture_and_withdrawal_batch_interval,
};

const ZONE_ID: u32 = 1;

#[tokio::test(flavor = "multi_thread")]
async fn spf_batch_execute() -> eyre::Result<()> {
    let genesis = funded_zone_genesis();
    let (zone, mut fixture) =
        start_local_zone_with_fixture_and_withdrawal_batch_interval(ZONE_ID, 2, 2, genesis.clone())
            .await?;
    let provider = zone.provider();
    let (wallet_provider, sender) = crate::utils::local_dev_zone_account(&zone)?;
    let initial_fee_balance = zone.balance_of(PATH_USD_ADDRESS, sender).await?;

    let first_pending = wallet_provider
        .send_transaction(state_changing_transaction(address!(
            "000000000000000000000000000000000000b001"
        )))
        .await?;
    let first_transaction_hash = *first_pending.tx_hash();
    let first_tempo_block = fixture.next_block();
    fixture.enqueue(&first_tempo_block, zone.deposit_queue(), vec![]);
    assert!(first_pending.get_receipt().await?.status());
    zone.wait_for_block_number(1, DEFAULT_TIMEOUT).await?;

    let second_pending = wallet_provider
        .send_transaction(state_changing_transaction(address!(
            "000000000000000000000000000000000000b002"
        )))
        .await?;
    let second_transaction_hash = *second_pending.tx_hash();
    let second_tempo_block = fixture.next_block();
    fixture.enqueue(&second_tempo_block, zone.deposit_queue(), vec![]);
    assert!(second_pending.get_receipt().await?.status());
    zone.wait_for_block_number(2, DEFAULT_TIMEOUT).await?;

    let genesis_block = provider
        .get_block_by_number(0.into())
        .await?
        .expect("Zone genesis block");
    let first_built_block = provider
        .get_block_by_number(1.into())
        .await?
        .expect("first builder-produced Zone block");
    let second_built_block = provider
        .get_block_by_number(2.into())
        .await?
        .expect("second builder-produced Zone block");
    assert_eq!(first_built_block.transactions.len(), 2);
    assert_eq!(second_built_block.transactions.len(), 3);
    assert_eq!(provider.get_transaction_count(sender).await?, 2);
    assert!(
        zone.balance_of(PATH_USD_ADDRESS, sender).await? < initial_fee_balance,
        "both user transactions must charge the sender's proven fee-token balance"
    );

    let first_raw_transaction = provider
        .get_raw_transaction_by_hash(first_transaction_hash)
        .await?
        .expect("first raw user transaction");
    let second_raw_transaction = provider
        .get_raw_transaction_by_hash(second_transaction_hash)
        .await?
        .expect("second raw user transaction");

    let (state_root, zone_state_witness) = zone_state_witness(&genesis);
    assert_eq!(state_root, genesis_block.header.state_root);
    let tempo_header = TempoHeader::default();
    let tempo_header_rlp = Bytes::from(alloy_rlp::encode(&tempo_header));
    let config = spf_config(&genesis);
    let parent_header = config.zone_chain_spec.genesis_header().clone();
    let parent_hash = parent_header.hash_slow();
    let first_hash = first_built_block.header.hash;
    let expected_hash = second_built_block.header.hash;

    let witness = BatchWitness {
        public_inputs: PublicInputs {
            zone_id: ZONE_ID,
            portal: Address::ZERO,
            tempo_block_number: second_tempo_block.header.number(),
            anchor_block_number: second_tempo_block.header.number(),
            anchor_block_hash: second_tempo_block.header.hash_slow(),
            expected_withdrawal_batch_index: 1,
        },
        zone_blocks: vec![
            ZoneBlock {
                number: first_built_block.header.number,
                parent_hash,
                timestamp: first_built_block.header.timestamp,
                beneficiary: first_built_block.header.beneficiary,
                tempo_header_rlp: Some(Bytes::from(alloy_rlp::encode(&first_tempo_block.header))),
                deposits: vec![],
                decryptions: vec![],
                enabled_tokens: vec![],
                finalize_withdrawal_batch_count: None,
                finalize_withdrawal_batch_encrypted_senders: vec![],
                transactions: vec![first_raw_transaction],
            },
            ZoneBlock {
                number: second_built_block.header.number,
                parent_hash: first_hash,
                timestamp: second_built_block.header.timestamp,
                beneficiary: second_built_block.header.beneficiary,
                tempo_header_rlp: Some(Bytes::from(alloy_rlp::encode(&second_tempo_block.header))),
                deposits: vec![],
                decryptions: vec![],
                enabled_tokens: vec![],
                finalize_withdrawal_batch_count: Some(U256::ZERO),
                finalize_withdrawal_batch_encrypted_senders: vec![],
                transactions: vec![second_raw_transaction],
            },
        ],
        parent_header: parent_header.clone(),
        zone_state_witness,
        tempo_state_witness: TempoStateWitness {
            initial_tempo_header_rlp: tempo_header_rlp,
            node_pool: vec![],
        },
        tempo_ancestry_headers: vec![],
    };

    let output = prove_zone_batch(&config, witness)?;

    assert_eq!(output.block_transition.prevBlockHash, parent_hash);
    assert_eq!(output.block_transition.nextBlockHash, expected_hash);
    assert_eq!(
        output.deposit_queue_transition.prevProcessedHash,
        B256::ZERO
    );
    assert_eq!(
        output.deposit_queue_transition.nextProcessedHash,
        B256::ZERO
    );
    assert_eq!(output.deposit_queue_transition.prevDepositNumber, 0);
    assert_eq!(output.deposit_queue_transition.nextDepositNumber, 0);
    assert_eq!(output.withdrawal_queue_hash, B256::ZERO);
    assert_eq!(output.last_batch_commitment.withdrawal_batch_index, 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn spf_builder_equivalence() -> eyre::Result<()> {
    let genesis = funded_zone_genesis();
    let (zone, mut fixture) =
        start_local_zone_with_fixture_and_withdrawal_batch_interval(ZONE_ID, 1, 1, genesis.clone())
            .await?;
    let (wallet_provider, sender) = crate::utils::local_dev_zone_account(&zone)?;
    let initial_fee_balance = zone.balance_of(PATH_USD_ADDRESS, sender).await?;
    let recipient = address!("000000000000000000000000000000000000b0b0");
    let pending = wallet_provider
        .send_transaction(state_changing_transaction(recipient))
        .await?;
    let user_transaction_hash = *pending.tx_hash();

    let mut l1_block = fixture.next_block();
    l1_block.header.timestamp_millis_part = 321;
    fixture.enqueue(&l1_block, zone.deposit_queue(), vec![]);
    let receipt = pending.get_receipt().await?;
    assert!(receipt.status(), "user transaction must succeed");
    zone.wait_for_block_number(1, DEFAULT_TIMEOUT).await?;

    let provider = zone.provider();
    let genesis_block = provider
        .get_block_by_number(0.into())
        .await?
        .expect("Zone genesis block");
    let built_block = provider
        .get_block_by_number(1.into())
        .await?
        .expect("builder-produced Zone block");

    // The production builder emits advanceTempo first, then the user transaction, and
    // finalization last.
    assert_eq!(built_block.transactions.len(), 3);
    assert_eq!(provider.get_transaction_count(sender).await?, 1);
    assert!(
        zone.balance_of(PATH_USD_ADDRESS, sender).await? < initial_fee_balance,
        "the user transaction must charge the sender's proven fee-token balance"
    );
    let raw_user_transaction = provider
        .get_raw_transaction_by_hash(user_transaction_hash)
        .await?
        .expect("raw user transaction");

    let (state_root, zone_state_witness) = zone_state_witness(&genesis);
    assert_eq!(state_root, genesis_block.header.state_root);

    let config = spf_config(&genesis);
    let parent_header = config.zone_chain_spec.genesis_header().clone();
    let parent_hash = parent_header.hash_slow();
    let expected_hash = built_block.header.hash;

    let tempo_header_rlp = Bytes::from(alloy_rlp::encode(&l1_block.header));
    let initial_tempo_header = TempoHeader::default();
    let sequencer = built_block.header.beneficiary;
    let witness = BatchWitness {
        public_inputs: PublicInputs {
            zone_id: ZONE_ID,
            portal: Address::ZERO,
            tempo_block_number: l1_block.header.number(),
            anchor_block_number: l1_block.header.number(),
            anchor_block_hash: l1_block.header.hash_slow(),
            expected_withdrawal_batch_index: 1,
        },
        parent_header: parent_header.clone(),
        zone_blocks: vec![ZoneBlock {
            number: built_block.header.number,
            parent_hash,
            timestamp: built_block.header.timestamp,
            beneficiary: sequencer,
            tempo_header_rlp: Some(tempo_header_rlp),
            deposits: vec![],
            decryptions: vec![],
            enabled_tokens: vec![],
            finalize_withdrawal_batch_count: Some(U256::ZERO),
            finalize_withdrawal_batch_encrypted_senders: vec![],
            transactions: vec![raw_user_transaction],
        }],
        zone_state_witness,
        tempo_state_witness: TempoStateWitness {
            initial_tempo_header_rlp: Bytes::from(alloy_rlp::encode(&initial_tempo_header)),
            node_pool: vec![],
        },
        tempo_ancestry_headers: vec![],
    };

    let output = prove_zone_batch(&config, witness)?;

    assert_eq!(
        output.block_transition.nextBlockHash, expected_hash,
        "SPF state, transaction, or receipt roots diverged from the production builder"
    );
    assert_eq!(
        output.deposit_queue_transition.prevProcessedHash,
        B256::ZERO
    );
    assert_eq!(
        output.deposit_queue_transition.nextProcessedHash,
        B256::ZERO
    );
    assert_eq!(output.withdrawal_queue_hash, B256::ZERO);
    assert_eq!(output.last_batch_commitment.withdrawal_batch_index, 1);
    Ok(())
}

fn state_changing_transaction(recipient: Address) -> TransactionRequest {
    TransactionRequest::default()
        .to(recipient)
        .value(U256::ZERO)
        .gas_limit(TIP20_TX_GAS)
        .gas_price(TEMPO_T0_BASE_FEE as u128)
}

fn funded_zone_genesis() -> Genesis {
    let mut genesis = zone_node::genesis::genesis_template().expect("valid Zone genesis template");
    let sender = address!("f39fd6e51aad88f6f4ce6ab8827279cfffb92266");
    let fee_balance_slot = sender.mapping_slot(tip20_slots::BALANCES);
    genesis
        .alloc
        .get_mut(&PATH_USD_ADDRESS)
        .expect("pathUSD genesis account")
        .storage
        .get_or_insert_default()
        .insert(
            B256::from(fee_balance_slot.to_be_bytes::<32>()),
            B256::from(U256::from(1_000_000_000_u64).to_be_bytes::<32>()),
        );
    genesis
}

fn spf_config(genesis: &Genesis) -> SpfConfig {
    let chain_spec =
        ZoneChainSpec::from_genesis(genesis.clone()).with_tempo_hardforks_from(DEV.as_ref());
    SpfConfig::new(Arc::new(chain_spec))
}

/// Convert the complete genesis allocation into the flat witness format consumed by SPF.
fn zone_state_witness(genesis: &Genesis) -> (B256, ZoneStateWitness) {
    let mut account_leaves = Vec::with_capacity(genesis.alloc.len());
    let mut node_pool = BTreeMap::<B256, Bytes>::new();
    let mut bytecodes = BTreeMap::<B256, Bytes>::new();

    for (address, account) in &genesis.alloc {
        let (storage_root, storage_nodes) = storage_trie(account);
        for node in storage_nodes {
            node_pool.entry(keccak256(&node)).or_insert(node);
        }

        let code_hash = account
            .code
            .as_ref()
            .map_or_else(|| keccak256([]), keccak256);
        if let Some(code) = &account.code {
            bytecodes.entry(code_hash).or_insert_with(|| code.clone());
        }
        let trie_account = TrieAccount {
            nonce: account.nonce.unwrap_or_default(),
            balance: account.balance,
            storage_root,
            code_hash,
        };
        account_leaves.push((keccak256(address), alloy_rlp::encode(trie_account)));
    }

    let (state_root, account_nodes) = trie(account_leaves);
    for node in account_nodes {
        node_pool.entry(keccak256(&node)).or_insert(node);
    }
    (
        state_root,
        ZoneStateWitness {
            node_pool: node_pool.into_values().collect(),
            bytecodes: bytecodes.into_values().collect(),
        },
    )
}

fn storage_trie(account: &GenesisAccount) -> (B256, Vec<Bytes>) {
    let leaves = account
        .storage
        .iter()
        .flat_map(|storage| storage.iter())
        .filter(|(_, value)| !value.is_zero())
        .map(|(slot, value)| {
            let value = U256::from_be_bytes(value.0);
            (keccak256(slot), alloy_rlp::encode(value))
        })
        .collect();
    trie(leaves)
}

fn trie(mut leaves: Vec<(B256, Vec<u8>)>) -> (B256, Vec<Bytes>) {
    if leaves.is_empty() {
        return (EMPTY_ROOT_HASH, vec![]);
    }

    leaves.sort_unstable_by_key(|(key, _)| *key);
    let targets = leaves.iter().map(|(key, _)| Nibbles::unpack(key)).collect();
    let mut builder: HashBuilder =
        HashBuilder::default().with_proof_retainer(ProofRetainer::new(targets));
    for (key, value) in leaves {
        builder.add_leaf(Nibbles::unpack(key), &value);
    }
    let root = builder.root();
    let nodes = builder
        .take_proof_nodes()
        .into_nodes_sorted()
        .into_iter()
        .map(|(_, node)| node)
        .collect();
    (root, nodes)
}
