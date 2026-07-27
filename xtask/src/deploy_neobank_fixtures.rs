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
use tempo_contracts::precompiles::{IRolesAuth, IStablecoinDEX, ITIP20, ITIP20Factory};
use tempo_precompiles::{STABLECOIN_DEX_ADDRESS, TIP20_FACTORY_ADDRESS};
use tempo_zone_contracts::{ZonePortal, ZonePortal::Role as PortalRole};

use crate::{create_zone::read_private_address_file, zone_utils::check};

alloy::sol! {
    #[sol(rpc)]
    interface ZonePortalMessengerView {
        function messenger() external view returns (address);
    }

    #[sol(rpc)]
    contract FixtureEarnFactory {
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

        struct EarnFeesInit {
            address administrator;
            address guardian;
            uint96 fixedFeeCap;
            uint96 excessFeeCap;
            FeeConfig initialConfig;
        }

        struct EarnVaultControls {
            address emergencyGuardian;
            address asyncJanitor;
            uint8 migrationMode;
        }

        struct DeployParams {
            bytes32 deploymentId;
            address engine;
            address owner;
            EarnVaultControls controls;
            EarnFeesInit fees;
        }

        function deploy(DeployParams calldata params)
            external
            returns (address earnShare, address earnVault, address earnFees);
    }

    #[sol(rpc)]
    interface ERC4626EngineInitializer {
        function initializeEarnVault(address earnVault) external;
    }

    #[sol(rpc)]
    interface BridgeTIP20Controller {
        function initialize(address admin) external;
        function MINT_RATE_LIMIT_SETTER_ROLE() external view returns (bytes32);
        function UNWRAPPER_ROLE() external view returns (bytes32);
        function BRIDGE_ECOSYSTEM_CONTRACT_ROLE() external view returns (bytes32);
        function setTxnMintLimit(address stablecoinContract, uint256 mintTxnLimit) external;
        function getStablecoinTxnMintLimit(address stablecoinContract) external view returns (uint256);
        function mintBridgeEcosystem(address stablecoinContract, address to, uint256 amount) external;
        function getReserveStore(address stablecoinContract) external view returns (address);
    }

    #[sol(rpc)]
    interface BridgeDirectSwapHandler {
        function DIRECT_SWAP_SETTER_ROLE() external view returns (bytes32);
        function STABLECOIN_CONFIG_ROLE() external view returns (bytes32);
        function PAUSER_ROLE() external view returns (bytes32);
        function TOKEN_AUTHORITY() external view returns (address);
        function registerStablecoin(address stablecoin, bool registered) external;
        function setDirectSwapContract(address directSwap) external;
    }

    contract FixtureSimple4626Vault {
        constructor(address asset_, string name_, string symbol_, uint8 decimals_);
    }

    contract FixtureERC4626Engine {
        constructor(address vault_, address owner_, string nameOverride_, string symbolOverride_);
    }

    contract FixtureTIP20Controller {
        constructor(address reserveLedgerToken_, bool disableInitializer_);
    }

    contract FixtureERC1967Proxy {
        constructor(address implementation, bytes initialization);
    }

    contract FixtureReserveLedgerWrappedHandler {
        constructor(address admin_, address tokenAuthority_, address reserveLedgerToken_);
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

        function getTransactionLimit() external view returns (uint256);
    }

    contract FixtureSingleZoneEarnRouter {
        constructor(uint32 allowedZoneId_, address earnVault_, address privateAsset_);
    }

    contract FixtureSingleZoneBridgeEarnRouter {
        constructor(
            uint32 allowedZoneId_,
            address earnVault_,
            address privateAsset_,
            address directSwap_
        );
    }

    contract FixtureSingleZoneMinimalEarnRouter {
        constructor(uint32 allowedZoneId_, address earnVault_, address privateAsset_, address tokenAuthority_);
    }

    contract FixtureEarnFactoryConstructor {
        constructor(
            address tip20Factory_,
            address earnVaultImplementation_,
            address earnFeesImplementation_
        );
    }

    contract FixtureEarnContributionController {
        constructor(address earnVault_, address owner_);
    }
}

const DEFAULT_DEPLOYMENT_GAS_LIMIT: u64 = 30_000_000;
const DIRECT_SWAP_ABSOLUTE_MAX: u128 = 1_000_000_000_000_000;
const STABLECOIN_DEX_LIQUIDITY_TICK: i16 = 0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SwapMechanism {
    #[default]
    DirectSwap,
    Simple,
    StablecoinDex,
}

impl SwapMechanism {
    fn as_str(self) -> &'static str {
        match self {
            Self::DirectSwap => "direct-swap",
            Self::Simple => "simple",
            Self::StablecoinDex => "stablecoin-dex",
        }
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

    /// Native TIP-20 used as the private asset for this benchmark preset.
    #[arg(long, env = "ZONES_BENCH_PRIVATE_ASSET")]
    private_asset: Address,

    /// Directory containing Foundry artifacts from the external Earn checkout and local fixtures.
    #[arg(long, default_value = "specs/ref-impls/out")]
    specs_out: PathBuf,

    /// Exact revision of the external Earn checkout used to build the Foundry artifacts.
    #[arg(long, env = "ZONES_BENCH_EARN_REVISION")]
    earn_revision: String,

    /// Non-secret fixture metadata written for rendering the runtime scenario.
    #[arg(long)]
    output: PathBuf,

    /// Mode-0600 newline-delimited benchmark account allowlist.
    #[arg(long)]
    allowed_accounts_file: PathBuf,

    /// Untimed per-side liquidity seeded into the selected swap mechanism.
    #[arg(long, default_value_t = 10_000_000_000_u128)]
    liquidity: u128,

    /// L1 swap implementation exercised by the EarnRouter.
    #[arg(
        long,
        env = "ZONES_BENCH_SWAP_MECHANISM",
        value_enum,
        default_value_t = SwapMechanism::DirectSwap
    )]
    swap_mechanism: SwapMechanism,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FixtureMetadata {
    portal: String,
    messenger: String,
    zone_id: u32,
    dlusd: String,
    pathusd: String,
    private_asset: String,
    earn_token: String,
    earn_share: String,
    swap_mechanism: SwapMechanism,
    default_swapper: String,
    route_swapper: Option<String>,
    route_override: bool,
    stablecoin_dex: String,
    direct_swap: Option<String>,
    simple_swap: Option<String>,
    swap_adapter: Option<String>,
    tip20_controller: Option<String>,
    tip20_handler: Option<String>,
    auth_registry: Option<String>,
    reserve_ledger: Option<String>,
    liquidity: u128,
    liquidity_tick: i16,
    earn_fixture_revision: String,
    vault: String,
    engine: String,
    earn_factory: String,
    earn_vault: String,
    earn_fees: String,
    earn_router: String,
    contribution_controller: String,
    // Compatibility aliases consumed by the benchmark runtime while its environment variable
    // names remain stable. Each points at the corresponding canonical v1 contract above.
    vault_adapter: String,
    rewards: String,
    gateway: String,
    bridge_wallet: String,
}

