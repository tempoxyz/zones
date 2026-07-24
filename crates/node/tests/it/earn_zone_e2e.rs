//! Tempo Earn scenarios that cross the public L1 / private Zone boundary.
//!
//! CI builds the Solidity artifacts from the Tempo Earn `main` branch. These tests deliberately
//! exercise the complete callback path: a private withdrawal settles on L1, the Earn router
//! deposits or redeems through the vault stack, and the output is encrypted back into the
//! originating Zone.

use crate::utils::{
    L1TestNode, WithdrawalArgs, ZoneAccount, ZoneTestNode, forge_bytecode, spawn_sequencer,
};
use alloy::{
    primitives::{Address, B256, Bytes, TxKind, U256, keccak256},
    providers::{Provider, ProviderBuilder},
};
use alloy_network::ReceiptResponse;
use alloy_rpc_types_eth::{Filter, TransactionRequest};
use alloy_sol_types::{SolCall, SolConstructor, SolValue};
use eyre::WrapErr;
use std::time::Duration;
use tempo_alloy::{TempoNetwork, rpc::TempoTransactionRequest};
use tempo_contracts::precompiles::{ITIP20, ITIP403Registry};
use tempo_precompiles::{PATH_USD_ADDRESS, TIP20_FACTORY_ADDRESS, TIP403_REGISTRY_ADDRESS};
use tempo_primitives::transaction::Call;
use tempo_zone_contracts::{EncryptedDepositPayload, ZONE_OUTBOX_ADDRESS, ZonePortal};

const AMOUNT: u128 = 1_000_000;
const REWARD_AMOUNT: u128 = AMOUNT / 10;
const PUBLIC_USER_BALANCE: u128 = 50_000_000;
const PRIVATE_FEE_BALANCE: u128 = 10_000_000;
const DEX_LIQUIDITY: u128 = 300_000_000;
const TOKEN_SUPPLY: u128 = 1_000_000_000;
// Match Earn's current callback budget.
const CALLBACK_GAS_LIMIT: u64 = 10_000_000;
const REWARD_FUNDING_TX_GAS_LIMIT: u64 = 5_000_000;
const MIGRATION_TX_GAS_LIMIT: u64 = 10_000_000;
// Tempo's transaction cap is 30M.
const CONTRACT_DEPLOYMENT_TX_GAS_LIMIT: u64 = 30_000_000;
const MIN_GAS_HEADROOM_PERCENT: u64 = 15;
const E2E_TIMEOUT: Duration = Duration::from_secs(90);
const BOUNCE_TIMEOUT: Duration = Duration::from_secs(180);

alloy_sol_types::sol! {
    enum EarnFlow {
        Deposit,
        Redeem
    }

    enum EngineMigrationMode {
        UserOnly,
        OperatorEnabled
    }

    struct EarnVaultControls {
        address emergencyGuardian;
        address asyncJanitor;
        EngineMigrationMode migrationMode;
    }

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

    struct EarnEncryptedDepositPayload {
        bytes32 ephemeralPubkeyX;
        uint8 ephemeralPubkeyYParity;
        bytes ciphertext;
        bytes12 nonce;
        bytes16 tag;
    }

    enum EarnDestination {
        Zone,
        Public
    }

    struct EarnZoneDelivery {
        address portal;
        uint256 keyIndex;
        EarnEncryptedDepositPayload encrypted;
        address refundRecipient;
    }

    struct EarnZoneReturn {
        uint256 keyIndex;
        EarnEncryptedDepositPayload encrypted;
        address refundRecipient;
    }

    struct EarnCallbackData {
        EarnFlow flow;
        address earnVault;
        EarnDestination destination;
        address outputToken;
        uint128 minVaultAssets;
        uint128 minEarnShares;
        uint128 minOutputAmount;
        bytes32 actionId;
        bytes destinationData;
    }

    struct EarnDeployParams {
        bytes32 deploymentId;
        address engine;
        address owner;
        EarnVaultControls controls;
        EarnFeesInit fees;
    }

    #[sol(rpc)]
    contract EarnZonePortalView {
        function messenger() external view returns (address);
    }

    #[sol(rpc)]
    contract EarnShare {
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
        function totalSupply() external view returns (uint256);
    }

    #[sol(rpc)]
    contract VenueVault {
        constructor(address asset_, string name_, string symbol_, uint8 decimals_);
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
        function deposit(uint256 assets, address receiver) external returns (uint256 shares);
        function totalAssets() external view returns (uint256);
    }

    #[sol(rpc)]
    contract EarnEngine {
        constructor(address vault_, address owner_, string nameOverride_, string symbolOverride_);
        function initializeEarnVault(address earnVault) external;
    }

    #[sol(rpc)]
    contract EarnFactory {
        constructor(
            address tip20Factory_,
            address earnVaultImplementation_,
            address earnFeesImplementation_
        );
        function deploy(EarnDeployParams params)
            external
            returns (address earnShare, address earnVault, address earnFees);
    }

    #[sol(rpc)]
    contract EarnVault {
        function depositVenueShares(uint256 venueShares, address receiver, uint256 minEarnShares)
            external
            returns (uint256 earnShares);
        function depositsPaused() external view returns (bool);
        function engine() external view returns (address);
        function engineShares() external view returns (uint256);
        function migrateEngine(address newEngine, uint256 minNewShares, uint256 minAssetsRetained)
            external
            returns (uint256 newShares);
        function previewRedeem(uint256 shares) external view returns (uint256 assets);
        function setDepositsPaused(bool paused) external;
        function convertEngineSharesToEarnShares(uint256 shares) external view returns (uint256);
        function totalEarnShares() external view returns (uint256);
        function anchorEngineShares() external view returns (uint256);
        function anchorEarnShares() external view returns (uint256);
    }

    #[sol(rpc)]
    contract UniversalEarnRouter {
        function deposit(address earnVault, uint256 assets, uint256 minEarnShares, address recipient)
            external
            returns (uint256 earnShares);
        function depositToZone(
            address earnVault,
            uint256 assets,
            uint256 minEarnShares,
            EarnZoneDelivery delivery
        ) external returns (uint256 earnShares, bytes32 zoneDepositHash);
        function redeem(address earnVault, uint256 earnShares, uint256 minAssets, address recipient)
            external
            returns (uint256 assets);
        function redeemToZone(
            address earnVault,
            uint256 earnShares,
            uint256 minAssets,
            EarnZoneDelivery delivery
        ) external returns (uint256 assets, bytes32 zoneDepositHash);
    }

    #[sol(rpc)]
    contract EarnContributionController {
        function fund(address funder, uint256 requested, uint256 maxEarnShareSupply)
            external
            returns (uint256 funded);
        function executeFunding(address funder, uint256 requested, uint256 maxEarnShareSupply)
            external
            returns (uint256 funded);
    }
}

#[derive(Clone, Copy)]
struct EarnLimits {
    min_vault_assets: u128,
    min_earn_shares: u128,
    min_output_amount: u128,
}

impl Default for EarnLimits {
    fn default() -> Self {
        Self {
            min_vault_assets: 1,
            min_earn_shares: 1,
            min_output_amount: 1,
        }
    }
}

#[derive(Clone, Copy)]
struct EarnAccessPolicy {
    compound_id: u64,
    whitelist_id: u64,
}

struct EarnZoneFixture {
    l1: L1TestNode,
    zone: ZoneTestNode,
    portal: Address,
    vault_asset: Address,
    alternate_asset: Address,
    venue_vault: Address,
    engine: Address,
    earn_share: Address,
    earn_vault: Address,
    router: Address,
    contribution_controller: Address,
    access_policy: Option<EarnAccessPolicy>,
    user: ZoneAccount,
    _sequencer: zone_sequencer::ZoneSequencerHandle,
}

impl EarnZoneFixture {
    async fn start() -> eyre::Result<Self> {
        Self::start_with_access_policy(false).await
    }

    async fn start_protected() -> eyre::Result<Self> {
        Self::start_with_access_policy(true).await
    }

