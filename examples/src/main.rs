use alloy::primitives::Address;
use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use tracing_subscriber::EnvFilter;
use zone_examples::{
    HandoffDemoOptions, ServerConfig,
    chain::{detect_local_zone_config, normalize_http_rpc},
    run_handoff_demo, run_server,
};

#[derive(Debug, Parser)]
#[command(name = "handoff-demo")]
#[command(about = "Handoff-style encrypted deposit example for Tempo Zones")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Demo(DemoArgs),
    Server(ServerArgs),
}

#[derive(Debug, Args)]
struct DemoArgs {
    #[arg(long)]
    base_url: Option<String>,
    #[arg(long, default_value = "user@example.com")]
    email: String,
    #[arg(long, default_value_t = 5_000_000)]
    amount: u128,
    #[arg(long)]
    portal_address: Option<String>,
    #[arg(long)]
    l1_rpc_url: Option<String>,
    #[arg(long, default_value = "http://127.0.0.1:8546")]
    zone_rpc_url: String,
    #[arg(long, default_value = "0x20C0000000000000000000000000000000000000")]
    token_address: String,
    #[arg(long)]
    sender_private_key: Option<String>,
    #[arg(long, default_value_t = false)]
    skip_faucet: bool,
    #[arg(long, default_value_t = 45)]
    wait_timeout_secs: u64,
}

#[derive(Debug, Args)]
struct ServerArgs {
    #[arg(long, default_value = "127.0.0.1:3000")]
    addr: String,
    #[arg(long)]
    portal_address: Option<String>,
    #[arg(long)]
    l1_rpc_url: Option<String>,
    #[arg(long, default_value = "http://127.0.0.1:8546")]
    zone_rpc_url: String,
    #[arg(long, default_value = "0x20C0000000000000000000000000000000000000")]
    token_address: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("zone_examples=info")),
        )
        .with_target(false)
        .compact()
        .init();

    match Cli::parse().command {
        Command::Demo(args) => {
            run_handoff_demo(HandoffDemoOptions {
                base_url: args.base_url,
                email: args.email,
                amount: args.amount,
                portal_address: args.portal_address,
                l1_rpc_url: args.l1_rpc_url,
                zone_rpc_url: args.zone_rpc_url,
                token_address: args.token_address,
                sender_private_key: args.sender_private_key,
                skip_faucet: args.skip_faucet,
                wait_timeout_secs: args.wait_timeout_secs,
            })
            .await
        }
        Command::Server(args) => {
            let config = build_server_config(args)?;
            run_server(config).await
        }
    }
}

fn build_server_config(args: ServerArgs) -> Result<ServerConfig> {
    let detected = detect_local_zone_config(&args.zone_rpc_url)?;
    let portal_address: Address = args
        .portal_address
        .or_else(|| detected.as_ref().map(|config| config.portal_address.clone()))
        .context(
            "missing portal address; pass --portal-address or run against a detectable local tempo-zone process",
        )?
        .parse()
        .context("portal address is not a valid 0x-prefixed address")?;
    let l1_rpc_url = args
        .l1_rpc_url
        .or_else(|| detected.and_then(|config| config.l1_rpc_url))
        .map(|value| normalize_http_rpc(&value))
        .unwrap_or_else(|| zone_examples::chain::DEFAULT_L1_RPC_URL.to_string());

    Ok(ServerConfig {
        addr: args.addr.parse().context("server addr is not valid")?,
        l1_rpc_url,
        portal_address,
        token_address: args
            .token_address
            .parse()
            .context("token address is not a valid 0x-prefixed address")?,
    })
}