#[derive(Default)]
struct SwapSetup {
    route_swapper: Option<Address>,
    direct_swap: Option<Address>,
    simple_swap: Option<Address>,
    tip20_controller: Option<Address>,
    tip20_handler: Option<Address>,
    auth_registry: Option<Address>,
    reserve_ledger: Option<Address>,
}

impl DeployNeobankFixtures {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        validate_earn_revision(&self.earn_revision)?;
        validate_liquidity(self.swap_mechanism, self.liquidity)?;
        ensure!(
            self.private_asset == self.dlusd || self.private_asset == self.pathusd,
            "--private-asset must match either --dlusd or --pathusd"
        );
        let deployer = signer_from_env("FIXTURE_DEPLOYER_KEY")?;
        let deployer_address = deployer.address();
        let portal_admin = signer_from_env("PORTAL_ADMIN_KEY")?;
        let portal_admin_address = portal_admin.address();
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
        let mut allowed_accounts = read_private_address_file(&self.allowed_accounts_file)?;
        allowed_accounts.sort_unstable();
        allowed_accounts.dedup();
        ensure!(
            !allowed_accounts.is_empty(),
            "benchmark account allowlist must not be empty"
        );
        let admin_portal = ZonePortal::new(self.portal, &admin_provider);
        ensure!(
            admin_portal
                .admin()
                .call()
                .await
                .wrap_err("failed querying ZonePortal admin")?
                == portal_admin_address,
            "PORTAL_ADMIN_KEY does not control the configured ZonePortal"
        );
        ensure!(
            admin_portal
                .isAccessEnforced()
                .call()
                .await
                .wrap_err("failed querying ZonePortal access mode")?,
            "ZonePortal access enforcement must be enabled before deploying fixtures"
        );
        ensure!(
            admin_portal
                .isGatewayOpen()
                .call()
                .await
                .wrap_err("failed querying ZonePortal gateway mode")?,
            "ZonePortal gateway enforcement must remain disabled until fixture roles are configured"
        );

        let mut swap_setup = match self.swap_mechanism {
            SwapMechanism::DirectSwap => {
                configure_direct_swap(
                    &deployer_provider,
                    &self.specs_out,
                    deployer_address,
                    self.dlusd,
                    self.pathusd,
                    self.liquidity,
                )
                .await?
            }
            SwapMechanism::Simple => {
                configure_simple_swap(
                    &deployer_provider,
                    &self.specs_out,
                    deployer_address,
                    self.dlusd,
                    self.pathusd,
                    self.liquidity,
                )
                .await?
            }
            SwapMechanism::StablecoinDex => {
                configure_stablecoin_dex(
                    &deployer_provider,
                    self.dlusd,
                    self.pathusd,
                    self.liquidity,
                )
                .await?;
                SwapSetup::default()
            }
        };
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
        let earn_vault_implementation = deploy(
            &deployer_provider,
            load_bytecode(&self.specs_out, "EarnVault.sol/EarnVault")?,
            "EarnVault implementation",
        )
        .await?;
        let earn_fees_implementation = deploy(
            &deployer_provider,
            load_bytecode(&self.specs_out, "EarnFees.sol/EarnFees")?,
            "EarnFees implementation",
        )
        .await?;
        let earn_factory = deploy(
            &deployer_provider,
            with_constructor(
                load_bytecode(&self.specs_out, "EarnFactory.sol/EarnFactory")?,
                FixtureEarnFactoryConstructor::constructorCall {
                    tip20Factory_: TIP20_FACTORY_ADDRESS,
                    earnVaultImplementation_: earn_vault_implementation,
                    earnFeesImplementation_: earn_fees_implementation,
                }
                .abi_encode(),
            ),
            "EarnFactory",
        )
        .await?;
        let factory = FixtureEarnFactory::new(earn_factory, &deployer_provider);
        let deploy_params = FixtureEarnFactory::DeployParams {
            deploymentId: keccak256("zones-neobank-benchmark-earn-v1"),
            engine,
            owner: deployer_address,
            controls: FixtureEarnFactory::EarnVaultControls {
                emergencyGuardian: deployer_address,
                asyncJanitor: deployer_address,
                migrationMode: 1,
            },
            fees: zero_fee_init(),
        };
        let predicted = factory
            .deploy_call(deploy_params.clone())
            .call()
            .await
            .wrap_err("failed simulating canonical Earn stack deployment")?;
        let receipt = factory
            .deploy_call(deploy_params)
            .gas(DEFAULT_DEPLOYMENT_GAS_LIMIT)
            .fee_token(self.pathusd)
            .send()
            .await
            .wrap_err("failed deploying canonical Earn stack")?
            .get_receipt()
            .await
            .wrap_err("failed waiting for canonical Earn stack deployment")?;
        check(&receipt, "deploy canonical Earn stack")?;
        let earn_share = predicted.earnShare;
        let earn_vault = predicted.earnVault;
        let earn_fees = predicted.earnFees;
        for (label, address) in [
            ("EarnShare", earn_share),
            ("EarnVault", earn_vault),
            ("EarnFees", earn_fees),
        ] {
            ensure!(address != Address::ZERO, "{label} address is zero");
        }

        let receipt = ERC4626EngineInitializer::new(engine, &deployer_provider)
            .initializeEarnVault(earn_vault)
            .fee_token(self.pathusd)
            .send()
            .await
            .wrap_err("failed initializing the canonical ERC4626 engine")?
            .get_receipt()
            .await
            .wrap_err("failed waiting for canonical ERC4626 engine initialization")?;
        check(&receipt, "initialize canonical ERC4626 engine")?;

