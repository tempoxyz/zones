//! Verifies an existing Earn deployment's closed-loop ZonePortal configuration.

use alloy::{
    primitives::{Address, B256, U256},
    providers::{Provider, ProviderBuilder},
};
use alloy_rpc_types_eth::BlockId;
use eyre::{WrapErr as _, ensure};
use std::collections::{BTreeMap, BTreeSet};
use tempo_alloy::TempoNetwork;
use tempo_zone_contracts::{
    ZONE_FACTORY_ADDRESS, ZoneFactory, ZonePortal, ZonePortal::Role as PortalRole,
};

use crate::zone_utils::normalize_http_rpc;

const LOG_QUERY_BLOCK_CHUNK: u64 = 5_000;

alloy::sol! {
    #[sol(rpc)]
    interface EarnRouterView {
        function allowedZoneId() external view returns (uint32);
        function earnVault() external view returns (address);
        function privateAsset() external view returns (address);
        function vaultAsset() external view returns (address);
        function earnShare() external view returns (address);
        function supportsFlow(uint8 flow) external view returns (bool);
    }

    #[sol(rpc)]
    interface EarnVaultView {
        function asset() external view returns (address);
        function earnShare() external view returns (address);
    }

    #[sol(rpc)]
    interface PortalTokenView {
        function areDepositsActive(address token) external view returns (bool);
    }
}

#[derive(Debug, clap::Parser)]
pub(crate) struct VerifyClosedLoop {
    /// Tempo L1 HTTP RPC URL.
    #[arg(long, env = "L1_RPC_URL")]
    l1_rpc_url: String,

    /// Deployed SingleZoneEarnRouter address.
    #[arg(long)]
    earn_router: Address,
}

