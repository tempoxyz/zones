//! Local Zone provisioning against a Tempo development L1.

use std::{net::SocketAddr, num::NonZeroUsize, path::Path, sync::Arc, time::Duration};

use alloy_consensus::Sealable;
use alloy_genesis::{Genesis, GenesisAccount};
use alloy_network::{EthereumWallet, ReceiptResponse as _};
use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_provider::{PendingTransactionBuilder, Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolEvent, SolValue as _};
use futures::{FutureExt as _, future::BoxFuture};
use reth_db::init_db;
use reth_node_builder::{NodeBuilder, NodeConfig};
use reth_node_core::args::{DatadirArgs, RpcServerArgs};
use reth_rpc_builder::RpcModuleSelection;
use reth_tasks::TaskExecutor;
use tempo_alloy::TempoNetwork;
use tempo_chainspec::TempoChainSpec;
use tempo_contracts::precompiles::{
    ITIP20, PATH_USD_ADDRESS, TIP403_REGISTRY_ADDRESS, initial_zone_factory_state,
};
use tempo_node::node::TempoNode;
use tempo_precompiles::tip403_registry::{ALLOW_ALL_POLICY_ID, tip403_registry_slots};
use tempo_zone_contracts::{ZONE_FACTORY_ADDRESS, ZoneFactory};
use zone_chainspec::ZoneChainSpec;
use zone_primitives::constants::zone_chain_id;
use zone_sequencer::register_encryption_key;

const DEV_DATADIR_MARKER: &str = ".tempo-zone-dev";
const DEV_DATADIR_MARKER_CONTENTS: &[u8] = b"tempo-zone-dev\n";

/// Inputs for provisioning a fresh zone on an already-running Tempo L1.
#[derive(Debug)]
pub struct ProvisionConfig {
    pub l1_rpc_url: String,
    pub dev_key: PrivateKeySigner,
    pub factory: Option<Address>,
    pub initial_token: Address,
    pub is_access_open: bool,
    pub is_gateway_enforced: bool,
    pub zone_gateways: Vec<Address>,
    pub allowed_accounts: Vec<Address>,
    pub rpc_url: String,
}

/// The L1 contract addresses and L1-anchored genesis produced by provisioning.
#[derive(Debug)]
pub struct ProvisionedZone {
    pub zone_id: u32,
    pub chain_id: u64,
    pub factory: Address,
    pub portal: Address,
    pub anchor_block_number: u64,
    pub genesis: Genesis,
}

pub(crate) struct DevStartup {
    pub(crate) l1_rpc_url: String,
    pub(crate) portal: Address,
    pub(crate) signer: PrivateKeySigner,
    pub(crate) l1_exit: BoxFuture<'static, eyre::Result<()>>,
}

