//! Tempo Earn scenarios that cross the public L1 / private Zone boundary.
//!
//! The Solidity fixtures under `specs/ref-impls/test/fixtures/earn` are copied from Tempo Earn. These
//! tests deliberately exercise the complete callback path: a private withdrawal settles on L1,
//! the Earn gateway deposits or redeems through the vault stack, and the output is encrypted back
//! into the originating Zone.

use crate::utils::{
    L1TestNode, STABLECOIN_DEX_ADDRESS, WithdrawalArgs, ZoneAccount, ZoneTestNode, forge_bytecode,
    spawn_sequencer,
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
use tempo_contracts::precompiles::{IRolesAuth, ITIP20, ITIP403Registry};
use tempo_precompiles::{PATH_USD_ADDRESS, TIP403_REGISTRY_ADDRESS, tip20::ISSUER_ROLE};
use tempo_primitives::transaction::Call;
use tempo_zone_contracts::{EncryptedDepositPayload, ZONE_OUTBOX_ADDRESS, ZonePortal};

const AMOUNT: u128 = 1_000_000;
const REWARD_AMOUNT: u128 = AMOUNT / 10;
const PUBLIC_USER_BALANCE: u128 = 50_000_000;
const PRIVATE_FEE_BALANCE: u128 = 10_000_000;
const DEX_LIQUIDITY: u128 = 300_000_000;
const TOKEN_SUPPLY: u128 = 1_000_000_000;
// Callback budgets include headroom above the measured inclusive gateway traces:
// direct deposit 3.22M, swapped deposit 5.12M, direct redeem 1.45M, swapped redeem 3.10M.
const DIRECT_DEPOSIT_CALLBACK_GAS_LIMIT: u64 = 5_000_000;
const SWAPPED_DEPOSIT_CALLBACK_GAS_LIMIT: u64 = 7_000_000;
const DIRECT_REDEEM_CALLBACK_GAS_LIMIT: u64 = 3_000_000;
const SWAPPED_REDEEM_CALLBACK_GAS_LIMIT: u64 = 5_000_000;
const REWARD_FUNDING_TX_GAS_LIMIT: u64 = 5_000_000;
const MIGRATION_TX_GAS_LIMIT: u64 = 10_000_000;
// The transaction cap is 30M. VaultAdapter currently deploys at ~24.86M, leaving 17.1% headroom.
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

    struct ControlInit {
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

    struct FeeInit {
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

    struct EarnCallbackData {
        EarnFlow flow;
        address outputToken;
        uint256 keyIndex;
        EarnEncryptedDepositPayload encrypted;
        uint128 minVaultAssets;
        uint128 minVaultShares;
        uint128 minOutputAmount;
        bytes32 actionId;
        address refundRecipient;
    }

    #[sol(rpc)]
    contract EarnZonePortalView {
        function messenger() external view returns (address);
    }

    #[sol(rpc)]
    contract EarnToken {
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
        function totalSupply() external view returns (uint256);
    }

    #[sol(rpc)]
    contract EarnVault {
        constructor(address asset_, string name_, string symbol_, uint8 decimals_);
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
        function deposit(uint256 assets, address receiver) external returns (uint256 shares);
        function totalAssets() external view returns (uint256);
    }

    #[sol(rpc)]
    contract EarnEngine {
        constructor(address vault_, address owner_, string nameOverride_, string symbolOverride_);
        function initializeCore(address core) external;
    }

    #[sol(rpc)]
    contract EarnProxy {
        constructor(address implementation, bytes initialization);
    }

    #[sol(rpc)]
    contract EarnVaultAdapter {
        function initialize(
            address engine_,
            address shareToken_,
            address operator_,
            ControlInit controlInit_,
            FeeInit feeInit_
        ) external;
        function depositShares(uint256 venueShares, address receiver, uint256 minEarnShares)
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
        function sharesToTokens(uint256 shares) external view returns (uint256 tokens);
        function shareSupply() external view returns (uint256);
        function anchorEngineShares() external view returns (uint256);
        function anchorSupply() external view returns (uint256);
    }

    #[sol(rpc)]
    contract EarnGateway {
        function setDepositRoute(address inputToken, address swapper) external;
        function setRedeemRoute(address outputToken, address swapper) external;
    }

    #[sol(rpc)]
    contract EarnRewards {
        function fund(address funder, uint256 requested) external returns (uint256 funded);
        function executeFunding(address funder, uint256 requested) external returns (uint256 funded);
    }
}

#[derive(Clone, Copy)]
struct EarnLimits {
    min_vault_assets: u128,
    min_vault_shares: u128,
    min_output_amount: u128,
}

impl Default for EarnLimits {
    fn default() -> Self {
        Self {
            min_vault_assets: 1,
            min_vault_shares: 1,
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
    vault: Address,
    engine: Address,
    share_token: Address,
    adapter: Address,
    gateway: Address,
    rewards: Address,
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

        let vault = deploy_contract(
            &l1,
            "Simple4626Vault",
            EarnVault::constructorCall {
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
                vault_: vault,
                owner_: owner,
                nameOverride_: String::new(),
                symbolOverride_: String::new(),
            }
            .abi_encode(),
        )
        .await?;
        let share_token = l1
            .create_tip20(
                "Tempo Earn Zone Share",
                "teZONE-E",
                B256::with_last_byte(0xE3),
            )
            .await?;
        let adapter_implementation = deploy_contract(&l1, "VaultAdapter", Vec::new()).await?;

        let fee_init = FeeInit {
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
        let initialization = EarnVaultAdapter::initializeCall {
            engine_: engine,
            shareToken_: share_token,
            operator_: owner,
            controlInit_: ControlInit {
                emergencyGuardian: Address::ZERO,
                asyncJanitor: Address::ZERO,
                migrationMode: EngineMigrationMode::OperatorEnabled,
            },
            feeInit_: fee_init,
        }
        .abi_encode();
        let adapter = deploy_contract(
            &l1,
            "TestERC1967Proxy",
            EarnProxy::constructorCall {
                implementation: adapter_implementation,
                initialization: Bytes::from(initialization),
            }
            .abi_encode(),
        )
        .await?;

        let provider = l1.dev_provider();
        let receipt = IRolesAuth::new(share_token, &provider)
            .grantRole(*ISSUER_ROLE, adapter)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "granting the adapter issuer role failed");

        let receipt = EarnEngine::new(engine, &provider)
            .initializeCore(adapter)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "initializing the Earn engine failed");

        let swapper = deploy_contract(
            &l1,
            "TempoStablecoinDexStableSwapAdapter",
            (STABLECOIN_DEX_ADDRESS,).abi_encode(),
        )
        .await?;
        let gateway = deploy_contract(
            &l1,
            "ZoneGateway",
            (adapter, swapper, portal, messenger, owner).abi_encode(),
        )
        .await?;
        let rewards =
            deploy_contract(&l1, "VaultRewards", (adapter, user_address).abi_encode()).await?;

        let access_policy = if protected {
            let whitelist_id = l1.create_whitelist_policy().await?;
            let outsider = l1.signer_at(3).address();
            for account in [
                adapter,
                gateway,
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
            l1.change_transfer_policy_id(share_token, compound_id)
                .await?;
            Some(EarnAccessPolicy {
                compound_id,
                whitelist_id,
            })
        } else {
            None
        };

        // The intervening deployments used fresh providers and advanced the dev-account nonce.
        let provider = l1.dev_provider();
        let gateway_contract = EarnGateway::new(gateway, &provider);
        let receipt = gateway_contract
            .setDepositRoute(alternate_asset, swapper)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(
            receipt.status(),
            "configuring the alternate deposit route failed"
        );
        let receipt = gateway_contract
            .setRedeemRoute(alternate_asset, swapper)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(
            receipt.status(),
            "configuring the alternate redeem route failed"
        );

        l1.enable_token_on_portal(portal, vault_asset).await?;
        l1.enable_token_on_portal(portal, alternate_asset).await?;
        l1.enable_token_on_portal(portal, share_token).await?;

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
            vault,
            engine,
            share_token,
            adapter,
            gateway,
            rewards,
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

    async fn set_deposits_paused(&self, paused: bool) -> eyre::Result<()> {
        let provider = self.l1.dev_provider();
        let adapter = EarnVaultAdapter::new(self.adapter, &provider);
        let receipt = adapter
            .setDepositsPaused(paused)
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "setting Earn deposit pause failed");
        eyre::ensure!(
            adapter.depositsPaused().call().await? == paused,
            "Earn deposit pause state did not update"
        );
        Ok(())
    }

    async fn assert_engine_migration_guardrails(&self) -> eyre::Result<()> {
        let adapter = EarnVaultAdapter::new(self.adapter, self.l1.provider());
        let engine_before = adapter.engine().call().await?;
        let engine_shares_before = adapter.engineShares().call().await?;
        let supply_before = adapter.shareSupply().call().await?;

        let unauthorized = EarnVaultAdapter::new(self.adapter, self.user.l1_provider())
            .migrateEngine(Address::ZERO, U256::from(1), U256::from(1))
            .from(self.user.address())
            .call()
            .await;
        eyre::ensure!(
            unauthorized.is_err(),
            "non-operator unexpectedly passed the engine migration guard"
        );

        let provider = self.l1.dev_provider();
        let operator_adapter = EarnVaultAdapter::new(self.adapter, &provider);
        eyre::ensure!(
            operator_adapter
                .migrateEngine(Address::ZERO, U256::from(1), U256::from(1))
                .call()
                .await
                .is_err(),
            "operator unexpectedly migrated to the zero engine"
        );
        eyre::ensure!(
            operator_adapter
                .migrateEngine(engine_before, U256::from(1), U256::from(1))
                .call()
                .await
                .is_err(),
            "operator unexpectedly migrated to the active engine"
        );

        assert_eq!(adapter.engine().call().await?, engine_before);
        assert_eq!(adapter.engineShares().call().await?, engine_shares_before);
        assert_eq!(adapter.shareSupply().call().await?, supply_before);
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
        Ok(EarnCallbackData {
            flow,
            outputToken: output_token,
            keyIndex: key_index,
            encrypted: map_encrypted_payload(encrypted),
            minVaultAssets: limits.min_vault_assets,
            minVaultShares: limits.min_vault_shares,
            minOutputAmount: limits.min_output_amount,
            actionId: action_id,
            refundRecipient: refund_recipient,
        }
        .abi_encode()
        .into())
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
            self.l1.balance_of(token, self.gateway).await? == U256::ZERO,
            "{description} left tokens on the closed-loop gateway"
        );
        Ok(())
    }

    async fn assert_gateway_event_privacy(
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
                keccak256("EarnDeposit(bytes32,address,uint256,uint256,uint256,bytes32)")
            }
            EarnFlow::Redeem => {
                keccak256("EarnRedeem(bytes32,address,uint256,uint256,uint256,bytes32)")
            }
            _ => unreachable!("EarnFlow contains only synchronous deposit and redeem"),
        };
        let private_user_topic = B256::left_padding_from(private_user.as_slice());
        let filter = Filter::new()
            .address(self.gateway)
            .from_block(0)
            .event_signature(signature);
        let gateway_logs = self.l1.provider().get_logs(&filter).await?;
        let gateway_log = gateway_logs
            .last()
            .ok_or_else(|| eyre::eyre!("Earn gateway event not found"))?;
        let transaction_hash = gateway_log
            .transaction_hash
            .ok_or_else(|| eyre::eyre!("Earn gateway log has no transaction hash"))?;
        let (processed, _) = events
            .iter()
            .find(|(event, log)| {
                log.transaction_hash == Some(transaction_hash)
                    && event.to == self.gateway
                    && event.token == withdrawal_token
                    && event.amount == withdrawal_amount
                    && event.callbackSuccess
            })
            .ok_or_else(|| {
                eyre::eyre!("matching Earn WithdrawalProcessed event not found for gateway event")
            })?;
        eyre::ensure!(
            gateway_log.inner.topics().len() == 3,
            "Earn gateway event unexpectedly indexed private callback data"
        );
        eyre::ensure!(
            gateway_log
                .inner
                .topics()
                .iter()
                .all(|topic| *topic != processed.senderTag && *topic != private_user_topic),
            "Earn gateway event exposed the withdrawal sender tag or private user address"
        );
        Ok(())
    }

    async fn zone_deposit(
        &mut self,
        input_token: Address,
        recipient: Address,
    ) -> eyre::Result<u128> {
        self.encrypted_entry(input_token, AMOUNT).await?;
        let before = self.zone.balance_of(self.share_token, recipient).await?;
        let public_recipient_before = self.l1.balance_of(self.share_token, recipient).await?;
        let portal_before = self.l1.balance_of(self.share_token, self.portal).await?;
        let data = self
            .callback_data(
                EarnFlow::Deposit,
                self.share_token,
                recipient,
                self.user.address(),
                EarnLimits::default(),
            )
            .await?;
        let callback_gas_limit = if input_token == self.vault_asset {
            DIRECT_DEPOSIT_CALLBACK_GAS_LIMIT
        } else {
            SWAPPED_DEPOSIT_CALLBACK_GAS_LIMIT
        };
        self.user
            .withdraw_token_with(
                input_token,
                WithdrawalArgs {
                    amount: AMOUNT,
                    to: Some(self.gateway),
                    memo: B256::ZERO,
                    gas_limit: callback_gas_limit,
                    zone_fallback_recipient: Some(self.user.address()),
                    data,
                    reveal_to: Bytes::new(),
                },
            )
            .await?;

        let after = self
            .zone
            .wait_for_balance(
                self.share_token,
                recipient,
                before + U256::from(1),
                E2E_TIMEOUT,
            )
            .await?;
        let minted = after - before;
        self.assert_private_return(
            self.share_token,
            recipient,
            public_recipient_before,
            portal_before,
            minted,
            "Earn deposit",
        )
        .await?;
        self.l1
            .assert_withdrawal_processed_with_status(
                self.portal,
                self.gateway,
                input_token,
                AMOUNT,
                true,
            )
            .await?;
        self.assert_gateway_event_privacy(
            input_token,
            AMOUNT,
            EarnFlow::Deposit,
            self.user.address(),
        )
        .await?;
        u256_to_u128(minted, "EarnToken minted by Zone deposit")
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
        let recipient_earn_before = self.zone.balance_of(self.share_token, recipient).await?;
        let share_supply_before = EarnToken::new(self.share_token, self.l1.provider())
            .totalSupply()
            .call()
            .await?;
        let backing_before = EarnVault::new(self.vault, self.l1.provider())
            .balanceOf(self.engine)
            .call()
            .await?;
        let data = self
            .callback_data(
                EarnFlow::Deposit,
                self.share_token,
                recipient,
                self.user.address(),
                limits,
            )
            .await?;
        let callback_gas_limit = if input_token == self.vault_asset {
            DIRECT_DEPOSIT_CALLBACK_GAS_LIMIT
        } else {
            SWAPPED_DEPOSIT_CALLBACK_GAS_LIMIT
        };
        self.user
            .withdraw_token_with(
                input_token,
                WithdrawalArgs {
                    amount: AMOUNT,
                    to: Some(self.gateway),
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
            self.zone.balance_of(self.share_token, recipient).await?,
            recipient_earn_before,
            "failed Earn callback credited private EarnToken"
        );
        assert_eq!(
            EarnToken::new(self.share_token, self.l1.provider())
                .totalSupply()
                .call()
                .await?,
            share_supply_before,
            "failed Earn callback changed share supply"
        );
        assert_eq!(
            EarnVault::new(self.vault, self.l1.provider())
                .balanceOf(self.engine)
                .call()
                .await?,
            backing_before,
            "failed Earn callback changed engine backing"
        );
        self.l1
            .assert_withdrawal_processed_with_status(
                self.portal,
                self.gateway,
                input_token,
                AMOUNT,
                false,
            )
            .await?;
        assert_eq!(
            self.l1.balance_of(input_token, self.gateway).await?,
            U256::ZERO,
            "failed Earn callback left input on the gateway"
        );
        assert_eq!(
            self.l1.balance_of(self.share_token, self.gateway).await?,
            U256::ZERO,
            "failed Earn callback left shares on the gateway"
        );
        Ok(())
    }

    async fn zone_deposit_expect_recipient_bounce(
        &mut self,
        input_token: Address,
        recipient: Address,
    ) -> eyre::Result<()> {
        self.encrypted_entry(input_token, AMOUNT).await?;
        let recipient_earn_before = self.zone.balance_of(self.share_token, recipient).await?;
        let public_refund_before = self
            .l1
            .balance_of(self.share_token, self.user.address())
            .await?;
        let share_supply_before = EarnToken::new(self.share_token, self.l1.provider())
            .totalSupply()
            .call()
            .await?;
        let backing_before = EarnVault::new(self.vault, self.l1.provider())
            .balanceOf(self.engine)
            .call()
            .await?;
        let data = self
            .callback_data(
                EarnFlow::Deposit,
                self.share_token,
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
                    to: Some(self.gateway),
                    memo: B256::ZERO,
                    gas_limit: DIRECT_DEPOSIT_CALLBACK_GAS_LIMIT,
                    zone_fallback_recipient: Some(self.user.address()),
                    data,
                    reveal_to: Bytes::new(),
                },
            )
            .await?;

        let public_refund_after = self
            .l1
            .wait_for_balance(
                self.share_token,
                self.user.address(),
                public_refund_before + U256::from(1),
                BOUNCE_TIMEOUT,
            )
            .await?;
        let share_supply_after = EarnToken::new(self.share_token, self.l1.provider())
            .totalSupply()
            .call()
            .await?;
        let backing_after = EarnVault::new(self.vault, self.l1.provider())
            .balanceOf(self.engine)
            .call()
            .await?;
        let minted = share_supply_after - share_supply_before;
        let refunded = public_refund_after - public_refund_before;
        assert_eq!(
            self.zone.balance_of(self.share_token, recipient).await?,
            recipient_earn_before,
            "ineligible recipient received private EarnToken"
        );
        assert!(
            refunded > U256::ZERO && refunded <= minted,
            "invalid ineligible-recipient refund: refunded={refunded}, minted={minted}"
        );
        assert!(minted > U256::ZERO, "recipient bounce minted no EarnToken");
        assert!(
            backing_after > backing_before,
            "successful gateway deposit added no engine backing"
        );
        self.l1
            .assert_withdrawal_processed_with_status(
                self.portal,
                self.gateway,
                input_token,
                AMOUNT,
                true,
            )
            .await?;
        self.assert_gateway_event_privacy(
            input_token,
            AMOUNT,
            EarnFlow::Deposit,
            self.user.address(),
        )
        .await?;
        assert_eq!(
            self.l1.balance_of(self.share_token, self.gateway).await?,
            U256::ZERO,
            "recipient bounce left shares on the gateway"
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
        let data = self
            .callback_data(
                EarnFlow::Redeem,
                output_token,
                recipient,
                account.address(),
                EarnLimits::default(),
            )
            .await?;
        let callback_gas_limit = if output_token == self.vault_asset {
            DIRECT_REDEEM_CALLBACK_GAS_LIMIT
        } else {
            SWAPPED_REDEEM_CALLBACK_GAS_LIMIT
        };
        account
            .withdraw_token_with(
                self.share_token,
                WithdrawalArgs {
                    amount: shares,
                    to: Some(self.gateway),
                    memo: B256::ZERO,
                    gas_limit: callback_gas_limit,
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
                self.gateway,
                self.share_token,
                shares,
                true,
            )
            .await?;
        self.assert_gateway_event_privacy(
            self.share_token,
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
            .approve(self.vault, U256::from(AMOUNT))
            .send()
            .await?
            .get_receipt()
            .await?;
        eyre::ensure!(receipt.status(), "vault asset approval failed");

        let vault = EarnVault::new(self.vault, provider);
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

        let adapter = EarnVaultAdapter::new(self.adapter, self.l1.provider());
        let earn_shares = adapter.sharesToTokens(venue_shares).call().await?;
        let earn_shares_u128 = u256_to_u128(earn_shares, "migration EarnToken quote")?;
        eyre::ensure!(
            earn_shares_u128 > 0,
            "migration quote returned no EarnToken"
        );

        let (key_index, encrypted) = self
            .l1
            .encrypt_deposit_for_portal(self.portal, user, B256::ZERO)
            .await?;
        let private_before = self.zone.balance_of(self.share_token, user).await?;
        let public_before = EarnToken::new(self.share_token, self.l1.provider())
            .balanceOf(user)
            .call()
            .await?;
        let supply_before = EarnToken::new(self.share_token, self.l1.provider())
            .totalSupply()
            .call()
            .await?;
        let engine_venue_before = EarnVault::new(self.vault, self.l1.provider())
            .balanceOf(self.engine)
            .call()
            .await?;

        let calls = vec![
            Call {
                to: TxKind::Call(self.vault),
                value: U256::ZERO,
                input: EarnVault::approveCall {
                    spender: self.engine,
                    amount: venue_shares,
                }
                .abi_encode()
                .into(),
            },
            Call {
                to: TxKind::Call(self.adapter),
                value: U256::ZERO,
                input: EarnVaultAdapter::depositSharesCall {
                    venueShares: venue_shares,
                    receiver: user,
                    minEarnShares: earn_shares,
                }
                .abi_encode()
                .into(),
            },
            Call {
                to: TxKind::Call(self.share_token),
                value: U256::ZERO,
                input: EarnToken::approveCall {
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
                    token: self.share_token,
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
                self.share_token,
                user,
                private_before + earn_shares,
                E2E_TIMEOUT,
            )
            .await?;
        let user_venue_after = EarnVault::new(self.vault, self.l1.provider())
            .balanceOf(user)
            .call()
            .await?;
        let engine_venue_after = EarnVault::new(self.vault, self.l1.provider())
            .balanceOf(self.engine)
            .call()
            .await?;
        let public_after = EarnToken::new(self.share_token, self.l1.provider())
            .balanceOf(user)
            .call()
            .await?;
        let supply_after = EarnToken::new(self.share_token, self.l1.provider())
            .totalSupply()
            .call()
            .await?;

        assert_eq!(venue_after - user_venue_after, venue_shares);
        assert_eq!(engine_venue_after - engine_venue_before, venue_shares);
        assert_eq!(
            public_after, public_before,
            "migrated EarnToken remained public"
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
async fn zone_entry_failure_boundaries() -> eyre::Result<()> {
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
async fn zone_access_policy_lifecycle() -> eyre::Result<()> {
    let mut fixture = EarnZoneFixture::start_protected().await?;
    let user = fixture.user.address();
    let outsider_signer = fixture.l1.signer_at(3);
    let outsider = outsider_signer.address();
    fixture.l1.fund_user(outsider, PUBLIC_USER_BALANCE).await?;
    let mut outsider_account =
        ZoneAccount::with_signer(outsider_signer, &fixture.l1, &fixture.zone, fixture.portal);
    outsider_account
        .deposit(PRIVATE_FEE_BALANCE, E2E_TIMEOUT, &fixture.zone)
        .await?;

    let outsider_shares = fixture.zone_deposit(fixture.vault_asset, outsider).await?;
    assert_eq!(
        outsider_shares, AMOUNT,
        "eligible outsider deposit was not 1:1"
    );
    let user_shares = fixture.zone_deposit(fixture.vault_asset, user).await?;
    fixture.set_access_eligibility(outsider, false).await?;

    let user_before = fixture.zone.balance_of(fixture.share_token, user).await?;
    let outsider_before = fixture
        .zone
        .balance_of(fixture.share_token, outsider)
        .await?;
    let user_provider = ProviderBuilder::new()
        .wallet(fixture.l1.user_signer())
        .connect_http(fixture.zone.http_url().clone());
    let transfer = ITIP20::new(fixture.share_token, &user_provider)
        .transfer(outsider, U256::from(1))
        .from(user)
        .call()
        .await;
    eyre::ensure!(
        transfer.is_err(),
        "private EarnToken transfer to an ineligible recipient unexpectedly succeeded"
    );
    assert_eq!(
        fixture.zone.balance_of(fixture.share_token, user).await?,
        user_before,
        "rejected private transfer changed the sender balance"
    );
    assert_eq!(
        fixture
            .zone
            .balance_of(fixture.share_token, outsider)
            .await?,
        outsider_before,
        "rejected private transfer changed the recipient balance"
    );

    fixture
        .zone_deposit_expect_recipient_bounce(fixture.vault_asset, outsider)
        .await?;
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
            .balance_of(fixture.share_token, outsider)
            .await?,
        U256::ZERO,
        "removed private holder retained EarnToken after exit"
    );

    let user_output = fixture
        .zone_redeem(user_shares, fixture.vault_asset, user)
        .await?;
    assert_eq!(user_output, AMOUNT, "eligible holder cleanup was not 1:1");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zone_rewards_private_holder() -> eyre::Result<()> {
    let mut fixture = EarnZoneFixture::start().await?;
    let user = fixture.user.address();
    let shares = fixture.zone_deposit(fixture.vault_asset, user).await?;
    assert!(shares > 1, "private reward setup minted too few shares");

    let private_after_entry = fixture.zone.balance_of(fixture.share_token, user).await?;
    let value_before = EarnVaultAdapter::new(fixture.adapter, fixture.l1.provider())
        .previewRedeem(U256::from(shares))
        .call()
        .await?;
    let user_assets_before = fixture.l1.balance_of(fixture.vault_asset, user).await?;
    let vault_assets_before = EarnVault::new(fixture.vault, fixture.l1.provider())
        .totalAssets()
        .call()
        .await?;

    let provider = fixture.user.l1_provider();
    let receipt = ITIP20::new(fixture.vault_asset, provider)
        .approve(fixture.rewards, U256::from(REWARD_AMOUNT))
        .send()
        .await?
        .get_receipt()
        .await?;
    eyre::ensure!(receipt.status(), "reward asset approval failed");
    let rewards = EarnRewards::new(fixture.rewards, provider);
    let simulated_funding = rewards
        .fund(user, U256::from(REWARD_AMOUNT))
        .gas(REWARD_FUNDING_TX_GAS_LIMIT)
        .call()
        .await?;
    if simulated_funding != U256::from(REWARD_AMOUNT) {
        // `fund` intentionally catches provider failures. Re-run its isolated leg as the rewards
        // contract so a failing E2E reports the underlying revert instead of only "funded = 0".
        EarnRewards::new(fixture.rewards, fixture.l1.provider())
            .executeFunding(user, U256::from(REWARD_AMOUNT))
            .from(fixture.rewards)
            .call()
            .await
            .wrap_err("diagnosing zero VaultRewards funding")?;
        eyre::bail!("VaultRewards simulated zero funding without an underlying revert");
    }
    let receipt = rewards
        .fund(user, U256::from(REWARD_AMOUNT))
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

    let adapter = EarnVaultAdapter::new(fixture.adapter, fixture.l1.provider());
    let value_after = adapter.previewRedeem(U256::from(shares)).call().await?;
    let user_assets_after = fixture.l1.balance_of(fixture.vault_asset, user).await?;
    let vault_assets_after = EarnVault::new(fixture.vault, fixture.l1.provider())
        .totalAssets()
        .call()
        .await?;
    let anchor_engine_shares = adapter.anchorEngineShares().call().await?;
    let anchor_supply = adapter.anchorSupply().call().await?;
    assert_eq!(
        user_assets_before - user_assets_after,
        U256::from(REWARD_AMOUNT),
        "VaultRewards did not pull the requested provider assets"
    );
    assert_eq!(
        vault_assets_after - vault_assets_before,
        U256::from(REWARD_AMOUNT),
        "VaultRewards did not invest the requested provider assets"
    );
    assert!(
        value_after > value_before,
        "private EarnToken received no reward value: before={value_before}, after={value_after}, anchorEngineShares={anchor_engine_shares}, anchorSupply={anchor_supply}"
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
    let private_after_partial = fixture.zone.balance_of(fixture.share_token, user).await?;
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
        fixture.zone.balance_of(fixture.share_token, user).await?,
        U256::ZERO,
        "private reward lifecycle left EarnToken behind"
    );
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
    assert_eq!(output, AMOUNT, "migrated EarnToken did not redeem 1:1");
    Ok(())
}
