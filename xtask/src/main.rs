//! xtask is a Swiss army knife of tools that help with running and testing tempo.
use crate::{
    benchmark_preflight::BenchmarkPreflight, benchmark_results::BenchmarkResults,
    configure_benchmark_fees::ConfigureBenchmarkFees, create_zone::CreateZone,
    demo_blacklist::DemoBlacklist, demo_swap_and_deposit::DemoSwapAndDeposit,
    deploy_router::DeployRouter, encrypted_deposit::EncryptedDeposit,
    generate_p2p_key::GenerateP2pKey, generate_zone_genesis::GenerateZoneGenesis,
    install_reference_zone_factory::InstallReferenceZoneFactory,
    set_encryption_key::SetEncryptionKey, spam_deposits::SpamDeposits, zone_info::ZoneInfoCmd,
};
use clap::Parser as _;
use eyre::Context;

mod benchmark_preflight;
mod benchmark_results;
mod configure_benchmark_fees;
mod create_zone;
mod demo_blacklist;
mod demo_swap_and_deposit;
mod deploy_router;
mod encrypted_deposit;
mod generate_p2p_key;
mod generate_zone_genesis;
mod install_reference_zone_factory;
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
        Action::BenchmarkPreflight(args) => {
            (*args).run().await.wrap_err("benchmark preflight failed")
        }
        Action::BenchmarkResults(args) => args.run().wrap_err("failed to render benchmark results"),
        Action::ConfigureBenchmarkFees(args) => args
            .run()
            .await
            .wrap_err("failed to configure benchmark fees"),
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
        Action::GenerateP2pKey(args) => args.run().wrap_err("failed to generate P2P key"),
        Action::InstallReferenceZoneFactory(args) => args
            .run()
            .wrap_err("failed to install reference ZoneFactory"),
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
    BenchmarkPreflight(Box<BenchmarkPreflight>),
    BenchmarkResults(BenchmarkResults),
    ConfigureBenchmarkFees(ConfigureBenchmarkFees),
    CreateZone(CreateZone),
    DemoBlacklist(DemoBlacklist),
    DemoSwapAndDeposit(DemoSwapAndDeposit),
    DeployRouter(DeployRouter),
    EncryptedDeposit(EncryptedDeposit),
    GenerateP2pKey(GenerateP2pKey),
    GenerateZoneGenesis(GenerateZoneGenesis),
    InstallReferenceZoneFactory(InstallReferenceZoneFactory),
    SetEncryptionKey(SetEncryptionKey),
    SpamDeposits(SpamDeposits),
    ZoneInfo(ZoneInfoCmd),
}
