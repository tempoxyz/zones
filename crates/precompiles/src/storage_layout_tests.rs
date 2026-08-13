//! Programmatic conformance tests for Solidity and native Rust storage layouts.

use std::path::PathBuf;

use tempo_precompiles::test_util::conformance::{
    RustStorageField, SolidityStorageLayout, compare_storage_layout, load_foundry_storage_layout,
};
use tempo_precompiles_macros::gen_test_fields_layout as layout_fields;

fn artifact(contract: &str) -> SolidityStorageLayout {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../specs/ref-impls/out")
        .join(format!("{contract}.sol/{contract}.json"));
    load_foundry_storage_layout(&path).unwrap_or_else(|error| {
        panic!(
            "failed to read {}: {error}; run `forge build --root specs/ref-impls` first",
            path.display()
        )
    })
}

fn assert_layout(contract: &str, rust: Vec<RustStorageField>) {
    let solidity = artifact(contract);
    compare_storage_layout(&solidity, &rust).unwrap_or_else(|errors| {
        panic!("{contract} storage layout differs:\n{}", errors.join("\n"))
    });
}

#[test]
fn tempo_state_layout_matches_solidity() {
    use crate::tempo_state::slots;
    assert_layout(
        "TempoState",
        layout_fields!(tempo_block_hash, tempo_block_number),
    );
}

#[test]
fn zone_inbox_layout_matches_solidity() {
    use crate::inbox::slots;
    assert_layout(
        "ZoneInbox",
        layout_fields!(
            processed_deposit_queue_hash,
            processed_deposit_number,
            withdrawal_bounce_backs,
            processed_token_enablement_hash
        )
        .into_iter()
        .map(|field| match field.name {
            "withdrawalBounceBacks" => field.solidity_name("_refunds"),
            _ => field,
        })
        .collect(),
    );
}

#[test]
fn zone_outbox_layout_matches_solidity() {
    use crate::outbox::slots;
    assert_layout(
        "ZoneOutbox",
        layout_fields!(
            tempo_gas_rate,
            next_withdrawal_index,
            withdrawal_queue_hash,
            withdrawal_batch_index,
            max_withdrawals_per_block,
            withdrawals_this_block,
            current_block_number,
            last_finalized_timestamp,
            pending_withdrawals,
            last_fallback_nonce,
            fallback_recipients
        )
        .into_iter()
        .map(|field| match field.name {
            "withdrawalQueueHash" => field.solidity_name("_withdrawalQueueHash"),
            "withdrawalBatchIndex" => field.solidity_name("_withdrawalBatchIndex"),
            "withdrawalsThisBlock" => field.solidity_name("_withdrawalsThisBlock"),
            "currentBlockNumber" => field.solidity_name("_currentBlockNumber"),
            "pendingWithdrawals" => field.solidity_name("_pendingWithdrawals"),
            "fallbackRecipients" => field.solidity_name("_zoneFallbackRecipients"),
            _ => field,
        })
        .collect(),
    );
}
