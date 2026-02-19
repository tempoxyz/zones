//! End-to-end test for the witness generation pipeline.
//!
//! Simulates the full builder -> WitnessStore -> ProofGenerator -> prover flow:
//!
//! 1. Deploy zone contracts into a CacheDB.
//! 2. Build a `BuiltBlockWitness` as the builder would after executing a block.
//! 3. Insert it into a `WitnessStore`.
//! 4. Take the range and merge access snapshots (as `ProofGenerator` does).
//! 5. Build the zone state witness from the merged access data.
//! 6. Assemble the full `BatchWitness` and run the prover.
//!
//! This validates that the data structures flowing through the pipeline are
//! compatible and that the prover accepts the result.

use alloy_evm::Evm;
use alloy_primitives::{Address, B256, U256};
use std::collections::{BTreeMap, BTreeSet};
use zone_prover::{
    execute,
    prove_zone_batch,
    testutil::{TestAccount, build_zone_state_fixture_with_absent, compute_state_root},
    types::*,
};
use zone_test_utils::{
    TEMPO_STATE_READER_ADDRESS, extract_db_accounts, setup_zone_evm, build_dummy_header_rlp,
    register_mock_tempo_state_reader,
};

const CHAIN_ID: u64 = 13371;
const SEQUENCER: Address = Address::ZERO;

fn snapshot_to_test_account(snap: &zone_test_utils::AccountSnapshot) -> TestAccount {
    TestAccount {
        nonce: snap.nonce,
        balance: snap.balance,
        code_hash: snap.code_hash,
        code: snap.code.clone(),
        storage: snap.storage.clone(),
    }
}

/// Execute a zone block on a CacheDB to discover all state accesses.
fn reference_execute(
    db: revm::database::CacheDB<revm::database::EmptyDB>,
    block: &ZoneBlock,
) -> Vec<(Address, TestAccount)> {
    use alloy_evm::{Evm, EvmEnv, EvmFactory};
    use alloy_sol_types::SolCall;
    use revm::{DatabaseCommit, context::BlockEnv};
    use tempo_chainspec::hardfork::TempoHardfork;
    use tempo_evm::evm::TempoEvmFactory;
    use tempo_revm::TempoBlockEnv;

    let block_env = TempoBlockEnv {
        inner: BlockEnv {
            number: U256::from(block.number),
            beneficiary: block.beneficiary,
            timestamp: U256::from(block.timestamp),
            gas_limit: u64::MAX,
            basefee: 0,
            ..Default::default()
        },
        timestamp_millis_part: 0,
    };

    let mut cfg_env = revm::context::CfgEnv::new_with_spec(TempoHardfork::T0);
    cfg_env.chain_id = CHAIN_ID;
    cfg_env.tx_gas_limit_cap = Some(u64::MAX);

    let env = EvmEnv { cfg_env, block_env };
    let factory = TempoEvmFactory::default();
    let mut evm = factory.create_evm(db, env);
    register_mock_tempo_state_reader(&mut evm);

    if let Some(count) = block.finalize_withdrawal_batch_count {
        let calldata = finalizeWithdrawalBatchCall {
            count,
            blockNumber: block.number,
        }
        .abi_encode();

        let result = evm
            .transact_system_call(
                Address::ZERO,
                execute::ZONE_OUTBOX_ADDRESS,
                alloy_primitives::Bytes::from(calldata),
            )
            .expect("finalizeWithdrawalBatch should succeed");
        assert!(result.result.is_success());
        evm.db_mut().commit(result.state);
    }

    let (post_db, _) = evm.finish();
    post_db
        .cache
        .accounts
        .iter()
        .map(|(addr, acct)| {
            let info = &acct.info;
            let code = info.code.as_ref().map(|c| c.bytes().to_vec());
            let storage: Vec<(U256, U256)> =
                acct.storage.iter().map(|(k, v)| (*k, *v)).collect();
            (
                *addr,
                TestAccount {
                    nonce: info.nonce,
                    balance: info.balance,
                    code_hash: info.code_hash,
                    code,
                    storage,
                },
            )
        })
        .collect()
}