impl VerifyClosedLoop {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let rpc_url = normalize_http_rpc(&self.l1_rpc_url);
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&rpc_url)
            .await
            .wrap_err("failed connecting to Tempo L1")?;

        let snapshot_block = provider
            .get_block_number()
            .await
            .wrap_err("failed reading the latest Tempo L1 block")?;
        let snapshot_block_id = BlockId::number(snapshot_block);

        ensure_has_code(
            &provider,
            self.earn_router,
            "Earn router",
            snapshot_block_id,
        )
        .await?;

        let router = EarnRouterView::new(self.earn_router, &provider);
        let (
            zone_id,
            earn_vault,
            private_asset,
            vault_asset,
            earn_share,
            supports_deposit,
            supports_redeem,
        ) = provider
            .multicall()
            .block(snapshot_block_id)
            .add(router.allowedZoneId())
            .add(router.earnVault())
            .add(router.privateAsset())
            .add(router.vaultAsset())
            .add(router.earnShare())
            .add(router.supportsFlow(0))
            .add(router.supportsFlow(1))
            .aggregate()
            .await
            .wrap_err("failed reading Earn router configuration")?;

        let zone = ZoneFactory::new(ZONE_FACTORY_ADDRESS, &provider)
            .zones(zone_id)
            .block(snapshot_block_id)
            .call()
            .await
            .wrap_err("failed resolving Zone through ZoneFactory")?;
        ensure!(
            zone.portal != Address::ZERO,
            "router targets unknown Zone {zone_id}"
        );
        ensure_has_code(&provider, zone.portal, "ZonePortal", snapshot_block_id).await?;
        ensure_has_code(&provider, earn_vault, "EarnVault", snapshot_block_id).await?;

        let deployment_block =
            find_zone_deployment_block(&provider, zone_id, zone.portal, snapshot_block).await?;

        println!("Closed-loop deployment");
        println!("  Snapshot block: {snapshot_block}");
        println!("  Deployment block: {deployment_block}");
        println!("  Zone ID:      {zone_id}");
        println!("  ZonePortal:   {}", zone.portal);
        println!("  Earn router:  {}", self.earn_router);
        println!("  EarnVault:    {earn_vault}");
        println!("  Private asset: {private_asset}");
        println!("  Vault asset:  {vault_asset}");
        println!("  EarnShare:    {earn_share}");
        println!();

        let portal = ZonePortal::new(zone.portal, &provider);
        let portal_tokens = PortalTokenView::new(zone.portal, &provider);
        let vault = EarnVaultView::new(earn_vault, &provider);
        let mut checks = Checks::default();

        let expected_tokens = BTreeSet::from([private_asset, earn_share]);
        let (roles, event_tokens) = tokio::try_join!(
            read_role_updates(&provider, zone.portal, deployment_block, snapshot_block,),
            read_token_enablements(&provider, zone.portal, deployment_block, snapshot_block,),
        )?;

        checks.expect(
            "ZonePortal Zone ID matches the Earn router",
            portal
                .zoneId()
                .block(snapshot_block_id)
                .call()
                .await
                .wrap_err("failed reading ZonePortal Zone ID")?
                == zone_id,
        );
        checks.expect(
            "account allowlist enforcement is enabled",
            portal
                .isAccessEnforced()
                .block(snapshot_block_id)
                .call()
                .await
                .wrap_err("failed reading ZonePortal access mode")?,
        );
        checks.expect(
            "callback gateway enforcement is enabled",
            !portal
                .isGatewayOpen()
                .block(snapshot_block_id)
                .call()
                .await
                .wrap_err("failed reading ZonePortal gateway mode")?,
        );

        let callback_gateways: BTreeSet<_> = roles
            .iter()
            .filter_map(|(&account, &role)| {
                (role == PortalRole::CallbackGateway).then_some(account)
            })
            .collect();
        checks.expect_equal(
            "Earn router is the only ZonePortal callback gateway",
            &callback_gateways,
            &BTreeSet::from([self.earn_router]),
        );

        let account_count = roles
            .values()
            .filter(|&&role| role == PortalRole::Account)
            .count();
        println!("\nMANUAL REVIEW: ZonePortal Account roles ({account_count})");
        for (&account, &role) in &roles {
            if role == PortalRole::Account {
                println!("  {account}");
            }
        }
        if account_count == 0 {
            println!("  (none)");
        }
        println!("  Confirm this set exactly matches the approved deployment record.\n");

        checks.expect("Earn router supports deposit callbacks", supports_deposit);
        checks.expect("Earn router supports redeem callbacks", supports_redeem);
        checks.expect(
            "Earn router vault asset matches EarnVault",
            vault
                .asset()
                .block(snapshot_block_id)
                .call()
                .await
                .wrap_err("failed reading EarnVault asset")?
                == vault_asset,
        );
        checks.expect(
            "Earn router EarnShare matches EarnVault",
            vault
                .earnShare()
                .block(snapshot_block_id)
                .call()
                .await
                .wrap_err("failed reading EarnVault EarnShare")?
                == earn_share,
        );

        let onchain_tokens: BTreeSet<_> = portal
            .enabled_tokens_at(snapshot_block_id)
            .await
            .wrap_err("failed reading the ZonePortal enabled-token registry")?
            .into_iter()
            .collect();
        checks.expect_equal(
            "TokenEnabled history matches the ZonePortal token registry",
            &event_tokens,
            &onchain_tokens,
        );
        checks.expect_equal(
            "enabled token set exactly matches the private asset and EarnShare",
            &event_tokens,
            &expected_tokens,
        );

        for token in expected_tokens {
            checks.expect(
                format!("deposits for token {token} are active"),
                portal_tokens
                    .areDepositsActive(token)
                    .block(snapshot_block_id)
                    .call()
                    .await
                    .wrap_err_with(|| format!("failed reading deposit status for token {token}"))?,
            );
        }

        checks.finish()
    }
}

