//! Local dev mode.
//!
//! Provisions a self-contained zone against a Tempo dev L1: funds the dev account,
//! deploys the bundled `ZoneFactory` when needed, calls `createZone`, registers the
//! sequencer encryption key, and builds an L1-anchored genesis. The `tempo-zone dev`
//! command wraps [`provision_zone`] and then runs the zone node.

use alloy_consensus::Sealable;
use alloy_genesis::Genesis;
use alloy_network::{EthereumWallet, ReceiptResponse as _};
use alloy_primitives::{Address, B256, TxKind};
use alloy_provider::{PendingTransactionBuilder, Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolEvent;
use tempo_alloy::TempoNetwork;
use tempo_contracts::precompiles::{ITIP20, PATH_USD_ADDRESS};
use tempo_zone_contracts::ZoneFactory;
use zone_primitives::constants::zone_chain_id;
use zone_sequencer::register_encryption_key;

/// Provisioning options for [`provision_zone`].
#[derive(Debug)]
pub struct ProvisionConfig {
    /// Tempo L1 RPC URL (http(s) or ws(s)).
    pub l1_rpc_url: String,
    /// Dev key: L1 fee payer, factory deployer, portal admin, and zone sequencer.
    pub dev_key: PrivateKeySigner,
    /// Existing `ZoneFactory` address. Deploys the bundled factory when `None`.
    pub factory: Option<Address>,
    /// Initial TIP-20 enabled on the portal.
    pub initial_token: Address,
    /// Public zone RPC URL registered on the portal.
    pub rpc_url: String,
}

/// A zone provisioned by [`provision_zone`].
#[derive(Debug)]
pub struct ProvisionedZone {
    /// Zone ID assigned by the factory.
    pub zone_id: u32,
    /// Zone chain ID derived from the zone ID.
    pub chain_id: u64,
    /// `ZoneFactory` address on L1.
    pub factory: Address,
    /// `ZonePortal` address on L1.
    pub portal: Address,
    /// L1 anchor block number immediately before `createZone`.
    pub anchor_block_number: u64,
    /// Zone genesis anchored to the L1.
    pub genesis: Genesis,
}

/// Provisions a fresh zone on a Tempo dev L1.
///
/// Funds the dev account via `tempo_fundAddress` when needed, deploys the bundled
/// `ZoneFactory` when no address is given, calls `createZone` with the dev account as
/// both admin and sequencer, registers the sequencer encryption key on the portal, and
/// builds a genesis anchored immediately before `createZone` so the zone replays the
/// portal's initial `TokenEnabled` event.
pub async fn provision_zone(config: ProvisionConfig) -> eyre::Result<ProvisionedZone> {
    let ProvisionConfig {
        l1_rpc_url,
        dev_key,
        factory,
        initial_token,
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

    let factory_address = match factory {
        Some(address) => address,
        None => deploy_zone_factory(&l1_rpc_url, wallet).await?,
    };

    let factory = ZoneFactory::new(factory_address, &provider);
    let verifier = factory.verifier().call().await?;

    // Anchor before createZone so the L1 subscriber replays the creation block,
    // including the initial TokenEnabled event emitted by the portal constructor.
    let anchor_block_number = provider.get_block_number().await?;
    let anchor_header = provider
        .get_header_by_number(anchor_block_number.into())
        .await?
        .ok_or_else(|| eyre::eyre!("anchor header {anchor_block_number} not found"))?
        .inner
        .inner;
    let anchor_block_hash = anchor_header.hash_slow();

    let receipt = factory
        .createZone(ZoneFactory::CreateZoneParams {
            initialToken: initial_token,
            admin: dev_address,
            sequencer: dev_address,
            verifier,
            zoneParams: ZoneFactory::ZoneParams {
                genesisBlockHash: B256::ZERO,
                genesisTempoBlockHash: anchor_block_hash,
                genesisTempoBlockNumber: anchor_block_number,
            },
            rpcUrl: rpc_url,
        })
        .send()
        .await?
        .get_receipt()
        .await?;
    eyre::ensure!(receipt.status(), "createZone reverted");

    let zone_created = receipt
        .inner
        .logs()
        .iter()
        .find_map(|log| ZoneFactory::ZoneCreated::decode_log(&log.inner).ok())
        .ok_or_else(|| eyre::eyre!("ZoneCreated event not found"))?;
    let zone_id = zone_created.zoneId;
    let portal = zone_created.portal;
    let chain_id = zone_chain_id(zone_id);

    register_encryption_key(&provider, portal, &dev_key).await?;

    let (mut genesis, anchor_block_number) =
        crate::genesis::l1_anchored_genesis(&anchor_header, portal)?;
    genesis.config.chain_id = chain_id;

    Ok(ProvisionedZone {
        zone_id,
        chain_id,
        factory: factory_address,
        portal,
        anchor_block_number,
        genesis,
    })
}

/// Ensures the L1 reports the canonical hash of its Tempo header.
///
/// A client that mines Ethereum headers and only adds Tempo fields at the RPC layer
/// produces a different hash from the header that Zones submits to `finalizeTempo`.
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
         {canonical_hash}; use an L1 that mines canonically hashed Tempo headers"
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
        Err(err) => {
            tracing::debug!(%err, %dev_address, "tempo_fundAddress unavailable");
        }
    }

    let fee_balance = ITIP20::new(PATH_USD_ADDRESS, provider)
        .balanceOf(dev_address)
        .call()
        .await?;
    eyre::ensure!(
        !fee_balance.is_zero(),
        "dev account {dev_address} has no pathUSD for L1 fees; enable tempo_fundAddress, pre-fund the account, or use the default Anvil dev key"
    );
    Ok(())
}