fn filter_real_accounts(
    post_accounts: Vec<(Address, TestAccount)>,
    pre_accounts: &[(Address, TestAccount)],
) -> Vec<(Address, TestAccount)> {
    post_accounts
        .into_iter()
        .filter(|(addr, acct)| {
            let in_pre = pre_accounts.iter().any(|(a, _)| a == addr);
            let is_non_empty = acct.nonce > 0
                || !acct.balance.is_zero()
                || acct.code_hash != alloy_primitives::KECCAK256_EMPTY;
            in_pre || is_non_empty
        })
        .collect()
}

/// Collect (address, slot) pairs from post-execution accounts + mandatory prover slots.
fn collect_accessed_slots(post_accounts: &[(Address, TestAccount)]) -> Vec<(Address, U256)> {
    let mut slots: Vec<(Address, U256)> = post_accounts
        .iter()
        .flat_map(|(addr, acct)| acct.storage.iter().map(move |(slot, _)| (*addr, *slot)))
        .collect();

    let mandatory = [
        (execute::TEMPO_STATE_ADDRESS, execute::storage::TEMPO_STATE_BLOCK_HASH_SLOT),
        (execute::TEMPO_STATE_ADDRESS, execute::storage::TEMPO_STATE_STATE_ROOT_SLOT),
        (execute::TEMPO_STATE_ADDRESS, execute::storage::TEMPO_STATE_PACKED_SLOT),
        (execute::ZONE_INBOX_ADDRESS, execute::storage::ZONE_INBOX_PROCESSED_HASH_SLOT),
        (execute::ZONE_OUTBOX_ADDRESS, execute::storage::ZONE_OUTBOX_LAST_BATCH_BASE_SLOT),
        (execute::ZONE_OUTBOX_ADDRESS, execute::storage::ZONE_OUTBOX_LAST_BATCH_BASE_SLOT + U256::from(1)),
    ];

    for (addr, slot) in mandatory {
        if !slots.iter().any(|(a, s)| *a == addr && *s == slot) {
            slots.push((addr, slot));
        }
    }

    slots
}

/// Build a witness fixture enriched with all accessed storage slots.
fn build_witness_with_accessed_slots(
    pre_accounts: &[(Address, TestAccount)],
    accessed_slots: &[(Address, U256)],
    absent_addresses: &[Address],
) -> zone_prover::testutil::ZoneStateFixture {
    let mut enriched: Vec<(Address, TestAccount)> = pre_accounts.to_vec();

    for (addr, slot) in accessed_slots {
        if let Some((_, acct)) = enriched.iter_mut().find(|(a, _)| a == addr) {
            if !acct.storage.iter().any(|(s, _)| s == slot) {
                acct.storage.push((*slot, U256::ZERO));
            }
        }
    }

    build_zone_state_fixture_with_absent(&enriched, absent_addresses)
}

fn find_storage_value(
    accounts: &[(Address, TestAccount)],
    target_addr: Address,
    slot: U256,
) -> U256 {
    accounts
        .iter()
        .find(|(a, _)| *a == target_addr)
        .and_then(|(_, acct)| {
            acct.storage.iter().find(|(s, _)| *s == slot).map(|(_, v)| *v)
        })
        .unwrap_or(U256::ZERO)
}