        let earn_router = if self.private_asset == self.pathusd {
            deploy(
                &deployer_provider,
                with_constructor(
                    load_bytecode(
                        &self.specs_out,
                        "SingleZoneEarnRouter.sol/SingleZoneEarnRouter",
                    )?,
                    FixtureSingleZoneEarnRouter::constructorCall {
                        allowedZoneId_: zone_id,
                        earnVault_: earn_vault,
                        privateAsset_: self.private_asset,
                    }
                    .abi_encode(),
                ),
                "SingleZoneEarnRouter",
            )
            .await?
        } else {
            match self.swap_mechanism {
                SwapMechanism::DirectSwap => {
                    let direct_swap = swap_setup.direct_swap.ok_or_else(|| {
                        eyre!("DirectSwap setup did not return a DirectSwap address")
                    })?;
                    deploy(
                        &deployer_provider,
                        with_constructor(
                            load_bytecode(
                                &self.specs_out,
                                "SingleZoneBridgeEarnRouter.sol/SingleZoneBridgeEarnRouter",
                            )?,
                            FixtureSingleZoneBridgeEarnRouter::constructorCall {
                                allowedZoneId_: zone_id,
                                earnVault_: earn_vault,
                                privateAsset_: self.dlusd,
                                directSwap_: direct_swap,
                            }
                            .abi_encode(),
                        ),
                        "SingleZoneBridgeEarnRouter",
                    )
                    .await?
                }
                SwapMechanism::Simple => {
                    let controller = swap_setup.tip20_controller.ok_or_else(|| {
                        eyre!("minimal swap setup did not return a token authority")
                    })?;
                    let router = deploy(
                        &deployer_provider,
                        with_constructor(
                            load_bytecode(
                                &self.specs_out,
                                "SingleZoneMinimalEarnRouter.sol/SingleZoneMinimalEarnRouter",
                            )?,
                            FixtureSingleZoneMinimalEarnRouter::constructorCall {
                                allowedZoneId_: zone_id,
                                earnVault_: earn_vault,
                                privateAsset_: self.dlusd,
                                tokenAuthority_: controller,
                            }
                            .abi_encode(),
                        ),
                        "SingleZoneMinimalEarnRouter",
                    )
                    .await?;
                    grant_controller_unwrapper(
                        &deployer_provider,
                        controller,
                        router,
                        self.pathusd,
                    )
                    .await?;
                    router
                }
                SwapMechanism::StablecoinDex => {
                    deploy(
                        &deployer_provider,
                        with_constructor(
                            load_bytecode(
                                &self.specs_out,
                                "SingleZoneEarnRouter.sol/SingleZoneEarnRouter",
                            )?,
                            FixtureSingleZoneEarnRouter::constructorCall {
                                allowedZoneId_: zone_id,
                                earnVault_: earn_vault,
                                privateAsset_: self.dlusd,
                            }
                            .abi_encode(),
                        ),
                        "SingleZoneEarnRouter",
                    )
                    .await?
                }
            }
        };
        if self.private_asset == self.dlusd && self.swap_mechanism != SwapMechanism::StablecoinDex {
            swap_setup.route_swapper = Some(earn_router);
            swap_setup.simple_swap =
                (self.swap_mechanism == SwapMechanism::Simple).then_some(earn_router);
        }

        let contribution_controller = deploy(
            &deployer_provider,
            with_constructor(
                load_bytecode(
                    &self.specs_out,
                    "EarnContributionController.sol/EarnContributionController",
                )?,
                FixtureEarnContributionController::constructorCall {
                    earnVault_: earn_vault,
                    owner_: deployer_address,
                }
                .abi_encode(),
            ),
            "EarnContributionController",
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
        let role_assignments =
            portal_role_assignments(&allowed_accounts, bridge_wallet, earn_router, messenger)?;

        if !admin_portal
            .isTokenEnabled(earn_share)
            .call()
            .await
            .wrap_err("failed querying EarnShare ZonePortal enablement")?
        {
            let receipt = admin_portal
                .enableToken(earn_share)
                .fee_token(self.pathusd)
                .send()
                .await
                .wrap_err("failed enabling EarnShare on ZonePortal")?
                .get_receipt()
                .await
                .wrap_err("failed waiting for EarnShare enablement receipt")?;
            check(&receipt, "enable EarnShare on ZonePortal")?;
        }
        configure_closed_loop_portal(
            &admin_provider,
            self.portal,
            self.pathusd,
            &role_assignments,
        )
        .await?;

        let metadata = FixtureMetadata {
            portal: self.portal.to_string(),
            messenger: messenger.to_string(),
            zone_id,
            dlusd: self.dlusd.to_string(),
            pathusd: self.pathusd.to_string(),
            private_asset: self.private_asset.to_string(),
            earn_token: earn_share.to_string(),
            earn_share: earn_share.to_string(),
            swap_mechanism: self.swap_mechanism,
            default_swapper: STABLECOIN_DEX_ADDRESS.to_string(),
            route_swapper: swap_setup.route_swapper.map(|address| address.to_string()),
            route_override: swap_setup.route_swapper.is_some(),
            stablecoin_dex: STABLECOIN_DEX_ADDRESS.to_string(),
            direct_swap: swap_setup.direct_swap.map(|address| address.to_string()),
            simple_swap: swap_setup.simple_swap.map(|address| address.to_string()),
            swap_adapter: swap_setup.route_swapper.map(|address| address.to_string()),
            tip20_controller: swap_setup
                .tip20_controller
                .map(|address| address.to_string()),
            tip20_handler: swap_setup.tip20_handler.map(|address| address.to_string()),
            auth_registry: swap_setup.auth_registry.map(|address| address.to_string()),
            reserve_ledger: swap_setup.reserve_ledger.map(|address| address.to_string()),
            liquidity: self.liquidity,
            liquidity_tick: STABLECOIN_DEX_LIQUIDITY_TICK,
            earn_fixture_revision: self.earn_revision.clone(),
            vault: vault.to_string(),
            engine: engine.to_string(),
            earn_factory: earn_factory.to_string(),
            earn_vault: earn_vault.to_string(),
            earn_fees: earn_fees.to_string(),
            earn_router: earn_router.to_string(),
            contribution_controller: contribution_controller.to_string(),
            vault_adapter: earn_vault.to_string(),
            rewards: contribution_controller.to_string(),
            gateway: earn_router.to_string(),
            bridge_wallet: bridge_wallet.to_string(),
        };
        if let Some(parent) = self.output.parent() {
            fs::create_dir_all(parent)
                .wrap_err_with(|| format!("failed creating {}", parent.display()))?;
        }
        fs::write(&self.output, serde_json::to_string_pretty(&metadata)?)
            .wrap_err_with(|| format!("failed writing {}", self.output.display()))?;
        println!("Private-Zone benchmark fixtures deployed");
        println!("  Swap mechanism: {}", self.swap_mechanism.as_str());
        println!("  Default swapper: {STABLECOIN_DEX_ADDRESS}");
        if let Some(route_swapper) = swap_setup.route_swapper {
            println!("  Route swapper:   {route_swapper}");
        }
        println!("  Earn router:             {earn_router}");
        println!("  Earn vault:              {earn_vault}");
        println!("  Earn share:              {earn_share}");
        println!("  Earn fees:               {earn_fees}");
        println!("  Contribution controller: {contribution_controller}");
        println!("  Bridge wallet:           {bridge_wallet}");
        println!("  Portal policy:           closed-loop enforcement enabled");
        println!("  Metadata:                {}", self.output.display());
        Ok(())
    }
}

fn portal_role_assignments(
    allowed_accounts: &[Address],
    bridge_wallet: Address,
    earn_router: Address,
    messenger: Address,
) -> eyre::Result<Vec<(Address, PortalRole)>> {
    ensure!(
        bridge_wallet != Address::ZERO,
        "Bridge wallet address is zero"
    );
    ensure!(earn_router != Address::ZERO, "EarnRouter address is zero");
    ensure!(
        bridge_wallet != earn_router,
        "Bridge wallet and EarnRouter addresses must be distinct"
    );
    ensure!(
        bridge_wallet != messenger,
        "the ZonePortal messenger cannot be assigned the Bridge wallet account role"
    );
    ensure!(
        earn_router != messenger,
        "the ZonePortal messenger cannot be assigned the EarnRouter gateway role"
    );
    let mut assignments = Vec::with_capacity(allowed_accounts.len() + 2);
    for account in allowed_accounts {
        ensure!(
            *account != Address::ZERO,
            "the benchmark account allowlist contains the zero address"
        );
        ensure!(
            *account != messenger,
            "the ZonePortal messenger cannot be assigned the benchmark account role"
        );
        ensure!(
            *account != bridge_wallet && *account != earn_router,
            "a benchmark account conflicts with a closed-loop fixture role"
        );
        assignments.push((*account, PortalRole::Account));
    }
    assignments.push((bridge_wallet, PortalRole::Account));
    assignments.push((earn_router, PortalRole::CallbackGateway));
    Ok(assignments)
}

