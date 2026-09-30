//! Exercise the shared dump format through the Zone node's offline CLI.

use std::{fs, process::Command};

use clap::Parser;
use tempo_state_bloat::GenerateStateBloat;
use zone_node::cli::ZoneCli;

#[derive(Parser)]
struct Generate {
    #[command(flatten)]
    args: GenerateStateBloat,
}

/// Nextest archives relocate binaries, so prefer its runtime path over the compile-time one.
fn zone_command() -> Command {
    Command::new(
        std::env::var_os("NEXTEST_BIN_EXE_tempo-zone")
            .or_else(|| std::env::var_os("NEXTEST_BIN_EXE_tempo_zone"))
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_tempo-zone").into()),
    )
}

#[test]
fn binary_dump_import_requires_zone_genesis_without_node_arguments() {
    let mut chain = zone_node::genesis::genesis_template().unwrap();
    chain.config.chain_id = zone_primitives::constants::zone_chain_id(1337, 1).unwrap();
    let parsed = ZoneCli::try_parse_from([
        "tempo-zone",
        "init-from-binary-dump",
        "--chain",
        &serde_json::to_string(&chain).unwrap(),
        "state-bloat.bin",
        "--output-genesis",
        "bloated.json",
        "--manifest",
        "manifest.json",
    ])
    .unwrap();
    assert!(matches!(parsed, ZoneCli::Node(_)));

    chain.config.chain_id = 1337;
    assert!(
        ZoneCli::try_parse_from([
            "tempo-zone",
            "init-from-binary-dump",
            "--chain",
            &serde_json::to_string(&chain).unwrap(),
            "state-bloat.bin",
            "--output-genesis",
            "bloated.json",
            "--manifest",
            "manifest.json",
        ])
        .is_err()
    );
}

#[tokio::test]
async fn import_tempo_dump_into_zone_database() {
    let directory = tempfile::tempdir().unwrap();
    let genesis = directory.path().join("genesis.json");
    let datadir = directory.path().join("data");
    let dump = directory.path().join("state-bloat.bin");
    let bloated = directory.path().join("bloated.json");
    let manifest = directory.path().join("manifest.json");
    let mut chain = zone_node::genesis::genesis_template().unwrap();
    chain.config.chain_id = zone_primitives::constants::zone_chain_id(1337, 1).unwrap();
    fs::write(&genesis, serde_json::to_vec(&chain).unwrap()).unwrap();

    Generate::parse_from([
        "generate-state-bloat",
        "--size",
        "1",
        "--token",
        "0",
        "--signable-count",
        "1",
        "--mnemonic",
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
        "--out",
        dump.to_str().unwrap(),
    ])
    .args
    .run()
    .await
    .unwrap();

    for subcommand in ["init-from-binary-dump", "init", "init"] {
        let mut command = zone_command();
        command
            .arg(subcommand)
            .arg("--chain")
            .arg(if subcommand == "init" {
                &bloated
            } else {
                &genesis
            })
            .arg("--datadir")
            .arg(&datadir);
        if subcommand == "init-from-binary-dump" {
            command
                .arg(&dump)
                .arg("--output-genesis")
                .arg(&bloated)
                .arg("--manifest")
                .arg(&manifest);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{subcommand} failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    let evidence: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(
        evidence["committed_state_root"],
        evidence["database_state_root"]
    );
    assert_eq!(evidence["reopened"], true);
    assert!(evidence["entry_count"].as_u64().unwrap() > 16_000);
    let initialized: serde_json::Value =
        serde_json::from_slice(&fs::read(&bloated).unwrap()).unwrap();
    let mut checked = 0;
    tempo_state_bloat::read_dump(fs::File::open(&dump).unwrap(), |address, slot, value| {
        assert_eq!(
            initialized["alloc"][format!("{address:#x}")]["storage"][format!("{slot:#x}")],
            format!("0x{value:064x}")
        );
        checked += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(checked, evidence["entry_count"].as_u64().unwrap());
    // The original unbloated configuration must not reopen this database.
    let wrong_chain = zone_command()
        .arg("init")
        .arg("--chain")
        .arg(&genesis)
        .arg("--datadir")
        .arg(&datadir)
        .output()
        .unwrap();
    assert!(!wrong_chain.status.success());
    // Import never mutates an existing database, even if it is still at block zero.
    let repeat = zone_command()
        .arg("init-from-binary-dump")
        .arg("--chain")
        .arg(&genesis)
        .arg("--datadir")
        .arg(&datadir)
        .arg(&dump)
        .arg("--output-genesis")
        .arg(directory.path().join("repeat.json"))
        .arg("--manifest")
        .arg(directory.path().join("repeat-manifest.json"))
        .output()
        .unwrap();
    assert!(!repeat.status.success());
}

#[test]
fn invalid_dumps_do_not_initialize_a_database() {
    let directory = tempfile::tempdir().unwrap();
    let mut chain = zone_node::genesis::genesis_template().unwrap();
    chain.config.chain_id = zone_primitives::constants::zone_chain_id(1337, 1).unwrap();
    let genesis = directory.path().join("genesis.json");
    fs::write(&genesis, serde_json::to_vec(&chain).unwrap()).unwrap();
    let mut valid = b"TEMPOSB\0\0\x01\0\0".to_vec();
    valid.extend_from_slice(&[0x20, 0xc0]);
    valid.extend_from_slice(&[0; 18]);
    valid.extend_from_slice(&1u64.to_be_bytes());
    valid.extend_from_slice(&[42; 32]);
    valid.extend_from_slice(&[1; 32]);
    let mut duplicate = valid.clone();
    duplicate.extend_from_slice(&valid);
    let mut wrong_token = valid.clone();
    wrong_token[31] = 1;
    let mut conflicting = valid.clone();
    conflicting[40..72].fill(0);
    conflicting[71] = 2; // PathUSD name already present in genesis.
    for (index, dump) in [
        Vec::new(),
        valid[..103].to_vec(),
        duplicate,
        wrong_token,
        conflicting,
    ]
    .iter()
    .enumerate()
    {
        let dump_path = directory.path().join(format!("bad-{index}.bin"));
        let datadir = directory.path().join(format!("data-{index}"));
        fs::write(&dump_path, dump).unwrap();
        let output = zone_command()
            .arg("init-from-binary-dump")
            .arg("--chain")
            .arg(&genesis)
            .arg("--datadir")
            .arg(&datadir)
            .arg(&dump_path)
            .arg("--output-genesis")
            .arg(directory.path().join(format!("out-{index}.json")))
            .arg("--manifest")
            .arg(directory.path().join(format!("manifest-{index}.json")))
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!datadir.exists());
    }
    chain.alloc.clear();
    fs::write(&genesis, serde_json::to_vec(&chain).unwrap()).unwrap();
    fs::write(directory.path().join("valid.bin"), valid).unwrap();
    let output = zone_command()
        .arg("init-from-binary-dump")
        .arg("--chain")
        .arg(&genesis)
        .arg("--datadir")
        .arg(directory.path().join("missing-token"))
        .arg(directory.path().join("valid.bin"))
        .arg("--output-genesis")
        .arg(directory.path().join("missing-token.json"))
        .arg("--manifest")
        .arg(directory.path().join("missing-manifest.json"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!directory.path().join("missing-token").exists());
}