/// Full pipeline test: WitnessStore -> AccessSnapshot merge -> assemble -> prove.
///
/// This exercises the same data flow as `ProofGenerator::generate_batch_proof`,
/// validating that the `BuiltBlockWitness` -> merge -> assemble -> prover path
/// produces a valid proof.
#[test]
fn test_witness_pipeline_single_block() {
    use zone::witness::{AccessSnapshot, BuiltBlockWitness, WitnessGeneratorConfig, WitnessGenerator, WitnessStore};

    let db = {
        let mut evm = setup_zone_evm(CHAIN_ID);
        use revm::state::AccountInfo;
        if !evm.db_mut().cache.accounts.contains_key(&Address::ZERO) {
            evm.db_mut().insert_account_info(Address::ZERO, AccountInfo::default());
        }
        let (db, _) = evm.finish();
        db
    };

    let pre_accounts: Vec<(Address, TestAccount)> = extract_db_accounts(&db)
        .into_iter()
        .map(|(addr, snap)| (addr, snapshot_to_test_account(&snap)))
        .collect();

    let placeholder_root = compute_state_root(&pre_accounts);
    let genesis_header = ZoneHeader {
        parent_hash: B256::ZERO,
        beneficiary: SEQUENCER,
        state_root: placeholder_root,
        transactions_root: alloy_trie::EMPTY_ROOT_HASH,
        receipts_root: alloy_trie::EMPTY_ROOT_HASH,
        number: 0,
        timestamp: 0,
    };
    let genesis_hash = genesis_header.block_hash();

    let zone_block = ZoneBlock {
        number: 1,
        parent_hash: genesis_hash,
        timestamp: 1000,
        beneficiary: SEQUENCER,
        tempo_header_rlp: None,
        deposits: vec![],
        decryptions: vec![],
        finalize_withdrawal_batch_count: Some(U256::ZERO),
        transactions: vec![],
        expected_state_root: B256::ZERO,
    };

    // Reference execution discovers all accessed accounts/storage.
    let raw_post = reference_execute(db, &zone_block);
    let accessed_slots = collect_accessed_slots(&raw_post);
    let absent = [TEMPO_STATE_READER_ADDRESS];
    let fixture = build_witness_with_accessed_slots(&pre_accounts, &accessed_slots, &absent);

    let genesis_header = ZoneHeader {
        state_root: fixture.state_root,
        ..genesis_header
    };
    let genesis_hash = genesis_header.block_hash();

    let post_accounts = filter_real_accounts(raw_post, &pre_accounts);
    let expected_state_root = compute_state_root(&post_accounts);

    let zone_block = ZoneBlock {
        parent_hash: genesis_hash,
        expected_state_root,
        ..zone_block
    };

    // Build the AccessSnapshot as the builder would from RecordedAccesses.
    let mut access_accounts = BTreeSet::new();
    let mut access_storage: BTreeMap<Address, BTreeSet<U256>> = BTreeMap::new();
    for (addr, slot) in &accessed_slots {
        access_accounts.insert(*addr);
        access_storage.entry(*addr).or_default().insert(*slot);
    }
    for (addr, _) in &pre_accounts {
        access_accounts.insert(*addr);
    }

    let access_snapshot = AccessSnapshot {
        accounts: access_accounts,
        storage: access_storage,
    };

    // Store in WitnessStore (mimicking builder).
    let mut store = WitnessStore::default();
    let built_witness = BuiltBlockWitness {
        zone_block: zone_block.clone(),
        access_snapshot: access_snapshot.clone(),
        prev_block_header: genesis_header.clone(),
        parent_block_hash: genesis_hash,
        l1_reads: vec![],
        chain_id: CHAIN_ID,
        tempo_header_rlp: Some(build_dummy_header_rlp()),
    };
    store.insert(1, built_witness);

    assert_eq!(store.len(), 1);

    // Take from store (mimicking ProofGenerator).
    let block_witnesses = store.take_range(1, 1);
    assert_eq!(block_witnesses.len(), 1);
    assert!(store.is_empty());

    // Merge access snapshots (trivial for single block, but exercises the path).
    let mut merged = AccessSnapshot::default();
    for (_, bw) in &block_witnesses {
        merged.merge(&bw.access_snapshot);
    }

    // Assemble using WitnessGenerator.
    let witness_gen = WitnessGenerator::new(WitnessGeneratorConfig { sequencer: SEQUENCER });

    let tempo_packed = find_storage_value(
        &pre_accounts,
        execute::TEMPO_STATE_ADDRESS,
        execute::storage::TEMPO_STATE_PACKED_SLOT,
    );
    let tempo_block_number = execute::storage::extract_tempo_block_number(tempo_packed);
    let tempo_block_hash = B256::from(
        find_storage_value(
            &pre_accounts,
            execute::TEMPO_STATE_ADDRESS,
            execute::storage::TEMPO_STATE_BLOCK_HASH_SLOT,
        )
        .to_be_bytes(),
    );
    let outbox_wbi = find_storage_value(
        &pre_accounts,
        execute::ZONE_OUTBOX_ADDRESS,
        execute::storage::ZONE_OUTBOX_LAST_BATCH_BASE_SLOT + U256::from(1),
    )
    .to::<u64>();

    let public_inputs = PublicInputs {
        prev_block_hash: genesis_hash,
        tempo_block_number,
        anchor_block_number: tempo_block_number,
        anchor_block_hash: tempo_block_hash,
        expected_withdrawal_batch_index: outbox_wbi,
        sequencer: SEQUENCER,
    };

    let batch_witness = witness_gen.assemble_witness(
        public_inputs,
        CHAIN_ID,
        genesis_header,
        vec![zone_block],
        fixture.witness,
        BatchStateProof {
            node_pool: alloy_primitives::map::HashMap::default(),
            reads: vec![],
            account_proofs: vec![],
        },
        vec![],
    );

    // Run the prover.
    let output = prove_zone_batch(batch_witness)
        .expect("prove_zone_batch should succeed through witness pipeline");

    assert_eq!(output.block_transition.prev_block_hash, genesis_hash);
    assert_ne!(output.block_transition.next_block_hash, genesis_hash);
    assert_eq!(output.last_batch.withdrawal_batch_index, outbox_wbi);
}

