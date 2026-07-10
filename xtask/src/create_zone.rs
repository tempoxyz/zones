// The `sol!`generated `ZoneFactory` event/contract bindings expand to functions
// with more than 7 parameters, which trips `clippy::too_many_arguments`.
#![allow(clippy::too_many_arguments)]

use alloy::{
    network::{EthereumWallet, primitives::ReceiptResponse},
    primitives::{Address, B256, address, keccak256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol,
};
use alloy_rlp::Encodable;
use eyre::{WrapErr as _, eyre};
use std::path::PathBuf;
use tempo_alloy::TempoNetwork;
use tempo_chainspec::spec::TEMPO_T0_BASE_FEE;
use zone_primitives::constants::zone_chain_id;

use crate::zone_utils::MODERATO_ZONE_FACTORY;

sol! {
    struct ZoneParams {
        bytes32 genesisBlockHash;
        bytes32 genesisTempoBlockHash;
        uint64 genesisTempoBlockNumber;
    }

    struct CreateZoneParams {
        address initialToken;
        address admin;
        address sequencer;
        address verifier;
        ZoneParams zoneParams;
        string rpcUrl;
    }

    #[sol(rpc)]
    contract ZoneFactory {
        event ZoneCreated(
            uint32 indexed zoneId,
            address indexed portal,
            address indexed messenger,
            address initialToken,
            address admin,
            address sequencer,
            address verifier,
            bytes32 genesisBlockHash,
            bytes32 genesisTempoBlockHash,
            uint64 genesisTempoBlockNumber
        );

        function verifier() external view returns (address);
        function createZone(CreateZoneParams calldata params) external returns (uint32 zoneId, address portal);
    }
}

#[derive(Debug, clap::Parser)]
pub(crate) struct CreateZone {
    /// Output directory where genesis.json will be written.
    #[arg(short, long)]
    output: PathBuf,

    /// Tempo L1 HTTP RPC URL used to fetch headers and send the createZone transaction.
    #[arg(long, default_value = "https://rpc.moderato.tempo.xyz")]
    l1_rpc_url: String,

    /// ZoneFactory contract address on Tempo L1.
    #[arg(long, env = "ZONE_FACTORY", default_value_t = MODERATO_ZONE_FACTORY)]
    zone_factory: Address,

    /// Initial TIP-20 token address for the zone (additional tokens can be enabled later).
    /// Defaults to pathUSD (0x20C0000000000000000000000000000000000000).
    #[arg(long, default_value_t = address!("0x20C0000000000000000000000000000000000000"))]
    initial_token: Address,

    /// Sequencer address that will operate the zone.
    #[arg(long)]
    sequencer: Address,

    /// Admin address that controls token enablement and deposit pause/resume.
    /// Pass the sequencer address explicitly when both roles should use the same key.
    #[arg(long)]
    admin: Address,

    /// Public RPC endpoint for the zone, published on-chain in the portal.
    /// Can be left empty and set later via `ZonePortal.setRpcUrl`.
    #[arg(long, default_value = "")]
    rpc_url: String,

    /// Private key (hex) for signing the createZone transaction on L1.
    #[arg(long)]
    private_key: String,

    /// Base fee per gas for the zone L2.
    #[arg(long, default_value_t = TEMPO_T0_BASE_FEE.into())]
    base_fee_per_gas: u128,

    /// Genesis block gas limit for the zone L2.
    #[arg(long, default_value_t = 30_000_000)]
    gas_limit: u64,

    /// Path to the Foundry compiled output directory containing zone contract artifacts.
    #[arg(long, default_value = "specs/ref-impls/out")]
    specs_out: PathBuf,
}

impl CreateZone {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let key_str = self
            .private_key
            .strip_prefix("0x")
            .unwrap_or(&self.private_key);
        let signer: PrivateKeySigner = key_str.parse()?;
        let wallet = EthereumWallet::from(signer);
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .wallet(wallet)
            .connect(&self.l1_rpc_url)
            .await?;

        let factory = ZoneFactory::new(self.zone_factory, &provider);
        println!("Fetching verifier address from ZoneFactory...");
        let verifier = Address::from(factory.verifier().call().await?.0);
        println!("Verifier: {verifier}");

        // Anchor before createZone so the zone replays the creation block and its
        // initial TokenEnabled event during L1 backfill.
        let anchor_block_number = provider.get_block_number().await?;
        let anchor_header = provider
            .get_header_by_number(anchor_block_number.into())
            .await?
            .ok_or_else(|| eyre!("anchor header {anchor_block_number} not found"))?
            .inner
            .inner;
        let mut genesis_header_rlp = Vec::new();
        anchor_header.encode(&mut genesis_header_rlp);
        let anchor_hash = keccak256(&genesis_header_rlp);

        println!("Admin: {}", self.admin);
        println!("Sequencer: {}", self.sequencer);

        let params = CreateZoneParams {
            initialToken: self.initial_token,
            admin: self.admin,
            sequencer: self.sequencer,
            verifier,
            zoneParams: ZoneParams {
                genesisBlockHash: B256::ZERO,
                genesisTempoBlockHash: anchor_hash,
                genesisTempoBlockNumber: anchor_block_number,
            },
            rpcUrl: self.rpc_url.clone(),
        };

        println!(
            "Creating zone on L1 via ZoneFactory at {}...",
            self.zone_factory
        );
        let receipt = factory.createZone(params).send_sync().await?;
        println!("Transaction confirmed in block {:?}", receipt.block_number);
        println!("Status: {}", receipt.status());
        println!("Gas used: {:?}", receipt.gas_used);

        if !receipt.status() {
            return Err(eyre!(
                "createZone transaction reverted (tx: {:?})",
                receipt.transaction_hash
            ));
        }

        let event = receipt
            .inner
            .logs()
            .iter()
            .find_map(|log| {
                log.log_decode::<ZoneFactory::ZoneCreated>()
                    .ok()
                    .map(|decoded| decoded.inner.data)
            })
            .ok_or_else(|| eyre!("no ZoneCreated event in receipt"))?;

        let zone_id = event.zoneId;
        let portal = event.portal;
        let chain_id = zone_chain_id(zone_id);

        println!(
            "Using pre-creation block {} (hash: {anchor_hash}) as genesis anchor",
            anchor_header.inner.number
        );

        let header_rlp_hex = const_hex::encode(&genesis_header_rlp);

        let genesis_cmd = crate::generate_zone_genesis::GenerateZoneGenesis {
            output: self.output.clone(),
            chain_id,
            base_fee_per_gas: self.base_fee_per_gas,
            gas_limit: self.gas_limit,
            tempo_portal: portal,
            tempo_genesis_header_rlp: Some(header_rlp_hex),
            admin: self.admin,
            sequencer: Some(self.sequencer),
            specs_out: self.specs_out.clone(),
            with_createx: true,
            with_safe_deployer: true,
            with_create2_factory: true,
            with_zone_factory_bytecode: false,
        };
        genesis_cmd.run().await?;

        // Write zone.json with deployment metadata for downstream tooling (e.g. `just zone-up`).
        let zone_json = serde_json::json!({
            "zoneId": zone_id,
            "chainId": chain_id,
            "portal": format!("{portal}"),
            "initialToken": format!("{}", self.initial_token),
            "admin": format!("{}", self.admin),
            "sequencer": format!("{}", self.sequencer),
            "tempoAnchorBlock": anchor_header.inner.number,
            "zoneFactory": format!("{}", self.zone_factory),
            "rpcUrl": self.rpc_url,
        });
        let zone_json_path = self.output.join("zone.json");
        std::fs::write(
            &zone_json_path,
            serde_json::to_string_pretty(&zone_json).wrap_err("failed encoding zone.json")?,
        )
        .wrap_err("failed writing zone.json")?;

        println!("Zone created successfully!");
        println!("  Zone ID: {zone_id}");
        println!("  Chain ID: {chain_id}");
        println!("  Portal: {portal}");
        println!("  Initial Token: {}", self.initial_token);
        println!("  Admin: {}", self.admin);
        println!("  Sequencer: {}", self.sequencer);
        println!("  ZoneFactory: {}", self.zone_factory);
        if !self.rpc_url.is_empty() {
            println!("  RPC URL: {}", self.rpc_url);
        }
        println!("  Tempo anchor block: {}", anchor_header.inner.number);
        println!(
            "  Genesis written to: {}",
            self.output.join("genesis.json").display()
        );
        println!("  Zone metadata written to: {}", zone_json_path.display());

        Ok(())
    }
}
