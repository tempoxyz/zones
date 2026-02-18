//! End-to-end integration test with real deployed Solidity contracts.
//!
//! This test deploys all four zone predeploy contracts (TempoState, ZoneConfig,
//! ZoneInbox, ZoneOutbox) into an in-memory CacheDB, then builds a witness from
//! the initial state and runs `prove_zone_batch` to verify that the prover can
//! correctly execute real contract bytecode and compute the post-execution state
//! root.
//!
//! Requires Foundry artifacts: run `forge build --skip test` in `docs/specs/`.

use alloy_evm::Evm;
use alloy_primitives::{Address, B256, U256, keccak256, map::HashMap};
use revm::{
    DatabaseCommit,
    database::{CacheDB, EmptyDB},
    state::AccountInfo,
};
use zone_prover::{
    execute,
    prove_zone_batch,
    testutil::{TestAccount, build_zone_state_fixture, compute_state_root},
    types::*,
};
use zone_test_utils::{extract_db_accounts, setup_zone_evm, build_dummy_header_rlp};

const CHAIN_ID: u64 = 13371;
const SEQUENCER: Address = Address::ZERO;

/// Look up a storage slot value from the extracted account list.
fn find_storage_value(
    accounts: &[(Address, TestAccount)],
    target_addr: Address,
    slot: U256,
) -> U256 {
    accounts
        .iter()
        .find(|(a, _)| *a == target_addr)
        .and_then(|(_, acct)| {
            acct.storage
                .iter()
                .find(|(s, _)| *s == slot)
                .map(|(_, v)| *v)
        })
        .unwrap_or(U256::ZERO)
}

/// Convert `AccountSnapshot` from zone-test-utils to `TestAccount` from zone-prover.
fn snapshot_to_test_account(snap: &zone_test_utils::AccountSnapshot) -> TestAccount {
    TestAccount {
        nonce: snap.nonce,
        balance: snap.balance,
        code_hash: snap.code_hash,
        code: snap.code.clone(),
        storage: snap.storage.clone(),
    }
}

