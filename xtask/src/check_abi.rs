//! ABI compatibility checker between Zone Rust bindings and Solidity interfaces.

use std::path::PathBuf;

use eyre::bail;
use tempo_precompiles::test_util::conformance::{AbiSurface, load_foundry_abi};

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
            let path = self
                .artifacts
                .join(spec.source)
                .join(format!("{}.json", spec.artifact_name));
            let solidity =
                AbiSurface::from_abi(&load_foundry_abi(&path).map_err(|error| eyre::eyre!(error))?);
            let rust = (spec.rust)();

            let (rust_only, solidity_only) = rust.diff(&solidity);
            if rust_only.is_empty() && solidity_only.is_empty() {
                eprintln!("  ✓  {}", spec.name);
                continue;
            }
            failed = true;
            eprintln!("  ✗  {}", spec.name);
            for signature in rust_only {
                eprintln!("    only in Rust: {signature}");
            }
            for signature in solidity_only {
                eprintln!("    only in Solidity: {signature}");
            }
        }
        if failed {
            bail!("Zone ABI compatibility check found differences");
        }
        Ok(())
    }
}
