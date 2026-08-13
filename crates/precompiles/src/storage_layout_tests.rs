//! Programmatic conformance tests for Solidity and native Rust storage layouts.

use std::path::PathBuf;

use tempo_precompiles::test_util::conformance::{
    RustStorageField, RustStorageSlot, SolidityStorageLayout, compare_storage_layout,
    compare_storage_slots, load_foundry_storage_layout,
};
use tempo_precompiles_macros::gen_test_fields_layout as layout_fields;
use zone_primitives::constants::{
    PORTAL_ADMIN_SLOT, PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT, PORTAL_ENCRYPTION_KEYS_SLOT,
    PORTAL_ENFORCEMENT_MODES_SLOT, PORTAL_IS_SEQUENCER_SLOT, PORTAL_MAX_TEMPO_GAS_RATE_SLOT,
    PORTAL_ROLE_SLOT, PORTAL_TOKEN_CONFIGS_SLOT, PORTAL_TOKEN_ENABLEMENT_HASH_SLOT,
};

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
fn zone_portal_slot_constants_match_solidity() {
    let solidity = artifact("ZonePortal");
    let fields = [
        ("admin", PORTAL_ADMIN_SLOT),
        (
            "currentDepositQueueHash",
            PORTAL_CURRENT_DEPOSIT_QUEUE_HASH_SLOT,
        ),
        ("_encryptionKeys", PORTAL_ENCRYPTION_KEYS_SLOT),
        ("_tokenConfigs", PORTAL_TOKEN_CONFIGS_SLOT),
        ("isSequencer", PORTAL_IS_SEQUENCER_SLOT),
        ("role", PORTAL_ROLE_SLOT),
        ("_isAccessEnforced", PORTAL_ENFORCEMENT_MODES_SLOT),
        ("maxTempoGasRate", PORTAL_MAX_TEMPO_GAS_RATE_SLOT),
        ("tokenEnablementHash", PORTAL_TOKEN_ENABLEMENT_HASH_SLOT),
    ]
    .map(|(name, slot)| RustStorageSlot::new(name, slot.into()));
    compare_storage_slots(&solidity, &fields)
        .unwrap_or_else(|errors| panic!("ZonePortal storage slots differ:\n{}", errors.join("\n")));
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
