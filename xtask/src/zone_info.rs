use alloy::{
    primitives::{Address, U256},
    providers::ProviderBuilder,
};
use eyre::eyre;
use tempo_alloy::TempoNetwork;
use zone::abi::{ZoneFactory, ZonePortal};

#[derive(Debug, clap::Parser)]
pub(crate) struct ZoneInfoCmd {
    /// Zone ID (integer) or portal address (0x...) to look up.
    identifier: String,

    /// Tempo L1 HTTP RPC URL.
    #[arg(
        long,
        default_value = "https://eng:bold-raman-silly-torvalds@rpc.moderato.tempo.xyz"
    )]
    l1_rpc_url: String,

    /// ZoneFactory contract address on Tempo L1.
    #[arg(long, default_value = "0x8F3F0d21D01648d9373B3688CAc91b5253D3874C")]
    zone_factory: Address,
}

impl ZoneInfoCmd {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&self.l1_rpc_url)
            .await?;

        let factory = ZoneFactory::new(self.zone_factory, &provider);

        let zone_id = if self.identifier.starts_with("0x") {
            // Look up by portal address — scan all zones
            let portal: Address = self.identifier.parse()?;
            let count = factory.zoneCount().call().await?;

            let mut found = None;
            for id in 1..=count {
                let info = factory.zones(id).call().await?;
                if info.portal == portal {
                    found = Some(id);
                    break;
                }
            }
            found.ok_or_else(|| eyre!("no zone found with portal address {portal}"))?
        } else {
            self.identifier
                .parse::<u64>()
                .map_err(|_| eyre!("expected a zone ID (integer) or portal address (0x...)"))?
        };

        let info = factory.zones(zone_id).call().await?;
        if info.portal == Address::ZERO {
            return Err(eyre!("zone {zone_id} does not exist"));
        }

        println!("Zone {}", info.zoneId);
        println!("  Portal:                {}", info.portal);
        println!("  Messenger:             {}", info.messenger);
        println!("  Initial Token:         {}", info.initialToken);
        println!("  Sequencer:             {}", info.sequencer);
        println!("  Verifier:              {}", info.verifier);
        println!("  Genesis Block Hash:    {}", info.genesisBlockHash);
        println!("  Genesis Tempo Hash:    {}", info.genesisTempoBlockHash);
        println!("  Genesis Tempo Block:   {}", info.genesisTempoBlockNumber);

        // Query live portal state
        let portal = ZonePortal::new(info.portal, &provider);

        let sequencer = portal.sequencer().call().await?;
        let pending = portal.pendingSequencer().call().await?;
        let gas_rate = portal.zoneGasRate().call().await?;
        let batch_index = portal.withdrawalBatchIndex().call().await?;
        let block_hash = portal.blockHash().call().await?;
        let deposit_queue = portal.currentDepositQueueHash().call().await?;
        let last_synced = portal.lastSyncedTempoBlockNumber().call().await?;

        println!("\nPortal State");
        println!("  Sequencer (live):      {sequencer}");
        if pending != Address::ZERO {
            println!("  Pending Sequencer:     {pending}");
        }
        println!("  Zone Gas Rate:         {gas_rate}");
        println!("  Withdrawal Batch:      {batch_index}");
        println!("  Block Hash:            {block_hash}");
        println!("  Deposit Queue Hash:    {deposit_queue}");
        println!("  Last Synced Block:     {last_synced}");

        // Encryption key
        match portal.sequencerEncryptionKey().call().await {
            Ok(key) => {
                println!("\nEncryption Key");
                println!("  X:                     {}", key.x);
                println!("  Y Parity:              0x{:02x}", key.yParity);
            }
            Err(_) => println!("\nEncryption Key:          (not set)"),
        }

        // Enabled tokens
        let token_count = portal.enabledTokenCount().call().await?;
        println!("\nEnabled Tokens ({token_count})");
        for i in 0..token_count.to::<u64>() {
            let token = portal.enabledTokenAt(U256::from(i)).call().await?;
            println!("  [{i}] {token}");
        }

        Ok(())
    }
}
