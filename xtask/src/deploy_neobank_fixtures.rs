//! Deploy and configure the non-secret L1 fixtures used by the private-Zone benchmark.

use alloy::{
    network::{EthereumWallet, TransactionBuilder, primitives::ReceiptResponse},
    primitives::{Address, Bytes, U256, Uint},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol_types::{SolCall, SolConstructor, SolValue},
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

    #[sol(rpc)]
    interface StablecoinDEX {
        function createPair(address base) external returns (bytes32 key);
        function place(address token, uint128 amount, bool isBid, int16 tick) external returns (uint128 orderId);
    }

    #[sol(rpc)]
    contract FixtureVaultAdapter {
        struct FixedFeeRecipient {
            address account;
            uint96 rate;
        }

        struct ExcessReturnFee {
            bool enabled;
            address account;
            uint96 annualTargetRate;
            uint96 excessFeeRate;
        }

        struct FeeConfig {
            uint8 fixedFeeCount;
            FixedFeeRecipient[4] fixedFees;
            ExcessReturnFee excess;
        }

        struct FeeInit {
            address administrator;
            address guardian;
            uint96 fixedFeeCap;
            uint96 excessFeeCap;
            FeeConfig initialConfig;
        }

        function initialize(
            address engine_,
            address shareToken_,
            address operator_,
            FeeInit calldata feeInit_
        ) external;
    }

    #[sol(rpc)]
    interface ERC4626EngineInitializer {
        function initializeCore(address core) external;
    }

    #[sol(rpc)]
    interface ClosedLoopZoneGatewayRoutes {
        function setDepositRoute(address inputToken, address swapper) external;
        function setRedeemRoute(address outputToken, address swapper) external;
    }

    contract FixtureSimple4626Vault {
        constructor(address asset_, string name_, string symbol_, uint8 decimals_);
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

    /// Native TIP-20 minted and burned by the copied Earn VaultAdapter fixture.
    #[arg(long, env = "ZONES_BENCH_EARN_TOKEN")]
    earn_token: Address,

    /// Directory containing Foundry artifacts for the copied Earn fixtures and bridge wallet.
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
        let deployer_address = deployer.address();
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

        let swapper = deploy(
            &deployer_provider,
            with_constructor(
                load_bytecode(
                    &self.specs_out,
                    "TempoStablecoinDexStableSwapAdapter.sol/TempoStablecoinDexStableSwapAdapter",
                )?,
                (crate::zone_utils::STABLECOIN_DEX_ADDRESS,).abi_encode(),
            ),
            "TempoStablecoinDexStableSwapAdapter",
        )
        .await?;
        let vault = deploy(
            &deployer_provider,
            with_constructor(
                load_bytecode(&self.specs_out, "Simple4626Vault.sol/Simple4626Vault")?,
                FixtureSimple4626Vault::constructorCall {
                    asset_: self.pathusd,
                    name_: "Neobank benchmark vault".to_owned(),
                    symbol_: "nbVAULT".to_owned(),
                    decimals_: 6,
                }
                .abi_encode(),
            ),
            "Simple4626Vault",
        )
        .await?;
        let engine = deploy(
            &deployer_provider,
            with_constructor(
                load_bytecode(&self.specs_out, "ERC4626Engine.sol/ERC4626Engine")?,
                (vault, deployer_address, String::new(), String::new()).abi_encode(),
            ),
            "ERC4626Engine",
        )
        .await?;
        let adapter_implementation = deploy(
            &deployer_provider,
            load_bytecode(&self.specs_out, "VaultAdapter.sol/VaultAdapter")?,
            "VaultAdapter implementation",
        )
        .await?;
        let initialization = FixtureVaultAdapter::initializeCall {
            engine_: engine,
            shareToken_: self.earn_token,
            operator_: deployer_address,
            feeInit_: zero_fee_init(),
        }
        .abi_encode();
        let vault_adapter = deploy(
            &deployer_provider,
            with_constructor(
                load_bytecode(&self.specs_out, "TestERC1967Proxy.sol/TestERC1967Proxy")?,
                (adapter_implementation, Bytes::from(initialization)).abi_encode(),
            ),
            "TestERC1967Proxy",
        )
        .await?;
        let issuer_role = ITIP20::new(self.earn_token, &deployer_provider)
            .ISSUER_ROLE()
            .call()
            .await
            .wrap_err("failed querying EarnToken issuer role")?;
        let receipt = IRolesAuth::new(self.earn_token, &deployer_provider)
            .grantRole(issuer_role, vault_adapter)
            .fee_token(self.pathusd)
            .send()
            .await
            .wrap_err("failed granting the copied VaultAdapter issuer role")?
            .get_receipt()
            .await
            .wrap_err("failed waiting for copied VaultAdapter issuer role")?;
        check(&receipt, "grant copied VaultAdapter issuer role")?;
        let receipt = ERC4626EngineInitializer::new(engine, &deployer_provider)
            .initializeCore(vault_adapter)
            .fee_token(self.pathusd)
            .send()
            .await
            .wrap_err("failed initializing the canonical ERC4626 engine")?
            .get_receipt()
            .await
            .wrap_err("failed waiting for canonical ERC4626 engine initialization")?;
        check(&receipt, "initialize canonical ERC4626 engine")?;
        let gateway = deploy(
            &deployer_provider,
            with_constructor(
                load_bytecode(
                    &self.specs_out,
                    "ClosedLoopZoneGateway.sol/ClosedLoopZoneGateway",
                )?,
                (
                    vault_adapter,
                    swapper,
                    self.portal,
                    messenger,
                    deployer_address,
                )
                    .abi_encode(),
            ),
            "ClosedLoopZoneGateway",
        )
        .await?;
        let bridge_wallet = deploy(
            &deployer_provider,
            load_bytecode(
                &self.specs_out,
                "BridgeWalletFixture.sol/BridgeWalletFixture",
            )?,
            "BridgeWalletFixture",
        )
        .await?;

        let gateway_routes = ClosedLoopZoneGatewayRoutes::new(gateway, &deployer_provider);
        let receipt = gateway_routes
            .setDepositRoute(self.dlusd, swapper)
            .fee_token(self.pathusd)
            .send()
            .await
            .wrap_err("failed to configure DLUSD deposit route")?
            .get_receipt()
            .await
            .wrap_err("failed waiting to configure DLUSD deposit route")?;
        check(&receipt, "configure DLUSD deposit route")?;
        let receipt = gateway_routes
            .setRedeemRoute(self.dlusd, swapper)
            .fee_token(self.pathusd)
            .send()
            .await
            .wrap_err("failed to configure DLUSD redeem route")?
            .get_receipt()
            .await
            .wrap_err("failed waiting to configure DLUSD redeem route")?;
        check(&receipt, "configure DLUSD redeem route")?;

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

        let dex = StablecoinDEX::new(
            crate::zone_utils::STABLECOIN_DEX_ADDRESS,
            &deployer_provider,
        );
        let receipt = dex
            .createPair(self.dlusd)
            .fee_token(self.pathusd)
            .send()
            .await
            .wrap_err("failed creating DLUSD/PathUSD StablecoinDEX pair")?
            .get_receipt()
            .await
            .wrap_err("failed waiting for DLUSD/PathUSD StablecoinDEX pair")?;
        check(&receipt, "create DLUSD/PathUSD StablecoinDEX pair")?;
        for (token, is_bid, label) in [
            (self.pathusd, true, "seed PathUSD bid liquidity"),
            (self.dlusd, false, "seed DLUSD ask liquidity"),
        ] {
            let receipt = ITIP20::new(token, &deployer_provider)
                .approve(crate::zone_utils::STABLECOIN_DEX_ADDRESS, U256::MAX)
                .fee_token(self.pathusd)
                .send()
                .await
                .wrap_err_with(|| format!("failed approving StablecoinDEX to {label}"))?
                .get_receipt()
                .await
                .wrap_err_with(|| {
                    format!("failed waiting for StablecoinDEX approval to {label}")
                })?;
            check(&receipt, label)?;
            let receipt = dex
                .place(self.dlusd, self.liquidity, is_bid, 0)
                .fee_token(self.pathusd)
                .send()
                .await
                .wrap_err_with(|| format!("failed to {label}"))?
                .get_receipt()
                .await
                .wrap_err_with(|| format!("failed waiting to {label}"))?;
            check(&receipt, label)?;
        }

        let metadata = FixtureMetadata {
            portal: self.portal.to_string(),
            messenger: messenger.to_string(),
            zone_id,
            dlusd: self.dlusd.to_string(),
            pathusd: self.pathusd.to_string(),
            earn_token: self.earn_token.to_string(),
            direct_swap: swapper.to_string(),
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
                // Fixture constructors can make contract calls that Tempo's generic
                // estimator underestimates. Keep this in sync with the #750 E2E
                // deployer and the benchmark L1 block limit.
                .with_gas_limit(30_000_000)
                .input(Bytes::from(bytecode).into())
                .into(),
        )
        .await
        .wrap_err_with(|| format!("failed deploying {label}"))?
        .get_receipt()
        .await
        .wrap_err_with(|| format!("failed waiting for {label} deployment receipt"))?;
    if !receipt.status() {
        return Err(eyre!(
            "{label} reverted after using {} gas (transaction {})",
            receipt.gas_used(),
            receipt.transaction_hash(),
        ));
    }
    receipt
        .contract_address
        .ok_or_else(|| eyre!("{label} deployment receipt did not contain a contract address"))
}

fn with_constructor(mut bytecode: Vec<u8>, constructor: Vec<u8>) -> Vec<u8> {
    bytecode.extend_from_slice(&constructor);
    bytecode
}

fn zero_fee_init() -> FixtureVaultAdapter::FeeInit {
    let zero_fee_recipient = FixtureVaultAdapter::FixedFeeRecipient {
        account: Address::ZERO,
        rate: Uint::<96, 2>::ZERO,
    };
    FixtureVaultAdapter::FeeInit {
        administrator: Address::ZERO,
        guardian: Address::ZERO,
        fixedFeeCap: Uint::<96, 2>::ZERO,
        excessFeeCap: Uint::<96, 2>::ZERO,
        initialConfig: FixtureVaultAdapter::FeeConfig {
            fixedFeeCount: 0,
            fixedFees: std::array::from_fn(|_| zero_fee_recipient.clone()),
            excess: FixtureVaultAdapter::ExcessReturnFee {
                enabled: false,
                account: Address::ZERO,
                annualTargetRate: Uint::<96, 2>::ZERO,
                excessFeeRate: Uint::<96, 2>::ZERO,
            },
        },
    }
}

fn load_bytecode(artifacts: &std::path::Path, contract: &str) -> eyre::Result<Vec<u8>> {
    let artifact_path = artifact_path(artifacts, contract);
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

fn artifact_path(artifacts: &std::path::Path, contract: &str) -> PathBuf {
    artifacts.join(format!("{contract}.json"))
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
    fn fixture_deployment_uses_native_tip20_addresses_and_earn_token() {
        let command = DeployNeobankFixtures::try_parse_from(required_args()).unwrap();
        assert_eq!(command.liquidity, 10_000_000_000);
        assert_eq!(
            command.dlusd.to_string(),
            "0x20C0000000000000000000000000000000000001"
        );
        assert_eq!(
            command.earn_token.to_string(),
            "0x20C0000000000000000000000000000000000002"
        );
    }

    #[test]
    fn fixture_deployment_uses_checked_in_earn_fixture_artifacts() {
        let out = PathBuf::from("specs/ref-impls/out");
        assert_eq!(
            artifact_path(&out, "ClosedLoopZoneGateway.sol/ClosedLoopZoneGateway"),
            PathBuf::from(
                "specs/ref-impls/out/ClosedLoopZoneGateway.sol/ClosedLoopZoneGateway.json"
            )
        );
        assert_eq!(
            artifact_path(&out, "VaultAdapter.sol/VaultAdapter"),
            PathBuf::from("specs/ref-impls/out/VaultAdapter.sol/VaultAdapter.json")
        );
    }
}
