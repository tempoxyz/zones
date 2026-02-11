use alloy::{
    network::{EthereumWallet, primitives::{HeaderResponse, ReceiptResponse}},
    primitives::{Address, B256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol,
};
use alloy_rlp::Encodable;
use eyre::eyre;
use std::path::PathBuf;
use tempo_alloy::TempoNetwork;
use tempo_chainspec::spec::TEMPO_BASE_FEE;

sol! {
    struct ZoneParams {
        bytes32 genesisBlockHash;
        bytes32 genesisTempoBlockHash;
        uint64 genesisTempoBlockNumber;
    }

    struct CreateZoneParams {
        address token;
        address sequencer;
        address verifier;
        ZoneParams zoneParams;
    }

    #[sol(rpc)]
    contract ZoneFactory {
        event ZoneCreated(
            uint64 indexed zoneId,
            address indexed portal,
            address indexed messenger,
            address token,
            address sequencer,
            address verifier,
            bytes32 genesisBlockHash,
            bytes32 genesisTempoBlockHash,
            uint64 genesisTempoBlockNumber
        );

        function verifier() external view returns (address);
        function createZone(CreateZoneParams calldata params) external returns (uint64 zoneId, address portal);
    }
}

#[derive(Debug, clap::Parser)]
pub(crate) struct CreateZone {
    /// Output directory where genesis.json will be written.
    #[arg(short, long)]
    output: PathBuf,

    /// Tempo L1 HTTP RPC URL used to fetch headers and send the createZone transaction.
    #[arg(long, default_value = "https://eng:bold-raman-silly-torvalds@rpc.moderato.tempo.xyz")]
    l1_rpc_url: String,

    /// ZoneFactory contract address on Tempo L1.
    #[arg(long, default_value = "0xb425C093b3f303f63d7af6bd85a45ae15De0d3d9")]
    zone_factory: Address,

    /// TIP-20 token address for the zone (same address on both Tempo and the zone L2).
    #[arg(long)]
    zone_token: Address,

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
    #[arg(long, default_value_t = TEMPO_BASE_FEE.into())]
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
        let key_str = self.private_key.strip_prefix("0x").unwrap_or(&self.private_key);
        let signer: PrivateKeySigner = key_str.parse()?;
        let wallet = EthereumWallet::from(signer);
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .wallet(wallet)
            .connect_http(self.l1_rpc_url.parse()?);

        println!("Fetching Tempo block header...");
        let block = provider
            .get_block_by_number(alloy::eips::BlockNumberOrTag::Latest)
            .await?
            .ok_or_else(|| eyre!("block not found"))?;

        let header = block.header.as_ref();
        let block_hash = block.header.hash();

        let mut header_rlp = Vec::new();
        header.encode(&mut header_rlp);
        println!("Tempo block {} (hash: {block_hash})", header.inner.number);

        let factory = ZoneFactory::new(self.zone_factory, &provider);
        println!("Fetching verifier address from ZoneFactory...");
        let verifier = Address::from(factory.verifier().call().await?.0);
        println!("Verifier: {verifier}");

        let tempo_block_number = header.inner.number;

        let params = CreateZoneParams {
            token: self.zone_token,
            sequencer: self.sequencer,
            verifier,
            zoneParams: ZoneParams {
                genesisBlockHash: B256::ZERO,
                genesisTempoBlockHash: block_hash,
                genesisTempoBlockNumber: tempo_block_number,
            },
        };

        println!(
            "Creating zone on L1 via ZoneFactory at {}...",
            self.zone_factory
        );
        let pending = factory.createZone(params).send().await?;
        println!("Transaction sent, waiting for receipt...");
        let receipt = pending.get_receipt().await?;
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

        let header_rlp_hex = const_hex::encode(&header_rlp);

        let genesis_cmd = crate::generate_zone_genesis::GenerateZoneGenesis {
            output: self.output.clone(),
            chain_id: self.chain_id,
            base_fee_per_gas: self.base_fee_per_gas,
            gas_limit: self.gas_limit,
            zone_token: self.zone_token,
            tempo_portal: portal,
            tempo_genesis_header_rlp: header_rlp_hex,
            sequencer: Some(self.sequencer),
            specs_out: self.specs_out.clone(),
        };
        genesis_cmd.run().await?;

        println!("Zone created successfully!");
        println!("  Zone ID: {zone_id}");
        println!("  Portal: {portal}");
        println!("  Token: {}", self.zone_token);
        println!("  Sequencer: {}", self.sequencer);
        println!("  Tempo anchor block: {tempo_block_number}");
        println!(
            "  Genesis written to: {}",
            self.output.join("genesis.json").display()
        );

        Ok(())
    }
}