/// Deploys the bundled `ZoneFactory` on L1 and returns its address.
///
/// The factory constructor also deploys a Verifier internally.
pub async fn deploy_zone_factory(
    l1_rpc_url: &str,
    wallet: EthereumWallet,
) -> eyre::Result<Address> {
    use alloy_rpc_types_eth::TransactionRequest;

    let provider = ProviderBuilder::new()
        .wallet(wallet)
        .connect(l1_rpc_url)
        .await?;

    let mut deploy_tx =
        TransactionRequest::default().input(crate::genesis::zone_factory_bytecode()?.into());
    deploy_tx.to = Some(TxKind::Create);
    let receipt = provider
        .send_transaction(deploy_tx)
        .await?
        .get_receipt()
        .await?;
    eyre::ensure!(receipt.status(), "ZoneFactory deployment failed");

    receipt
        .contract_address
        .ok_or_else(|| eyre::eyre!("ZoneFactory deployment missing contract address"))
}

#[cfg(feature = "cli")]
pub use command::DevCommand;

#[cfg(feature = "cli")]
mod command {
    use std::path::{Path, PathBuf};

    use alloy_primitives::Address;
    use alloy_signer_local::PrivateKeySigner;

    use super::{ProvisionConfig, provision_zone};
    use crate::cli::ZoneCli;
    use tempo_contracts::precompiles::PATH_USD_ADDRESS;