async fn configure_closed_loop_portal<P: Provider<TempoNetwork>>(
    provider: &P,
    portal_address: Address,
    fee_token: Address,
    assignments: &[(Address, PortalRole)],
) -> eyre::Result<()> {
    let portal = ZonePortal::new(portal_address, provider);
    println!(
        "Configuring {} closed-loop ZonePortal roles...",
        assignments.len()
    );
    for (index, (account, expected_role)) in assignments.iter().enumerate() {
        if portal
            .role(*account)
            .call()
            .await
            .wrap_err_with(|| format!("failed querying ZonePortal role at index {index}"))?
            as u8
            != *expected_role as u8
        {
            let receipt = portal
                .setRole(*account, *expected_role)
                .fee_token(fee_token)
                .send()
                .await
                .wrap_err_with(|| format!("failed assigning ZonePortal role at index {index}"))?
                .get_receipt()
                .await
                .wrap_err_with(|| {
                    format!("failed waiting for ZonePortal role receipt at index {index}")
                })?;
            check(&receipt, "assign ZonePortal benchmark role")?;
        }
        if (index + 1) % 10 == 0 || index + 1 == assignments.len() {
            println!(
                "ZonePortal role setup progress: {}/{}",
                index + 1,
                assignments.len()
            );
        }
    }
    for (index, (account, expected_role)) in assignments.iter().enumerate() {
        ensure!(
            portal.role(*account).call().await.wrap_err_with(|| {
                format!("failed verifying ZonePortal role at index {index}")
            })? as u8
                == *expected_role as u8,
            "ZonePortal role verification failed at index {index}"
        );
    }

    if portal
        .isGatewayOpen()
        .call()
        .await
        .wrap_err("failed querying ZonePortal gateway mode")?
    {
        let receipt = portal
            .setGatewayMode(true)
            .fee_token(fee_token)
            .send()
            .await
            .wrap_err("failed enabling ZonePortal gateway enforcement")?
            .get_receipt()
            .await
            .wrap_err("failed waiting for ZonePortal gateway enforcement receipt")?;
        check(&receipt, "enable ZonePortal gateway enforcement")?;
    }
    ensure!(
        !portal
            .isGatewayOpen()
            .call()
            .await
            .wrap_err("failed verifying ZonePortal gateway mode")?,
        "ZonePortal gateway enforcement did not activate"
    );

    ensure!(
        portal
            .isAccessEnforced()
            .call()
            .await
            .wrap_err("failed verifying ZonePortal access mode")?,
        "ZonePortal access enforcement was disabled while configuring fixture roles"
    );
    Ok(())
}

fn validate_liquidity(mechanism: SwapMechanism, liquidity: u128) -> eyre::Result<()> {
    ensure!(liquidity > 0, "--liquidity must be greater than zero");
    if mechanism == SwapMechanism::DirectSwap {
        ensure!(
            liquidity < (1_u128 << 96),
            "--liquidity exceeds the Bridge DirectSwap uint96 transaction limit"
        );
    }
    if matches!(mechanism, SwapMechanism::DirectSwap | SwapMechanism::Simple) {
        ensure!(
            liquidity < DIRECT_SWAP_ABSOLUTE_MAX,
            "--liquidity must be below the Bridge controller absolute mint limit of {DIRECT_SWAP_ABSOLUTE_MAX}"
        );
    }
    Ok(())
}

fn validate_earn_revision(revision: &str) -> eyre::Result<()> {
    ensure!(
        revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "ZONES_BENCH_EARN_REVISION must be the exact 40-character commit SHA of the external Earn checkout"
    );
    Ok(())
}