/// Starts an embedded Tempo L1 and provisions the local zone.
pub(crate) async fn init(
    config: &mut NodeConfig<ZoneChainSpec>,
    executor: TaskExecutor,
) -> eyre::Result<DevStartup> {
    let signer = dev_signer(&config.dev.dev_mnemonic)?;
    let l1_chain_spec = Arc::new(dev_l1_chain_spec(signer.address()));
    let l1_datadir = config.datadir().data_dir().join("l1");
    let mut l1_config = NodeConfig::new(l1_chain_spec.clone())
        .with_datadir_args(DatadirArgs {
            datadir: l1_datadir.into(),
            ..Default::default()
        })
        .with_unused_ports()
        .dev()
        .with_rpc(
            RpcServerArgs::default()
                .with_unused_ports()
                .with_http()
                .with_http_api(RpcModuleSelection::All)
                .with_ws()
                .with_ws_api(RpcModuleSelection::All),
        );
    l1_config.dev = config.dev.clone();
    l1_config.dev.dev = true;
    l1_config.dev.block_time = l1_config
        .dev
        .block_time
        .or(Some(Duration::from_millis(500)));
    l1_config.dev.finality_depth = NonZeroUsize::MIN;

    let l1_database = init_db(l1_config.datadir().db(), Default::default())?;
    let l1 = NodeBuilder::new(l1_config)
        .with_database(l1_database)
        .with_launch_context(executor)
        .node(TempoNode::default())
        .launch_with_debug_capabilities()
        .await?;
    let l1_rpc_url = l1
        .node
        .rpc_server_handle()
        .ws_url()
        .ok_or_else(|| eyre::eyre!("embedded Tempo L1 WebSocket RPC did not start"))?;

    prefund_custom_dev_account(&l1_rpc_url, signer.address()).await?;
    let zone_rpc_url = zone_rpc_url(&config.rpc, config.instance);
    let provisioned = provision_zone(ProvisionConfig {
        l1_rpc_url: l1_rpc_url.clone(),
        dev_key: signer.clone(),
        factory: None,
        initial_token: PATH_USD_ADDRESS,
        is_access_open: true,
        is_gateway_enforced: false,
        zone_gateways: Vec::new(),
        allowed_accounts: Vec::new(),
        rpc_url: zone_rpc_url.clone(),
    })
    .await?;

    config.chain = Arc::new(ZoneChainSpec::from_genesis_with_l1(
        provisioned.genesis.clone(),
        l1_chain_spec.as_ref(),
    )?);
    let datadir = config.datadir().data_dir().to_path_buf();
    write_owner_only(
        &datadir.join("genesis.json"),
        serde_json::to_string_pretty(&provisioned.genesis)?.as_bytes(),
    )?;

    let private_key = signer.to_bytes().to_string();
    let zone_json = serde_json::json!({
        "zoneId": provisioned.zone_id,
        "chainId": provisioned.chain_id,
        "portal": provisioned.portal.to_string(),
        "initialToken": PATH_USD_ADDRESS.to_string(),
        "accessMode": false,
        "gatewayMode": false,
        "zoneGateways": [],
        "allowedAccounts": [],
        "admin": signer.address().to_string(),
        "sequencer": signer.address().to_string(),
        "sequencerKey": private_key,
        "tempoAnchorBlock": provisioned.anchor_block_number,
        "zoneFactory": provisioned.factory.to_string(),
        "rpcUrl": &zone_rpc_url,
    });
    write_owner_only(
        &datadir.join("zone.json"),
        serde_json::to_string_pretty(&zone_json)?.as_bytes(),
    )?;
    write_owner_only(&datadir.join("sequencer.key"), private_key.as_bytes())?;

    tracing::info!(
        target: "reth::cli",
        zone_id = provisioned.zone_id,
        chain_id = provisioned.chain_id,
        portal = %provisioned.portal,
        l1_rpc = %l1_rpc_url,
        zone_rpc = %zone_rpc_url,
        "Tempo Zone dev stack ready"
    );
    Ok(DevStartup {
        l1_rpc_url,
        portal: provisioned.portal,
        signer,
        l1_exit: l1.wait_for_node_exit().boxed(),
    })
}

fn zone_rpc_url(rpc: &RpcServerArgs, instance: Option<u16>) -> String {
    let mut rpc = rpc.clone();
    rpc.adjust_instance_ports(instance);
    format!("http://{}", SocketAddr::new(rpc.http_addr, rpc.http_port))
}

