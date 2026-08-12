use alloy::{
    primitives::{Address, U256},
    providers::ProviderBuilder,
};
use eyre::eyre;
use tempo_alloy::TempoNetwork;
use tempo_zone_contracts::{ZONE_MESSENGER_ADDRESS, ZoneFactory, ZonePortal};

use crate::zone_utils::MODERATO_ZONE_FACTORY;

#[derive(Debug, clap::Parser)]
pub(crate) struct ZoneInfoCmd {
    /// Zone ID (integer) or portal address (0x...) to look up.
    identifier: String,

    /// Tempo L1 HTTP RPC URL.
    #[arg(long, default_value = "https://rpc.moderato.tempo.xyz")]
    l1_rpc_url: String,

    /// ZoneFactory contract address on Tempo L1.
    #[arg(long, env = "ZONE_FACTORY", default_value_t = MODERATO_ZONE_FACTORY)]
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
            let next_zone_id = factory.nextZoneId().call().await?;

            let mut found = None;
            for id in 1..next_zone_id {
                let info = factory.zones(id).call().await?;
                if info.portal == portal {
                    found = Some(id);
                    break;
                }
            }
            found.ok_or_else(|| eyre!("no zone found with portal address {portal}"))?
        } else {
            self.identifier
                .parse::<u32>()
                .map_err(|_| eyre!("expected a zone ID (integer) or portal address (0x...)"))?
        };

        let info = factory.zones(zone_id).call().await?;
        if info.portal == Address::ZERO {
            return Err(eyre!("zone {zone_id} does not exist"));
        }
        println!("Zone {}", info.zoneId);
        println!("  Portal:                {}", info.portal);
        println!("  Messenger:             {ZONE_MESSENGER_ADDRESS}");
        println!("  Admin:                 {}", info.admin);
        println!("  Sequencers:            {:?}", info.sequencers);
        println!("  Threshold:             {}", info.threshold);
        println!("  Verifier:              {}", info.verifier);
        println!("  RPC URL:               {}", info.rpcUrl);

        // Query live portal state
        let portal = ZonePortal::new(info.portal, &provider);

        let sequencer_count = portal.sequencerCount().call().await?.to::<usize>();
        let mut sequencers = Vec::with_capacity(sequencer_count);
        for index in 0..sequencer_count {
            sequencers.push(portal.sequencerAt(U256::from(index)).call().await?);
        }
        let gas_rate = portal.zoneGasRate().call().await?;
        let batch_index = portal.withdrawalBatchIndex().call().await?;
        let block_hash = portal.blockHash().call().await?;
        let deposit_queue = portal.currentDepositQueueHash().call().await?;
        let last_synced = portal.lastSyncedTempoBlockNumber().call().await?;
        let set_version = portal.sequencerSetVersion().call().await?;
        let leader = portal.leader().call().await?;
        let leader_epoch = portal.leaderEpoch().call().await?;
        let leader_activation = portal.leaderActivationTempoBlock().call().await?;
        let paused = portal.paused().call().await?;
        let pause_expiry = portal.pauseExpiry().call().await?;
        let pause_abdication_effective_at = portal
            .abdicationEffectiveAt(ZonePortal::Capability::PausePortal)
            .call()
            .await?;
        let access_abdication_effective_at = portal
            .abdicationEffectiveAt(ZonePortal::Capability::AccessPolicy)
            .call()
            .await?;

        println!("\nPortal State");
        println!("  Active Sequencers:     {sequencers:?}");
        println!("  Sequencer Set Version: {set_version}");
        if leader.is_zero() {
            println!("  Leader:                (uninitialized)");
        } else {
            println!("  Leader:                {leader}");
            println!("  Leader Epoch:          {leader_epoch}");
            println!("  Leader Activation:     Tempo block {leader_activation}");
        }
        println!("  Zone Gas Rate:         {gas_rate}");
        println!("  Paused:                {paused}");
        println!("  Pause expiry:          {pause_expiry}");
        println!("  Pause abdication at:   {pause_abdication_effective_at}");
        println!("  Access abdication at:  {access_abdication_effective_at}");
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
        let tokens = portal.enabled_tokens().await?;
        println!("\nEnabled Tokens ({})", tokens.len());
        for (i, token) in tokens.iter().enumerate() {
            println!("  [{i}] {token}");
        }

        Ok(())
    }
}
