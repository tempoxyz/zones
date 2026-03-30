//! xtask is a Swiss army knife of tools that help with running and testing tempo.
use crate::{
    create_zone::CreateZone, demo_blacklist::DemoBlacklist,
    demo_swap_and_deposit::DemoSwapAndDeposit, deploy_router::DeployRouter,
    encrypted_deposit::EncryptedDeposit, generate_zone_genesis::GenerateZoneGenesis,
    set_encryption_key::SetEncryptionKey, spam_deposits::SpamDeposits, zone_info::ZoneInfoCmd,
};
use clap::Parser as _;
use eyre::Context;

mod create_zone;
mod demo_blacklist;
mod demo_swap_and_deposit;
mod deploy_router;
mod encrypted_deposit;
mod generate_zone_genesis;
mod set_encryption_key;
mod spam_deposits;
mod zone_info;
mod zone_utils;

#[tokio::main]
async fn main() -> eyre::Result<()> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install rustls CryptoProvider");

    let args = Args::parse();
    match args.action {
        Action::CreateZone(args) => args.run().await.wrap_err("failed to create zone"),
        Action::DemoBlacklist(args) => args.run().await.wrap_err("failed to run blacklist demo"),
        Action::DemoSwapAndDeposit(args) => args
            .run()
            .await
            .wrap_err("failed to run swap-and-deposit demo"),
        Action::DeployRouter(args) => args.run().await.wrap_err("failed to deploy router"),
        Action::EncryptedDeposit(args) => args
            .run()
            .await
            .wrap_err("failed to send encrypted deposit"),
        Action::GenerateZoneGenesis(args) => {
            args.run().await.wrap_err("failed to generate zone genesis")
        }
        Action::SetEncryptionKey(args) => args.run().await.wrap_err("failed to set encryption key"),
        Action::SpamDeposits(args) => args.run().await.wrap_err("failed to spam deposits"),
        Action::ZoneInfo(args) => args.run().await.wrap_err("failed to fetch zone info"),
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
    DemoBlacklist(DemoBlacklist),
    DemoSwapAndDeposit(DemoSwapAndDeposit),
    DeployRouter(DeployRouter),
    EncryptedDeposit(EncryptedDeposit),
    GenerateZoneGenesis(GenerateZoneGenesis),
    SetEncryptionKey(SetEncryptionKey),
    SpamDeposits(SpamDeposits),
    ZoneInfo(ZoneInfoCmd),
}