    /// Default dev private key (account #0 of the standard `test test ... junk` mnemonic).
    const DEFAULT_DEV_KEY: &str =
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    /// Provisions a fresh zone against a Tempo dev L1 and runs the zone node.
    #[derive(Debug, clap::Parser)]
    #[command(
        name = "dev",
        about = "Provision a fresh zone against a Tempo dev L1 and run the zone node"
    )]
    pub struct DevCommand {
        /// Tempo L1 WebSocket RPC URL.
        #[arg(
            long = "l1.rpc-url",
            env = "L1_RPC_URL",
            default_value = "ws://localhost:8546"
        )]
        l1_rpc_url: String,

        /// Existing ZoneFactory address on L1. Deploys the bundled factory when omitted.
        #[arg(long = "l1.factory-address", env = "ZONE_FACTORY")]
        factory_address: Option<Address>,

        /// Dev private key (hex): L1 fee payer, factory deployer, portal admin, and zone
        /// sequencer. Funded via `tempo_fundAddress` when the L1 supports it.
        #[arg(long = "dev.key", env = "DEV_KEY", default_value = DEFAULT_DEV_KEY)]
        dev_key: String,

        /// Initial TIP-20 token enabled on the portal. Defaults to pathUSD.
        #[arg(long = "dev.token", default_value_t = PATH_USD_ADDRESS)]
        initial_token: Address,

        /// Directory for genesis.json, zone.json, node data, and logs. Wiped on start.
        #[arg(long, default_value_os_t = default_datadir())]
        datadir: PathBuf,

        /// Zone RPC listener address.
        #[arg(long = "http.addr", default_value = "127.0.0.1")]
        http_addr: String,

        /// Zone HTTP RPC port. The WebSocket RPC listens on the next port and the
        /// P2P listener on the one after.
        #[arg(long = "http.port", default_value_t = 9545)]
        http_port: u16,

        /// Zone private RPC port.
        #[arg(long = "private-rpc.port", default_value_t = 8544)]
        private_rpc_port: u16,

        /// Extra arguments forwarded to `tempo-zone node`.
        #[arg(last = true)]
        node_args: Vec<String>,
    }

    impl DevCommand {
        /// Provisions the zone, writes `genesis.json` and `zone.json` to the datadir,
        /// and runs the zone node.
        pub fn run(self) -> eyre::Result<()> {
            ensure_ws_url(&self.l1_rpc_url)?;
            let dev_key: PrivateKeySigner = self
                .dev_key
                .strip_prefix("0x")
                .unwrap_or(&self.dev_key)
                .parse()
                .map_err(|err| eyre::eyre!("invalid --dev.key: {err}"))?;
            let ws_port = self
                .http_port
                .checked_add(1)
                .ok_or_else(|| eyre::eyre!("--http.port too large for the WS port"))?;
            let p2p_port = self
                .http_port
                .checked_add(2)
                .ok_or_else(|| eyre::eyre!("--http.port too large for the P2P port"))?;

            prepare_datadir(&self.datadir)?;

            // Provision on a scoped runtime; the node builds its own afterwards.
            let provisioned = {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()?;
                runtime.block_on(provision_zone(ProvisionConfig {
                    l1_rpc_url: self.l1_rpc_url.clone(),
                    dev_key: dev_key.clone(),
                    factory: self.factory_address,
                    initial_token: self.initial_token,
                    rpc_url: format!("http://{}:{}", self.http_addr, self.http_port),
                }))?
            };

            let genesis_path = self.datadir.join("genesis.json");
            std::fs::write(
                &genesis_path,
                serde_json::to_string_pretty(&provisioned.genesis)?,
            )?;

            // zone.json metadata for downstream tooling, matching `create-zone`.
            // `sequencerKey` is a well-known dev key; `just zone-up` reads it.
            let zone_json = serde_json::json!({
                "zoneId": provisioned.zone_id,
                "chainId": provisioned.chain_id,
                "portal": format!("{}", provisioned.portal),
                "initialToken": format!("{}", self.initial_token),
                "admin": format!("{}", dev_key.address()),
                "sequencer": format!("{}", dev_key.address()),
                "sequencerKey": self.dev_key,
                "tempoAnchorBlock": provisioned.anchor_block_number,
                "zoneFactory": format!("{}", provisioned.factory),
                "rpcUrl": format!("http://{}:{}", self.http_addr, self.http_port),
            });
            std::fs::write(
                self.datadir.join("zone.json"),
                serde_json::to_string_pretty(&zone_json)?,
            )?;

            println!("Zone provisioned!");
            println!("  Zone ID:      {}", provisioned.zone_id);
            println!("  Chain ID:     {}", provisioned.chain_id);
            println!("  ZoneFactory:  {}", provisioned.factory);
            println!("  Portal:       {}", provisioned.portal);
            println!("  Anchor block: {}", provisioned.anchor_block_number);
            println!("  Dev account:  {}", dev_key.address());
            println!(
                "  HTTP RPC:     http://{}:{}",
                self.http_addr, self.http_port
            );
            println!("  WS RPC:       ws://{}:{ws_port}", self.http_addr);
            println!(
                "  Private RPC:  http://{}:{}",
                self.http_addr, self.private_rpc_port
            );
            println!("  Datadir:      {}", self.datadir.display());

            let mut argv: Vec<String> = [
                "tempo-zone",
                "node",
                "--chain",
                &genesis_path.display().to_string(),
                "--l1.rpc-url",
                &self.l1_rpc_url,
                "--l1.portal-address",
                &provisioned.portal.to_string(),
                "--l1.genesis-block-number",
                &provisioned.anchor_block_number.to_string(),
                "--zone.id",
                &provisioned.zone_id.to_string(),
                "--http",
                "--http.addr",
                &self.http_addr,
                "--http.port",
                &self.http_port.to_string(),
                "--http.api",
                "all",
                "--ws",
                "--ws.addr",
                &self.http_addr,
                "--ws.port",
                &ws_port.to_string(),
                "--ws.api",
                "all",
                "--port",
                &p2p_port.to_string(),
                "--private-rpc.port",
                &self.private_rpc_port.to_string(),
                "--datadir",
                &self.datadir.join("node").display().to_string(),
                "--log.file.directory",
                &self.datadir.join("logs").display().to_string(),
                "--sequencer",
                "--sequencer-key",
                &self.dev_key,
            ]
            .map(str::to_owned)
            .to_vec();
            argv.extend(self.node_args);

            ZoneCli::parse_from(argv).run()
        }
    }

    fn default_datadir() -> PathBuf {
        std::env::temp_dir().join("tempo-zone-dev")
    }

    /// Ensures the L1 RPC URL uses a WebSocket scheme, as `tempo-zone node` requires.
    fn ensure_ws_url(l1_rpc_url: &str) -> eyre::Result<()> {
        let url: url::Url = l1_rpc_url
            .parse()
            .map_err(|err| eyre::eyre!("failed parsing --l1.rpc-url as URL: {err}"))?;
        eyre::ensure!(
            matches!(url.scheme(), "ws" | "wss"),
            "--l1.rpc-url must use ws:// or wss://, got `{}`",
            url.scheme()
        );
        Ok(())
    }

    /// Wipes and recreates the datadir.
    ///
    /// Every run provisions a fresh zone anchored to fresh L1 state, so stale node
    /// data can never be reused. Refuses to wipe a directory that does not look
    /// like a previous dev datadir.
    fn prepare_datadir(datadir: &Path) -> eyre::Result<()> {
        if datadir.exists() {
            let is_dev_datadir =
                datadir.join("zone.json").exists() || std::fs::read_dir(datadir)?.next().is_none();
            eyre::ensure!(
                is_dev_datadir,
                "refusing to wipe {}: not a tempo-zone dev datadir (no zone.json); \
                 pass an empty or fresh --datadir",
                datadir.display()
            );
            std::fs::remove_dir_all(datadir)?;
        }
        std::fs::create_dir_all(datadir)?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::ensure_ws_url;

        #[test]
        fn ensure_ws_url_accepts_websocket_schemes() {
            assert!(ensure_ws_url("ws://localhost:8546").is_ok());
            assert!(ensure_ws_url("wss://rpc.moderato.tempo.xyz").is_ok());
        }

        #[test]
        fn ensure_ws_url_rejects_non_websocket_schemes() {
            assert!(ensure_ws_url("http://localhost:8545").is_err());
        }
    }
}
