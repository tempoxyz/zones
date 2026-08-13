//! ABI compatibility checker between Zone Rust bindings and Solidity interfaces.

use std::{collections::BTreeSet, fs, path::PathBuf};

use alloy_json_abi::{ContractObject, JsonAbi};
use eyre::{Context, ContextCompat, bail};

struct InterfaceSpec {
    name: &'static str,
    artifact_name: &'static str,
    source: &'static str,
    rust: fn() -> JsonAbi,
}

macro_rules! interface {
    ($name:ident, $artifact:literal, $source:literal) => {
        InterfaceSpec {
            name: stringify!($name),
            artifact_name: $artifact,
            source: $source,
            rust: tempo_zone_contracts::$name::abi::contract,
        }
    };
}

const INTERFACES: &[InterfaceSpec] = &[
    interface!(TempoState, "TempoState", "TempoState.sol"),
    interface!(IZoneInbox, "IZoneInbox", "IZone.sol"),
    interface!(IZoneOutbox, "IZoneOutbox", "IZone.sol"),
    interface!(ZoneFactory, "IZoneFactory", "IZone.sol"),
    interface!(ZonePortal, "IZonePortal", "IZone.sol"),
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
            let artifact: ContractObject = serde_json::from_str(
                &fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?,
            )?;
            let solidity = artifact.abi.context("Foundry artifact has no ABI")?;
            let rust = (spec.rust)();

            let rust = surface(&rust);
            let solidity = surface(&solidity);
            let rust_only: Vec<_> = rust.difference(&solidity).collect();
            let solidity_only: Vec<_> = solidity.difference(&rust).collect();
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

fn surface(abi: &JsonAbi) -> BTreeSet<String> {
    let mut result = BTreeSet::new();
    result.extend(
        abi.functions()
            .map(|item| format!("function {}", item.signature())),
    );
    result.extend(
        abi.errors()
            .map(|item| format!("error {}", item.signature())),
    );
    result.extend(
        abi.events()
            .map(|item| format!("event {}", item.signature())),
    );
    result
}