    async fn start_with_access_policy(protected: bool) -> eyre::Result<Self> {
        reth_tracing::init_test_tracing();

        let l1 = L1TestNode::start().await?;
        let vault_asset = l1
            .create_tip20("Earn Vault USD", "evUSD", B256::with_last_byte(0xE1))
            .await?;
        let alternate_asset = l1
            .create_tip20("Earn Alternate USD", "eaUSD", B256::with_last_byte(0xE2))
            .await?;
        l1.mint_tip20(vault_asset, l1.dev_address(), TOKEN_SUPPLY)
            .await?;
        l1.mint_tip20(alternate_asset, l1.dev_address(), TOKEN_SUPPLY)
            .await?;

        let portal = l1.deploy_zone().await?;
        let zone = ZoneTestNode::start_from_l1(l1.http_url(), l1.ws_url(), portal).await?;
        zone.wait_for_l2_tempo_finalized(0, E2E_TIMEOUT).await?;

        let encryption_key = k256::SecretKey::from(l1.dev_signer().credential());
        l1.set_sequencer_encryption_key(portal, &encryption_key)
            .await?;

        let messenger = EarnZonePortalView::new(portal, l1.provider())
            .messenger()
            .call()
            .await?;
        let owner = l1.dev_address();
        let user_address = l1.user_signer().address();

        let venue_vault = deploy_contract(
            &l1,
            "Simple4626Vault",
            VenueVault::constructorCall {
                asset_: vault_asset,
                name_: "Tempo Earn Zone Vault".to_string(),
                symbol_: "teZONE".to_string(),
                decimals_: 6,
            }
            .abi_encode(),
        )
        .await?;
        let engine = deploy_contract(
            &l1,
            "ERC4626Engine",
            EarnEngine::constructorCall {
                vault_: venue_vault,
                owner_: owner,
                nameOverride_: String::new(),
                symbolOverride_: String::new(),
            }
            .abi_encode(),
        )
        .await?;
        let earn_vault_implementation = deploy_contract(&l1, "EarnVault", Vec::new()).await?;
        let earn_fees_implementation = deploy_contract(&l1, "EarnFees", Vec::new()).await?;
        let factory = deploy_contract(
            &l1,
            "EarnFactory",
            EarnFactory::constructorCall {
                tip20Factory_: TIP20_FACTORY_ADDRESS,
                earnVaultImplementation_: earn_vault_implementation,
                earnFeesImplementation_: earn_fees_implementation,
            }
            .abi_encode(),
        )
        .await?;

        let fees = EarnFeesInit {
            administrator: Address::ZERO,
            guardian: Address::ZERO,
            fixedFeeCap: Default::default(),
            excessFeeCap: Default::default(),
            initialConfig: FeeConfig {
                fixedFeeCount: 0,
                fixedFees: std::array::from_fn(|_| FixedFeeRecipient {
                    account: Address::ZERO,
                    rate: Default::default(),
                }),
                excess: ExcessReturnFee {
                    enabled: false,
                    account: Address::ZERO,
                    annualTargetRate: Default::default(),
                    excessFeeRate: Default::default(),
                },
            },
        };
        let params = EarnDeployParams {
            deploymentId: keccak256("zones-earn-e2e-v1"),
            engine,
            owner,
            controls: EarnVaultControls {
                emergencyGuardian: Address::ZERO,
                asyncJanitor: Address::ZERO,
                migrationMode: EngineMigrationMode::OperatorEnabled,
            },
            fees,
        };

        let provider = l1.dev_provider();
        let factory_contract = EarnFactory::new(factory, &provider);
        let predicted = factory_contract.deploy_call(params.clone()).call().await?;
        let receipt = factory_contract
            .deploy_call(params)
            .gas(CONTRACT_DEPLOYMENT_TX_GAS_LIMIT)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "deploying the Earn stack failed");
        ensure_gas_headroom(
            receipt.gas_used,
            CONTRACT_DEPLOYMENT_TX_GAS_LIMIT,
            "Earn stack deployment",
        )?;
        let earn_share = predicted.earnShare;
        let earn_vault = predicted.earnVault;

