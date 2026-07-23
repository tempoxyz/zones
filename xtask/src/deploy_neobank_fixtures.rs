//! Deploy and configure the non-secret L1 fixtures used by the private-Zone benchmark.

use alloy::{
    network::{EthereumWallet, TransactionBuilder, primitives::ReceiptResponse},
    primitives::{Address, Bytes, Uint, keccak256},
    providers::{Provider, ProviderBuilder},
    rpc::types::TransactionRequest,
    signers::local::PrivateKeySigner,
    sol_types::{SolCall, SolConstructor},
};
use eyre::{Context as _, ensure, eyre};
use serde::Serialize;
use std::{fs, path::PathBuf};
use tempo_alloy::{TempoNetwork, rpc::TempoCallBuilderExt as _};
use tempo_contracts::precompiles::{IRolesAuth, ITIP20, ITIP20Factory};
use tempo_precompiles::TIP20_FACTORY_ADDRESS;
use tempo_zone_contracts::ZonePortal;

use crate::zone_utils::check;

alloy::sol! {
    #[sol(rpc)]
    interface ZonePortalMessengerView {
        function messenger() external view returns (address);
    }

    #[sol(rpc)]
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

    #[sol(rpc)]
    interface BridgeTIP20Controller {
        function initialize(address admin) external;
        function UNWRAPPER_ROLE() external view returns (bytes32);
        function BRIDGE_ECOSYSTEM_CONTRACT_ROLE() external view returns (bytes32);
        function mintBridgeEcosystem(address stablecoinContract, address to, uint256 amount) external;
        function getReserveStore(address stablecoinContract) external view returns (address);
    }

    #[sol(rpc)]
    interface BridgeDirectSwapHandler {
        function setDirectSwapContract(address directSwap) external;
    }

    contract FixtureSimple4626Vault {
        constructor(address asset_, string name_, string symbol_, uint8 decimals_);
    }

    contract FixtureERC4626Engine {
        constructor(address vault_, address owner_, string nameOverride_, string symbolOverride_);
    }

    contract FixtureTestERC1967Proxy {
        constructor(address implementation, bytes initialization);
    }

    contract FixtureTIP20Controller {
        constructor(address reserveLedgerToken_, bool disableInitializer_);
    }

    contract FixtureTIP20DirectSwapHandler {
        constructor(address admin_, address controller_, address reserveLedgerToken_);
    }

    contract FixtureDirectSwapV2 {
        constructor(
            address reserveLedgerToken_,
            address stablecoinHandler_,
            uint96 transactionLimit_,
            address feeRecipient_,
            uint256 feeBps_,
            address authRegistry_,
            uint64 allowedCallerPolicyId_
        );
    }

    contract FixtureBridgeStableSwapAdapter {
        constructor(address directSwap_, address tokenA_, address tokenB_);
    }

    contract FixtureClosedLoopZoneGateway {
        constructor(
            address vaultAdapter_,
            address defaultSwapper_,
            address zonePortal_,
            address zoneMessenger_,
            address owner_
        );
    }

    contract FixtureVaultRewards {
        constructor(address adapter_, address owner_);
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

    /// Directory containing Foundry artifacts for the copied Earn fixtures and bridge wallet.
    #[arg(long, default_value = "specs/ref-impls/out")]
    specs_out: PathBuf,

    /// Non-secret fixture metadata written for rendering the runtime scenario.
    #[arg(long)]
    output: PathBuf,

    /// DLUSD reserve capacity seeded into the Bridge controller outside measurement.
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
    swap_adapter: String,
    tip20_controller: String,
    tip20_handler: String,
    auth_registry: String,
    reserve_ledger: String,
    vault: String,
    engine: String,
    vault_adapter: String,
    rewards: String,
    gateway: String,
    bridge_wallet: String,
}

impl DeployNeobankFixtures {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        ensure!(self.liquidity > 0, "--liquidity must be greater than zero");
        ensure!(
            self.liquidity <= (1_u128 << 96) - 1,
            "--liquidity exceeds the Bridge DirectSwap uint96 transaction limit"
        );
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
        let earn_token =
            create_earn_token(&deployer_provider, deployer_address, self.pathusd).await?;
        let reserve_ledger =
            create_reserve_ledger(&deployer_provider, deployer_address, self.pathusd).await?;

        let auth_registry = deploy(
            &deployer_provider,
            load_bytecode(&self.specs_out, "AuthRegistry.sol/AuthRegistry")?,
            "Bridge AuthRegistry",
        )
        .await?;
        let controller_implementation = deploy(
            &deployer_provider,
            with_constructor(
                load_bytecode(&self.specs_out, "TIP20Controller.sol/TIP20Controller")?,
                FixtureTIP20Controller::constructorCall {
                    reserveLedgerToken_: reserve_ledger,
                    disableInitializer_: true,
                }
                .abi_encode(),
            ),
            "Bridge TIP20Controller implementation",
        )
        .await?;
        let controller = deploy(
            &deployer_provider,
            with_constructor(
                load_bytecode(&self.specs_out, "TestERC1967Proxy.sol/TestERC1967Proxy")?,
                FixtureTestERC1967Proxy::constructorCall {
                    implementation: controller_implementation,
                    initialization: Bytes::from(
                        BridgeTIP20Controller::initializeCall {
                            admin: deployer_address,
                        }
                        .abi_encode(),
                    ),
                }
                .abi_encode(),
            ),
            "Bridge TIP20Controller proxy",
        )
        .await?;
        for (token, label) in [
            (self.dlusd, "DLUSD"),
            (self.pathusd, "pathUSD"),
            (reserve_ledger, "reserve ledger"),
        ] {
            let issuer_role = ITIP20::new(token, &deployer_provider)
                .ISSUER_ROLE()
                .call()
                .await
                .wrap_err_with(|| format!("failed querying {label} issuer role"))?;
            let receipt = IRolesAuth::new(token, &deployer_provider)
                .grantRole(issuer_role, controller)
                .fee_token(self.pathusd)
                .send()
                .await
                .wrap_err_with(|| format!("failed granting Bridge controller {label} issuer role"))?
                .get_receipt()
                .await
                .wrap_err_with(|| {
                    format!("failed waiting for Bridge controller {label} issuer role")
                })?;
            check(
                &receipt,
                &format!("grant Bridge controller {label} issuer role"),
            )?;
        }
        let handler = deploy(
            &deployer_provider,
            with_constructor(
                load_bytecode(
                    &self.specs_out,
                    "TIP20DirectSwapHandler.sol/TIP20DirectSwapHandler",
                )?,
                FixtureTIP20DirectSwapHandler::constructorCall {
                    admin_: deployer_address,
                    controller_: controller,
                    reserveLedgerToken_: reserve_ledger,
                }
                .abi_encode(),
            ),
            "Bridge TIP20DirectSwapHandler",
        )
        .await?;
        let controller_contract = BridgeTIP20Controller::new(controller, &deployer_provider);
        for (role, account, label) in [
            (
                controller_contract
                    .UNWRAPPER_ROLE()
                    .call()
                    .await
                    .wrap_err("failed querying Bridge controller unwrapper role")?,
                handler,
                "grant Bridge handler unwrapper role",
            ),
            (
                controller_contract
                    .BRIDGE_ECOSYSTEM_CONTRACT_ROLE()
                    .call()
                    .await
                    .wrap_err("failed querying Bridge controller ecosystem role")?,
                deployer_address,
                "grant Bridge deployer ecosystem role",
            ),
        ] {
            let receipt = IRolesAuth::new(controller, &deployer_provider)
                .grantRole(role, account)
                .fee_token(self.pathusd)
                .send()
                .await
                .wrap_err(label)?
                .get_receipt()
                .await
                .wrap_err_with(|| format!("failed waiting to {label}"))?;
            check(&receipt, label)?;
        }
        let direct_swap = deploy(
            &deployer_provider,
            with_constructor(
                load_bytecode(&self.specs_out, "DirectSwapV2.sol/DirectSwapV2")?,
                FixtureDirectSwapV2::constructorCall {
                    reserveLedgerToken_: reserve_ledger,
                    stablecoinHandler_: handler,
                    transactionLimit_: Uint::<96, 2>::from_limbs([
                        self.liquidity as u64,
                        (self.liquidity >> 64) as u64,
                    ]),
                    feeRecipient_: deployer_address,
                    feeBps_: Uint::<256, 4>::ZERO,
                    authRegistry_: auth_registry,
                    allowedCallerPolicyId_: 1,
                }
                .abi_encode(),
            ),
            "Bridge DirectSwapV2",
        )
        .await?;
        let receipt = BridgeDirectSwapHandler::new(handler, &deployer_provider)
            .setDirectSwapContract(direct_swap)
            .fee_token(self.pathusd)
            .send()
            .await
            .wrap_err("failed configuring Bridge DirectSwap handler")?
            .get_receipt()
            .await
            .wrap_err("failed waiting to configure Bridge DirectSwap handler")?;
        check(&receipt, "configure Bridge DirectSwap handler")?;
        let swap_adapter = deploy(
            &deployer_provider,
            with_constructor(
                load_bytecode(
                    &self.specs_out,
                    "BridgeStableSwapAdapter.sol/BridgeStableSwapAdapter",
                )?,
                FixtureBridgeStableSwapAdapter::constructorCall {
                    directSwap_: direct_swap,
                    tokenA_: self.dlusd,
                    tokenB_: self.pathusd,
                }
                .abi_encode(),
            ),
            "BridgeStableSwapAdapter",
        )
        .await?;
        let receipt = controller_contract
            .mintBridgeEcosystem(self.dlusd, deployer_address, self.liquidity.into())
            .fee_token(self.pathusd)
            .send()
            .await
            .wrap_err("failed seeding the Bridge DLUSD reserve")?
            .get_receipt()
            .await
            .wrap_err("failed waiting to seed the Bridge DLUSD reserve")?;
        check(&receipt, "seed Bridge DLUSD reserve")?;
        ensure!(
            controller_contract
                .getReserveStore(self.dlusd)
                .call()
                .await
                .wrap_err("failed querying the Bridge DLUSD reserve store")?
                != Address::ZERO,
            "Bridge DLUSD reserve store was not created"
        );
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
                FixtureERC4626Engine::constructorCall {
                    vault_: vault,
                    owner_: deployer_address,
                    nameOverride_: String::new(),
                    symbolOverride_: String::new(),
                }
                .abi_encode(),
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
            shareToken_: earn_token,
            operator_: deployer_address,
            feeInit_: zero_fee_init(),
        }
        .abi_encode();
        let vault_adapter = deploy(
            &deployer_provider,
            with_constructor(
                load_bytecode(&self.specs_out, "TestERC1967Proxy.sol/TestERC1967Proxy")?,
                FixtureTestERC1967Proxy::constructorCall {
                    implementation: adapter_implementation,
                    initialization: Bytes::from(initialization),
                }
                .abi_encode(),
            ),
            "TestERC1967Proxy",
        )
        .await?;
        let issuer_role = ITIP20::new(earn_token, &deployer_provider)
            .ISSUER_ROLE()
            .call()
            .await
            .wrap_err("failed querying EarnToken issuer role")?;
        let receipt = IRolesAuth::new(earn_token, &deployer_provider)
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
        let rewards = deploy(
            &deployer_provider,
            with_constructor(
                load_bytecode(&self.specs_out, "VaultRewards.sol/VaultRewards")?,
                FixtureVaultRewards::constructorCall {
                    adapter_: vault_adapter,
                    owner_: deployer_address,
                }
                .abi_encode(),
            ),
            "VaultRewards",
        )
        .await?;
        let gateway = deploy(
            &deployer_provider,
            with_constructor(
                load_bytecode(
                    &self.specs_out,
                    "ClosedLoopZoneGateway.sol/ClosedLoopZoneGateway",
                )?,
                FixtureClosedLoopZoneGateway::constructorCall {
                    vaultAdapter_: vault_adapter,
                    defaultSwapper_: swap_adapter,
                    zonePortal_: self.portal,
                    zoneMessenger_: messenger,
                    owner_: deployer_address,
                }
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
            .setDepositRoute(self.dlusd, swap_adapter)
            .fee_token(self.pathusd)
            .send()
            .await
            .wrap_err("failed to configure DLUSD deposit route")?
            .get_receipt()
            .await
            .wrap_err("failed waiting to configure DLUSD deposit route")?;
        check(&receipt, "configure DLUSD deposit route")?;
        let receipt = gateway_routes
            .setRedeemRoute(self.dlusd, swap_adapter)
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
            .isTokenEnabled(earn_token)
            .call()
            .await
            .wrap_err("failed querying EarnToken ZonePortal enablement")?
        {
            let receipt = admin_portal
                .enableToken(earn_token)
                .fee_token(self.pathusd)
                .send()
                .await
                .wrap_err("failed enabling EarnToken on ZonePortal")?
                .get_receipt()
                .await
                .wrap_err("failed waiting for EarnToken enablement receipt")?;
            check(&receipt, "enable EarnToken on ZonePortal")?;
        }

        let metadata = FixtureMetadata {
            portal: self.portal.to_string(),
            messenger: messenger.to_string(),
            zone_id,
            dlusd: self.dlusd.to_string(),
            pathusd: self.pathusd.to_string(),
            earn_token: earn_token.to_string(),
            direct_swap: direct_swap.to_string(),
            swap_adapter: swap_adapter.to_string(),
            tip20_controller: controller.to_string(),
            tip20_handler: handler.to_string(),
            auth_registry: auth_registry.to_string(),
            reserve_ledger: reserve_ledger.to_string(),
            vault: vault.to_string(),
            engine: engine.to_string(),
            vault_adapter: vault_adapter.to_string(),
            rewards: rewards.to_string(),
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
        println!("  Rewards:       {rewards}");
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

async fn create_earn_token<P: Provider<TempoNetwork>>(
    provider: &P,
    owner: Address,
    fee_token: Address,
) -> eyre::Result<Address> {
    let factory = ITIP20Factory::new(TIP20_FACTORY_ADDRESS, provider);
    let salt = keccak256("zones-neobank-benchmark-earn-token");
    let token = factory
        .getTokenAddress(owner, salt)
        .call()
        .await
        .wrap_err("failed computing benchmark EarnToken address")?;
    let receipt = factory
        .createToken_0(
            "Neobank benchmark EarnToken".to_owned(),
            "nbEARN".to_owned(),
            "USD".to_owned(),
            fee_token,
            owner,
            salt,
        )
        .fee_token(fee_token)
        .send()
        .await
        .wrap_err("failed creating benchmark EarnToken")?
        .get_receipt()
        .await
        .wrap_err("failed waiting for benchmark EarnToken creation")?;
    check(&receipt, "create benchmark EarnToken")?;
    ensure!(
        token != Address::ZERO,
        "benchmark EarnToken address is zero"
    );
    println!("Created benchmark EarnToken: {token}");
    Ok(token)
}

async fn create_reserve_ledger<P: Provider<TempoNetwork>>(
    provider: &P,
    owner: Address,
    fee_token: Address,
) -> eyre::Result<Address> {
    let factory = ITIP20Factory::new(TIP20_FACTORY_ADDRESS, provider);
    let salt = keccak256("zones-neobank-benchmark-bridge-reserve");
    let token = factory
        .getTokenAddress(owner, salt)
        .call()
        .await
        .wrap_err("failed computing Bridge reserve ledger address")?;
    let receipt = factory
        .createToken_0(
            "Neobank benchmark Bridge reserve".to_owned(),
            "nbBRL".to_owned(),
            "USD".to_owned(),
            fee_token,
            owner,
            salt,
        )
        .fee_token(fee_token)
        .send()
        .await
        .wrap_err("failed creating Bridge reserve ledger")?
        .get_receipt()
        .await
        .wrap_err("failed waiting for Bridge reserve ledger creation")?;
    check(&receipt, "create Bridge reserve ledger")?;
    ensure!(
        token != Address::ZERO,
        "Bridge reserve ledger address is zero"
    );
    println!("Created Bridge reserve ledger: {token}");
    Ok(token)
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
    let contract = receipt
        .contract_address
        .ok_or_else(|| eyre!("{label} deployment receipt did not contain a contract address"))?;
    println!(
        "Deployed {label}: {contract} (gas used: {})",
        receipt.gas_used()
    );
    Ok(contract)
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

    fn function_shapes(abi: &serde_json::Value, name: &str) -> Vec<serde_json::Value> {
        abi.as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["type"] == "function" && entry["name"] == name)
            .map(|entry| {
                serde_json::json!({
                    "inputs": entry["inputs"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|input| input["type"].clone())
                        .collect::<Vec<_>>(),
                    "outputs": entry["outputs"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|output| output["type"].clone())
                        .collect::<Vec<_>>(),
                    "stateMutability": entry["stateMutability"],
                })
            })
            .collect()
    }

    fn event_shapes(abi: &serde_json::Value, name: &str) -> Vec<serde_json::Value> {
        abi.as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["type"] == "event" && entry["name"] == name)
            .map(|entry| {
                serde_json::json!({
                    "anonymous": entry["anonymous"],
                    "inputs": entry["inputs"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|input| serde_json::json!({
                            "type": input["type"],
                            "indexed": input["indexed"],
                        }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect()
    }

    fn required_args() -> [&'static str; 11] {
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
    fn fixture_deployment_uses_native_tip20_addresses() {
        let command = DeployNeobankFixtures::try_parse_from(required_args()).unwrap();
        assert_eq!(command.liquidity, 10_000_000_000);
        assert_eq!(
            command.dlusd.to_string(),
            "0x20C0000000000000000000000000000000000001"
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
        assert_eq!(
            artifact_path(&out, "VaultRewards.sol/VaultRewards"),
            PathBuf::from("specs/ref-impls/out/VaultRewards.sol/VaultRewards.json")
        );
        assert_eq!(
            artifact_path(&out, "DirectSwapV2.sol/DirectSwapV2"),
            PathBuf::from("specs/ref-impls/out/DirectSwapV2.sol/DirectSwapV2.json")
        );
        assert_eq!(
            artifact_path(&out, "BridgeStableSwapAdapter.sol/BridgeStableSwapAdapter"),
            PathBuf::from(
                "specs/ref-impls/out/BridgeStableSwapAdapter.sol/BridgeStableSwapAdapter.json"
            )
        );
    }

    #[test]
    fn benchmark_abis_match_pinned_reward_interfaces() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        for (minimal, artifact, functions) in [
            (
                "contrib/bench/neobank/abis/vault-rewards.json",
                "specs/ref-impls/out/VaultRewards.sol/VaultRewards.json",
                &["fund"][..],
            ),
            (
                "contrib/bench/neobank/abis/vault-adapter.json",
                "specs/ref-impls/out/VaultAdapter.sol/VaultAdapter.json",
                &["redeem", "shareSupply", "previewRedeem"][..],
            ),
        ] {
            let minimal: serde_json::Value =
                serde_json::from_slice(&fs::read(root.join(minimal)).unwrap()).unwrap();
            let artifact: serde_json::Value =
                serde_json::from_slice(&fs::read(root.join(artifact)).unwrap()).unwrap();
            for function in functions {
                assert_eq!(
                    function_shapes(&minimal, function),
                    function_shapes(&artifact["abi"], function),
                    "minimal {function} ABI diverges from its pinned artifact"
                );
            }
            if minimal
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["name"] == "Funded")
            {
                assert_eq!(
                    event_shapes(&minimal, "Funded"),
                    event_shapes(&artifact["abi"], "Funded"),
                    "minimal Funded ABI diverges from its pinned artifact"
                );
            }
        }
    }
}