async fn find_zone_deployment_block<P: Provider<TempoNetwork>>(
    provider: &P,
    zone_id: u32,
    portal: Address,
    snapshot_block: u64,
) -> eyre::Result<u64> {
    let events = ZoneFactory::new(ZONE_FACTORY_ADDRESS, provider)
        .ZoneCreated_filter()
        .topic1(B256::from(U256::from(zone_id)))
        .topic2(portal.into_word())
        .from_block(0)
        .to_block(snapshot_block)
        .chunked()
        .chunk_size(LOG_QUERY_BLOCK_CHUNK)
        .query()
        .await
        .wrap_err("failed scanning ZoneFactory ZoneCreated events")?;

    ensure!(
        events.len() == 1,
        "expected exactly one ZoneCreated event for Zone {zone_id} and portal {portal}, found {}",
        events.len()
    );
    let (event, log) = &events[0];
    ensure!(
        event.zoneId == zone_id && event.portal == portal,
        "ZoneCreated event does not match Zone {zone_id} and portal {portal}"
    );
    ensure!(!log.removed, "ZoneCreated query returned a removed log");
    log.block_number
        .ok_or_else(|| eyre::eyre!("ZoneCreated log is missing its block number"))
}

async fn read_role_updates<P: Provider<TempoNetwork>>(
    provider: &P,
    portal: Address,
    deployment_block: u64,
    snapshot_block: u64,
) -> eyre::Result<BTreeMap<Address, PortalRole>> {
    let updates = ZonePortal::new(portal, provider)
        .RoleUpdated_filter()
        .from_block(deployment_block)
        .to_block(snapshot_block)
        .chunked()
        .chunk_size(LOG_QUERY_BLOCK_CHUNK)
        .query()
        .await
        .wrap_err("failed scanning ZonePortal RoleUpdated events")?;

    let mut roles = BTreeMap::new();
    for (update, _) in updates {
        if update.next == PortalRole::None {
            roles.remove(&update.account);
        } else {
            roles.insert(update.account, update.next);
        }
    }
    Ok(roles)
}

async fn read_token_enablements<P: Provider<TempoNetwork>>(
    provider: &P,
    portal: Address,
    deployment_block: u64,
    snapshot_block: u64,
) -> eyre::Result<BTreeSet<Address>> {
    let events = ZonePortal::new(portal, provider)
        .TokenEnabled_filter()
        .from_block(deployment_block)
        .to_block(snapshot_block)
        .chunked()
        .chunk_size(LOG_QUERY_BLOCK_CHUNK)
        .query()
        .await
        .wrap_err("failed scanning ZonePortal TokenEnabled events")?;
    Ok(events.into_iter().map(|(event, _)| event.token).collect())
}

async fn ensure_has_code<P: Provider<TempoNetwork>>(
    provider: &P,
    address: Address,
    label: &str,
    block_id: BlockId,
) -> eyre::Result<()> {
    ensure!(address != Address::ZERO, "{label} address is zero");
    ensure!(
        !provider
            .get_code_at(address)
            .block_id(block_id)
            .await
            .wrap_err_with(|| format!("failed reading {label} bytecode"))?
            .is_empty(),
        "{label} has no bytecode at {address}"
    );
    Ok(())
}

#[derive(Default)]
struct Checks {
    failures: Vec<String>,
}

impl Checks {
    fn expect(&mut self, label: impl Into<String>, passed: bool) {
        let label = label.into();
        if passed {
            println!("  PASS  {label}");
        } else {
            println!("  FAIL  {label}");
            self.failures.push(label);
        }
    }

    fn expect_equal<T: std::fmt::Debug + PartialEq>(
        &mut self,
        label: impl Into<String>,
        actual: &T,
        expected: &T,
    ) {
        let label = label.into();
        if actual == expected {
            println!("  PASS  {label}");
        } else {
            println!("  FAIL  {label}");
            println!("        expected: {expected:?}");
            println!("        actual:   {actual:?}");
            self.failures.push(format!(
                "{label}\n  expected: {expected:?}\n  actual:   {actual:?}"
            ));
        }
    }

    fn finish(self) -> eyre::Result<()> {
        ensure!(
            self.failures.is_empty(),
            "{} check(s) failed:\n- {}",
            self.failures.len(),
            self.failures.join("\n- ")
        );
        println!("\nAutomated closed-loop configuration checks passed");
        println!("Manual Account-role confirmation is still required");
        Ok(())
    }
}
