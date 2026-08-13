//! ABI compatibility checker between Zone Rust bindings and Solidity interfaces.

use std::path::PathBuf;

use eyre::bail;
use tempo_precompiles::test_util::{
    abi_conformance::{AbiSurface, compare_abi},
    foundry_artifact_path,
};

struct InterfaceSpec {
    name: &'static str,
    artifact_name: &'static str,
    source: &'static str,
    rust: fn() -> AbiSurface,
}

macro_rules! interface {
    ($name:ident, $artifact:literal) => {
        InterfaceSpec {
            name: stringify!($name),
            artifact_name: $artifact,
            source: "IZone.sol",
            rust: || AbiSurface::from_abi(&tempo_zone_contracts::$name::abi::contract()),
        }
    };
}

const INTERFACES: &[InterfaceSpec] = &[
    interface!(TempoState, "ITempoState"),
    interface!(IZoneInbox, "IZoneInbox"),
    interface!(IZoneOutbox, "IZoneOutbox"),
    interface!(ZoneFactory, "IZoneFactory"),
    interface!(ZonePortal, "IZonePortal"),
];

#[derive(Debug, clap::Args)]
pub(crate) struct CheckAbi {
    /// Foundry output directory produced from `specs/ref-impls`.
    #[arg(long, default_value = "specs/ref-impls/out")]
    artifacts: PathBuf,
}

impl CheckAbi {
    pub(crate) fn run(self) -> eyre::Result<()> {
        let mut failed = false;
        for spec in INTERFACES {
            let path = foundry_artifact_path(&self.artifacts, spec.source, spec.artifact_name);
            let rust = (spec.rust)();
            let errors = compare_abi(&path, &rust).err().unwrap_or_default();
            if errors.is_empty() {
                eprintln!("  ✓  {}", spec.name);
                continue;
            }
            failed = true;
            eprintln!("  ✗  {}", spec.name);
            for error in errors {
                eprintln!("    {error}");
            }
        }
        if failed {
            bail!("Zone ABI compatibility check found differences");
        }
        Ok(())
    }
}
