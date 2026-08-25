//! Updates ZonePortal closed-loop enforcement and membership.

use alloy::{
    network::{EthereumWallet, ReceiptResponse as _},
    primitives::Address,
    providers::{DynProvider, Provider as _, ProviderBuilder},
    signers::local::PrivateKeySigner,
};
use eyre::{WrapErr as _, ensure};
use tempo_alloy::{TempoNetwork, provider::ext::TempoProviderExt, rpc::TempoCallBuilderExt};
use tempo_zone_contracts::{ZonePortal, ZonePortal::Role};
use zone_sequencer::nonce_keys::ADMIN_OPS_NONCE_KEY;

#[derive(Debug, clap::Args)]
struct PortalAccessArgs {
    /// Tempo L1 RPC URL.
    #[arg(long, env = "L1_RPC_URL")]
    l1_rpc_url: String,

    /// ZonePortal contract address on Tempo L1.
    #[arg(long, env = "L1_PORTAL_ADDRESS")]
    portal: Address,

    /// ZonePortal admin private key.
    #[arg(long, env = "ADMIN_KEY", hide_env_values = true)]
    admin_key: String,
}

#[derive(Debug, clap::Parser)]
pub(crate) struct SetAccessMode {
    #[command(flatten)]
    args: PortalAccessArgs,

    /// Require the Account role for deposits, refunds, and plain withdrawals.
    #[arg(long)]
    enforced: bool,
}

#[derive(Debug, clap::Parser)]
pub(crate) struct SetGatewayMode {
    #[command(flatten)]
    args: PortalAccessArgs,

    /// Require callback targets to have the CallbackGateway role.
    #[arg(long)]
    enforced: bool,
}

#[derive(Debug, clap::Parser)]
pub(crate) struct SetAllowedAccount {
    #[command(flatten)]
    args: PortalAccessArgs,

    /// Account whose closed-loop membership should be updated.
    account: Address,

    /// Add the Account role. Omit to remove it.
    #[arg(long)]
    allowed: bool,
}

#[derive(Debug, clap::Parser)]
pub(crate) struct SetGateway {
    #[command(flatten)]
    args: PortalAccessArgs,

    /// Callback gateway whose role should be updated.
    account: Address,

    /// Add the CallbackGateway role. Omit to remove it.
    #[arg(long)]
    allowed: bool,
}

impl SetAccessMode {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let (portal, provider, nonce) = connect(self.args).await?;
        let receipt = portal
            .setAccessMode(self.enforced)
            .nonce_key(ADMIN_OPS_NONCE_KEY)
            .nonce(nonce)
            .send()
            .await
            .wrap_err("failed sending ZonePortal.setAccessMode")?
            .get_receipt()
            .await
            .wrap_err("failed waiting for setAccessMode receipt")?;
        ensure!(receipt.status(), "setAccessMode transaction reverted");
        ensure!(
            portal.isAccessEnforced().call().await? == self.enforced,
            "portal access mode did not update"
        );
        print_success("account access mode", &receipt.transaction_hash());
        drop(provider);
        Ok(())
    }
}

impl SetGatewayMode {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let (portal, provider, nonce) = connect(self.args).await?;
        let receipt = portal
            .setGatewayMode(self.enforced)
            .nonce_key(ADMIN_OPS_NONCE_KEY)
            .nonce(nonce)
            .send()
            .await
            .wrap_err("failed sending ZonePortal.setGatewayMode")?
            .get_receipt()
            .await
            .wrap_err("failed waiting for setGatewayMode receipt")?;
        ensure!(receipt.status(), "setGatewayMode transaction reverted");
        ensure!(
            portal.isGatewayOpen().call().await? != self.enforced,
            "portal gateway mode did not update"
        );
        print_success("callback gateway mode", &receipt.transaction_hash());
        drop(provider);
        Ok(())
    }
}

impl SetAllowedAccount {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let (portal, provider, nonce) = connect(self.args).await?;
        let receipt = portal
            .setAllowedAccount(self.account, self.allowed)
            .nonce_key(ADMIN_OPS_NONCE_KEY)
            .nonce(nonce)
            .send()
            .await
            .wrap_err("failed sending ZonePortal.setAllowedAccount")?
            .get_receipt()
            .await
            .wrap_err("failed waiting for setAllowedAccount receipt")?;
        ensure!(receipt.status(), "setAllowedAccount transaction reverted");
        ensure!(
            portal.hasRole(self.account, Role::Account).call().await? == self.allowed,
            "portal account role did not update"
        );
        print_success("allowed account", &receipt.transaction_hash());
        drop(provider);
        Ok(())
    }
}

impl SetGateway {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let (portal, provider, nonce) = connect(self.args).await?;
        let receipt = portal
            .setGateway(self.account, self.allowed)
            .nonce_key(ADMIN_OPS_NONCE_KEY)
            .nonce(nonce)
            .send()
            .await
            .wrap_err("failed sending ZonePortal.setGateway")?
            .get_receipt()
            .await
            .wrap_err("failed waiting for setGateway receipt")?;
        ensure!(receipt.status(), "setGateway transaction reverted");
        ensure!(
            portal
                .hasRole(self.account, Role::CallbackGateway)
                .call()
                .await?
                == self.allowed,
            "portal gateway role did not update"
        );
        print_success("callback gateway", &receipt.transaction_hash());
        drop(provider);
        Ok(())
    }
}

async fn connect(
    args: PortalAccessArgs,
) -> eyre::Result<(
    tempo_zone_contracts::ZonePortal::ZonePortalInstance<DynProvider<TempoNetwork>, TempoNetwork>,
    DynProvider<TempoNetwork>,
    u64,
)> {
    let key = args.admin_key.strip_prefix("0x").unwrap_or(&args.admin_key);
    let signer: PrivateKeySigner = key.parse().wrap_err("ADMIN_KEY is not valid")?;
    let signer_address = signer.address();
    let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
        .wallet(EthereumWallet::from(signer))
        .connect(&args.l1_rpc_url)
        .await
        .wrap_err("failed connecting to Tempo L1 RPC")?
        .erased();
    let portal = ZonePortal::new(args.portal, provider.clone());
    let admin = portal
        .admin()
        .call()
        .await
        .wrap_err("failed reading portal admin")?;
    ensure!(
        signer_address == admin,
        "ADMIN_KEY signer {signer_address} is not portal admin {admin}"
    );
    let nonce = provider
        .get_transaction_count_with_nonce_key(signer_address, ADMIN_OPS_NONCE_KEY)
        .await
        .wrap_err("failed reading portal admin nonce")?;
    Ok((portal, provider, nonce))
}

fn print_success(operation: &str, tx_hash: &impl std::fmt::Display) {
    println!("Updated {operation}");
    println!("Transaction: {tx_hash}");
}