        let receipt = EarnEngine::new(engine, &provider)
            .initializeEarnVault(earn_vault)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "initializing the Earn engine failed");

        let router = deploy_contract(&l1, "UniversalEarnRouter", Vec::new()).await?;
        let router_block = l1.set_zone_gateway_on_portal(portal, router, true).await?;
        zone.wait_for_l2_tempo_finalized(router_block, E2E_TIMEOUT)
            .await?;
        zone.assert_zone_gateway(router, true).await?;
        let contribution_controller = deploy_contract(
            &l1,
            "EarnContributionController",
            (earn_vault, user_address).abi_encode(),
        )
        .await?;

        let access_policy = if protected {
            let whitelist_id = l1.create_whitelist_policy().await?;
            let outsider = l1.signer_at(3).address();
            for account in [
                earn_vault,
                router,
                portal,
                messenger,
                ZONE_OUTBOX_ADDRESS,
                owner,
                user_address,
                outsider,
            ] {
                l1.whitelist_address(whitelist_id, account).await?;
            }
            let compound_id = l1
                .create_compound_policy(1, whitelist_id, whitelist_id)
                .await?;
            l1.change_transfer_policy_id(earn_share, compound_id)
                .await?;
            Some(EarnAccessPolicy {
                compound_id,
                whitelist_id,
            })
        } else {
            None
        };

        l1.enable_token_on_portal(portal, vault_asset).await?;
        l1.enable_token_on_portal(portal, alternate_asset).await?;
        l1.enable_token_on_portal(portal, earn_share).await?;

        l1.create_dex_pair(vault_asset).await?;
        l1.create_dex_pair(alternate_asset).await?;
        l1.place_dex_bid_order(vault_asset, DEX_LIQUIDITY, 0)
            .await?;
        l1.place_dex_ask_order(vault_asset, DEX_LIQUIDITY, 0)
            .await?;
        l1.place_dex_bid_order(alternate_asset, DEX_LIQUIDITY, 0)
            .await?;
        l1.place_dex_ask_order(alternate_asset, DEX_LIQUIDITY, 0)
            .await?;

        let configured_at = l1.provider().get_block_number().await?;
        zone.wait_for_l2_tempo_finalized(configured_at, E2E_TIMEOUT)
            .await?;

        l1.fund_user(user_address, PUBLIC_USER_BALANCE).await?;
        l1.fund_user_token(vault_asset, user_address, PUBLIC_USER_BALANCE)
            .await?;
        l1.fund_user_token(alternate_asset, user_address, PUBLIC_USER_BALANCE)
            .await?;

        let mut user = ZoneAccount::from_l1_and_zone(&l1, &zone, portal);
        user.deposit(PRIVATE_FEE_BALANCE, E2E_TIMEOUT, &zone)
            .await?;

        let sequencer = spawn_sequencer(&l1, &zone, portal, l1.dev_signer()).await;

        Ok(Self {
            l1,
            zone,
            portal,
            vault_asset,
            alternate_asset,
            venue_vault,
            engine,
            earn_share,
            earn_vault,
            router,
            contribution_controller,
            access_policy,
            user,
            _sequencer: sequencer,
        })
    }

    async fn set_access_eligibility(&self, account: Address, eligible: bool) -> eyre::Result<()> {
        let policy = self
            .access_policy
            .ok_or_else(|| eyre::eyre!("Earn access policy is not configured"))?;
        let provider = self.l1.dev_provider();
        let receipt = ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, &provider)
            .modifyPolicyWhitelist(policy.whitelist_id, account, eligible)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "updating Earn eligibility failed");

        let policy_block = self.l1.provider().get_block_number().await?;
        self.zone
            .wait_for_l2_tempo_finalized(policy_block, E2E_TIMEOUT)
            .await?;
        let zone_registry = ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, self.zone.provider());
        let recipient_authorized = zone_registry
            .isAuthorizedRecipient(policy.compound_id, account)
            .call()
            .await?;
        let mint_authorized = zone_registry
            .isAuthorizedMintRecipient(policy.compound_id, account)
            .call()
            .await?;
        eyre::ensure!(
            recipient_authorized == eligible && mint_authorized == eligible,
            "Zone did not mirror Earn eligibility for {account}: recipient={recipient_authorized}, mint={mint_authorized}, expected={eligible}"
        );
        Ok(())
    }

    async fn allow_zone_account(&self, account: Address) -> eyre::Result<()> {
        let block = self
            .l1
            .set_allowed_account_on_portal(self.portal, account, true)
            .await?;
        self.zone
            .wait_for_l2_tempo_finalized(block, E2E_TIMEOUT)
            .await?;
        self.zone.assert_allowed_account(account, true).await
    }

    async fn set_deposits_paused(&self, paused: bool) -> eyre::Result<()> {
        let provider = self.l1.dev_provider();
        let earn_vault = EarnVault::new(self.earn_vault, &provider);
        let receipt = earn_vault
            .setDepositsPaused(paused)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "setting Earn deposit pause failed");
        eyre::ensure!(
            earn_vault.depositsPaused().call().await? == paused,
            "Earn deposit pause state did not update"
        );
        Ok(())
    }

    async fn assert_engine_migration_guardrails(&self) -> eyre::Result<()> {
        let earn_vault = EarnVault::new(self.earn_vault, self.l1.provider());
        let engine_before = earn_vault.engine().call().await?;
        let engine_shares_before = earn_vault.engineShares().call().await?;
        let supply_before = earn_vault.totalEarnShares().call().await?;

        let unauthorized = EarnVault::new(self.earn_vault, self.user.l1_provider())
            .migrateEngine(Address::ZERO, U256::from(1), U256::from(1))
            .from(self.user.address())
            .call()
            .await;
        eyre::ensure!(
            unauthorized.is_err(),
            "non-operator unexpectedly passed the engine migration guard"
        );

        let provider = self.l1.dev_provider();
        let operator_vault = EarnVault::new(self.earn_vault, &provider);
        eyre::ensure!(
            operator_vault
                .migrateEngine(Address::ZERO, U256::from(1), U256::from(1))
                .call()
                .await
                .is_err(),
            "operator unexpectedly migrated to the zero engine"
        );
        eyre::ensure!(
            operator_vault
                .migrateEngine(engine_before, U256::from(1), U256::from(1))
                .call()
                .await
                .is_err(),
            "operator unexpectedly migrated to the active engine"
        );

        assert_eq!(earn_vault.engine().call().await?, engine_before);
        assert_eq!(
            earn_vault.engineShares().call().await?,
            engine_shares_before
        );
        assert_eq!(earn_vault.totalEarnShares().call().await?, supply_before);
        Ok(())
    }

    async fn encrypted_entry(&mut self, token: Address, amount: u128) -> eyre::Result<()> {
        let recipient = self.user.address();
        let provider = self.user.l1_provider();
        let receipt = ITIP20::new(token, provider)
            .approve(self.portal, U256::MAX)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "encrypted entry approval failed");

        let (key_index, encrypted) = self
            .l1
            .encrypt_deposit_for_portal(self.portal, recipient, B256::ZERO)
            .await?;
        let balance_before = self.zone.balance_of(token, recipient).await?;
        let receipt = ZonePortal::new(self.portal, provider)
            .depositEncrypted(token, amount, key_index, encrypted, recipient)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "encrypted Zone entry failed");

        self.zone
            .wait_for_balance(
                token,
                recipient,
                balance_before + U256::from(amount),
                E2E_TIMEOUT,
            )
            .await?;
        Ok(())
    }

    async fn callback_data(
        &self,
        flow: EarnFlow,
        output_token: Address,
        recipient: Address,
        refund_recipient: Address,
        limits: EarnLimits,
    ) -> eyre::Result<Bytes> {
        let (key_index, encrypted) = self
            .l1
            .encrypt_deposit_for_portal(self.portal, recipient, B256::ZERO)
            .await?;
        let action_id = keccak256(encrypted.ciphertext.as_ref());
        let destination_data = EarnZoneReturn {
            keyIndex: key_index,
            encrypted: map_encrypted_payload(encrypted),
            refundRecipient: refund_recipient,
        }
        .abi_encode()
        .into();
        Ok(EarnCallbackData {
            flow,
            earnVault: self.earn_vault,
            destination: EarnDestination::Zone,
            outputToken: output_token,
            minVaultAssets: limits.min_vault_assets,
            minEarnShares: limits.min_earn_shares,
            minOutputAmount: limits.min_output_amount,
            actionId: action_id,
            destinationData: destination_data,
        }
        .abi_encode()
        .into())
    }

    async fn deposit_limits(&self, input_token: Address, amount: u128) -> eyre::Result<EarnLimits> {
        let expected_vault_assets = if input_token == self.vault_asset {
            amount
        } else {
            self.l1
                .quote_dex_swap_exact_amount_in(input_token, self.vault_asset, amount)
                .await?
        };
        let min_vault_assets = minimum_output(expected_vault_assets);
        let expected_earn_shares = EarnVault::new(self.earn_vault, self.l1.provider())
            .convertEngineSharesToEarnShares(U256::from(min_vault_assets))
            .call()
            .await?;
        Ok(EarnLimits {
            min_vault_assets,
            min_earn_shares: minimum_output(u256_to_u128(
                expected_earn_shares,
                "Earn deposit preview",
            )?),
            min_output_amount: 0,
        })
    }

    async fn redeem_limits(
        &self,
        earn_shares: u128,
        output_token: Address,
    ) -> eyre::Result<EarnLimits> {
        let expected_vault_assets = EarnVault::new(self.earn_vault, self.l1.provider())
            .previewRedeem(U256::from(earn_shares))
            .call()
            .await?;
        let min_vault_assets =
            minimum_output(u256_to_u128(expected_vault_assets, "Earn redeem preview")?);
        let min_output_amount = if output_token == self.vault_asset {
            min_vault_assets
        } else {
            minimum_output(
                self.l1
                    .quote_dex_swap_exact_amount_in(
                        self.vault_asset,
                        output_token,
                        min_vault_assets,
                    )
                    .await?,
            )
        };
        Ok(EarnLimits {
            min_vault_assets,
            min_earn_shares: 0,
            min_output_amount,
        })
    }

    fn public_callback_data(
        &self,
        flow: EarnFlow,
        output_token: Address,
        recipient: Address,
        limits: EarnLimits,
    ) -> Bytes {
        EarnCallbackData {
            flow,
            earnVault: self.earn_vault,
            destination: EarnDestination::Public,
            outputToken: output_token,
            minVaultAssets: limits.min_vault_assets,
            minEarnShares: limits.min_earn_shares,
            minOutputAmount: limits.min_output_amount,
            actionId: keccak256((recipient, output_token).abi_encode()),
            destinationData: recipient.abi_encode().into(),
        }
        .abi_encode()
        .into()
    }

    async fn assert_router_empty(&self) -> eyre::Result<()> {
        for token in [self.vault_asset, self.alternate_asset, self.earn_share] {
            eyre::ensure!(
                self.l1.balance_of(token, self.router).await? == U256::ZERO,
                "UniversalEarnRouter retained token {token}"
            );
        }
        Ok(())
    }

    async fn enable_public_callback_destinations(&self) -> eyre::Result<()> {
        // Closed-access portals require every callback to append a return deposit. Earn's public
        // matrix destinations intentionally leave the Zone system, so mirror upstream's open Zone
        // environment for only those paths.
        let block = self
            .l1
            .set_access_mode_on_portal(self.portal, false)
            .await?;
        self.zone
            .wait_for_l2_tempo_finalized(block, E2E_TIMEOUT)
            .await?;
        Ok(())
    }

    async fn deposit_public_to_private(&self, recipient: Address) -> eyre::Result<u128> {
        let provider = self.user.l1_provider();
        let limits = self.deposit_limits(self.vault_asset, AMOUNT).await?;
        let private_before = self.zone.balance_of(self.earn_share, recipient).await?;
        let public_before = self
            .l1
            .balance_of(self.earn_share, self.user.address())
            .await?;
        let receipt = ITIP20::new(self.vault_asset, provider)
            .approve(self.router, U256::from(AMOUNT))
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "public deposit router approval failed");

        let (key_index, encrypted) = self
            .l1
            .encrypt_deposit_for_portal(self.portal, recipient, B256::ZERO)
            .await?;
        let delivery = EarnZoneDelivery {
            portal: self.portal,
            keyIndex: key_index,
            encrypted: map_encrypted_payload(encrypted),
            refundRecipient: self.user.address(),
        };
        let receipt = UniversalEarnRouter::new(self.router, provider)
            .depositToZone(
                self.earn_vault,
                U256::from(AMOUNT),
                U256::from(limits.min_earn_shares),
                delivery,
            )
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "public-to-private Earn deposit failed");

        let private_after = self
            .zone
            .wait_for_balance(
                self.earn_share,
                recipient,
                private_before + U256::from(1),
                E2E_TIMEOUT,
            )
            .await?;
        assert_eq!(
            self.l1
                .balance_of(self.earn_share, self.user.address())
                .await?,
            public_before,
            "public-to-private deposit left EarnShare on the caller"
        );
        self.assert_router_empty().await?;
        u256_to_u128(
            private_after - private_before,
            "EarnShare returned by public-to-private deposit",
        )
    }

    async fn deposit_private_to_public(&mut self, recipient: Address) -> eyre::Result<u128> {
        self.enable_public_callback_destinations().await?;
        self.encrypted_entry(self.vault_asset, AMOUNT).await?;
        let before = self.l1.balance_of(self.earn_share, recipient).await?;
        let limits = self.deposit_limits(self.vault_asset, AMOUNT).await?;
        let data = self.public_callback_data(EarnFlow::Deposit, self.earn_share, recipient, limits);
        self.user
            .withdraw_token_with(
                self.vault_asset,
                WithdrawalArgs {
                    amount: AMOUNT,
                    to: Some(self.router),
                    memo: B256::ZERO,
                    gas_limit: CALLBACK_GAS_LIMIT,
                    zone_fallback_recipient: Some(self.user.address()),
                    data,
                    reveal_to: Bytes::new(),
                },
            )
            .await?;
        let after = self
            .l1
            .wait_for_balance(
                self.earn_share,
                recipient,
                before + U256::from(1),
                E2E_TIMEOUT,
            )
            .await?;
        self.assert_router_empty().await?;
        u256_to_u128(
            after - before,
            "EarnShare returned by private-to-public deposit",
        )
    }

    async fn mint_public_earn(&self) -> eyre::Result<u128> {
        let provider = self.user.l1_provider();
        let limits = self.deposit_limits(self.vault_asset, AMOUNT).await?;
        let receipt = ITIP20::new(self.vault_asset, provider)
            .approve(self.router, U256::from(AMOUNT))
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "public Earn deposit approval failed");
        let before = self
            .l1
            .balance_of(self.earn_share, self.user.address())
            .await?;
        let receipt = UniversalEarnRouter::new(self.router, provider)
            .deposit(
                self.earn_vault,
                U256::from(AMOUNT),
                U256::from(limits.min_earn_shares),
                self.user.address(),
            )
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "public Earn deposit failed");
        let after = self
            .l1
            .balance_of(self.earn_share, self.user.address())
            .await?;
        self.assert_router_empty().await?;
        u256_to_u128(after - before, "public Earn deposit")
    }

    async fn redeem_public_to_private(&self, recipient: Address) -> eyre::Result<u128> {
        let earn_shares = self.mint_public_earn().await?;
        let limits = self.redeem_limits(earn_shares, self.vault_asset).await?;
        let provider = self.user.l1_provider();
        let receipt = ITIP20::new(self.earn_share, provider)
            .approve(self.router, U256::from(earn_shares))
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "public Earn redeem approval failed");

        let private_before = self.zone.balance_of(self.vault_asset, recipient).await?;
        let (key_index, encrypted) = self
            .l1
            .encrypt_deposit_for_portal(self.portal, recipient, B256::ZERO)
            .await?;
        let delivery = EarnZoneDelivery {
            portal: self.portal,
            keyIndex: key_index,
            encrypted: map_encrypted_payload(encrypted),
            refundRecipient: self.user.address(),
        };
        let receipt = UniversalEarnRouter::new(self.router, provider)
            .redeemToZone(
                self.earn_vault,
                U256::from(earn_shares),
                U256::from(limits.min_vault_assets),
                delivery,
            )
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "public-to-private Earn redeem failed");
        let private_after = self
            .zone
            .wait_for_balance(
                self.vault_asset,
                recipient,
                private_before + U256::from(1),
                E2E_TIMEOUT,
            )
            .await?;
        self.assert_router_empty().await?;
        u256_to_u128(
            private_after - private_before,
            "assets returned by public-to-private redeem",
        )
    }

    async fn redeem_private_to_public(&mut self, recipient: Address) -> eyre::Result<u128> {
        let user = self.user.address();
        let earn_shares = self.zone_deposit(self.vault_asset, user).await?;
        self.enable_public_callback_destinations().await?;
        let before = self.l1.balance_of(self.vault_asset, recipient).await?;
        let limits = self.redeem_limits(earn_shares, self.vault_asset).await?;
        let data = self.public_callback_data(EarnFlow::Redeem, self.vault_asset, recipient, limits);
        self.user
            .withdraw_token_with(
                self.earn_share,
                WithdrawalArgs {
                    amount: earn_shares,
                    to: Some(self.router),
                    memo: B256::ZERO,
                    gas_limit: CALLBACK_GAS_LIMIT,
                    zone_fallback_recipient: Some(user),
                    data,
                    reveal_to: Bytes::new(),
                },
            )
            .await?;
        let after = self
            .l1
            .wait_for_balance(
                self.vault_asset,
                recipient,
                before + U256::from(1),
                E2E_TIMEOUT,
            )
            .await?;
        self.assert_router_empty().await?;
        u256_to_u128(
            after - before,
            "assets returned by private-to-public redeem",
        )
    }

    async fn assert_private_return(
        &self,
        token: Address,
        recipient: Address,
        public_recipient_before: U256,
        portal_before: U256,
        private_delta: U256,
        description: &str,
    ) -> eyre::Result<()> {
        let public_recipient_after = self.l1.balance_of(token, recipient).await?;
        eyre::ensure!(
            public_recipient_after == public_recipient_before,
            "{description} leaked to the recipient's public L1 balance: before={public_recipient_before}, after={public_recipient_after}"
        );

        let portal_after = self.l1.balance_of(token, self.portal).await?;
        eyre::ensure!(
            portal_after >= portal_before,
            "{description} reduced the portal escrow balance: before={portal_before}, after={portal_after}"
        );
        eyre::ensure!(
            portal_after - portal_before == private_delta,
            "{description} did not conserve the encrypted return: private={private_delta}, L1 escrow={}",
            portal_after - portal_before
        );
        eyre::ensure!(
            self.l1.balance_of(token, self.router).await? == U256::ZERO,
            "{description} left tokens on the closed-loop router"
        );
        Ok(())
    }

    async fn assert_router_event_privacy(
        &self,
        withdrawal_token: Address,
        withdrawal_amount: u128,
        flow: EarnFlow,
        private_user: Address,
    ) -> eyre::Result<()> {
        let portal = ZonePortal::new(self.portal, self.l1.provider());
        let events = portal
            .WithdrawalProcessed_filter()
            .from_block(0)
            .query()
            .await?;
        let signature = match flow {
            EarnFlow::Deposit => {
                keccak256("EarnDeposit(bytes32,address,address,uint256,uint256,uint256,bytes32)")
            }
            EarnFlow::Redeem => {
                keccak256("EarnRedeem(bytes32,address,address,uint256,uint256,uint256,bytes32)")
            }
            _ => unreachable!("EarnFlow contains only synchronous deposit and redeem"),
        };
        let private_user_topic = B256::left_padding_from(private_user.as_slice());
        let filter = Filter::new()
            .address(self.router)
            .from_block(0)
            .event_signature(signature);
        let router_logs = self.l1.provider().get_logs(&filter).await?;
        let router_log = router_logs
            .last()
            .ok_or_else(|| eyre::eyre!("Earn router event not found"))?;
        let transaction_hash = router_log
            .transaction_hash
            .ok_or_else(|| eyre::eyre!("Earn router log has no transaction hash"))?;
        let (processed, _) = events
            .iter()
            .find(|(event, log)| {
                log.transaction_hash == Some(transaction_hash)
                    && event.to == self.router
                    && event.token == withdrawal_token
                    && event.amount == withdrawal_amount
                    && event.callbackSuccess
            })
            .ok_or_else(|| {
                eyre::eyre!("matching Earn WithdrawalProcessed event not found for router event")
            })?;
        eyre::ensure!(
            router_log.inner.topics().len() == 4,
            "Earn router event has an unexpected indexed-field layout"
        );
        eyre::ensure!(
            router_log
                .inner
                .topics()
                .iter()
                .all(|topic| *topic != processed.senderTag && *topic != private_user_topic),
            "Earn router event exposed the withdrawal sender tag or private user address"
        );
        Ok(())
    }

    async fn zone_deposit(
        &mut self,
        input_token: Address,
        recipient: Address,
    ) -> eyre::Result<u128> {
        self.encrypted_entry(input_token, AMOUNT).await?;
        let before = self.zone.balance_of(self.earn_share, recipient).await?;
        let public_recipient_before = self.l1.balance_of(self.earn_share, recipient).await?;
        let portal_before = self.l1.balance_of(self.earn_share, self.portal).await?;
        let limits = self.deposit_limits(input_token, AMOUNT).await?;
        let data = self
            .callback_data(
                EarnFlow::Deposit,
                self.earn_share,
                recipient,
                self.user.address(),
                limits,
            )
            .await?;
        self.user
            .withdraw_token_with(
                input_token,
                WithdrawalArgs {
                    amount: AMOUNT,
                    to: Some(self.router),
                    memo: B256::ZERO,
                    gas_limit: CALLBACK_GAS_LIMIT,
                    zone_fallback_recipient: Some(self.user.address()),
                    data,
                    reveal_to: Bytes::new(),
                },
            )
            .await?;

        let callback_success = self
            .l1
            .wait_for_withdrawal_processed_status(
                self.portal,
                self.router,
                input_token,
                AMOUNT,
                E2E_TIMEOUT,
            )
            .await?;
        eyre::ensure!(callback_success, "Earn deposit callback failed");
        let after = self
            .zone
            .wait_for_balance(
                self.earn_share,
                recipient,
                before + U256::from(1),
                E2E_TIMEOUT,
            )
            .await?;
        let minted = after - before;
        self.assert_private_return(
            self.earn_share,
            recipient,
            public_recipient_before,
            portal_before,
            minted,
            "Earn deposit",
        )
        .await?;
        self.assert_router_event_privacy(
            input_token,
            AMOUNT,
            EarnFlow::Deposit,
            self.user.address(),
        )
        .await?;
        u256_to_u128(minted, "EarnShare minted by Zone deposit")
    }

    async fn zone_deposit_expect_callback_bounce(
        &mut self,
        input_token: Address,
        recipient: Address,
        limits: EarnLimits,
    ) -> eyre::Result<()> {
        self.encrypted_entry(input_token, AMOUNT).await?;
        let private_input_before = self
            .zone
            .balance_of(input_token, self.user.address())
            .await?;
        let recipient_earn_before = self.zone.balance_of(self.earn_share, recipient).await?;
        let share_supply_before = EarnShare::new(self.earn_share, self.l1.provider())
            .totalSupply()
            .call()
            .await?;
        let backing_before = VenueVault::new(self.venue_vault, self.l1.provider())
            .balanceOf(self.engine)
            .call()
            .await?;
        let data = self
            .callback_data(
                EarnFlow::Deposit,
                self.earn_share,
                recipient,
                self.user.address(),
                limits,
            )
            .await?;
        let callback_gas_limit = CALLBACK_GAS_LIMIT;
        self.user
            .withdraw_token_with(
                input_token,
                WithdrawalArgs {
                    amount: AMOUNT,
                    to: Some(self.router),
                    memo: B256::ZERO,
                    gas_limit: callback_gas_limit,
                    zone_fallback_recipient: Some(self.user.address()),
                    data,
                    reveal_to: Bytes::new(),
                },
            )
            .await?;

        let private_input_after = self
            .zone
            .wait_for_balance(
                input_token,
                self.user.address(),
                private_input_before,
                BOUNCE_TIMEOUT,
            )
            .await?;
        assert_eq!(
            private_input_after, private_input_before,
            "failed Earn callback did not restore the private input"
        );
        assert_eq!(
            self.zone.balance_of(self.earn_share, recipient).await?,
            recipient_earn_before,
            "failed Earn callback credited private EarnShare"
        );
        assert_eq!(
            EarnShare::new(self.earn_share, self.l1.provider())
                .totalSupply()
                .call()
                .await?,
            share_supply_before,
            "failed Earn callback changed share supply"
        );
        assert_eq!(
            VenueVault::new(self.venue_vault, self.l1.provider())
                .balanceOf(self.engine)
                .call()
                .await?,
            backing_before,
            "failed Earn callback changed engine backing"
        );
        self.l1
            .assert_withdrawal_processed_with_status(
                self.portal,
                self.router,
                input_token,
                AMOUNT,
                false,
            )
            .await?;
        assert_eq!(
            self.l1.balance_of(input_token, self.router).await?,
            U256::ZERO,
            "failed Earn callback left input on the router"
        );
        assert_eq!(
            self.l1.balance_of(self.earn_share, self.router).await?,
            U256::ZERO,
            "failed Earn callback left shares on the router"
        );
        Ok(())
    }

    async fn zone_deposit_expect_recipient_bounce(
        &mut self,
        input_token: Address,
        recipient: Address,
    ) -> eyre::Result<()> {
        self.encrypted_entry(input_token, AMOUNT).await?;
        let recipient_earn_before = self.zone.balance_of(self.earn_share, recipient).await?;
        let public_refund_before = self
            .l1
            .balance_of(self.earn_share, self.user.address())
            .await?;
        let share_supply_before = EarnShare::new(self.earn_share, self.l1.provider())
            .totalSupply()
            .call()
            .await?;
        let backing_before = VenueVault::new(self.venue_vault, self.l1.provider())
            .balanceOf(self.engine)
            .call()
            .await?;
        let data = self
            .callback_data(
                EarnFlow::Deposit,
                self.earn_share,
                recipient,
                self.user.address(),
                EarnLimits::default(),
            )
            .await?;
        self.user
            .withdraw_token_with(
                input_token,
                WithdrawalArgs {
                    amount: AMOUNT,
                    to: Some(self.router),
                    memo: B256::ZERO,
                    gas_limit: CALLBACK_GAS_LIMIT,
                    zone_fallback_recipient: Some(self.user.address()),
                    data,
                    reveal_to: Bytes::new(),
                },
            )
            .await?;

        let public_refund_after = self
            .l1
            .wait_for_balance(
                self.earn_share,
                self.user.address(),
                public_refund_before + U256::from(1),
                BOUNCE_TIMEOUT,
            )
            .await?;
        let share_supply_after = EarnShare::new(self.earn_share, self.l1.provider())
            .totalSupply()
            .call()
            .await?;
        let backing_after = VenueVault::new(self.venue_vault, self.l1.provider())
            .balanceOf(self.engine)
            .call()
            .await?;
        let minted = share_supply_after - share_supply_before;
        let refunded = public_refund_after - public_refund_before;
        assert_eq!(
            self.zone.balance_of(self.earn_share, recipient).await?,
            recipient_earn_before,
            "ineligible recipient received private EarnShare"
        );
        assert!(
            refunded > U256::ZERO && refunded <= minted,
            "invalid ineligible-recipient refund: refunded={refunded}, minted={minted}"
        );
        assert!(minted > U256::ZERO, "recipient bounce minted no EarnShare");
        assert!(
            backing_after > backing_before,
            "successful router deposit added no engine backing"
        );
        self.l1
            .assert_withdrawal_processed_with_status(
                self.portal,
                self.router,
                input_token,
                AMOUNT,
                true,
            )
            .await?;
        self.assert_router_event_privacy(
            input_token,
            AMOUNT,
            EarnFlow::Deposit,
            self.user.address(),
        )
        .await?;
        assert_eq!(
            self.l1.balance_of(self.earn_share, self.router).await?,
            U256::ZERO,
            "recipient bounce left shares on the router"
        );
        Ok(())
    }

    async fn zone_redeem(
        &self,
        shares: u128,
        output_token: Address,
        recipient: Address,
    ) -> eyre::Result<u128> {
        let mut user = ZoneAccount::from_l1_and_zone(&self.l1, &self.zone, self.portal);
        self.zone_redeem_as(&mut user, shares, output_token, recipient)
            .await
    }

    async fn zone_redeem_as(
        &self,
        account: &mut ZoneAccount,
        shares: u128,
        output_token: Address,
        recipient: Address,
    ) -> eyre::Result<u128> {
        let before = self.zone.balance_of(output_token, recipient).await?;
        let public_recipient_before = self.l1.balance_of(output_token, recipient).await?;
        let portal_before = self.l1.balance_of(output_token, self.portal).await?;
        let limits = self.redeem_limits(shares, output_token).await?;
        let data = self
            .callback_data(
                EarnFlow::Redeem,
                output_token,
                recipient,
                account.address(),
                limits,
            )
            .await?;
        account
            .withdraw_token_with(
                self.earn_share,
                WithdrawalArgs {
                    amount: shares,
                    to: Some(self.router),
                    memo: B256::ZERO,
                    gas_limit: CALLBACK_GAS_LIMIT,
                    zone_fallback_recipient: Some(account.address()),
                    data,
                    reveal_to: Bytes::new(),
                },
            )
            .await?;

        let after = self
            .zone
            .wait_for_balance(output_token, recipient, before + U256::from(1), E2E_TIMEOUT)
            .await?;
        let returned = after - before;
        self.assert_private_return(
            output_token,
            recipient,
            public_recipient_before,
            portal_before,
            returned,
            "Earn redemption",
        )
        .await?;
        self.l1
            .assert_withdrawal_processed_with_status(
                self.portal,
                self.router,
                self.earn_share,
                shares,
                true,
            )
            .await?;
        self.assert_router_event_privacy(
            self.earn_share,
            shares,
            EarnFlow::Redeem,
            account.address(),
        )
        .await?;
        u256_to_u128(returned, "assets returned by Zone redemption")
    }

    async fn migrate_existing_vault_shares(&mut self) -> eyre::Result<u128> {
        let user = self.user.address();
        let provider = self.user.l1_provider();
        let asset = ITIP20::new(self.vault_asset, provider);
        let receipt = asset
            .approve(self.venue_vault, U256::from(AMOUNT))
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "vault asset approval failed");

        let vault = VenueVault::new(self.venue_vault, provider);
        let venue_before = vault.balanceOf(user).call().await?;
        let receipt = vault
            .deposit(U256::from(AMOUNT), user)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "creating existing venue shares failed");
        let venue_after = vault.balanceOf(user).call().await?;
        let venue_shares = venue_after - venue_before;
        eyre::ensure!(
            venue_shares > U256::ZERO,
            "migration setup minted no venue shares"
        );

        let earn_vault = EarnVault::new(self.earn_vault, self.l1.provider());
        let earn_shares = earn_vault
            .convertEngineSharesToEarnShares(venue_shares)
            .call()
            .await?;
        let earn_shares_u128 = u256_to_u128(earn_shares, "migration EarnShare quote")?;
        eyre::ensure!(
            earn_shares_u128 > 0,
            "migration quote returned no EarnShare"
        );

        let (key_index, encrypted) = self
            .l1
            .encrypt_deposit_for_portal(self.portal, user, B256::ZERO)
            .await?;
        let private_before = self.zone.balance_of(self.earn_share, user).await?;
        let public_before = EarnShare::new(self.earn_share, self.l1.provider())
            .balanceOf(user)
            .call()
            .await?;
        let supply_before = EarnShare::new(self.earn_share, self.l1.provider())
            .totalSupply()
            .call()
            .await?;
        let engine_venue_before = VenueVault::new(self.venue_vault, self.l1.provider())
            .balanceOf(self.engine)
            .call()
            .await?;

        let calls = vec![
            Call {
                to: TxKind::Call(self.venue_vault),
                value: U256::ZERO,
                input: VenueVault::approveCall {
                    spender: self.engine,
                    amount: venue_shares,
                }
                .abi_encode()
                .into(),
            },
            Call {
                to: TxKind::Call(self.earn_vault),
                value: U256::ZERO,
                input: EarnVault::depositVenueSharesCall {
                    venueShares: venue_shares,
                    receiver: user,
                    minEarnShares: earn_shares,
                }
                .abi_encode()
                .into(),
            },
            Call {
                to: TxKind::Call(self.earn_share),
                value: U256::ZERO,
                input: EarnShare::approveCall {
                    spender: self.portal,
                    amount: earn_shares,
                }
                .abi_encode()
                .into(),
            },
            Call {
                to: TxKind::Call(self.portal),
                value: U256::ZERO,
                input: ZonePortal::depositEncryptedCall {
                    token: self.earn_share,
                    amount: earn_shares_u128,
                    keyIndex: key_index,
                    encrypted,
                    tempoRefundRecipient: user,
                }
                .abi_encode()
                .into(),
            },
        ];
        let tempo_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .wallet(self.l1.user_signer())
            .connect_http(self.l1.http_url().clone());
        let request = TempoTransactionRequest {
            inner: TransactionRequest {
                from: Some(user),
                gas: Some(MIGRATION_TX_GAS_LIMIT),
                ..Default::default()
            },
            calls,
            fee_token: Some(PATH_USD_ADDRESS),
            ..Default::default()
        };
        let receipt = tempo_provider
            .send_transaction(request)
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "atomic venue-share migration failed");
        ensure_gas_headroom(
            receipt.gas_used,
            MIGRATION_TX_GAS_LIMIT,
            "atomic venue-share migration",
        )?;

        let private_after = self
            .zone
            .wait_for_balance(
                self.earn_share,
                user,
                private_before + earn_shares,
                E2E_TIMEOUT,
            )
            .await?;
        let user_venue_after = VenueVault::new(self.venue_vault, self.l1.provider())
            .balanceOf(user)
            .call()
            .await?;
        let engine_venue_after = VenueVault::new(self.venue_vault, self.l1.provider())
            .balanceOf(self.engine)
            .call()
            .await?;
        let public_after = EarnShare::new(self.earn_share, self.l1.provider())
            .balanceOf(user)
            .call()
            .await?;
        let supply_after = EarnShare::new(self.earn_share, self.l1.provider())
            .totalSupply()
            .call()
            .await?;

        assert_eq!(venue_after - user_venue_after, venue_shares);
        assert_eq!(engine_venue_after - engine_venue_before, venue_shares);
        assert_eq!(
            public_after, public_before,
            "migrated EarnShare remained public"
        );
        assert_eq!(supply_after - supply_before, earn_shares);
        assert_eq!(private_after - private_before, earn_shares);

        Ok(earn_shares_u128)
    }
}

