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
        "--out",
        dump.to_str().unwrap(),
    ])
    .args
    .run()
    .await
    .unwrap();

    for subcommand in ["init", "init-from-binary-dump"] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_tempo-zone"));
        command
            .arg(subcommand)
            .arg("--chain")
            .arg(&genesis)
            .arg("--datadir")
            .arg(&datadir);
        if subcommand == "init-from-binary-dump" {
            command.arg(&dump);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{subcommand} failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
