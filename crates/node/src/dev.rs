//! Local Zone provisioning against a Tempo development L1.

#[cfg(feature = "cli")]
use std::path::Path;

use alloy_consensus::Sealable;
use alloy_genesis::Genesis;
use alloy_network::{EthereumWallet, ReceiptResponse as _};
use alloy_primitives::{Address, B256};
use alloy_provider::{PendingTransactionBuilder, Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolEvent;
use tempo_alloy::TempoNetwork;
use tempo_contracts::precompiles::{ITIP20, PATH_USD_ADDRESS};
use tempo_zone_contracts::{ZONE_FACTORY_ADDRESS, ZoneFactory};
use zone_primitives::constants::zone_chain_id;
use zone_sequencer::register_encryption_key;

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
    /// L1 block used as the Zone genesis anchor. Defaults to the pre-createZone head.
    pub anchor_block_number: Option<u64>,
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
        anchor_block_number,
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
    let anchor_block_number = match anchor_block_number {
        Some(number) => number,
        None => provider.get_block_number().await?,
    };
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

#[cfg(feature = "cli")]
pub(crate) fn dev_signer(mnemonic: &str) -> eyre::Result<PrivateKeySigner> {
    use alloy_signer_local::MnemonicBuilder;

    MnemonicBuilder::try_from_phrase_first(mnemonic)
        .map_err(|error| eyre::eyre!("failed to derive dev account from --dev.mnemonic: {error}"))
}

#[cfg(feature = "cli")]
pub(crate) fn dev_l1_chain_spec(owner: Address) -> tempo_chainspec::TempoChainSpec {
    use alloy_genesis::GenesisAccount;
    use alloy_primitives::{U256, keccak256};
    use alloy_sol_types::SolValue as _;
    use tempo_chainspec::TempoChainSpec;
    use tempo_contracts::precompiles::{TIP403_REGISTRY_ADDRESS, initial_zone_factory_state};
    use tempo_precompiles::tip403_registry::{ALLOW_ALL_POLICY_ID, tip403_registry_slots};

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

#[cfg(feature = "cli")]
pub(crate) async fn prefund_custom_dev_account(
    l1_rpc_url: &str,
    recipient: Address,
) -> eyre::Result<()> {
    use alloy_primitives::U256;

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

#[cfg(feature = "cli")]
pub(crate) fn write_owner_only(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::{
            fs::{OpenOptions, Permissions},
            io::Write as _,
            os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _},
        };

        let mut options = OpenOptions::new();
        options.write(true).create(true).mode(0o600);
        let mut file = options.open(path)?;
        file.set_permissions(Permissions::from_mode(0o600))?;
        file.set_len(0)?;
        file.write_all(contents)
    }

    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}
