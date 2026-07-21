//! Deploy and configure the non-secret L1 fixtures used by the private-Zone benchmark.

use alloy::{
    network::{EthereumWallet, TransactionBuilder},
    primitives::{Address, Bytes, U256},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol_types::SolValue,
};
use eyre::{Context as _, ensure, eyre};
use serde::Serialize;
use std::{fs, path::PathBuf};
use tempo_alloy::{TempoNetwork, rpc::TempoCallBuilderExt as _};
use tempo_contracts::precompiles::{IRolesAuth, ITIP20};
use tempo_zone_contracts::ZonePortal;

use crate::zone_utils::check;

alloy::sol! {
    #[sol(rpc)]
    interface ZonePortalMessengerView {
        function messenger() external view returns (address);
    }
}

#[derive(Debug, clap::Parser)]
pub(crate) struct DeployNeobankFixtures {
    /// Tempo L1 HTTP RPC URL. This command never defaults to a public endpoint.
    #[arg(long, env = "L1_RPC_URL")]
    l1_rpc_url: String,

    /// ZonePortal address created for this benchmark topology.
    #[arg(long, env = "L1_PORTAL_ADDRESS")]
    portal: Address,

    /// Native TIP-20 used for Zone deposits, transfer fees, and off-ramp.
    #[arg(long, env = "ZONES_BENCH_DLUSD")]
    dlusd: Address,

    /// Native TIP-20 used as the vault asset on L1.
    #[arg(long, env = "ZONES_BENCH_PATHUSD")]
    pathusd: Address,

    /// Native TIP-20 minted and burned by the vault fixture.
    #[arg(long, env = "ZONES_BENCH_EARN_TOKEN")]
    earn_token: Address,

    /// Directory containing Foundry artifacts for the fixture contracts.
    #[arg(long, default_value = "specs/ref-impls/out")]
    specs_out: PathBuf,

    /// Non-secret fixture metadata written for rendering the runtime scenario.
    #[arg(long)]
    output: PathBuf,

    /// Amount of each input asset seeded into the 1:1 swap fixture outside measurement.
    #[arg(long, default_value_t = 10_000_000_000_u128)]
    liquidity: u128,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FixtureMetadata {
    portal: String,
    messenger: String,
    zone_id: u32,
    dlusd: String,
    pathusd: String,
    earn_token: String,
    direct_swap: String,
    vault_adapter: String,
    gateway: String,
    bridge_wallet: String,
}

impl DeployNeobankFixtures {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        ensure!(self.liquidity > 0, "--liquidity must be greater than zero");
        let deployer = signer_from_env("FIXTURE_DEPLOYER_KEY")?;
        let portal_admin = signer_from_env("PORTAL_ADMIN_KEY")?;
        let deployer_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .wallet(EthereumWallet::from(deployer))
            .connect(&self.l1_rpc_url)
            .await
            .wrap_err("failed connecting fixture deployer to Tempo L1")?;
        let admin_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .wallet(EthereumWallet::from(portal_admin))
            .connect(&self.l1_rpc_url)
            .await
            .wrap_err("failed connecting portal admin to Tempo L1")?;

        let portal = ZonePortal::new(self.portal, &deployer_provider);
        let messenger = ZonePortalMessengerView::new(self.portal, &deployer_provider)
            .messenger()
            .call()
            .await
            .wrap_err("failed querying ZonePortal messenger")?;
        let zone_id = portal
            .zoneId()
            .call()
            .await
            .wrap_err("failed querying ZonePortal zone ID")?;
        ensure!(messenger != Address::ZERO, "ZonePortal messenger is zero");

        let direct_swap = deploy(
            &deployer_provider,
            load_bytecode(&self.specs_out, "DirectSwapFixture")?,
            "DirectSwapFixture",
        )
        .await?;
        let vault_adapter = deploy(
            &deployer_provider,
            with_constructor(
                load_bytecode(&self.specs_out, "VaultAdapterFixture")?,
                (self.pathusd, self.earn_token).abi_encode(),
            ),
            "VaultAdapterFixture",
        )
        .await?;
        let gateway = deploy(
            &deployer_provider,
            with_constructor(
                load_bytecode(&self.specs_out, "ZoneGatewayFixture")?,
                (vault_adapter, direct_swap, self.portal, messenger).abi_encode(),
            ),
            "ZoneGatewayFixture",
        )
        .await?;
        let bridge_wallet = deploy(
            &deployer_provider,
            load_bytecode(&self.specs_out, "BridgeWalletFixture")?,
            "BridgeWalletFixture",
        )
        .await?;

        let earn = ITIP20::new(self.earn_token, &deployer_provider);
        let issuer_role = earn
            .ISSUER_ROLE()
            .call()
            .await
            .wrap_err("failed querying EarnToken issuer role")?;
        let roles = IRolesAuth::new(self.earn_token, &deployer_provider);
        if !roles
            .hasRole(vault_adapter, issuer_role)
            .call()
            .await
            .wrap_err("failed querying vault issuer role")?
        {
            let receipt = roles
                .grantRole(issuer_role, vault_adapter)
                .fee_token(self.pathusd)
                .send()
                .await
                .wrap_err("failed granting the vault EarnToken issuer role")?
                .get_receipt()
                .await
                .wrap_err("failed waiting for vault issuer-role receipt")?;
            check(&receipt, "grant vault EarnToken issuer role")?;
        }