/// Two-block pipeline test: verifies AccessSnapshot merging across blocks.
///
/// Block 1 is a non-final block (no system tx), block 2 is final with
/// finalizeWithdrawalBatch. The merged access snapshot should be the union
/// of both blocks' accesses.
#[test]
fn test_witness_pipeline_two_block_merge() {
    use zone::witness::{AccessSnapshot, BuiltBlockWitness, WitnessGeneratorConfig, WitnessGenerator, WitnessStore};

    let db = {
        let mut evm = setup_zone_evm(CHAIN_ID);
        use revm::state::AccountInfo;
        if !evm.db_mut().cache.accounts.contains_key(&Address::ZERO) {
            evm.db_mut().insert_account_info(Address::ZERO, AccountInfo::default());
        }
        let (db, _) = evm.finish();
        db
    };

    let pre_accounts: Vec<(Address, TestAccount)> = extract_db_accounts(&db)
        .into_iter()
        .map(|(addr, snap)| (addr, snapshot_to_test_account(&snap)))
        .collect();

    let placeholder_root = compute_state_root(&pre_accounts);
    let genesis_header = ZoneHeader {
        parent_hash: B256::ZERO,
        beneficiary: SEQUENCER,
        state_root: placeholder_root,
        transactions_root: alloy_trie::EMPTY_ROOT_HASH,
        receipts_root: alloy_trie::EMPTY_ROOT_HASH,
        number: 0,
        timestamp: 0,
    };

    // Block 1: non-final, no system txs.
    let block1 = ZoneBlock {
        number: 1,
        parent_hash: genesis_header.block_hash(),
        timestamp: 1000,
        beneficiary: SEQUENCER,
        tempo_header_rlp: None,
        deposits: vec![],
        decryptions: vec![],
        finalize_withdrawal_batch_count: None,
        transactions: vec![],
        expected_state_root: placeholder_root,
    };

    let block1_header = ZoneHeader {
        parent_hash: genesis_header.block_hash(),
        beneficiary: SEQUENCER,
        state_root: placeholder_root,
        transactions_root: alloy_trie::EMPTY_ROOT_HASH,
        receipts_root: alloy_trie::EMPTY_ROOT_HASH,
        number: 1,
        timestamp: 1000,
    };
    let block1_hash = block1_header.block_hash();

    // Block 2: final, with finalizeWithdrawalBatch.
    let block2 = ZoneBlock {
        number: 2,
        parent_hash: block1_hash,
        timestamp: 2000,
        beneficiary: SEQUENCER,
        tempo_header_rlp: None,
        deposits: vec![],
        decryptions: vec![],
        finalize_withdrawal_batch_count: Some(U256::ZERO),
        transactions: vec![],
        expected_state_root: B256::ZERO,
    };

    // Reference execution for block 2 (final block with system tx).
    let raw_post = reference_execute(db, &block2);
    let accessed_slots = collect_accessed_slots(&raw_post);
    let absent = [TEMPO_STATE_READER_ADDRESS];
    let fixture = build_witness_with_accessed_slots(&pre_accounts, &accessed_slots, &absent);

    let genesis_header = ZoneHeader {
        state_root: fixture.state_root,
        ..genesis_header
    };
    let genesis_hash = genesis_header.block_hash();

    let post_accounts = filter_real_accounts(raw_post, &pre_accounts);
    let expected_state_root = compute_state_root(&post_accounts);

    let block1 = ZoneBlock {
        parent_hash: genesis_hash,
        expected_state_root: fixture.state_root,
        ..block1
    };
    let block1_header = ZoneHeader {
        parent_hash: genesis_hash,
        state_root: fixture.state_root,
        ..block1_header
    };
    let block1_hash = block1_header.block_hash();
    let block2 = ZoneBlock {
        parent_hash: block1_hash,
        expected_state_root,
        ..block2
    };

    // Build access snapshots for each block.
    // Block 1 just touches the pre-state accounts.
    let snap1 = {
        let mut accounts = BTreeSet::new();
        for (addr, _) in &pre_accounts {
            accounts.insert(*addr);
        }
        AccessSnapshot { accounts, storage: BTreeMap::new() }
    };

    // Block 2 touches the same accounts + storage from execution.
    let snap2 = {
        let mut accounts = BTreeSet::new();
        let mut storage: BTreeMap<Address, BTreeSet<U256>> = BTreeMap::new();
        for (addr, slot) in &accessed_slots {
            accounts.insert(*addr);
            storage.entry(*addr).or_default().insert(*slot);
        }
        for (addr, _) in &pre_accounts {
            accounts.insert(*addr);
        }
        AccessSnapshot { accounts, storage }
    };

    // Store both blocks in WitnessStore.
    let mut store = WitnessStore::default();
    store.insert(
        1,
        BuiltBlockWitness {
            zone_block: block1.clone(),
            access_snapshot: snap1.clone(),
            prev_block_header: genesis_header.clone(),
            parent_block_hash: genesis_hash,
            l1_reads: vec![],
            chain_id: CHAIN_ID,
            tempo_header_rlp: Some(build_dummy_header_rlp()),
        },
    );
    store.insert(
        2,
        BuiltBlockWitness {
            zone_block: block2.clone(),
            access_snapshot: snap2.clone(),
            prev_block_header: block1_header.clone(),
            parent_block_hash: block1_hash,
            l1_reads: vec![],
            chain_id: CHAIN_ID,
            tempo_header_rlp: Some(build_dummy_header_rlp()),
        },
    );

    assert_eq!(store.len(), 2);

    // Take range and merge (as ProofGenerator does).
    let block_witnesses = store.take_range(1, 2);
    assert_eq!(block_witnesses.len(), 2);
    assert!(store.is_empty());

    let mut merged = AccessSnapshot::default();
    for (_, bw) in &block_witnesses {
        merged.merge(&bw.access_snapshot);
    }

    // Merged should contain the union.
    assert!(merged.accounts.len() >= snap1.accounts.len());
    assert!(merged.accounts.len() >= snap2.accounts.len());

    // Assemble and prove.
    let witness_gen = WitnessGenerator::new(WitnessGeneratorConfig { sequencer: SEQUENCER });

    let tempo_packed = find_storage_value(
        &pre_accounts,
        execute::TEMPO_STATE_ADDRESS,
        execute::storage::TEMPO_STATE_PACKED_SLOT,
    );
    let tempo_block_number = execute::storage::extract_tempo_block_number(tempo_packed);
    let tempo_block_hash = B256::from(
        find_storage_value(
            &pre_accounts,
            execute::TEMPO_STATE_ADDRESS,
            execute::storage::TEMPO_STATE_BLOCK_HASH_SLOT,
        )
        .to_be_bytes(),
    );
    let outbox_wbi = find_storage_value(
        &pre_accounts,
        execute::ZONE_OUTBOX_ADDRESS,
        execute::storage::ZONE_OUTBOX_LAST_BATCH_BASE_SLOT + U256::from(1),
    )
    .to::<u64>();

    let public_inputs = PublicInputs {
        prev_block_hash: genesis_hash,
        tempo_block_number,
        anchor_block_number: tempo_block_number,
        anchor_block_hash: tempo_block_hash,
        expected_withdrawal_batch_index: outbox_wbi,
        sequencer: SEQUENCER,
    };

    let batch_witness = witness_gen.assemble_witness(
        public_inputs,
        CHAIN_ID,
        genesis_header,
        vec![block1, block2],
        fixture.witness,
        BatchStateProof {
            node_pool: alloy_primitives::map::HashMap::default(),
            reads: vec![],
            account_proofs: vec![],
        },
        vec![],
    );

    let output = prove_zone_batch(batch_witness)
        .expect("two-block pipeline should succeed");

    assert_eq!(output.block_transition.prev_block_hash, genesis_hash);
    assert_ne!(output.block_transition.next_block_hash, genesis_hash);
    assert_ne!(output.block_transition.next_block_hash, block1_hash);
    assert_eq!(output.last_batch.withdrawal_batch_index, outbox_wbi);
}