async fn deploy_contract(
    l1: &L1TestNode,
    contract: &str,
    constructor_args: Vec<u8>,
) -> eyre::Result<Address> {
    let mut deployment = forge_bytecode(contract)?.to_vec();
    deployment.extend_from_slice(&constructor_args);
    let mut request = TransactionRequest::default().input(Bytes::from(deployment).into());
    request.to = Some(TxKind::Create);
    // Contract-to-contract constructor calls are valid on Tempo but can be under-estimated by the
    // generic Ethereum gas estimator. Earn's local deployer likewise applies explicit headroom.
    request.gas = Some(CONTRACT_DEPLOYMENT_TX_GAS_LIMIT);
    let receipt = l1
        .dev_provider()
        .send_transaction(request)
        .await
        .wrap_err_with(|| format!("submitting {contract} deployment"))?
        .get_receipt()
        .await
        .wrap_err_with(|| format!("waiting for {contract} deployment receipt"))?;
    eyre::ensure!(
        receipt.status(),
        "{contract} deployment failed after using {} gas",
        receipt.gas_used
    );
    ensure_gas_headroom(
        receipt.gas_used,
        CONTRACT_DEPLOYMENT_TX_GAS_LIMIT,
        &format!("{contract} deployment"),
    )?;
    receipt
        .contract_address
        .ok_or_else(|| eyre::eyre!("{contract} deployment returned no contract address"))
}