/// Execute a zone block on a CacheDB using the standard TempoEvm, then extract
/// all accounts from the post-state.
///
/// Uses `transact_system_call` for system transactions (same as the zone node),
/// avoiding the prover's custom TempoStateReader precompile.
///
/// Returns post-state accounts with ALL accessed storage slots (including zero-valued).
fn execute_on_cache_db(
    db: CacheDB<EmptyDB>,
    block: &ZoneBlock,
    is_last_block: bool,
) -> Vec<(Address, TestAccount)> {
    use alloy_evm::{EvmEnv, EvmFactory};
    use alloy_sol_types::SolCall;
    use revm::context::BlockEnv;
    use tempo_evm::evm::TempoEvmFactory;
    use tempo_chainspec::hardfork::TempoHardfork;
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

    // 1. advanceTempo (if block advances Tempo).
    if let Some(tempo_header_rlp) = &block.tempo_header_rlp {
        let calldata = zone_prover::types::advanceTempoCall {
            header: alloy_primitives::Bytes::copy_from_slice(tempo_header_rlp),
            deposits: vec![],
            decryptions: vec![],
        }
        .abi_encode();

        let result = evm
            .transact_system_call(
                Address::ZERO,
                execute::ZONE_INBOX_ADDRESS,
                alloy_primitives::Bytes::from(calldata),
            )
            .expect("advanceTempo should succeed");
        assert!(
            result.result.is_success(),
            "advanceTempo reverted: {:?}",
            result.result
        );
        evm.db_mut().commit(result.state);
    }

    // 2. finalizeWithdrawalBatch (final block only).
    if is_last_block {
        if let Some(count) = block.finalize_withdrawal_batch_count {
            let calldata = zone_prover::types::finalizeWithdrawalBatchCall {
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
            assert!(
                result.result.is_success(),
                "finalizeWithdrawalBatch reverted: {:?}",
                result.result
            );
            evm.db_mut().commit(result.state);
        }
    }

    // Extract all accounts (including zero-valued storage slots cached by CacheDB).
    let (post_db, _env) = evm.finish();
    post_db
        .cache
        .accounts
        .iter()
        .map(|(addr, acct)| {
            let info = &acct.info;
            let code = info.code.as_ref().map(|c| c.bytes().to_vec());
            let storage: Vec<(U256, U256)> = acct
                .storage
                .iter()
                .map(|(k, v)| (*k, *v))
                .collect();
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

/// Deploy all zone contracts and return the CacheDB with the system sender added.
fn deploy_zone_contracts() -> CacheDB<EmptyDB> {
    let mut evm = setup_zone_evm(CHAIN_ID);

    // Ensure Address::ZERO (system tx sender) exists in the state since
    // the prover accesses it as the transaction caller.
    if !evm.db_mut().cache.accounts.contains_key(&Address::ZERO) {
        evm.db_mut()
            .insert_account_info(Address::ZERO, AccountInfo::default());
    }

    let (db, _env) = evm.finish();
    db
}

/// Build a `ZoneStateWitness` from pre-execution account data, including all storage
/// slots that will be accessed during execution (discovered by the reference run).
///
/// `accessed_slots` contains (address, slot) pairs for every slot accessed during the
/// reference execution. Zero-valued slots are included in the witness `storage` map
/// so the prover can read them, but only non-zero slots get MPT storage proofs.
fn build_witness_with_accessed_slots(
    pre_accounts: &[(Address, TestAccount)],
    accessed_slots: &[(Address, U256)],
) -> zone_prover::testutil::ZoneStateFixture {
    // Merge accessed zero-valued slots into the pre-accounts.
    let mut enriched: Vec<(Address, TestAccount)> = pre_accounts.to_vec();

    for (addr, slot) in accessed_slots {
        if let Some((_, acct)) = enriched.iter_mut().find(|(a, _)| a == addr) {
            if !acct.storage.iter().any(|(s, _)| s == slot) {
                acct.storage.push((*slot, U256::ZERO));
            }
        }
    }

    build_zone_state_fixture(&enriched)
}

/// Collect all (address, slot) pairs from the post-execution CacheDB,
/// plus mandatory slots the prover reads outside of execute_zone_block.
fn collect_accessed_slots(post_accounts: &[(Address, TestAccount)]) -> Vec<(Address, U256)> {
    let mut slots: Vec<(Address, U256)> = post_accounts
        .iter()
        .flat_map(|(addr, acct)| acct.storage.iter().map(move |(slot, _)| (*addr, *slot)))
        .collect();

    // The prover reads these mandatory slots before/after block execution
    // in prove_zone_batch (not inside execute_zone_block).
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

/// Single-block test: finalizeWithdrawalBatch only (no advanceTempo).
///
/// Exercises the prover with real ZoneOutbox bytecode executing the finalization
/// system transaction.
#[test]
fn test_real_contracts_finalize_only() {
    let db = deploy_zone_contracts();

    // Extract pre-execution accounts (non-zero storage only).
    let pre_accounts: Vec<(Address, TestAccount)> = extract_db_accounts(&db)
        .into_iter()
        .map(|(addr, snap)| (addr, snapshot_to_test_account(&snap)))
        .collect();

    println!("Account count: {}", pre_accounts.len());

    // Build a temporary genesis header with a placeholder state root — we'll
    // rebuild it after discovering all accessed slots.
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
        expected_state_root: B256::ZERO, // placeholder
    };

    // Reference execution on CacheDB discovers all accessed slots and post-state.
    let post_accounts = execute_on_cache_db(db, &zone_block, true);

    // Discover all accessed storage slots and build the witness with them.
    let accessed_slots = collect_accessed_slots(&post_accounts);
    let fixture = build_witness_with_accessed_slots(&pre_accounts, &accessed_slots);

    // The state root may differ from the placeholder if we added zero-valued slots.
    // Zero-valued slots don't appear in the MPT so the root should be the same.
    println!("Initial state root: {}", fixture.state_root);

    // Rebuild the genesis header with the correct state root.
    let genesis_header = ZoneHeader {
        state_root: fixture.state_root,
        ..genesis_header
    };
    let genesis_hash = genesis_header.block_hash();

    // Compute expected post-state root from post-execution accounts.
    let expected_state_root = compute_state_root(&post_accounts);
    println!("Expected post-state root: {expected_state_root}");

    let zone_block = ZoneBlock {
        parent_hash: genesis_hash,
        expected_state_root,
        ..zone_block
    };

    let tempo_packed = find_storage_value(
        &pre_accounts,
        execute::TEMPO_STATE_ADDRESS,
        execute::storage::TEMPO_STATE_PACKED_SLOT,
    );
    let tempo_block_number =
        execute::storage::extract_tempo_block_number(tempo_packed);

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

    let witness = BatchWitness {
        public_inputs,
        chain_id: CHAIN_ID,
        prev_block_header: genesis_header,
        zone_blocks: vec![zone_block],
        initial_zone_state: fixture.witness,
        tempo_state_proofs: BatchStateProof {
            node_pool: HashMap::default(),
            reads: vec![],
            account_proofs: vec![],
        },
        tempo_ancestry_headers: vec![],
    };

    let output =
        prove_zone_batch(witness).expect("prove_zone_batch should succeed with real contracts");

    println!("Prover succeeded!");
    println!(
        "  prev_block_hash: {}",
        output.block_transition.prev_block_hash
    );
    println!(
        "  next_block_hash: {}",
        output.block_transition.next_block_hash
    );

    assert_eq!(output.block_transition.prev_block_hash, genesis_hash);
    assert_ne!(output.block_transition.next_block_hash, genesis_hash);
}

/// Single-block test: advanceTempo + finalizeWithdrawalBatch with real contracts.
///
/// Exercises both system transactions with real TempoState and ZoneInbox bytecode.
/// TempoState storage is mutated by finalizeTempo (called internally by advanceTempo).
///
/// IGNORED: advanceTempo currently reverts with real contracts (known issue,
/// see `crates/tempo-zone/tests/advance_tempo.rs`). Enable once the Solidity
/// contracts are fixed.
#[test]
#[ignore]
fn test_real_contracts_advance_and_finalize() {
    let db = deploy_zone_contracts();

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

    // Build a child Tempo header: number=1, parent_hash=genesis_hash.
    let genesis_tempo_rlp = build_dummy_header_rlp();
    let genesis_tempo_hash = keccak256(&genesis_tempo_rlp);

    let next_tempo_header = tempo_primitives::TempoHeader {
        inner: alloy_consensus::Header {
            number: 1,
            parent_hash: genesis_tempo_hash,
            ..Default::default()
        },
        ..Default::default()
    };
    let next_tempo_rlp = {
        use alloy_rlp::Encodable;
        let mut buf = Vec::new();
        next_tempo_header.encode(&mut buf);
        buf
    };

    let zone_block = ZoneBlock {
        number: 1,
        parent_hash: genesis_hash,
        timestamp: 1000,
        beneficiary: SEQUENCER,
        tempo_header_rlp: Some(next_tempo_rlp.clone()),
        deposits: vec![],
        decryptions: vec![],
        finalize_withdrawal_batch_count: Some(U256::ZERO),
        transactions: vec![],
        expected_state_root: B256::ZERO,
    };

    // Reference execution.
    let post_accounts = execute_on_cache_db(db, &zone_block, true);

    let accessed_slots = collect_accessed_slots(&post_accounts);
    let fixture = build_witness_with_accessed_slots(&pre_accounts, &accessed_slots);

    let genesis_header = ZoneHeader {
        state_root: fixture.state_root,
        ..genesis_header
    };
    let genesis_hash = genesis_header.block_hash();

    let expected_state_root = compute_state_root(&post_accounts);
    println!("Expected post-state root: {expected_state_root}");

    let zone_block = ZoneBlock {
        parent_hash: genesis_hash,
        expected_state_root,
        ..zone_block
    };

    let new_tempo_block_number = 1u64;
    let new_tempo_block_hash = keccak256(&next_tempo_rlp);

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

    let witness = BatchWitness {
        public_inputs,
        chain_id: CHAIN_ID,
        prev_block_header: genesis_header,
        zone_blocks: vec![zone_block],
        initial_zone_state: fixture.witness,
        tempo_state_proofs: BatchStateProof {
            node_pool: HashMap::default(),
            reads: vec![],
            account_proofs: vec![],
        },
        tempo_ancestry_headers: vec![],
    };

    let output = prove_zone_batch(witness)
        .expect("prove_zone_batch with advanceTempo should succeed");

    println!("Prover succeeded with advanceTempo!");
    println!(
        "  prev_block_hash: {}",
        output.block_transition.prev_block_hash
    );
    println!(
        "  next_block_hash: {}",
        output.block_transition.next_block_hash
    );

    assert_eq!(output.block_transition.prev_block_hash, genesis_hash);
    assert_ne!(output.block_transition.next_block_hash, genesis_hash);
}