/// Verify that WitnessStore::prune_below discards stale entries.
#[test]
fn test_witness_store_pruning() {
    use zone::witness::{AccessSnapshot, BuiltBlockWitness, WitnessStore};

    let dummy_header = ZoneHeader {
        parent_hash: B256::ZERO,
        beneficiary: SEQUENCER,
        state_root: B256::ZERO,
        transactions_root: alloy_trie::EMPTY_ROOT_HASH,
        receipts_root: alloy_trie::EMPTY_ROOT_HASH,
        number: 0,
        timestamp: 0,
    };

    let make_witness = |number: u64| BuiltBlockWitness {
        zone_block: ZoneBlock {
            number,
            parent_hash: B256::ZERO,
            timestamp: number * 1000,
            beneficiary: SEQUENCER,
            tempo_header_rlp: None,
            deposits: vec![],
            decryptions: vec![],
            finalize_withdrawal_batch_count: None,
            transactions: vec![],
            expected_state_root: B256::ZERO,
        },
        access_snapshot: AccessSnapshot::default(),
        prev_block_header: dummy_header.clone(),
        parent_block_hash: B256::ZERO,
        l1_reads: vec![],
        chain_id: CHAIN_ID,
        tempo_header_rlp: None,
    };

    let mut store = WitnessStore::default();
    for i in 1..=10 {
        store.insert(i, make_witness(i));
    }
    assert_eq!(store.len(), 10);

    // Prune below 5 — keeps 5..=10.
    store.prune_below(5);
    assert_eq!(store.len(), 6);

    // Blocks 1-4 should be gone.
    assert!(store.take(1).is_none());
    assert!(store.take(4).is_none());

    // Block 5 should still be there.
    assert!(store.take(5).is_some());
    assert_eq!(store.len(), 5);

    // Prune below 100 — clears everything.
    store.prune_below(100);
    assert!(store.is_empty());
}