async fn configure_direct_swap<P: Provider<TempoNetwork>>(
    provider: &P,
    specs_out: &std::path::Path,
    deployer: Address,
    dlusd: Address,
    pathusd: Address,
    liquidity: u128,
) -> eyre::Result<SwapSetup> {
    let reserve_ledger = create_reserve_ledger(provider, deployer, pathusd).await?;
    let auth_registry = deploy(
        provider,
        load_bytecode(specs_out, "AuthRegistry.sol/AuthRegistry")?,
        "Bridge AuthRegistry",
    )
    .await?;
    let controller_implementation = deploy(
        provider,
        with_constructor(
            load_bytecode(specs_out, "TIP20Controller.sol/TIP20Controller")?,
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
        provider,
        with_constructor(
            load_bytecode(specs_out, "ERC1967Proxy.sol/ERC1967Proxy")?,
            FixtureERC1967Proxy::constructorCall {
                implementation: controller_implementation,
                initialization: Bytes::from(
                    BridgeTIP20Controller::initializeCall { admin: deployer }.abi_encode(),
                ),
            }
            .abi_encode(),
        ),
        "Bridge TIP20Controller proxy",
    )
    .await?;
    for (token, label) in [
        (dlusd, "DLUSD"),
        (pathusd, "pathUSD"),
        (reserve_ledger, "reserve ledger"),
    ] {
        let issuer_role = ITIP20::new(token, provider)
            .ISSUER_ROLE()
            .call()
            .await
            .wrap_err_with(|| format!("failed querying {label} issuer role"))?;
        let receipt = IRolesAuth::new(token, provider)
            .grantRole(issuer_role, controller)
            .fee_token(pathusd)
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
        provider,
        with_constructor(
            load_bytecode(
                specs_out,
                "ReserveLedgerWrappedHandler.sol/ReserveLedgerWrappedHandler",
            )?,
            FixtureReserveLedgerWrappedHandler::constructorCall {
                admin_: deployer,
                tokenAuthority_: controller,
                reserveLedgerToken_: reserve_ledger,
            }
            .abi_encode(),
        ),
        "Bridge ReserveLedgerWrappedHandler",
    )
    .await?;
    let controller_contract = BridgeTIP20Controller::new(controller, provider);
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
            deployer,
            "grant Bridge deployer ecosystem role",
        ),
        (
            controller_contract
                .MINT_RATE_LIMIT_SETTER_ROLE()
                .call()
                .await
                .wrap_err("failed querying Bridge controller limit setter role")?,
            deployer,
            "grant Bridge deployer limit setter role",
        ),
    ] {
        let receipt = IRolesAuth::new(controller, provider)
            .grantRole(role, account)
            .fee_token(pathusd)
            .send()
            .await
            .wrap_err(label)?
            .get_receipt()
            .await
            .wrap_err_with(|| format!("failed waiting to {label}"))?;
        check(&receipt, label)?;
    }
    let handler_contract = BridgeDirectSwapHandler::new(handler, provider);
    for (role, label) in [
        (
            handler_contract
                .DIRECT_SWAP_SETTER_ROLE()
                .call()
                .await
                .wrap_err("failed querying Bridge handler DirectSwap setter role")?,
            "grant Bridge handler DirectSwap setter role",
        ),
        (
            handler_contract
                .STABLECOIN_CONFIG_ROLE()
                .call()
                .await
                .wrap_err("failed querying Bridge handler stablecoin config role")?,
            "grant Bridge handler stablecoin config role",
        ),
        (
            handler_contract
                .PAUSER_ROLE()
                .call()
                .await
                .wrap_err("failed querying Bridge handler pauser role")?,
            "grant Bridge handler pauser role",
        ),
    ] {
        let receipt = IRolesAuth::new(handler, provider)
            .grantRole(role, deployer)
            .fee_token(pathusd)
            .send()
            .await
            .wrap_err(label)?
            .get_receipt()
            .await
            .wrap_err_with(|| format!("failed waiting to {label}"))?;
        check(&receipt, label)?;
    }
    for (token, label) in [(dlusd, "DLUSD"), (pathusd, "pathUSD")] {
        let receipt = handler_contract
            .registerStablecoin(token, true)
            .fee_token(pathusd)
            .send()
            .await
            .wrap_err_with(|| format!("failed registering Bridge handler {label}"))?
            .get_receipt()
            .await
            .wrap_err_with(|| format!("failed waiting to register Bridge handler {label}"))?;
        check(&receipt, &format!("register Bridge handler {label}"))?;
    }
    let direct_swap = deploy(
        provider,
        with_constructor(
            load_bytecode(specs_out, "DirectSwapV2.sol/DirectSwapV2")?,
            FixtureDirectSwapV2::constructorCall {
                reserveLedgerToken_: reserve_ledger,
                stablecoinHandler_: handler,
                transactionLimit_: Uint::<96, 2>::from_limbs([
                    liquidity as u64,
                    (liquidity >> 64) as u64,
                ]),
                feeRecipient_: deployer,
                feeBps_: Uint::<256, 4>::ZERO,
                authRegistry_: auth_registry,
                allowedCallerPolicyId_: 1,
            }
            .abi_encode(),
        ),
        "Bridge DirectSwapV2",
    )
    .await?;
    let receipt = handler_contract
        .setDirectSwapContract(direct_swap)
        .fee_token(pathusd)
        .send()
        .await
        .wrap_err("failed configuring Bridge DirectSwap handler")?
        .get_receipt()
        .await
        .wrap_err("failed waiting to configure Bridge DirectSwap handler")?;
    check(&receipt, "configure Bridge DirectSwap handler")?;
    let reserve_capacity =
        Uint::<256, 4>::from_limbs([liquidity as u64, (liquidity >> 64) as u64, 0, 0]);
    for (token, label) in [(dlusd, "DLUSD"), (pathusd, "pathUSD")] {
        let receipt = controller_contract
            .setTxnMintLimit(token, reserve_capacity)
            .fee_token(pathusd)
            .send()
            .await
            .wrap_err_with(|| format!("failed setting DirectSwap {label} transaction limit"))?
            .get_receipt()
            .await
            .wrap_err_with(|| {
                format!("failed waiting to set DirectSwap {label} transaction limit")
            })?;
        check(
            &receipt,
            &format!("set DirectSwap {label} transaction limit"),
        )?;
        let receipt = controller_contract
            .mintBridgeEcosystem(token, deployer, reserve_capacity)
            .fee_token(pathusd)
            .send()
            .await
            .wrap_err_with(|| format!("failed seeding the Bridge {label} reserve"))?
            .get_receipt()
            .await
            .wrap_err_with(|| format!("failed waiting to seed the Bridge {label} reserve"))?;
        check(&receipt, &format!("seed Bridge {label} reserve"))?;
        ensure!(
            controller_contract
                .getReserveStore(token)
                .call()
                .await
                .wrap_err_with(|| format!("failed querying the Bridge {label} reserve store"))?
                != Address::ZERO,
            "Bridge {label} reserve store was not created"
        );
    }

    Ok(SwapSetup {
        route_swapper: None,
        direct_swap: Some(direct_swap),
        simple_swap: None,
        tip20_controller: Some(controller),
        tip20_handler: Some(handler),
        auth_registry: Some(auth_registry),
        reserve_ledger: Some(reserve_ledger),
    })
}