/// Creates a zone through the protocol-managed ZoneFactory and constructs its genesis.
pub async fn provision_zone(config: ProvisionConfig) -> eyre::Result<ProvisionedZone> {
    let ProvisionConfig {
        l1_rpc_url,
        dev_key,
        factory,
        initial_token,
        is_access_open,
        is_gateway_enforced,
        zone_gateways,
        allowed_accounts,
        rpc_url,
    } = config;
    let dev_address = dev_key.address();
    let wallet = EthereumWallet::from(dev_key.clone());
    let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
        .wallet(wallet.clone())
        .connect(&l1_rpc_url)
        .await?;

    ensure_canonical_tempo_header_hash(&provider).await?;
    fund_dev_account(&provider, dev_address).await?;

    if let Some(address) = factory {
        eyre::ensure!(
            address == ZONE_FACTORY_ADDRESS,
            "ZoneFactory must use TIP-1091 address {ZONE_FACTORY_ADDRESS}, got {address}"
        );
    }
    let factory_address = native_zone_factory(&l1_rpc_url, wallet).await?;
    let factory = ZoneFactory::new(factory_address, &provider);
    let factory_owner = factory.owner().call().await?;
    eyre::ensure!(
        factory_owner == dev_address,
        "ZoneFactory owner is {factory_owner}, but the dev account is {dev_address}"
    );

    // Replay the createZone block, including its initial TokenEnabled event.
    let anchor_block_number = provider.get_block_number().await?;
    let anchor_header = provider
        .get_header_by_number(anchor_block_number.into())
        .await?
        .ok_or_else(|| eyre::eyre!("anchor header {anchor_block_number} not found"))?
        .inner
        .inner;

    let receipt = factory
        .createZone(ZoneFactory::CreateZoneParams {
            initialToken: initial_token,
            accessMode: !is_access_open,
            gatewayMode: is_gateway_enforced,
            allowedAccounts: allowed_accounts,
            zoneGateways: zone_gateways,
            admin: dev_address,
            sequencers: vec![dev_address],
            threshold: 1,
            rpcUrl: rpc_url,
        })
        .send()
        .await?
        .get_receipt()
        .await?;
    eyre::ensure!(receipt.status(), "createZone reverted");

    let event = receipt
        .inner
        .logs()
        .iter()
        .find_map(|log| ZoneFactory::ZoneCreated::decode_log(&log.inner).ok())
        .ok_or_else(|| eyre::eyre!("ZoneCreated event not found"))?;
    let parent_chain_id = provider.get_chain_id().await?;
    let chain_id = zone_chain_id(parent_chain_id, event.zoneId)?;

    register_encryption_key(&provider, event.portal, &dev_key).await?;
    let (mut genesis, anchor_block_number) =
        crate::genesis::l1_anchored_genesis(&anchor_header, initial_token)?;
    genesis.config.chain_id = chain_id;

    Ok(ProvisionedZone {
        zone_id: event.zoneId,
        chain_id,
        factory: factory_address,
        portal: event.portal,
        anchor_block_number,
        genesis,
    })
}

async fn ensure_canonical_tempo_header_hash<P: Provider<TempoNetwork>>(
    provider: &P,
) -> eyre::Result<()> {
    let block_number = provider.get_block_number().await?;
    let response = provider
        .get_header_by_number(block_number.into())
        .await?
        .ok_or_else(|| eyre::eyre!("L1 header not found for block {block_number}"))?;
    let rpc_hash = response.inner.hash;
    let canonical_hash = response.inner.inner.hash_slow();
    eyre::ensure!(
        rpc_hash == canonical_hash,
        "L1 block {block_number} reports hash {rpc_hash}, but its canonical Tempo header hash is \
         {canonical_hash}"
    );
    Ok(())
}

async fn fund_dev_account<P: Provider<TempoNetwork>>(
    provider: &P,
    dev_address: Address,
) -> eyre::Result<()> {
    let funding = provider
        .raw_request::<_, Vec<B256>>("tempo_fundAddress".into(), (dev_address,))
        .await;
    match funding {
        Ok(tx_hashes) => {
            for tx_hash in tx_hashes {
                let receipt = PendingTransactionBuilder::new(provider.root().clone(), tx_hash)
                    .get_receipt()
                    .await?;
                eyre::ensure!(receipt.status(), "tempo_fundAddress transaction reverted");
            }
        }
        Err(error) => tracing::debug!(%error, %dev_address, "tempo_fundAddress unavailable"),
    }

    let balance = ITIP20::new(PATH_USD_ADDRESS, provider)
        .balanceOf(dev_address)
        .call()
        .await?;
    eyre::ensure!(
        !balance.is_zero(),
        "dev account {dev_address} has no pathUSD for L1 fees"
    );
    Ok(())
}

/// Verifies and returns TIP-1091's fixed ZoneFactory address.
pub async fn native_zone_factory(
    l1_rpc_url: &str,
    wallet: EthereumWallet,
) -> eyre::Result<Address> {
    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect(l1_rpc_url)
        .await?;
    eyre::ensure!(
        !provider.get_code_at(ZONE_FACTORY_ADDRESS).await?.is_empty(),
        "ZoneFactory is not installed at TIP-1091 address {ZONE_FACTORY_ADDRESS}"
    );
    Ok(ZONE_FACTORY_ADDRESS)
}

fn dev_signer(mnemonic: &str) -> eyre::Result<PrivateKeySigner> {
    use alloy_signer_local::MnemonicBuilder;

    MnemonicBuilder::try_from_phrase_first(mnemonic)
        .map_err(|error| eyre::eyre!("failed to derive dev account from --dev.mnemonic: {error}"))
}

