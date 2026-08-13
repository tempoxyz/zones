//! Zone precompile test suite.

use std::path::PathBuf;

use tempo_precompiles::test_util::{
    foundry_artifact_path,
    storage_conformance::{RustStorageField, assert_foundry_layout},
};

mod storage_layouts;

fn artifact(contract: &str) -> PathBuf {
    foundry_artifact_path(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../specs/ref-impls/out"),
        &format!("{contract}.sol"),
        contract,
    )
}

fn assert_layout(contract: &str, rust: Vec<RustStorageField>) {
    assert_foundry_layout(&artifact(contract), &rust);
}
