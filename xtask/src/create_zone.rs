use alloy::{
    network::{
        EthereumWallet,
        primitives::{HeaderResponse, ReceiptResponse},
    },
    primitives::{Address, B256, address},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol,
};
use alloy_rlp::Encodable;
use eyre::{WrapErr as _, eyre};
use std::path::PathBuf;
use tempo_alloy::TempoNetwork;
use tempo_chainspec::spec::TEMPO_T0_BASE_FEE;

sol! {
    struct ZoneParams {
        bytes32 genesisBlockHash;
        bytes32 genesisTempoBlockHash;
        uint64 genesisTempoBlockNumber;
    }

    struct CreateZoneParams {
        address initialToken;
        address sequencer;
        address verifier;
        ZoneParams zoneParams;
    }

    #[sol(rpc)]
    contract ZoneFactory {
        event ZoneCreated(
            uint32 indexed zoneId,
            address indexed portal,
            address indexed messenger,
            address initialToken,
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
    #[arg(
        long,
        default_value = "https://eng:bold-raman-silly-torvalds@rpc.moderato.tempo.xyz"
    )]
    l1_rpc_url: String,

    /// ZoneFactory contract address on Tempo L1.
    /// Default is the ZoneFactory deployed on moderato.
    #[arg(long, default_value_t = address!("0x7F4528b1a555D704bC20f8328557240BED29488D"))]
    zone_factory: Address,

    /// Initial TIP-20 token address for the zone (additional tokens can be enabled later).
    /// Defaults to pathUSD (0x20C0000000000000000000000000000000000000).
    #[arg(long, default_value_t = address!("0x20C0000000000000000000000000000000000000"))]
    initial_token: Address,

    /// Sequencer address that will operate the zone.
    #[arg(long)]
    sequencer: Address,

    /// Private key (hex) for signing the createZone transaction on L1.
    #[arg(long)]
    private_key: String,

    /// Zone L2 chain ID.
    #[arg(long, default_value_t = 13371)]
    chain_id: u64,

    /// Base fee per gas for the zone L2.
    #[arg(long, default_value_t = TEMPO_T0_BASE_FEE.into())]
    base_fee_per_gas: u128,

    /// Genesis block gas limit for the zone L2.
    #[arg(long, default_value_t = 30_000_000)]
    gas_limit: u64,

    /// Path to the Foundry compiled output directory containing zone contract artifacts.
    #[arg(long, default_value = "docs/specs/out")]
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

        // We cannot know the confirmation block number before sending, so we pass
        // the current block number. The tx typically confirms in the next block, but
        // zone.json records the actual confirmation block for the zone node to use
        // via --l1.genesis-block-number.
        let current_block = provider.get_block_number().await?;

        let params = CreateZoneParams {
            initialToken: self.initial_token,
            sequencer: self.sequencer,
            verifier,
            zoneParams: ZoneParams {
                genesisBlockHash: B256::ZERO,
                genesisTempoBlockHash: B256::ZERO,
                genesisTempoBlockNumber: current_block,
            },
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

        // Re-fetch the header from the block that included the `createZone` tx.
        // The portal (and its sequencer storage slot) only exists from this block onward,
        // so using the pre-tx header would cause `readTempoStorageSlot` to read empty state.
        let confirm_block_number = receipt
            .block_number
            .ok_or_else(|| eyre!("receipt missing block number"))?;
        let confirm_block = provider
            .get_block_by_number(confirm_block_number.into())
            .await?
            .ok_or_else(|| eyre!("confirmation block {confirm_block_number} not found"))?;
        let confirm_header = confirm_block.header.as_ref();
        let confirm_hash = confirm_block.header.hash();

        let mut genesis_header_rlp = Vec::new();
        confirm_header.encode(&mut genesis_header_rlp);

        println!(
            "Using confirmation block {} (hash: {confirm_hash}) as genesis anchor",
            confirm_header.inner.number
        );

        let header_rlp_hex = const_hex::encode(&genesis_header_rlp);

        let genesis_cmd = crate::generate_zone_genesis::GenerateZoneGenesis {
            output: self.output.clone(),
            chain_id: self.chain_id,
            base_fee_per_gas: self.base_fee_per_gas,
            gas_limit: self.gas_limit,
            tempo_portal: portal,
            tempo_genesis_header_rlp: header_rlp_hex,
            sequencer: Some(self.sequencer),
            specs_out: self.specs_out.clone(),
            with_createx: true,
            with_safe_deployer: true,
            with_create2_factory: true,
        };
        genesis_cmd.run().await?;

        // Write zone.json with deployment metadata for downstream tooling (e.g. `just zone-up`).
        let zone_json = serde_json::json!({
            "zoneId": zone_id,
            "portal": format!("{portal}"),
            "initialToken": format!("{}", self.initial_token),
            "sequencer": format!("{}", self.sequencer),
            "tempoAnchorBlock": confirm_header.inner.number,
            "zoneFactory": format!("{}", self.zone_factory),
        });
        let zone_json_path = self.output.join("zone.json");
        std::fs::write(
            &zone_json_path,
            serde_json::to_string_pretty(&zone_json).wrap_err("failed encoding zone.json")?,
        )
        .wrap_err("failed writing zone.json")?;

        println!("Zone created successfully!");
        println!("  Zone ID: {zone_id}");
        println!("  Portal: {portal}");
        println!("  Initial Token: {}", self.initial_token);
        println!("  Sequencer: {}", self.sequencer);
        println!("  ZoneFactory: {}", self.zone_factory);
        println!("  Tempo anchor block: {}", confirm_header.inner.number);
        println!(
            "  Genesis written to: {}",
            self.output.join("genesis.json").display()
        );
        println!("  Zone metadata written to: {}", zone_json_path.display());

        Ok(())
    }
}
