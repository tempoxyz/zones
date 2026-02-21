//! xtask is a Swiss army knife of tools that help with running and testing tempo.
use crate::{create_zone::CreateZone, generate_zone_genesis::GenerateZoneGenesis};
use clap::Parser as _;
use eyre::Context;

mod create_zone;
mod generate_zone_genesis;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args = Args::parse();
    match args.action {
        Action::CreateZone(args) => args.run().await.wrap_err("failed to create zone"),
        Action::GenerateZoneGenesis(args) => {
            args.run().await.wrap_err("failed to generate zone genesis")
        }
    }
}

#[derive(Debug, clap::Parser)]
#[command(author)]
#[command(version)]
#[command(about)]
#[command(long_about = None)]
struct Args {
    #[command(subcommand)]
    action: Action,
}

#[derive(Debug, clap::Subcommand)]
enum Action {
    CreateZone(CreateZone),
    GenerateZoneGenesis(GenerateZoneGenesis),
}
