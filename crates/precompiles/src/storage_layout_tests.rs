//! Programmatic conformance tests for Solidity and native Rust storage layouts.

use std::{collections::BTreeMap, fs, path::PathBuf};

use alloy_primitives::U256;
use serde::Deserialize;
use tempo_precompiles_macros::gen_test_fields_layout as layout_fields;

#[derive(Debug, Deserialize)]
struct Artifact {
    #[serde(rename = "storageLayout")]
    storage_layout: StorageLayout,
}

#[derive(Debug, Deserialize)]
struct StorageLayout {
    storage: Vec<SolidityField>,
    types: BTreeMap<String, SolidityType>,
}

#[derive(Debug, Deserialize)]
struct SolidityField {
    label: String,
    slot: String,
    offset: usize,
    #[serde(rename = "type")]
    ty: String,
}

#[derive(Debug, Deserialize)]
struct SolidityType {
    #[serde(rename = "numberOfBytes")]
    number_of_bytes: String,
}

struct RustStorageField {
    name: &'static str,
    slot: U256,
    offset: usize,
    bytes: usize,
}

impl RustStorageField {
    fn new(name: &'static str, slot: U256, offset: usize, bytes: usize) -> Self {
        Self {
            name,
            slot,
            offset,
            bytes,
        }
    }

    fn solidity_name(mut self, name: &'static str) -> Self {
        self.name = name;
        self
    }
}

fn artifact(contract: &str) -> StorageLayout {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../specs/ref-impls/out")
        .join(format!("{contract}.sol/{contract}.json"));
    let json = fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "failed to read {}: {error}; run `forge build --root specs/ref-impls` first",
            path.display()
        )
    });
    serde_json::from_str::<Artifact>(&json)
        .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()))
        .storage_layout
}

fn assert_layout(contract: &str, rust: Vec<RustStorageField>) {
    let solidity = artifact(contract);
    let solidity_fields: BTreeMap<_, _> = solidity
        .storage
        .iter()
        .map(|field| (field.label.as_str(), field))
        .collect();

    let mut errors = Vec::new();
    for field in &rust {
        let Some(solidity_field) = solidity_fields.get(field.name) else {
            errors.push(format!("{} exists in Rust but not Solidity", field.name));
            continue;
        };
        let slot = U256::from_str_radix(&solidity_field.slot, 10).unwrap();
        let bytes = solidity
            .types
            .get(&solidity_field.ty)
            .and_then(|ty| ty.number_of_bytes.parse::<usize>().ok())
            .unwrap();
        if (slot, solidity_field.offset, bytes) != (field.slot, field.offset, field.bytes) {
            errors.push(format!(
                "{}: Solidity=({slot},{},{bytes}), Rust=({},{},{})",
                field.name, solidity_field.offset, field.slot, field.offset, field.bytes
            ));
        }
    }
    for name in solidity_fields.keys() {
        if !rust.iter().any(|field| field.name == *name) {
            errors.push(format!("{name} exists in Solidity but not Rust"));
        }
    }
    assert!(
        errors.is_empty(),
        "{contract} storage layout differs:\n{}",
        errors.join("\n")
    );
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
