use alloy::{
    network::EthereumWallet,
    primitives::{Address, Bloom, Bytes, B256, B64, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol,
};
use alloy_consensus::Header;
use alloy_rlp::Encodable;
use eyre::{WrapErr as _, eyre};
use std::path::PathBuf;
use tempo_chainspec::spec::TEMPO_BASE_FEE;
use tempo_primitives::TempoHeader;

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

    /// Tempo block number to anchor the zone genesis to.
    /// The header at this block is RLP-encoded and stored in TempoState.
    /// Defaults to the latest block if not specified.
    #[arg(long)]
    tempo_block_number: Option<u64>,

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
        let provider = ProviderBuilder::new()
            .wallet(wallet)
            .connect_http(self.l1_rpc_url.parse()?);

        let block_number = match self.tempo_block_number {
            Some(n) => alloy::eips::BlockNumberOrTag::Number(n),
            None => alloy::eips::BlockNumberOrTag::Latest,
        };

        println!("Fetching Tempo block header...");
        let raw_block: serde_json::Value = retry(|| async {
            provider
                .raw_request("eth_getBlockByNumber".into(), (block_number, false))
                .await
        })
        .await?;

        let header = parse_tempo_header(&raw_block)?;
        let mut header_rlp = Vec::new();
        header.encode(&mut header_rlp);
        let computed_hash = alloy_primitives::keccak256(&header_rlp);
        let expected_hash: B256 = raw_block
            .get("hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| eyre!("no hash in block"))?
            .parse()?;

        if computed_hash != expected_hash {
            return Err(eyre!(
                "reconstructed header hash {computed_hash} does not match block hash {expected_hash}"
            ));
        }
        println!("Tempo block {} header validated (hash: {computed_hash})", header.inner.number);

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let factory = ZoneFactory::new(self.zone_factory, &provider);
        println!("Fetching verifier address from ZoneFactory...");
        let verifier = Address::from(retry(|| async { factory.verifier().call().await }).await?.0);
        println!("Verifier: {verifier}");

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let tempo_block_number = header.inner.number;
        let tempo_block_hash = computed_hash;

        let params = CreateZoneParams {
            token: self.zone_token,
            sequencer: self.sequencer,
            verifier,
            zoneParams: ZoneParams {
                genesisBlockHash: B256::ZERO,
                genesisTempoBlockHash: tempo_block_hash,
                genesisTempoBlockNumber: tempo_block_number,
            },
        };

        println!("Creating zone on L1 via ZoneFactory at {}...", self.zone_factory);
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let pending = factory.createZone(params).send().await?;
        println!("Transaction sent, waiting for receipt...");
        let receipt = pending.get_receipt().await?;
        println!("Transaction confirmed in block {:?}", receipt.block_number);
        println!("Status: {}", receipt.status());
        println!("Gas used: {:?}", receipt.gas_used);
        println!("Logs: {}", receipt.inner.logs().len());

        if !receipt.status() {
            return Err(eyre!(
                "createZone transaction reverted (tx: {:?})",
                receipt.transaction_hash
            ));
        }

        let zone_created_events: Vec<ZoneFactory::ZoneCreated> = receipt
            .inner
            .logs()
            .iter()
            .filter_map(|log| {
                log.log_decode::<ZoneFactory::ZoneCreated>()
                    .ok()
                    .map(|decoded| decoded.inner.data)
            })
            .collect();

        let event = zone_created_events
            .first()
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

fn parse_tempo_header(block: &serde_json::Value) -> eyre::Result<TempoHeader> {
    let general_gas_limit = parse_u64_hex(block.get("mainBlockGeneralGasLimit"))?;
    let shared_gas_limit = parse_u64_hex(block.get("sharedGasLimit"))?;
    let timestamp_millis_part = parse_u64_hex(block.get("timestampMillisPart"))?;

    let inner = Header {
        parent_hash: parse_b256(block.get("parentHash"))?,
        ommers_hash: parse_b256(block.get("sha3Uncles"))?,
        beneficiary: parse_address(block.get("miner"))?,
        state_root: parse_b256(block.get("stateRoot"))?,
        transactions_root: parse_b256(block.get("transactionsRoot"))?,
        receipts_root: parse_b256(block.get("receiptsRoot"))?,
        logs_bloom: parse_bloom(block.get("logsBloom"))?,
        difficulty: parse_u256(block.get("difficulty"))?,
        number: parse_u64_hex(block.get("number"))?,
        gas_limit: parse_u64_hex(block.get("gasLimit"))?,
        gas_used: parse_u64_hex(block.get("gasUsed"))?,
        timestamp: parse_u64_hex(block.get("timestamp"))?,
        extra_data: parse_bytes(block.get("extraData"))?,
        mix_hash: parse_b256(block.get("mixHash"))?,
        nonce: parse_b64(block.get("nonce"))?,
        base_fee_per_gas: block
            .get("baseFeePerGas")
            .and_then(|v| parse_u64_hex(Some(v)).ok()),
        withdrawals_root: block
            .get("withdrawalsRoot")
            .and_then(|v| parse_b256(Some(v)).ok()),
        blob_gas_used: block
            .get("blobGasUsed")
            .and_then(|v| parse_u64_hex(Some(v)).ok()),
        excess_blob_gas: block
            .get("excessBlobGas")
            .and_then(|v| parse_u64_hex(Some(v)).ok()),
        parent_beacon_block_root: block
            .get("parentBeaconBlockRoot")
            .and_then(|v| parse_b256(Some(v)).ok()),
        requests_hash: block
            .get("requestsHash")
            .and_then(|v| parse_b256(Some(v)).ok()),
    };

    Ok(TempoHeader {
        general_gas_limit,
        shared_gas_limit,
        timestamp_millis_part,
        inner,
    })
}

fn parse_u64_hex(val: Option<&serde_json::Value>) -> eyre::Result<u64> {
    let s = val
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("missing hex field"))?;
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    u64::from_str_radix(stripped, 16).wrap_err("invalid u64 hex")
}

fn parse_u256(val: Option<&serde_json::Value>) -> eyre::Result<U256> {
    let s = val
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("missing hex field"))?;
    s.parse::<U256>().wrap_err("invalid U256")
}

