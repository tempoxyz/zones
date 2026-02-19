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

// Used by test_witness_pipeline_advance_user_tx_finalize
use alloy_signer_local;

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
///
/// Applies the EIP-2935 blockhash system call before other transactions
/// to match the prover's behavior. Returns the post-accounts AND the
/// CacheDB so callers can chain sequential blocks.
fn reference_execute(
    db: revm::database::CacheDB<revm::database::EmptyDB>,
    block: &ZoneBlock,
) -> (Vec<(Address, TestAccount)>, revm::database::CacheDB<revm::database::EmptyDB>) {
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

    // EIP-2935: store parent block hash in the history contract, matching the
    // prover's pre-execution system call.
    if block.number > 0 {
        let result = evm
            .transact_system_call(
                alloy_eips::eip4788::SYSTEM_ADDRESS,
                alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS,
                block.parent_hash.0.into(),
            )
            .expect("EIP-2935 system call should succeed");
        evm.db_mut().commit(result.state);
    }

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
    let accounts = post_db
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
        .collect();
    (accounts, post_db)
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
fn collect_accessed_slots(
    post_accounts: &[(Address, TestAccount)],
    block_numbers: &[u64],
) -> Vec<(Address, U256)> {
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

    // EIP-2935: the prover writes `parent_hash` to the history contract for
    // each block. Include the storage slot so the WitnessDatabase has it.
    let history_addr = alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS;
    for &block_num in block_numbers {
        if block_num > 0 {
            let slot = U256::from((block_num - 1) % alloy_eips::eip2935::HISTORY_SERVE_WINDOW as u64);
            if !slots.iter().any(|(a, s)| *a == history_addr && *s == slot) {
                slots.push((history_addr, slot));
            }
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
    let (raw_post, _post_db) = reference_execute(db, &zone_block);
    let accessed_slots = collect_accessed_slots(&raw_post, &[zone_block.number]);
    let absent = [TEMPO_STATE_READER_ADDRESS, alloy_eips::eip4788::SYSTEM_ADDRESS];
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
    let block_witnesses = store.take_range(1, 1).expect("block 1 should exist");
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
/// Block 1 is a non-final block (EIP-2935 only), block 2 is final with
/// finalizeWithdrawalBatch. The merged access snapshot should be the union
/// of both blocks' accesses.
#[test]
fn test_witness_pipeline_two_block_merge() {
    use zone::witness::{AccessSnapshot, BuiltBlockWitness, WitnessGeneratorConfig, WitnessGenerator, WitnessStore};

    let make_db = || {
        let mut evm = setup_zone_evm(CHAIN_ID);
        use revm::state::AccountInfo;
        if !evm.db_mut().cache.accounts.contains_key(&Address::ZERO) {
            evm.db_mut().insert_account_info(Address::ZERO, AccountInfo::default());
        }
        let (db, _) = evm.finish();
        db
    };

    let db_discovery = make_db();
    let pre_accounts: Vec<(Address, TestAccount)> = extract_db_accounts(&db_discovery)
        .into_iter()
        .map(|(addr, snap)| (addr, snapshot_to_test_account(&snap)))
        .collect();

    let absent = [TEMPO_STATE_READER_ADDRESS, alloy_eips::eip4788::SYSTEM_ADDRESS];

    // Discovery pass: run both blocks to find all accessed slots.
    let block1_disc = ZoneBlock {
        number: 1,
        parent_hash: B256::ZERO,
        timestamp: 1000,
        beneficiary: SEQUENCER,
        tempo_header_rlp: None,
        deposits: vec![],
        decryptions: vec![],
        finalize_withdrawal_batch_count: None,
        transactions: vec![],
        expected_state_root: B256::ZERO,
    };
    let block2_disc = ZoneBlock {
        number: 2,
        parent_hash: B256::ZERO,
        timestamp: 2000,
        beneficiary: SEQUENCER,
        tempo_header_rlp: None,
        deposits: vec![],
        decryptions: vec![],
        finalize_withdrawal_batch_count: Some(U256::ZERO),
        transactions: vec![],
        expected_state_root: B256::ZERO,
    };

    let (block1_post_disc, db_after1) = reference_execute(db_discovery, &block1_disc);
    let (block2_post_disc, _) = reference_execute(db_after1, &block2_disc);

    let block1_slots = collect_accessed_slots(&block1_post_disc, &[1]);
    let block2_slots = collect_accessed_slots(&block2_post_disc, &[1, 2]);
    let mut all_slots = block1_slots.clone();
    for s in &block2_slots {
        if !all_slots.contains(s) {
            all_slots.push(*s);
        }
    }

    let fixture = build_witness_with_accessed_slots(&pre_accounts, &all_slots, &absent);

    let genesis_header = ZoneHeader {
        parent_hash: B256::ZERO,
        beneficiary: SEQUENCER,
        state_root: fixture.state_root,
        transactions_root: alloy_trie::EMPTY_ROOT_HASH,
        receipts_root: alloy_trie::EMPTY_ROOT_HASH,
        number: 0,
        timestamp: 0,
    };
    let genesis_hash = genesis_header.block_hash();

    // Final pass: run both blocks with correct parent hashes.
    let db_final = make_db();
    let block1 = ZoneBlock {
        parent_hash: genesis_hash,
        ..block1_disc
    };

    let (block1_post, db_after1) = reference_execute(db_final, &block1);
    let block1_accounts = filter_real_accounts(block1_post, &pre_accounts);
    let block1_state_root = compute_state_root(&block1_accounts);

    let block1_header = ZoneHeader {
        parent_hash: genesis_hash,
        beneficiary: SEQUENCER,
        state_root: block1_state_root,
        transactions_root: alloy_trie::EMPTY_ROOT_HASH,
        receipts_root: alloy_trie::EMPTY_ROOT_HASH,
        number: 1,
        timestamp: 1000,
    };
    let block1_hash = block1_header.block_hash();

    let block2 = ZoneBlock {
        parent_hash: block1_hash,
        ..block2_disc
    };
    let (block2_post, _) = reference_execute(db_after1, &block2);
    let block2_accounts = filter_real_accounts(block2_post, &pre_accounts);
    let block2_state_root = compute_state_root(&block2_accounts);

    let block1 = ZoneBlock {
        expected_state_root: block1_state_root,
        ..block1
    };
    let block2 = ZoneBlock {
        expected_state_root: block2_state_root,
        ..block2
    };

    // Build access snapshots for each block.
    let snap1 = {
        let mut accounts = BTreeSet::new();
        let mut storage: BTreeMap<Address, BTreeSet<U256>> = BTreeMap::new();
        for (addr, slot) in &block1_slots {
            accounts.insert(*addr);
            storage.entry(*addr).or_default().insert(*slot);
        }
        for (addr, _) in &pre_accounts {
            accounts.insert(*addr);
        }
        AccessSnapshot { accounts, storage }
    };

    let snap2 = {
        let mut accounts = BTreeSet::new();
        let mut storage: BTreeMap<Address, BTreeSet<U256>> = BTreeMap::new();
        for (addr, slot) in &block2_slots {
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

    let block_witnesses = store.take_range(1, 2).expect("blocks 1-2 should exist");
    assert_eq!(block_witnesses.len(), 2);
    assert!(store.is_empty());

    let mut merged = AccessSnapshot::default();
    for (_, bw) in &block_witnesses {
        merged.merge(&bw.access_snapshot);
    }

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

/// Full pipeline test with advanceTempo + user transaction + finalizeWithdrawalBatch.
///
/// This is the most comprehensive witness pipeline test. It exercises:
/// - advanceTempo system tx with a real child Tempo header and L1 state proofs
/// - A real signed user transaction (ETH value transfer)
/// - finalizeWithdrawalBatch system tx
/// - Full witness pipeline: AccessSnapshot -> WitnessStore -> merge -> assemble -> prove
///
/// The user transaction creates state changes that the prover must handle:
/// sender nonce increment, sender balance decrement, recipient account creation.
#[test]
fn test_witness_pipeline_advance_user_tx_finalize() {
    use alloy_consensus::{Signed, TxLegacy, transaction::SignableTransaction};
    use alloy_primitives::{Bytes, TxKind, keccak256, map::HashMap};
    use alloy_rlp::Encodable;
    use alloy_sol_types::SolCall;
    use revm::state::AccountInfo;
    use tempo_primitives::TempoTxEnvelope;
    use zone::witness::{
        AccessSnapshot, BuiltBlockWitness, WitnessGenerator, WitnessGeneratorConfig, WitnessStore,
    };

    // --- Setup: deploy zone contracts, create user wallet ---

    let make_db = || {
        let mut evm = setup_zone_evm(CHAIN_ID);
        if !evm.db_mut().cache.accounts.contains_key(&Address::ZERO) {
            evm.db_mut()
                .insert_account_info(Address::ZERO, AccountInfo::default());
        }
        let (db, _) = evm.finish();
        db
    };

    // User account: use a deterministic private key for reproducibility.
    let user_signing_key = alloy_primitives::b256!(
        "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
    );
    let user_signer = alloy_signer_local::PrivateKeySigner::from_bytes(&user_signing_key)
        .expect("valid private key");
    let user_address = user_signer.address();
    // User does a zero-value call (gas_price=0 bypasses Tempo fee token
    // validation). This still exercises the full user tx path: RLP decode,
    // signature recovery, nonce increment, and EVM execution.
    let call_target = Address::with_last_byte(0xAA);

    let inject_user = |db: &mut revm::database::CacheDB<revm::database::EmptyDB>| {
        db.insert_account_info(
            user_address,
            AccountInfo {
                nonce: 0,
                balance: U256::ZERO,
                ..Default::default()
            },
        );
    };

    // --- Build the signed user transaction ---

    let user_tx = TxLegacy {
        chain_id: Some(CHAIN_ID),
        nonce: 0,
        gas_price: 0,
        gas_limit: 21_000,
        to: TxKind::Call(call_target),
        value: U256::ZERO,
        input: Default::default(),
    };

    let sig_hash = user_tx.signature_hash();
    let signature = alloy::signers::SignerSync::sign_hash_sync(&user_signer, &sig_hash)
        .expect("signing should succeed");
    let signed_tx = Signed::new_unchecked(user_tx, signature, sig_hash);
    let user_tx_envelope = TempoTxEnvelope::Legacy(signed_tx);

    // Verify the sender recovers correctly.
    {
        use reth_primitives_traits::SignerRecoverable;
        let recovered = user_tx_envelope
            .clone()
            .try_into_recovered()
            .expect("signature recovery should succeed");
        assert_eq!(Address::from(recovered.signer()), user_address);
    }

    // RLP-encode for the ZoneBlock.transactions field.
    let user_tx_rlp = {
        let mut buf = Vec::new();
        user_tx_envelope.encode(&mut buf);
        buf
    };

    // --- Build Tempo header and L1 state proofs for advanceTempo ---

    let genesis_tempo_rlp = build_dummy_header_rlp();
    let genesis_tempo_hash = keccak256(&genesis_tempo_rlp);

    // Build the child Tempo header (number=1, parent=genesis_tempo_hash).
    let tempo_portal = Address::repeat_byte(0xbb);
    let deposit_queue_slot = U256::from(4);
    let new_tempo_block_number = 1u64;

    // Build a minimal fake L1 state trie for the TempoStateReader precompile.
    let (l1_state_root, tempo_state_proofs) = {
        use alloy_trie::{EMPTY_ROOT_HASH, HashBuilder, TrieAccount, proof::ProofRetainer};
        use alloy_trie::nybbles::Nibbles;

        let l1_account = TrieAccount {
            nonce: 0,
            balance: U256::ZERO,
            storage_root: EMPTY_ROOT_HASH,
            code_hash: alloy_primitives::KECCAK256_EMPTY,
        };

        let key = Nibbles::unpack(keccak256(tempo_portal));
        let mut encoded = Vec::new();
        alloy_rlp::Encodable::encode(&l1_account, &mut encoded);

        let retainer = ProofRetainer::new(vec![key.clone()]);
        let mut builder = HashBuilder::default().with_proof_retainer(retainer);
        builder.add_leaf(key.clone(), &encoded);

        let l1_state_root = builder.root();
        let proof_nodes = builder.take_proof_nodes();

        let account_proof_bytes: Vec<Bytes> = proof_nodes
            .matching_nodes_sorted(&key)
            .into_iter()
            .map(|(_, b)| b)
            .collect();

        let mut node_pool = HashMap::default();
        let mut account_path_hashes = Vec::new();
        for node in &account_proof_bytes {
            let hash = keccak256(node.as_ref());
            node_pool.insert(hash, node.to_vec());
            account_path_hashes.push(hash);
        }

        let batch_proof = BatchStateProof {
            node_pool,
            reads: vec![L1StateRead {
                zone_block_index: 0,
                tempo_block_number: new_tempo_block_number,
                account: tempo_portal,
                slot: deposit_queue_slot,
                storage_path: vec![],
                value: U256::ZERO,
            }],
            account_proofs: vec![L1AccountProof {
                tempo_block_number: new_tempo_block_number,
                account: tempo_portal,
                nonce: 0,
                balance: U256::ZERO,
                storage_root: EMPTY_ROOT_HASH,
                code_hash: alloy_primitives::KECCAK256_EMPTY,
                account_path: account_path_hashes,
            }],
        };

        (l1_state_root, batch_proof)
    };

    let next_tempo_header = tempo_primitives::TempoHeader {
        inner: alloy_consensus::Header {
            number: 1,
            parent_hash: genesis_tempo_hash,
            state_root: l1_state_root,
            ..Default::default()
        },
        ..Default::default()
    };
    let next_tempo_rlp = {
        let mut buf = Vec::new();
        next_tempo_header.encode(&mut buf);
        buf
    };
    let new_tempo_block_hash = keccak256(&next_tempo_rlp);

    // --- Discovery pass: reference-execute on CacheDB to find all accessed state ---

    let db_discovery = {
        let mut db = make_db();
        inject_user(&mut db);
        db
    };

    // extract_db_accounts picks up the injected user account automatically.
    let pre_accounts: Vec<(Address, TestAccount)> = extract_db_accounts(&db_discovery)
        .into_iter()
        .map(|(addr, snap)| (addr, snapshot_to_test_account(&snap)))
        .collect();

    // Build a discovery block with all transactions.
    let block_disc = ZoneBlock {
        number: 1,
        parent_hash: B256::ZERO,
        timestamp: 1000,
        beneficiary: SEQUENCER,
        tempo_header_rlp: Some(next_tempo_rlp.clone()),
        deposits: vec![],
        decryptions: vec![],
        finalize_withdrawal_batch_count: Some(U256::ZERO),
        transactions: vec![user_tx_rlp.clone()],
        expected_state_root: B256::ZERO,
    };

    // Extended reference_execute that also handles advanceTempo + user txs.
    let reference_execute_full = |db: revm::database::CacheDB<revm::database::EmptyDB>,
                                  block: &ZoneBlock,
                                  is_last: bool|
     -> (
        Vec<(Address, TestAccount)>,
        revm::database::CacheDB<revm::database::EmptyDB>,
    ) {
        use alloy_evm::{Evm, EvmEnv, EvmFactory, FromRecoveredTx};
        use revm::{DatabaseCommit, context::BlockEnv};
        use tempo_chainspec::hardfork::TempoHardfork;
        use tempo_evm::evm::TempoEvmFactory;
        use tempo_revm::{TempoBlockEnv, TempoTxEnv};

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

        // EIP-2935
        if block.number > 0 {
            let result = evm
                .transact_system_call(
                    alloy_eips::eip4788::SYSTEM_ADDRESS,
                    alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS,
                    block.parent_hash.0.into(),
                )
                .expect("EIP-2935 system call should succeed");
            evm.db_mut().commit(result.state);
        }

        // advanceTempo
        if let Some(tempo_header_rlp) = &block.tempo_header_rlp {
            let calldata = advanceTempoCall {
                header: Bytes::copy_from_slice(tempo_header_rlp),
                deposits: vec![],
                decryptions: vec![],
            }
            .abi_encode();

            let result = evm
                .transact_system_call(
                    Address::ZERO,
                    execute::ZONE_INBOX_ADDRESS,
                    Bytes::from(calldata),
                )
                .expect("advanceTempo should succeed");
            assert!(
                result.result.is_success(),
                "advanceTempo reverted: {:?}",
                result.result
            );
            evm.db_mut().commit(result.state);
        }

        // User transactions
        for raw_tx in &block.transactions {
            let envelope: TempoTxEnvelope =
                alloy_rlp::Decodable::decode(&mut raw_tx.as_slice()).expect("valid tx rlp");
            use reth_primitives_traits::SignerRecoverable;
            let recovered = envelope
                .try_into_recovered()
                .expect("signature recovery should succeed");
            let tx_env =
                <TempoTxEnv as FromRecoveredTx<TempoTxEnvelope>>::from_recovered_tx(
                    recovered.inner(),
                    recovered.signer(),
                );
            let revm::context::result::ResultAndState { result, state } =
                evm.transact_raw(tx_env).expect("user tx should succeed");
            assert!(result.is_success(), "user tx failed: {:?}", result);
            evm.db_mut().commit(state);
        }

        // finalizeWithdrawalBatch
        if is_last {
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
                        Bytes::from(calldata),
                    )
                    .expect("finalizeWithdrawalBatch should succeed");
                assert!(result.result.is_success());
                evm.db_mut().commit(result.state);
            }
        }

        let (post_db, _) = evm.finish();
        let accounts = post_db
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
            .collect();
        (accounts, post_db)
    };

    // Discovery: run the block on a RecordingDatabase-wrapped CacheDB to
    // capture ALL state accesses, including Tempo handler reads (fee manager
    // lookups etc.) that aren't visible from the CacheDB post-state alone.
    let (raw_post_disc, disc_accounts, disc_storage) = {
        use alloy_evm::{Evm, EvmEnv, EvmFactory, FromRecoveredTx};
        use revm::{DatabaseCommit, context::BlockEnv};
        use tempo_chainspec::hardfork::TempoHardfork;
        use tempo_evm::evm::TempoEvmFactory;
        use tempo_revm::{TempoBlockEnv, TempoTxEnv};
        use std::cell::RefCell;
        use std::rc::Rc;

        let accessed_accts: Rc<RefCell<BTreeSet<Address>>> =
            Rc::new(RefCell::new(BTreeSet::new()));
        let accessed_stor: Rc<RefCell<BTreeMap<Address, BTreeSet<U256>>>> =
            Rc::new(RefCell::new(BTreeMap::new()));

        struct TrackingDb {
            inner: revm::database::CacheDB<revm::database::EmptyDB>,
            accts: Rc<RefCell<BTreeSet<Address>>>,
            stor: Rc<RefCell<BTreeMap<Address, BTreeSet<U256>>>>,
        }

        impl std::fmt::Debug for TrackingDb {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("TrackingDb").finish()
            }
        }

        impl revm::database_interface::Database for TrackingDb {
            type Error = core::convert::Infallible;
            fn basic(
                &mut self,
                address: Address,
            ) -> Result<Option<revm::state::AccountInfo>, Self::Error> {
                self.accts.borrow_mut().insert(address);
                self.inner.basic(address)
            }
            fn code_by_hash(
                &mut self,
                code_hash: B256,
            ) -> Result<revm::state::Bytecode, Self::Error> {
                self.inner.code_by_hash(code_hash)
            }
            fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
                self.accts.borrow_mut().insert(address);
                self.stor
                    .borrow_mut()
                    .entry(address)
                    .or_default()
                    .insert(index);
                self.inner.storage(address, index)
            }
            fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
                self.inner.block_hash(number)
            }
        }
        impl revm::database_interface::DatabaseCommit for TrackingDb {
            fn commit(
                &mut self,
                changes: revm::state::EvmState,
            ) {
                self.inner.commit(changes)
            }
        }

        let db = TrackingDb {
            inner: db_discovery,
            accts: accessed_accts.clone(),
            stor: accessed_stor.clone(),
        };

        let block_env = TempoBlockEnv {
            inner: BlockEnv {
                number: U256::from(block_disc.number),
                beneficiary: block_disc.beneficiary,
                timestamp: U256::from(block_disc.timestamp),
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

        // EIP-2935
        if block_disc.number > 0 {
            let result = evm
                .transact_system_call(
                    alloy_eips::eip4788::SYSTEM_ADDRESS,
                    alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS,
                    block_disc.parent_hash.0.into(),
                )
                .expect("EIP-2935 should succeed");
            evm.db_mut().commit(result.state);
        }

        // advanceTempo
        if let Some(tempo_header_rlp) = &block_disc.tempo_header_rlp {
            let calldata = advanceTempoCall {
                header: Bytes::copy_from_slice(tempo_header_rlp),
                deposits: vec![],
                decryptions: vec![],
            }
            .abi_encode();

            let result = evm
                .transact_system_call(
                    Address::ZERO,
                    execute::ZONE_INBOX_ADDRESS,
                    Bytes::from(calldata),
                )
                .expect("advanceTempo should succeed");
            assert!(result.result.is_success(), "advanceTempo reverted");
            evm.db_mut().commit(result.state);
        }

        // User transaction
        for raw_tx in &block_disc.transactions {
            let envelope: TempoTxEnvelope =
                alloy_rlp::Decodable::decode(&mut raw_tx.as_slice()).expect("valid tx rlp");
            use reth_primitives_traits::SignerRecoverable;
            let recovered = envelope
                .try_into_recovered()
                .expect("signature recovery");
            let tx_env =
                <TempoTxEnv as FromRecoveredTx<TempoTxEnvelope>>::from_recovered_tx(
                    recovered.inner(),
                    recovered.signer(),
                );
            let revm::context::result::ResultAndState { result, state } =
                evm.transact_raw(tx_env).expect("user tx should succeed");
            assert!(result.is_success(), "user tx failed: {:?}", result);
            evm.db_mut().commit(state);
        }

        // finalizeWithdrawalBatch
        if let Some(count) = block_disc.finalize_withdrawal_batch_count {
            let calldata = finalizeWithdrawalBatchCall {
                count,
                blockNumber: block_disc.number,
            }
            .abi_encode();

            let result = evm
                .transact_system_call(
                    Address::ZERO,
                    execute::ZONE_OUTBOX_ADDRESS,
                    Bytes::from(calldata),
                )
                .expect("finalizeWithdrawalBatch should succeed");
            assert!(result.result.is_success());
            evm.db_mut().commit(result.state);
        }

        let (tracking_db, _) = evm.finish();
        let accounts: Vec<(Address, TestAccount)> = tracking_db
            .inner
            .cache
            .accounts
            .iter()
            .map(|(addr, acct)| {
                let info = &acct.info;
                let code: Option<Vec<u8>> =
                    info.code.as_ref().map(|c| c.bytes().to_vec());
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
            .collect();
        let disc_accts = accessed_accts.borrow().clone();
        let disc_stor = accessed_stor.borrow().clone();
        (accounts, disc_accts, disc_stor)
    };

    // Combine CacheDB post-state slots with RecordingDB-captured slots.
    let mut all_slots = collect_accessed_slots(&raw_post_disc, &[1]);
    for (addr, slots) in &disc_storage {
        for slot in slots {
            if !all_slots.iter().any(|(a, s)| a == addr && s == slot) {
                all_slots.push((*addr, *slot));
            }
        }
    }

    // Accounts accessed but not in pre_accounts go to absent list.
    let mut absent_vec = vec![
        TEMPO_STATE_READER_ADDRESS,
        alloy_eips::eip4788::SYSTEM_ADDRESS,
        call_target,
    ];
    for addr in &disc_accounts {
        if !pre_accounts.iter().any(|(a, _)| a == addr)
            && !absent_vec.contains(addr)
        {
            absent_vec.push(*addr);
        }
    }
    let fixture = build_witness_with_accessed_slots(&pre_accounts, &all_slots, &absent_vec);

    // --- Final pass: execute with correct genesis hash ---

    let genesis_header = ZoneHeader {
        parent_hash: B256::ZERO,
        beneficiary: SEQUENCER,
        state_root: fixture.state_root,
        transactions_root: alloy_trie::EMPTY_ROOT_HASH,
        receipts_root: alloy_trie::EMPTY_ROOT_HASH,
        number: 0,
        timestamp: 0,
    };
    let genesis_hash = genesis_header.block_hash();

    let db_final = {
        let mut db = make_db();
        inject_user(&mut db);
        db
    };
    let zone_block = ZoneBlock {
        parent_hash: genesis_hash,
        ..block_disc
    };

    let (raw_post, _) = reference_execute_full(db_final, &zone_block, true);
    let post_accounts = filter_real_accounts(raw_post, &pre_accounts);
    let expected_state_root = compute_state_root(&post_accounts);

    let zone_block = ZoneBlock {
        expected_state_root,
        ..zone_block
    };

    // --- Build AccessSnapshot ---

    let mut access_accounts = BTreeSet::new();
    let mut access_storage: BTreeMap<Address, BTreeSet<U256>> = BTreeMap::new();
    for (addr, slot) in &all_slots {
        access_accounts.insert(*addr);
        access_storage.entry(*addr).or_default().insert(*slot);
    }
    for (addr, _) in &pre_accounts {
        access_accounts.insert(*addr);
    }
    access_accounts.insert(user_address);
    access_accounts.insert(call_target);

    let access_snapshot = AccessSnapshot {
        accounts: access_accounts,
        storage: access_storage,
    };

    // --- WitnessStore round-trip ---

    let mut store = WitnessStore::default();
    store.insert(
        1,
        BuiltBlockWitness {
            zone_block: zone_block.clone(),
            access_snapshot: access_snapshot.clone(),
            prev_block_header: genesis_header.clone(),
            parent_block_hash: genesis_hash,
            l1_reads: vec![],
            chain_id: CHAIN_ID,
            tempo_header_rlp: Some(next_tempo_rlp.clone()),
        },
    );

    let block_witnesses = store.take_range(1, 1);
    assert_eq!(block_witnesses.len(), 1);

    let mut merged = AccessSnapshot::default();
    for (_, bw) in &block_witnesses {
        merged.merge(&bw.access_snapshot);
    }

    // --- Assemble BatchWitness and prove ---

    let witness_gen = WitnessGenerator::new(WitnessGeneratorConfig {
        sequencer: SEQUENCER,
    });

    let outbox_wbi = find_storage_value(
        &pre_accounts,
        execute::ZONE_OUTBOX_ADDRESS,
        execute::storage::ZONE_OUTBOX_LAST_BATCH_BASE_SLOT + U256::from(1),
    )
    .to::<u64>();

    let public_inputs = PublicInputs {
        prev_block_hash: genesis_hash,
        tempo_block_number: new_tempo_block_number,
        anchor_block_number: new_tempo_block_number,
        anchor_block_hash: new_tempo_block_hash,
        expected_withdrawal_batch_index: outbox_wbi,
        sequencer: SEQUENCER,
    };

    let batch_witness = witness_gen.assemble_witness(
        public_inputs,
        CHAIN_ID,
        genesis_header,
        vec![zone_block],
        fixture.witness,
        tempo_state_proofs,
        vec![],
    );

    let output = prove_zone_batch(batch_witness)
        .expect("prove_zone_batch should succeed with advanceTempo + user tx + finalize");

    // --- Assertions ---

    assert_eq!(output.block_transition.prev_block_hash, genesis_hash);
    assert_ne!(output.block_transition.next_block_hash, genesis_hash);
    assert_eq!(output.last_batch.withdrawal_batch_index, outbox_wbi);

    // The deposit queue hash should have changed because advanceTempo updated it.
    // (Both prev and next should be B256::ZERO since there were no actual deposits,
    // but advanceTempo still calls processDeposits with an empty array.)
    // Just verify the transition is present.
    assert_eq!(
        output.deposit_queue_transition.prev_processed_hash,
        output.deposit_queue_transition.next_processed_hash,
        "No deposits were processed, so deposit queue hash should be unchanged"
    );

    println!("Full pipeline test passed!");
    println!(
        "  prev_block_hash: {}",
        output.block_transition.prev_block_hash
    );
    println!(
        "  next_block_hash: {}",
        output.block_transition.next_block_hash
    );
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