fn map_encrypted_payload(payload: EncryptedDepositPayload) -> EarnEncryptedDepositPayload {
    EarnEncryptedDepositPayload {
        ephemeralPubkeyX: payload.ephemeralPubkeyX,
        ephemeralPubkeyYParity: payload.ephemeralPubkeyYParity,
        ciphertext: payload.ciphertext,
        nonce: payload.nonce,
        tag: payload.tag,
    }
}

fn u256_to_u128(value: U256, description: &str) -> eyre::Result<u128> {
    value
        .try_into()
        .map_err(|_| eyre::eyre!("{description} does not fit into uint128: {value}"))
}

fn minimum_output(expected: u128) -> u128 {
    expected - expected * 50 / 10_000
}

fn ensure_gas_headroom(gas_used: u64, gas_limit: u64, operation: &str) -> eyre::Result<()> {
    let minimum_headroom = gas_limit * MIN_GAS_HEADROOM_PERCENT / 100;
    eyre::ensure!(
        gas_used <= gas_limit - minimum_headroom,
        "{operation} used {gas_used} of {gas_limit} gas, leaving less than {MIN_GAS_HEADROOM_PERCENT}% headroom"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zone_deposit_direct() -> eyre::Result<()> {
    let mut fixture = EarnZoneFixture::start().await?;
    let user = fixture.user.address();
    let shares = fixture.zone_deposit(fixture.vault_asset, user).await?;
    assert_eq!(shares, AMOUNT, "direct Zone deposit was not 1:1");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zone_deposit_third_party_recipient() -> eyre::Result<()> {
    let mut fixture = EarnZoneFixture::start().await?;
    let recipient = fixture.l1.signer_at(3).address();
    let shares = fixture.zone_deposit(fixture.vault_asset, recipient).await?;
    assert_eq!(shares, AMOUNT, "third-party Zone deposit was not 1:1");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zone_redeem_direct() -> eyre::Result<()> {
    let mut fixture = EarnZoneFixture::start().await?;
    let user = fixture.user.address();
    let shares = fixture.zone_deposit(fixture.vault_asset, user).await?;
    let output = fixture
        .zone_redeem(shares, fixture.vault_asset, user)
        .await?;
    assert_eq!(output, AMOUNT, "direct Zone lifecycle was not 1:1");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zone_redeem_third_party_recipient() -> eyre::Result<()> {
    let mut fixture = EarnZoneFixture::start().await?;
    let user = fixture.user.address();
    let recipient = fixture.l1.signer_at(3).address();
    let shares = fixture.zone_deposit(fixture.vault_asset, user).await?;
    let output = fixture
        .zone_redeem(shares, fixture.vault_asset, recipient)
        .await?;
    assert_eq!(output, AMOUNT, "third-party Zone lifecycle was not 1:1");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zone_lifecycle_swapped() -> eyre::Result<()> {
    let mut fixture = EarnZoneFixture::start().await?;
    let user = fixture.user.address();
    let shares = fixture.zone_deposit(fixture.alternate_asset, user).await?;
    assert_eq!(shares, AMOUNT, "swapped Zone deposit was not 1:1");
    let output = fixture
        .zone_redeem(shares, fixture.alternate_asset, user)
        .await?;
    assert_eq!(output, AMOUNT, "swapped Zone lifecycle was not 1:1");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zone_swap_slippage_bounces() -> eyre::Result<()> {
    let mut fixture = EarnZoneFixture::start().await?;
    let user = fixture.user.address();

    fixture
        .zone_deposit_expect_callback_bounce(
            fixture.alternate_asset,
            user,
            EarnLimits {
                min_vault_assets: u128::MAX,
                ..EarnLimits::default()
            },
        )
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn controls_pause_blocks_entry_but_preserves_exits() -> eyre::Result<()> {
    let mut fixture = EarnZoneFixture::start().await?;
    let user = fixture.user.address();
    let shares = fixture.zone_deposit(fixture.vault_asset, user).await?;
    fixture.set_deposits_paused(true).await?;
    fixture
        .zone_deposit_expect_callback_bounce(fixture.vault_asset, user, EarnLimits::default())
        .await?;
    let output = fixture
        .zone_redeem(shares, fixture.vault_asset, user)
        .await?;
    assert_eq!(output, AMOUNT, "deposit pause blocked a private exit");
    fixture.set_deposits_paused(false).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zone_ineligible_private_deposit_bounces() -> eyre::Result<()> {
    let mut fixture = EarnZoneFixture::start_protected().await?;
    let outsider = fixture.l1.signer_at(3).address();
    fixture.allow_zone_account(outsider).await?;
    fixture.set_access_eligibility(outsider, false).await?;
    fixture
        .zone_deposit_expect_recipient_bounce(fixture.vault_asset, outsider)
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn zone_ineligible_private_transfer_blocked() -> eyre::Result<()> {
    let mut fixture = EarnZoneFixture::start_protected().await?;
    let user = fixture.user.address();
    let outsider = fixture.l1.signer_at(3).address();
    fixture.allow_zone_account(outsider).await?;
    fixture.set_access_eligibility(outsider, false).await?;
    let user_shares = fixture.zone_deposit(fixture.vault_asset, user).await?;
    let user_before = fixture.zone.balance_of(fixture.earn_share, user).await?;
    let outsider_before = fixture
        .zone
        .balance_of(fixture.earn_share, outsider)
        .await?;
    let user_provider = ProviderBuilder::new()
        .wallet(fixture.l1.user_signer())
        .connect_http(fixture.zone.http_url().clone());
    let transfer = ITIP20::new(fixture.earn_share, &user_provider)
        .transfer(outsider, U256::from(1))
        .from(user)
        .call()
        .await;
    eyre::ensure!(
        transfer.is_err(),
        "private EarnShare transfer to an ineligible recipient unexpectedly succeeded"
    );
    assert_eq!(
        fixture.zone.balance_of(fixture.earn_share, user).await?,
        user_before,
        "rejected private transfer changed the sender balance"
    );
    assert_eq!(
        fixture
            .zone
            .balance_of(fixture.earn_share, outsider)
            .await?,
        outsider_before,
        "rejected private transfer changed the recipient balance"
    );
    let output = fixture
        .zone_redeem(user_shares, fixture.vault_asset, user)
        .await?;
    assert_eq!(output, AMOUNT, "eligible holder cleanup was not 1:1");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zone_removed_private_holder_can_exit() -> eyre::Result<()> {
    let mut fixture = EarnZoneFixture::start_protected().await?;
    let outsider_signer = fixture.l1.signer_at(3);
    let outsider = outsider_signer.address();
    fixture.l1.fund_user(outsider, PUBLIC_USER_BALANCE).await?;
    fixture.allow_zone_account(outsider).await?;
    let mut outsider_account =
        ZoneAccount::with_signer(outsider_signer, &fixture.l1, &fixture.zone, fixture.portal);
    outsider_account
        .deposit(PRIVATE_FEE_BALANCE, E2E_TIMEOUT, &fixture.zone)
        .await?;
    fixture.set_access_eligibility(outsider, true).await?;
    let outsider_shares = fixture.zone_deposit(fixture.vault_asset, outsider).await?;
    assert!(
        outsider_shares > 0,
        "eligible private outsider received no EarnShare"
    );
    fixture.set_access_eligibility(outsider, false).await?;
    let outsider_output = fixture
        .zone_redeem_as(
            &mut outsider_account,
            outsider_shares,
            fixture.vault_asset,
            outsider,
        )
        .await?;
    assert_eq!(
        outsider_output, AMOUNT,
        "removed private holder did not exit 1:1"
    );
    assert_eq!(
        fixture
            .zone
            .balance_of(fixture.earn_share, outsider)
            .await?,
        U256::ZERO,
        "removed private holder retained EarnShare after exit"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zone_rewards_private_holder() -> eyre::Result<()> {
    let mut fixture = EarnZoneFixture::start().await?;
    let user = fixture.user.address();
    let shares = fixture.zone_deposit(fixture.vault_asset, user).await?;
    assert!(shares > 1, "private reward setup minted too few shares");

    let private_after_entry = fixture.zone.balance_of(fixture.earn_share, user).await?;
    let value_before = EarnVault::new(fixture.earn_vault, fixture.l1.provider())
        .previewRedeem(U256::from(shares))
        .call()
        .await?;
    let user_assets_before = fixture.l1.balance_of(fixture.vault_asset, user).await?;
    let vault_assets_before = VenueVault::new(fixture.venue_vault, fixture.l1.provider())
        .totalAssets()
        .call()
        .await?;

    let provider = fixture.user.l1_provider();
    let receipt = ITIP20::new(fixture.vault_asset, provider)
        .approve(fixture.contribution_controller, U256::from(REWARD_AMOUNT))
        .send()
        .await?
        .get_receipt()
        .await?;
    eyre::ensure!(receipt.status(), "reward asset approval failed");
    let rewards = EarnContributionController::new(fixture.contribution_controller, provider);
    let max_earn_share_supply = EarnVault::new(fixture.earn_vault, fixture.l1.provider())
        .totalEarnShares()
        .call()
        .await?;
    let simulated_funding = rewards
        .fund(user, U256::from(REWARD_AMOUNT), max_earn_share_supply)
        .gas(REWARD_FUNDING_TX_GAS_LIMIT)
        .call()
        .await?;
    if simulated_funding != U256::from(REWARD_AMOUNT) {
        // `fund` intentionally catches provider failures. Re-run its isolated leg as the rewards
        // contract so a failing E2E reports the underlying revert instead of only "funded = 0".
        EarnContributionController::new(fixture.contribution_controller, fixture.l1.provider())
            .executeFunding(user, U256::from(REWARD_AMOUNT), max_earn_share_supply)
            .from(fixture.contribution_controller)
            .call()
            .await
            .wrap_err("diagnosing zero Earn contribution funding")?;
        eyre::bail!("Earn contribution simulated zero funding without an underlying revert");
    }
    let receipt = rewards
        .fund(user, U256::from(REWARD_AMOUNT), max_earn_share_supply)
        .gas(REWARD_FUNDING_TX_GAS_LIMIT)
        .send()
        .await?
        .get_receipt()
        .await?;
    eyre::ensure!(receipt.status(), "funding private-holder rewards failed");
    ensure_gas_headroom(
        receipt.gas_used,
        REWARD_FUNDING_TX_GAS_LIMIT,
        "private-holder reward funding",
    )?;

    let earn_vault = EarnVault::new(fixture.earn_vault, fixture.l1.provider());
    let value_after = earn_vault.previewRedeem(U256::from(shares)).call().await?;
    let user_assets_after = fixture.l1.balance_of(fixture.vault_asset, user).await?;
    let vault_assets_after = VenueVault::new(fixture.venue_vault, fixture.l1.provider())
        .totalAssets()
        .call()
        .await?;
    let anchor_engine_shares = earn_vault.anchorEngineShares().call().await?;
    let anchor_supply = earn_vault.anchorEarnShares().call().await?;
    assert_eq!(
        user_assets_before - user_assets_after,
        U256::from(REWARD_AMOUNT),
        "EarnContributionController did not pull the requested provider assets"
    );
    assert_eq!(
        vault_assets_after - vault_assets_before,
        U256::from(REWARD_AMOUNT),
        "EarnContributionController did not invest the requested provider assets"
    );
    assert!(
        value_after > value_before,
        "private EarnShare received no reward value: before={value_before}, after={value_after}, anchorEngineShares={anchor_engine_shares}, anchorEarnShares={anchor_supply}"
    );

    let first_shares = shares / 2;
    let second_shares = shares - first_shares;
    let first_output = fixture
        .zone_redeem(first_shares, fixture.vault_asset, user)
        .await?;
    assert!(
        first_output > 0,
        "partial private redemption returned no assets"
    );
    let private_after_partial = fixture.zone.balance_of(fixture.earn_share, user).await?;
    assert_eq!(
        private_after_partial,
        private_after_entry - U256::from(first_shares),
        "partial redemption changed the wrong private share amount"
    );

    let second_output = fixture
        .zone_redeem(second_shares, fixture.vault_asset, user)
        .await?;
    assert_eq!(
        first_output + second_output,
        AMOUNT + REWARD_AMOUNT,
        "private reward lifecycle did not return all principal and rewards"
    );
    assert_eq!(
        fixture.zone.balance_of(fixture.earn_share, user).await?,
        U256::ZERO,
        "private reward lifecycle left EarnShare behind"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn matrix_deposit_public_private_self() -> eyre::Result<()> {
    let fixture = EarnZoneFixture::start().await?;
    let amount = fixture
        .deposit_public_to_private(fixture.user.address())
        .await?;
    assert!(amount > 0, "public deposit produced no private EarnShare");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn matrix_deposit_public_private_third_party() -> eyre::Result<()> {
    let fixture = EarnZoneFixture::start().await?;
    let amount = fixture
        .deposit_public_to_private(fixture.l1.signer_at(3).address())
        .await?;
    assert!(amount > 0, "public deposit produced no private EarnShare");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn matrix_deposit_private_public_self() -> eyre::Result<()> {
    let mut fixture = EarnZoneFixture::start().await?;
    let amount = fixture
        .deposit_private_to_public(fixture.user.address())
        .await?;
    assert!(amount > 0, "private deposit produced no public EarnShare");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn matrix_deposit_private_public_third_party() -> eyre::Result<()> {
    let mut fixture = EarnZoneFixture::start().await?;
    let recipient = fixture.l1.signer_at(3).address();
    let amount = fixture.deposit_private_to_public(recipient).await?;
    assert!(amount > 0, "private deposit produced no public EarnShare");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn matrix_redeem_public_private_self() -> eyre::Result<()> {
    let fixture = EarnZoneFixture::start().await?;
    let amount = fixture
        .redeem_public_to_private(fixture.user.address())
        .await?;
    assert!(amount > 0, "public redeem produced no private assets");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn matrix_redeem_public_private_third_party() -> eyre::Result<()> {
    let fixture = EarnZoneFixture::start().await?;
    let amount = fixture
        .redeem_public_to_private(fixture.l1.signer_at(3).address())
        .await?;
    assert!(amount > 0, "public redeem produced no private assets");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn matrix_redeem_private_public_self() -> eyre::Result<()> {
    let mut fixture = EarnZoneFixture::start().await?;
    let amount = fixture
        .redeem_private_to_public(fixture.user.address())
        .await?;
    assert!(amount > 0, "private redeem produced no public assets");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn matrix_redeem_private_public_third_party() -> eyre::Result<()> {
    let mut fixture = EarnZoneFixture::start().await?;
    let recipient = fixture.l1.signer_at(3).address();
    let amount = fixture.redeem_private_to_public(recipient).await?;
    assert!(amount > 0, "private redeem produced no public assets");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn migration_existing_vault_shares_to_zone() -> eyre::Result<()> {
    let mut fixture = EarnZoneFixture::start().await?;
    let user = fixture.user.address();
    fixture.assert_engine_migration_guardrails().await?;
    let earn_shares = fixture.migrate_existing_vault_shares().await?;
    assert_eq!(earn_shares, AMOUNT, "venue-share migration was not 1:1");
    let output = fixture
        .zone_redeem(earn_shares, fixture.vault_asset, user)
        .await?;
    assert_eq!(output, AMOUNT, "migrated EarnShare did not redeem 1:1");
    Ok(())
}