        let admin_portal = ZonePortal::new(self.portal, &admin_provider);
        if !admin_portal
            .isTokenEnabled(self.earn_token)
            .call()
            .await
            .wrap_err("failed querying EarnToken ZonePortal enablement")?
        {
            let receipt = admin_portal
                .enableToken(self.earn_token)
                .fee_token(self.pathusd)
                .send()
                .await
                .wrap_err("failed enabling EarnToken on ZonePortal")?
                .get_receipt()
                .await
                .wrap_err("failed waiting for EarnToken enablement receipt")?;
            check(&receipt, "enable EarnToken on ZonePortal")?;
        }

        for token in [self.dlusd, self.pathusd] {
            let receipt = ITIP20::new(token, &deployer_provider)
                .transfer(direct_swap, U256::from(self.liquidity))
                .fee_token(self.pathusd)
                .send()
                .await
                .wrap_err("failed seeding DirectSwap fixture liquidity")?
                .get_receipt()
                .await
                .wrap_err("failed waiting for DirectSwap liquidity receipt")?;
            check(&receipt, "seed DirectSwap fixture liquidity")?;
        }

        let metadata = FixtureMetadata {
            portal: self.portal.to_string(),
            messenger: messenger.to_string(),
            zone_id,
            dlusd: self.dlusd.to_string(),
            pathusd: self.pathusd.to_string(),
            earn_token: self.earn_token.to_string(),
            direct_swap: direct_swap.to_string(),
            vault_adapter: vault_adapter.to_string(),
            gateway: gateway.to_string(),
            bridge_wallet: bridge_wallet.to_string(),
        };
        if let Some(parent) = self.output.parent() {
            fs::create_dir_all(parent)
                .wrap_err_with(|| format!("failed creating {}", parent.display()))?;
        }
        fs::write(&self.output, serde_json::to_string_pretty(&metadata)?)
            .wrap_err_with(|| format!("failed writing {}", self.output.display()))?;
        println!("Private-Zone benchmark fixtures deployed");
        println!("  Gateway:       {gateway}");
        println!("  Vault adapter: {vault_adapter}");
        println!("  Bridge wallet: {bridge_wallet}");
        println!("  Metadata:      {}", self.output.display());
        Ok(())
    }
}

fn signer_from_env(name: &str) -> eyre::Result<PrivateKeySigner> {
    let key =
        std::env::var(name).wrap_err_with(|| format!("{name} must be set in the environment"))?;
    key.strip_prefix("0x")
        .unwrap_or(&key)
        .parse()
        .wrap_err_with(|| format!("{name} is not a valid private key"))
}

async fn deploy<P: Provider<TempoNetwork>>(
    provider: &P,
    bytecode: Vec<u8>,
    label: &str,
) -> eyre::Result<Address> {
    let receipt = provider
        .send_transaction(
            TransactionRequest::default()
                .with_kind(alloy::primitives::TxKind::Create)
                .input(Bytes::from(bytecode).into())
                .into(),
        )
        .await
        .wrap_err_with(|| format!("failed deploying {label}"))?
        .get_receipt()
        .await
        .wrap_err_with(|| format!("failed waiting for {label} deployment receipt"))?;
    check(&receipt, label)?;
    receipt
        .contract_address
        .ok_or_else(|| eyre!("{label} deployment receipt did not contain a contract address"))
}

fn with_constructor(mut bytecode: Vec<u8>, constructor: Vec<u8>) -> Vec<u8> {
    bytecode.extend_from_slice(&constructor);
    bytecode
}

fn load_bytecode(specs_out: &std::path::Path, contract: &str) -> eyre::Result<Vec<u8>> {
    let artifact_path = specs_out.join(format!("NeobankFixtures.sol/{contract}.json"));
    let artifact = fs::read_to_string(&artifact_path).wrap_err_with(|| {
        format!(
            "{contract} artifact not found at {}",
            artifact_path.display()
        )
    })?;
    let artifact: serde_json::Value = serde_json::from_str(&artifact)
        .wrap_err_with(|| format!("failed parsing {}", artifact_path.display()))?;
    let bytecode = artifact["bytecode"]["object"]
        .as_str()
        .ok_or_else(|| eyre!("missing bytecode in {}", artifact_path.display()))?;
    alloy::primitives::hex::decode(bytecode)
        .wrap_err_with(|| format!("invalid bytecode in {}", artifact_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser as _;

    fn required_args() -> [&'static str; 13] {
        [
            "deploy-neobank-fixtures",
            "--l1-rpc-url",
            "http://127.0.0.1:8545",
            "--portal",
            "0x0000000000000000000000000000000000000001",
            "--dlusd",
            "0x20c0000000000000000000000000000000000001",
            "--pathusd",
            "0x20c0000000000000000000000000000000000000",
            "--earn-token",
            "0x20c0000000000000000000000000000000000002",
            "--output",
            "target/fixtures.json",
        ]
    }

    #[test]
    fn fixture_deployment_does_not_accept_private_keys_as_arguments() {
        let error = DeployNeobankFixtures::try_parse_from(
            required_args()
                .into_iter()
                .chain(["--fixture-deployer-key", "0x01"]),
        )
        .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn fixture_deployment_uses_native_tip20_addresses_supplied_by_the_topology() {
        let command = DeployNeobankFixtures::try_parse_from(required_args()).unwrap();
        assert_eq!(command.liquidity, 10_000_000_000);
        assert_eq!(
            command.dlusd.to_string(),
            "0x20C0000000000000000000000000000000000001"
        );
    }
}