async fn configure_simple_swap<P: Provider<TempoNetwork>>(
    provider: &P,
    specs_out: &std::path::Path,
    deployer: Address,
    dlusd: Address,
    pathusd: Address,
    liquidity: u128,
) -> eyre::Result<SwapSetup> {
    let reserve_ledger = create_reserve_ledger(provider, deployer, pathusd).await?;
    let controller_implementation = deploy(
        provider,
        with_constructor(
            load_bytecode(specs_out, "TIP20Controller.sol/TIP20Controller")?,
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
        provider,
        with_constructor(
            load_bytecode(specs_out, "ERC1967Proxy.sol/ERC1967Proxy")?,
            FixtureERC1967Proxy::constructorCall {
                implementation: controller_implementation,
                initialization: Bytes::from(
                    BridgeTIP20Controller::initializeCall { admin: deployer }.abi_encode(),
                ),
            }
            .abi_encode(),
        ),
        "Bridge TIP20Controller proxy",
    )
    .await?;
    for (token, label) in [
        (dlusd, "DLUSD"),
        (pathusd, "pathUSD"),
        (reserve_ledger, "reserve ledger"),
    ] {
        let issuer_role = ITIP20::new(token, provider)
            .ISSUER_ROLE()
            .call()
            .await
            .wrap_err_with(|| format!("failed querying {label} issuer role"))?;
        let receipt = IRolesAuth::new(token, provider)
            .grantRole(issuer_role, controller)
            .fee_token(pathusd)
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

    let controller_contract = BridgeTIP20Controller::new(controller, provider);
    for (role, label) in [
        (
            controller_contract
                .MINT_RATE_LIMIT_SETTER_ROLE()
                .call()
                .await
                .wrap_err("failed querying Bridge controller limit setter role")?,
            "grant Bridge deployer limit setter role",
        ),
        (
            controller_contract
                .BRIDGE_ECOSYSTEM_CONTRACT_ROLE()
                .call()
                .await
                .wrap_err("failed querying Bridge controller ecosystem role")?,
            "grant Bridge deployer ecosystem role",
        ),
    ] {
        let receipt = IRolesAuth::new(controller, provider)
            .grantRole(role, deployer)
            .fee_token(pathusd)
            .send()
            .await
            .wrap_err(label)?
            .get_receipt()
            .await
            .wrap_err_with(|| format!("failed waiting to {label}"))?;
        check(&receipt, label)?;
    }
    let reserve_capacity = Uint::<256, 4>::from(liquidity);
    for (token, label) in [(dlusd, "DLUSD"), (pathusd, "pathUSD")] {
        let receipt = controller_contract
            .setTxnMintLimit(token, reserve_capacity)
            .fee_token(pathusd)
            .send()
            .await
            .wrap_err_with(|| format!("failed setting minimal {label} transaction limit"))?
            .get_receipt()
            .await
            .wrap_err_with(|| format!("failed waiting to set minimal {label} transaction limit"))?;
        check(&receipt, &format!("set minimal {label} transaction limit"))?;
        let receipt = controller_contract
            .mintBridgeEcosystem(token, deployer, reserve_capacity)
            .fee_token(pathusd)
            .send()
            .await
            .wrap_err_with(|| format!("failed seeding the minimal {label} reserve"))?
            .get_receipt()
            .await
            .wrap_err_with(|| format!("failed waiting to seed the minimal {label} reserve"))?;
        check(&receipt, &format!("seed minimal {label} reserve"))?;
        ensure!(
            controller_contract
                .getReserveStore(token)
                .call()
                .await
                .wrap_err_with(|| format!("failed querying the minimal {label} reserve store"))?
                != Address::ZERO,
            "minimal {label} reserve store was not created"
        );
    }

    Ok(SwapSetup {
        tip20_controller: Some(controller),
        reserve_ledger: Some(reserve_ledger),
        ..SwapSetup::default()
    })
}

async fn grant_controller_unwrapper<P: Provider<TempoNetwork>>(
    provider: &P,
    controller: Address,
    router: Address,
    pathusd: Address,
) -> eyre::Result<()> {
    let role = BridgeTIP20Controller::new(controller, provider)
        .UNWRAPPER_ROLE()
        .call()
        .await
        .wrap_err("failed querying Bridge controller unwrapper role")?;
    let receipt = IRolesAuth::new(controller, provider)
        .grantRole(role, router)
        .fee_token(pathusd)
        .send()
        .await
        .wrap_err("failed granting SingleZoneMinimalEarnRouter unwrapper role")?
        .get_receipt()
        .await
        .wrap_err("failed waiting to grant SingleZoneMinimalEarnRouter unwrapper role")?;
    check(&receipt, "grant SingleZoneMinimalEarnRouter unwrapper role")
}

async fn configure_stablecoin_dex<P: Provider<TempoNetwork>>(
    provider: &P,
    dlusd: Address,
    pathusd: Address,
    liquidity: u128,
) -> eyre::Result<()> {
    let quote_token = ITIP20::new(dlusd, provider)
        .quoteToken()
        .call()
        .await
        .wrap_err("failed querying DLUSD quote token")?;
    ensure!(
        quote_token == pathusd,
        "DLUSD quote token {quote_token} does not match configured pathUSD {pathusd}"
    );

    let dex = IStablecoinDEX::new(STABLECOIN_DEX_ADDRESS, provider);
    let minimum_order = dex
        .MIN_ORDER_AMOUNT()
        .call()
        .await
        .wrap_err("failed querying StablecoinDEX minimum order amount")?;
    ensure!(
        liquidity >= minimum_order,
        "--liquidity {liquidity} is below the StablecoinDEX minimum order amount {minimum_order}"
    );
    let pair_key = dex
        .pairKey(dlusd, pathusd)
        .call()
        .await
        .wrap_err("failed computing StablecoinDEX DLUSD/pathUSD pair key")?;
    let orderbook = dex
        .books(pair_key)
        .call()
        .await
        .wrap_err("failed querying StablecoinDEX DLUSD/pathUSD orderbook")?;
    if orderbook.base == Address::ZERO {
        let receipt = dex
            .createPair(dlusd)
            .fee_token(pathusd)
            .send()
            .await
            .wrap_err("failed creating StablecoinDEX DLUSD/pathUSD pair")?
            .get_receipt()
            .await
            .wrap_err("failed waiting for StablecoinDEX DLUSD/pathUSD pair creation")?;
        check(&receipt, "create StablecoinDEX DLUSD/pathUSD pair")?;
    } else {
        ensure!(
            orderbook.base == dlusd && orderbook.quote == pathusd,
            "StablecoinDEX pair key resolves to unexpected tokens"
        );
    }

    let maximum = Uint::<256, 4>::MAX;
    let receipt = ITIP20::new(pathusd, provider)
        .approve(STABLECOIN_DEX_ADDRESS, maximum)
        .fee_token(pathusd)
        .send()
        .await
        .wrap_err("failed approving StablecoinDEX pathUSD liquidity")?
        .get_receipt()
        .await
        .wrap_err("failed waiting for StablecoinDEX pathUSD approval")?;
    check(&receipt, "approve StablecoinDEX pathUSD liquidity")?;
    let receipt = dex
        .place(dlusd, liquidity, true, STABLECOIN_DEX_LIQUIDITY_TICK)
        .fee_token(pathusd)
        .send()
        .await
        .wrap_err("failed placing StablecoinDEX DLUSD bid liquidity")?
        .get_receipt()
        .await
        .wrap_err("failed waiting for StablecoinDEX DLUSD bid liquidity")?;
    check(&receipt, "place StablecoinDEX DLUSD bid liquidity")?;

    let receipt = ITIP20::new(dlusd, provider)
        .approve(STABLECOIN_DEX_ADDRESS, maximum)
        .fee_token(pathusd)
        .send()
        .await
        .wrap_err("failed approving StablecoinDEX DLUSD liquidity")?
        .get_receipt()
        .await
        .wrap_err("failed waiting for StablecoinDEX DLUSD approval")?;
    check(&receipt, "approve StablecoinDEX DLUSD liquidity")?;
    let receipt = dex
        .place(dlusd, liquidity, false, STABLECOIN_DEX_LIQUIDITY_TICK)
        .fee_token(pathusd)
        .send()
        .await
        .wrap_err("failed placing StablecoinDEX DLUSD ask liquidity")?
        .get_receipt()
        .await
        .wrap_err("failed waiting for StablecoinDEX DLUSD ask liquidity")?;
    check(&receipt, "place StablecoinDEX DLUSD ask liquidity")?;

    for (token_in, token_out, label) in [
        (dlusd, pathusd, "DLUSD to pathUSD"),
        (pathusd, dlusd, "pathUSD to DLUSD"),
    ] {
        let amount_out = dex
            .quoteSwapExactAmountIn(token_in, token_out, minimum_order)
            .call()
            .await
            .wrap_err_with(|| format!("failed quoting StablecoinDEX {label}"))?;
        ensure!(
            amount_out > 0,
            "StablecoinDEX {label} quote returned zero after seeding liquidity"
        );
    }
    Ok(())
}

fn signer_from_env(name: &str) -> eyre::Result<PrivateKeySigner> {
    let key =
        std::env::var(name).wrap_err_with(|| format!("{name} must be set in the environment"))?;
    key.strip_prefix("0x")
        .unwrap_or(&key)
        .parse()
        .wrap_err_with(|| format!("{name} is not a valid private key"))
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
    let gas_limit = std::env::var("ZONES_BENCH_L1_GENERAL_GAS_LIMIT")
        .unwrap_or_else(|_| DEFAULT_DEPLOYMENT_GAS_LIMIT.to_string())
        .parse::<u64>()
        .wrap_err("ZONES_BENCH_L1_GENERAL_GAS_LIMIT must be an unsigned integer")?;
    ensure!(
        gas_limit > 0 && gas_limit <= DEFAULT_DEPLOYMENT_GAS_LIMIT,
        "ZONES_BENCH_L1_GENERAL_GAS_LIMIT must be between 1 and {DEFAULT_DEPLOYMENT_GAS_LIMIT}"
    );
    let receipt = provider
        .send_transaction(
            TransactionRequest::default()
                .with_kind(alloy::primitives::TxKind::Create)
                // Fixture constructors can make contract calls that Tempo's generic
                // estimator underestimates. Use the configured general-transaction
                // cap so the setup transaction is admissible on the provisioned L1.
                .with_gas_limit(gas_limit)
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

fn zero_fee_init() -> FixtureEarnFactory::EarnFeesInit {
    let zero_fee_recipient = FixtureEarnFactory::FixedFeeRecipient {
        account: Address::ZERO,
        rate: Uint::<96, 2>::ZERO,
    };
    FixtureEarnFactory::EarnFeesInit {
        administrator: Address::ZERO,
        guardian: Address::ZERO,
        fixedFeeCap: Uint::<96, 2>::ZERO,
        excessFeeCap: Uint::<96, 2>::ZERO,
        initialConfig: FixtureEarnFactory::FeeConfig {
            fixedFeeCount: 0,
            fixedFees: std::array::from_fn(|_| zero_fee_recipient.clone()),
            excess: FixtureEarnFactory::ExcessReturnFee {
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
                            "name": input["name"],
                            "type": input["type"],
                            "indexed": input["indexed"],
                        }))
                        .collect::<Vec<_>>(),
                })
            })
            .collect()
    }

    fn required_args() -> [&'static str; 17] {
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
            "--private-asset",
            "0x20c0000000000000000000000000000000000001",
            "--earn-revision",
            "8bd48a68617713ec5b5828fffbde9c0b8185946e",
            "--allowed-accounts-file",
            "/tmp/zones-benchmark-allowed-accounts",
            "--output",
            "target/fixtures.json",
        ]
    }

    #[test]
    fn fixture_deployment_does_not_accept_private_keys_as_arguments() {
        for argument in ["--fixture-deployer-key", "--portal-admin-key"] {
            let error = DeployNeobankFixtures::try_parse_from(
                required_args().into_iter().chain([argument, "0x01"]),
            )
            .unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
        }
    }

    #[test]
    fn fixture_deployment_uses_native_tip20_addresses() {
        let command = DeployNeobankFixtures::try_parse_from(required_args()).unwrap();
        assert_eq!(command.liquidity, 10_000_000_000);
        assert_eq!(command.swap_mechanism, SwapMechanism::DirectSwap);
        assert_eq!(
            command.dlusd.to_string(),
            "0x20C0000000000000000000000000000000000001"
        );
    }

    #[test]
    fn closed_loop_roles_cover_accounts_bridge_and_router() {
        let account_a = Address::repeat_byte(0x11);
        let account_b = Address::repeat_byte(0x22);
        let bridge = Address::repeat_byte(0x33);
        let router = Address::repeat_byte(0x44);
        let messenger = Address::repeat_byte(0x55);
        let assignments =
            portal_role_assignments(&[account_a, account_b], bridge, router, messenger).unwrap();
        assert_eq!(assignments.len(), 4);
        assert_eq!(assignments[0].0, account_a);
        assert_eq!(assignments[0].1 as u8, PortalRole::Account as u8);
        assert_eq!(assignments[1].0, account_b);
        assert_eq!(assignments[1].1 as u8, PortalRole::Account as u8);
        assert_eq!(assignments[2].0, bridge);
        assert_eq!(assignments[2].1 as u8, PortalRole::Account as u8);
        assert_eq!(assignments[3].0, router);
        assert_eq!(assignments[3].1 as u8, PortalRole::CallbackGateway as u8);
    }

    #[test]
    fn closed_loop_roles_reject_invalid_fixture_addresses() {
        let bridge = Address::repeat_byte(0x33);
        let router = Address::repeat_byte(0x44);
        let messenger = Address::repeat_byte(0x55);
        assert!(portal_role_assignments(&[], Address::ZERO, router, messenger).is_err());
        assert!(portal_role_assignments(&[], bridge, Address::ZERO, messenger).is_err());
        assert!(portal_role_assignments(&[], bridge, bridge, messenger).is_err());
        assert!(portal_role_assignments(&[], messenger, router, messenger).is_err());
        assert!(portal_role_assignments(&[], bridge, messenger, messenger).is_err());
        assert!(portal_role_assignments(&[Address::ZERO], bridge, router, messenger).is_err());
        assert!(portal_role_assignments(&[messenger], bridge, router, messenger).is_err());
        assert!(portal_role_assignments(&[bridge], bridge, router, messenger).is_err());
        assert!(portal_role_assignments(&[router], bridge, router, messenger).is_err());
    }

    #[test]
    fn fixture_deployment_accepts_each_swap_mechanism() {
        for (argument, expected) in [
            ("direct-swap", SwapMechanism::DirectSwap),
            ("simple", SwapMechanism::Simple),
            ("stablecoin-dex", SwapMechanism::StablecoinDex),
        ] {
            let command = DeployNeobankFixtures::try_parse_from(
                required_args()
                    .into_iter()
                    .chain(["--swap-mechanism", argument]),
            )
            .unwrap();
            assert_eq!(command.swap_mechanism, expected);
        }
    }

    #[test]
    fn controller_backed_swap_liquidity_respects_the_pinned_cap() {
        validate_liquidity(SwapMechanism::DirectSwap, DIRECT_SWAP_ABSOLUTE_MAX - 1).unwrap();
        validate_liquidity(SwapMechanism::Simple, DIRECT_SWAP_ABSOLUTE_MAX - 1).unwrap();
        assert!(validate_liquidity(SwapMechanism::DirectSwap, DIRECT_SWAP_ABSOLUTE_MAX).is_err());
        assert!(validate_liquidity(SwapMechanism::Simple, DIRECT_SWAP_ABSOLUTE_MAX).is_err());
        assert!(
            validate_liquidity(SwapMechanism::DirectSwap, DIRECT_SWAP_ABSOLUTE_MAX + 1).is_err()
        );
        assert!(validate_liquidity(SwapMechanism::Simple, DIRECT_SWAP_ABSOLUTE_MAX + 1).is_err());
        validate_liquidity(
            SwapMechanism::StablecoinDex,
            DIRECT_SWAP_ABSOLUTE_MAX.saturating_add(1),
        )
        .unwrap();
    }

    #[test]
    fn fixture_metadata_uses_null_for_unselected_swap_contracts() {
        let address = "0x0000000000000000000000000000000000000001".to_owned();
        let metadata = FixtureMetadata {
            portal: address.clone(),
            messenger: address.clone(),
            zone_id: 1,
            dlusd: address.clone(),
            pathusd: address.clone(),
            private_asset: address.clone(),
            earn_token: address.clone(),
            earn_share: address.clone(),
            swap_mechanism: SwapMechanism::StablecoinDex,
            default_swapper: STABLECOIN_DEX_ADDRESS.to_string(),
            route_swapper: None,
            route_override: false,
            stablecoin_dex: STABLECOIN_DEX_ADDRESS.to_string(),
            direct_swap: None,
            simple_swap: None,
            swap_adapter: None,
            tip20_controller: None,
            tip20_handler: None,
            auth_registry: None,
            reserve_ledger: None,
            liquidity: 10_000_000_000,
            liquidity_tick: 0,
            earn_fixture_revision: "8bd48a68617713ec5b5828fffbde9c0b8185946e".to_owned(),
            vault: address.clone(),
            engine: address.clone(),
            earn_factory: address.clone(),
            earn_vault: address.clone(),
            earn_fees: address.clone(),
            earn_router: address.clone(),
            contribution_controller: address.clone(),
            vault_adapter: address.clone(),
            rewards: address.clone(),
            gateway: address.clone(),
            bridge_wallet: address,
        };
        let metadata = serde_json::to_value(metadata).unwrap();
        assert_eq!(metadata["swapMechanism"], "stablecoin-dex");
        assert_eq!(metadata["routeOverride"], false);
        assert!(metadata["routeSwapper"].is_null());
        assert!(metadata["directSwap"].is_null());
        assert!(metadata["simpleSwap"].is_null());
        assert_eq!(metadata["earnShare"], metadata["earnToken"]);
        assert_eq!(metadata["vaultAdapter"], metadata["earnVault"]);
        assert_eq!(metadata["rewards"], metadata["contributionController"]);
        assert_eq!(metadata["gateway"], metadata["earnRouter"]);
        assert_eq!(
            metadata["stablecoinDex"],
            STABLECOIN_DEX_ADDRESS.to_string()
        );
    }

    #[test]
    fn fixture_deployment_uses_external_earn_artifacts() {
        let out = PathBuf::from("specs/ref-impls/out");
        assert_eq!(
            artifact_path(&out, "SingleZoneEarnRouter.sol/SingleZoneEarnRouter"),
            PathBuf::from("specs/ref-impls/out/SingleZoneEarnRouter.sol/SingleZoneEarnRouter.json")
        );
        assert_eq!(
            artifact_path(
                &out,
                "SingleZoneBridgeEarnRouter.sol/SingleZoneBridgeEarnRouter"
            ),
            PathBuf::from(
                "specs/ref-impls/out/SingleZoneBridgeEarnRouter.sol/SingleZoneBridgeEarnRouter.json"
            )
        );
        assert_eq!(
            artifact_path(
                &out,
                "SingleZoneMinimalEarnRouter.sol/SingleZoneMinimalEarnRouter"
            ),
            PathBuf::from(
                "specs/ref-impls/out/SingleZoneMinimalEarnRouter.sol/SingleZoneMinimalEarnRouter.json"
            )
        );
        assert_eq!(
            artifact_path(&out, "EarnVault.sol/EarnVault"),
            PathBuf::from("specs/ref-impls/out/EarnVault.sol/EarnVault.json")
        );
        assert_eq!(
            artifact_path(&out, "EarnFees.sol/EarnFees"),
            PathBuf::from("specs/ref-impls/out/EarnFees.sol/EarnFees.json")
        );
        assert_eq!(
            artifact_path(&out, "EarnFactory.sol/EarnFactory"),
            PathBuf::from("specs/ref-impls/out/EarnFactory.sol/EarnFactory.json")
        );
        assert_eq!(
            artifact_path(
                &out,
                "EarnContributionController.sol/EarnContributionController"
            ),
            PathBuf::from(
                "specs/ref-impls/out/EarnContributionController.sol/EarnContributionController.json"
            )
        );
        assert_eq!(
            artifact_path(&out, "DirectSwapV2.sol/DirectSwapV2"),
            PathBuf::from("specs/ref-impls/out/DirectSwapV2.sol/DirectSwapV2.json")
        );
        assert_eq!(
            artifact_path(
                &out,
                "ReserveLedgerWrappedHandler.sol/ReserveLedgerWrappedHandler"
            ),
            PathBuf::from(
                "specs/ref-impls/out/ReserveLedgerWrappedHandler.sol/ReserveLedgerWrappedHandler.json"
            )
        );
    }

    #[test]
    fn deployment_bindings_match_external_earn_selectors() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        let factory: serde_json::Value = serde_json::from_slice(
            &fs::read(root.join("specs/ref-impls/out/EarnFactory.sol/EarnFactory.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            alloy::primitives::hex::encode(FixtureEarnFactory::deployCall::SELECTOR),
            factory["methodIdentifiers"][FixtureEarnFactory::deployCall::SIGNATURE]
                .as_str()
                .unwrap()
        );

        let engine: serde_json::Value = serde_json::from_slice(
            &fs::read(root.join("specs/ref-impls/out/ERC4626Engine.sol/ERC4626Engine.json"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            alloy::primitives::hex::encode(
                ERC4626EngineInitializer::initializeEarnVaultCall::SELECTOR
            ),
            engine["methodIdentifiers"]
                [ERC4626EngineInitializer::initializeEarnVaultCall::SIGNATURE]
                .as_str()
                .unwrap()
        );

        let handler: serde_json::Value = serde_json::from_slice(
            &fs::read(
                root.join(
                    "specs/ref-impls/out/ReserveLedgerWrappedHandler.sol/ReserveLedgerWrappedHandler.json",
                ),
            )
            .unwrap(),
        )
        .unwrap();
        for (signature, selector) in [
            (
                BridgeDirectSwapHandler::registerStablecoinCall::SIGNATURE,
                BridgeDirectSwapHandler::registerStablecoinCall::SELECTOR,
            ),
            (
                BridgeDirectSwapHandler::setDirectSwapContractCall::SIGNATURE,
                BridgeDirectSwapHandler::setDirectSwapContractCall::SELECTOR,
            ),
        ] {
            assert_eq!(
                alloy::primitives::hex::encode(selector),
                handler["methodIdentifiers"][signature].as_str().unwrap()
            );
        }
    }

    #[test]
    fn benchmark_abis_match_external_earn_interfaces() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        for (minimal, artifact, functions) in [
            (
                "contrib/bench/neobank/abis/earn-contribution-controller.json",
                "specs/ref-impls/out/EarnContributionController.sol/EarnContributionController.json",
                &["fund"][..],
            ),
            (
                "contrib/bench/neobank/abis/earn-vault.json",
                "specs/ref-impls/out/EarnVault.sol/EarnVault.json",
                &["redeem", "totalEarnShares", "previewRedeem"][..],
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
                    "minimal {function} ABI diverges from the external Earn artifact"
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
                    "minimal Funded ABI diverges from the external Earn artifact"
                );
            }
        }
        let minimal: serde_json::Value = serde_json::from_slice(
            &fs::read(root.join("contrib/bench/neobank/abis/earn-router.json")).unwrap(),
        )
        .unwrap();
        let artifact: serde_json::Value = serde_json::from_slice(
            &fs::read(
                root.join("specs/ref-impls/out/SingleZoneEarnRouter.sol/SingleZoneEarnRouter.json"),
            )
            .unwrap(),
        )
        .unwrap();
        for event in ["EarnDeposit", "EarnRedeem"] {
            assert_eq!(
                event_shapes(&minimal, event),
                event_shapes(&artifact["abi"], event),
                "minimal {event} ABI diverges from the external Earn artifact"
            );
        }
    }
}
