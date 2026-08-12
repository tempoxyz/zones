//! Pauses or resumes ZonePortal deposits and L1 withdrawal processing.

use alloy::{
    network::{EthereumWallet, ReceiptResponse as _},
    primitives::Address,
    providers::ProviderBuilder,
    signers::local::PrivateKeySigner,
};
use eyre::{WrapErr as _, ensure};
use tempo_alloy::TempoNetwork;
use tempo_zone_contracts::ZonePortal;

#[derive(Debug, clap::Args)]
pub(crate) struct PortalPauseArgs {
    /// Tempo L1 RPC URL.
    #[arg(long, env = "L1_RPC_URL")]
    l1_rpc_url: String,

    /// ZonePortal contract address on Tempo L1.
    #[arg(long, env = "L1_PORTAL_ADDRESS")]
    portal: Address,

    /// Private key authorized to send the transaction.
    #[arg(long, env = "PRIVATE_KEY", hide_env_values = true)]
    private_key: String,
}

#[derive(Debug, clap::Parser)]
pub(crate) struct PausePortal {
    #[command(flatten)]
    args: PortalPauseArgs,
}

#[derive(Debug, clap::Parser)]
pub(crate) struct ResumePortal {
    #[command(flatten)]
    args: PortalPauseArgs,
}

impl PausePortal {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        set_paused(self.args, true).await
    }
}

impl ResumePortal {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        set_paused(self.args, false).await
    }
}

async fn set_paused(args: PortalPauseArgs, pause: bool) -> eyre::Result<()> {
    let key = args
        .private_key
        .strip_prefix("0x")
        .unwrap_or(&args.private_key);
    let signer: PrivateKeySigner = key.parse().wrap_err("PRIVATE_KEY is not valid")?;
    let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
        .wallet(EthereumWallet::from(signer))
        .connect(&args.l1_rpc_url)
        .await
        .wrap_err("failed connecting to Tempo L1 RPC")?;
    let portal = ZonePortal::new(args.portal, &provider);

    let pending = if pause {
        portal
            .pause()
            .send()
            .await
            .wrap_err("failed sending ZonePortal.pause")?
    } else {
        portal
            .resume()
            .send()
            .await
            .wrap_err("failed sending ZonePortal.resume")?
    };
    let receipt = pending
        .get_receipt()
        .await
        .wrap_err("failed waiting for portal pause transaction receipt")?;
    ensure!(receipt.status(), "portal pause transaction reverted");
    ensure!(
        portal.paused().call().await? == pause,
        "portal pause state did not update"
    );

    println!(
        "Portal {} {}",
        args.portal,
        if pause { "paused" } else { "resumed" }
    );
    println!("Transaction: {}", receipt.transaction_hash());
    Ok(())
}