fn dev_l1_chain_spec(owner: Address) -> TempoChainSpec {
    // This is the same complete Tempo genesis exercised by the in-process L1 test harness. The
    // upstream bare `DEV` spec does not configure PATH_USD's transfer policy, so ZoneFactory
    // correctly rejects it as an initial portal token.
    let mut genesis: Genesis =
        serde_json::from_str(include_str!("../tests/assets/test-genesis.json"))
            .expect("embedded Tempo dev genesis must be valid");
    genesis
        .alloc
        .extend(initial_zone_factory_state(owner).map(|account| {
            (
                account.address,
                GenesisAccount {
                    code: Some(account.code),
                    storage: account.storage.map(|(slot, value)| {
                        std::collections::BTreeMap::from([(
                            B256::from(slot.to_be_bytes()),
                            B256::from(value.to_be_bytes()),
                        )])
                    }),
                    ..Default::default()
                },
            )
        }));

    let token_policy_slot = keccak256(
        (
            PATH_USD_ADDRESS,
            tip403_registry_slots::TOKEN_TRANSFER_POLICIES,
        )
            .abi_encode(),
    );
    let packed_policy = U256::from(ALLOW_ALL_POLICY_ID) | (U256::ONE << u64::BITS);
    genesis
        .alloc
        .entry(TIP403_REGISTRY_ADDRESS)
        .or_default()
        .storage
        .get_or_insert_default()
        .insert(token_policy_slot, B256::from(packed_policy.to_be_bytes()));

    TempoChainSpec::from_genesis(genesis)
}

async fn prefund_custom_dev_account(l1_rpc_url: &str, recipient: Address) -> eyre::Result<()> {
    const DEFAULT_DEV_KEY: &str =
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    let funder: PrivateKeySigner = DEFAULT_DEV_KEY.parse()?;
    if recipient == funder.address() {
        return Ok(());
    }

    let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
        .wallet(EthereumWallet::from(funder))
        .connect(l1_rpc_url)
        .await?;
    let receipt = ITIP20::new(PATH_USD_ADDRESS, &provider)
        .transfer(recipient, U256::from(10_000_000_000u64))
        .send()
        .await?
        .get_receipt()
        .await?;
    eyre::ensure!(receipt.status(), "failed to fund custom dev account");
    Ok(())
}

/// Clears an empty or previously generated dev datadir before Reth opens its database.
pub(crate) fn prepare_datadir(datadir: &Path) -> eyre::Result<()> {
    if datadir.exists() {
        let empty = std::fs::read_dir(datadir)?.next().is_none();
        let known_dev = std::fs::read(datadir.join(DEV_DATADIR_MARKER))
            .is_ok_and(|contents| contents == DEV_DATADIR_MARKER_CONTENTS);
        eyre::ensure!(
            empty || known_dev,
            "refusing to wipe {}: not a Tempo Zone dev datadir",
            datadir.display()
        );
        std::fs::remove_dir_all(datadir)?;
    }
    std::fs::create_dir_all(datadir)?;
    write_owner_only(
        &datadir.join(DEV_DATADIR_MARKER),
        DEV_DATADIR_MARKER_CONTENTS,
    )?;
    Ok(())
}

fn write_owner_only(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::{fs::OpenOptions, io::Write as _};

    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(contents)
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv6Addr};

    use reth_node_core::args::RpcServerArgs;

    use super::{prepare_datadir, zone_rpc_url};

    #[test]
    fn prepare_datadir_only_reuses_dev_directories() {
        let root = tempfile::tempdir().unwrap();
        let other = root.path().join("other");
        std::fs::create_dir(&other).unwrap();
        std::fs::write(other.join("zone.json"), []).unwrap();
        assert!(prepare_datadir(&other).is_err());

        let dev = root.path().join("dev");
        prepare_datadir(&dev).unwrap();
        std::fs::write(dev.join("state"), []).unwrap();
        prepare_datadir(&dev).unwrap();
        assert!(!dev.join("state").exists());
    }

    #[test]
    fn zone_rpc_url_applies_instance_port_adjustment() {
        assert_eq!(
            zone_rpc_url(&RpcServerArgs::default(), Some(3)),
            "http://127.0.0.1:8543"
        );
    }

    #[test]
    fn zone_rpc_url_formats_ipv6_addresses() {
        let rpc = RpcServerArgs {
            http_addr: IpAddr::V6(Ipv6Addr::LOCALHOST),
            ..Default::default()
        };

        assert_eq!(zone_rpc_url(&rpc, None), "http://[::1]:8545");
    }
}