fn parse_b256(val: Option<&serde_json::Value>) -> eyre::Result<B256> {
    let s = val
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("missing bytes32 field"))?;
    s.parse::<B256>().wrap_err("invalid B256")
}

fn parse_address(val: Option<&serde_json::Value>) -> eyre::Result<Address> {
    let s = val
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("missing address field"))?;
    s.parse::<Address>().wrap_err("invalid address")
}

fn parse_bloom(val: Option<&serde_json::Value>) -> eyre::Result<Bloom> {
    let s = val
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("missing bloom field"))?;
    s.parse::<Bloom>().wrap_err("invalid bloom")
}

fn parse_bytes(val: Option<&serde_json::Value>) -> eyre::Result<Bytes> {
    let s = val
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("missing bytes field"))?;
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    Ok(Bytes::from(const_hex::decode(stripped)?))
}

fn parse_b64(val: Option<&serde_json::Value>) -> eyre::Result<B64> {
    let s = val
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre!("missing B64 field"))?;
    s.parse::<B64>().wrap_err("invalid B64")
}

async fn retry<F, Fut, T, E>(mut f: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    E: std::fmt::Debug,
{
    for attempt in 0..5 {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempt == 4 {
                    return Err(e);
                }
                let delay = std::time::Duration::from_millis(200 * (attempt + 1));
                eprintln!("RPC call failed (attempt {}), retrying in {:?}: {e:?}", attempt + 1, delay);
                tokio::time::sleep(delay).await;
            }
        }
    }
    unreachable!()
}
