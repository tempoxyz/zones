// The `sol!`generated `ZoneFactory` event/contract bindings expand to functions
// with more than 7 parameters, which trips `clippy::too_many_arguments`.
#![allow(clippy::too_many_arguments)]

use alloy::{
    network::{EthereumWallet, primitives::ReceiptResponse},
    primitives::{Address, address, keccak256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol_types::SolEvent,
};
use alloy_rlp::Encodable;
use alloy_rpc_types_eth::BlockId;
use eyre::{WrapErr as _, ensure, eyre};
use std::path::PathBuf;
use tempo_alloy::TempoNetwork;
use tempo_chainspec::spec::TEMPO_T0_BASE_FEE;
use tempo_contracts::precompiles::ITIP403Registry;
use tempo_precompiles::TIP403_REGISTRY_ADDRESS;
use tempo_zone_contracts::{
    ZONE_MESSENGER_ADDRESS, ZONE_VERIFIER_ADDRESS, ZoneFactory, ZonePortal,
};
use zone_primitives::constants::zone_chain_id;

use crate::zone_utils::MODERATO_ZONE_FACTORY;

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
    #[arg(long, default_value_t = address!("0x20C0000000000000000000000000000000000000"))]
    initial_token: Address,

    /// Enable account allowlist enforcement. Membership is retained while disabled.
    #[arg(long)]
    access_mode: bool,

    /// Enable callback gateway registration enforcement.
    #[arg(long)]
    gateway_mode: bool,

    /// Callback-only ZoneGateway implementation. Repeat to support legacy and replacement gateways.
    #[arg(long = "zone-gateway")]
    zone_gateways: Vec<Address>,

    /// Allowed plain-withdrawal/deposit account. Repeat for each member.
    /// Zone gateways are configured separately and must not be included.
    #[arg(long = "allowed-account")]
    allowed_accounts: Vec<Address>,

    /// Sequencer address that will operate the zone. Repeat for a
    /// multi-sequencer set; the first address is the leader.
    ///
    /// The complete set and threshold are installed atomically by `createZone`;
    /// the first address is also the initial block-production leader.
    #[arg(long = "sequencer", required = true)]
    sequencers: Vec<Address>,

    /// Number of sequencer signatures required for a settlement attestation
    /// quorum. Must be between 1 and the number of sequencers.
    #[arg(long, default_value_t = 1)]
    threshold: u8,

    /// Admin address that controls token enablement and deposit pause/resume.
    /// Pass the sequencer address explicitly when both roles should use the same key.
    #[arg(long)]
    admin: Address,

    /// Operator RPC endpoint for the zone, published on-chain in the portal.
    /// Can be left empty and set later via `ZonePortal.setRpcUrl`.
    #[arg(long, default_value = "")]
    rpc_url: String,

    /// ZoneFactory owner private key (hex) for signing the createZone transaction on L1.
    /// Prefer the ZONE_FACTORY_OWNER_KEY environment variable so the key is not exposed in the
    /// process argument list.
    #[arg(long, env = "ZONE_FACTORY_OWNER_KEY", hide_env_values = true)]
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

/// Mirrors `ZonePortal.MAX_SEQUENCERS` for a fast client-side error.
const MAX_SEQUENCERS: usize = 8;

impl CreateZone {
    fn factory_params(&self) -> ZoneFactory::CreateZoneParams {
        ZoneFactory::CreateZoneParams {
            initialToken: self.initial_token,
            accessMode: self.access_mode,
            gatewayMode: self.gateway_mode,
            allowedAccounts: self.allowed_accounts.clone(),
            zoneGateways: self.zone_gateways.clone(),
            admin: self.admin,
            sequencers: self.sequencers.clone(),
            threshold: self.threshold,
            rpcUrl: self.rpc_url.clone(),
        }
    }

    pub(crate) async fn run(self) -> eyre::Result<()> {
        let leader = *self
            .sequencers
            .first()
            .ok_or_else(|| eyre!("at least one --sequencer is required"))?;
        if self.sequencers.len() > MAX_SEQUENCERS {
            return Err(eyre!(
                "at most {MAX_SEQUENCERS} sequencers are supported, got {}",
                self.sequencers.len()
            ));
        }
        for (i, sequencer) in self.sequencers.iter().enumerate() {
            if sequencer.is_zero() {
                return Err(eyre!("sequencer address must not be zero"));
            }
            if self.sequencers[..i].contains(sequencer) {
                return Err(eyre!("duplicate sequencer address {sequencer}"));
            }
        }
        if self.threshold == 0 || usize::from(self.threshold) > self.sequencers.len() {
            return Err(eyre!(
                "threshold must be between 1 and the number of sequencers ({}), got {}",
                self.sequencers.len(),
                self.threshold
            ));
        }
        if self.sequencers.len() > 1 && self.threshold < 2 {
            // With threshold 1 a leader can settle blocks no follower holds, so an
            // empty-disk leader recovery cannot reconstruct the settled chain from
            // follower replicas. Threshold >= 2 guarantees every settled batch carries
            // at least one follower signature.
            println!(
                "WARNING: multi-sequencer zone with settlement threshold 1: settled blocks \
                 may not be recoverable from followers after leader disk loss; use a \
                 threshold of at least 2"
            );
        }

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

        println!("Verifier: {ZONE_VERIFIER_ADDRESS}");
        println!("Messenger: {ZONE_MESSENGER_ADDRESS}");

        let registry = ITIP403Registry::new(TIP403_REGISTRY_ADDRESS, &provider);
        let mut policy = registry
            .tokenTransferPolicyId(self.initial_token)
            .call()
            .await?;
        if !policy.isSet {
            println!(
                "Migrating legacy transfer policy for initial token {}...",
                self.initial_token
            );
            let receipt = registry
                .migrateTransferPolicyIds(vec![self.initial_token])
                .send_sync()
                .await?;
            if !receipt.status() {
                return Err(eyre!(
                    "transfer policy migration reverted (tx: {:?})",
                    receipt.transaction_hash
                ));
            }

            policy = registry
                .tokenTransferPolicyId(self.initial_token)
                .call()
                .await?;
        }
        if !policy.isSet {
            return Err(eyre!(
                "transfer policy is not set for initial token {} after migration",
                self.initial_token
            ));
        }

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
        println!("Sequencers: {:?}", self.sequencers);
        println!("Threshold: {}", self.threshold);

        println!(
            "Creating zone on L1 via ZoneFactory at {}...",
            self.zone_factory
        );
        // Install the requested set in the factory transaction. A separate
        // setSequencerSet call would leave the portal live as a temporary
        // 1-of-1 settlement authority before the intended quorum is active.
        // The portal bootstraps the first sequencer as the initial
        // block-production leader (leaderEpoch 1); later transfers go through
        // setLeader. The factory-installed set starts at version 0.
        let receipt = factory
            .createZone(self.factory_params())
            .send_sync()
            .await?;
        println!("Transaction confirmed in block {:?}", receipt.block_number);
        println!("Status: {}", receipt.status());
        println!("Gas used: {:?}", receipt.gas_used);

        if !receipt.status() {
            return Err(eyre!(
                "createZone transaction reverted (tx: {:?})",
                receipt.transaction_hash
            ));
        }
        let creation_block = receipt
            .block_number
            .ok_or_else(|| eyre!("createZone receipt is missing its block number"))?;

        let event = receipt
            .inner
            .logs()
            .iter()
            .find_map(|log| ZoneFactory::ZoneCreated::decode_log(&log.inner).ok())
            .ok_or_else(|| eyre!("no ZoneCreated event in receipt"))?;

        let zone_id = event.zoneId;
        let portal = event.portal;
        let chain_id = zone_chain_id(zone_id);

        let portal_contract = ZonePortal::new(portal, &provider);
        let creation_block_id = BlockId::number(creation_block);
        let sequencer_set_version = portal_contract
            .sequencerSetVersion()
            .block(creation_block_id)
            .call()
            .await?;
        let initial_leader = portal_contract
            .leader()
            .block(creation_block_id)
            .call()
            .await?;
        let leader_epoch = portal_contract
            .leaderEpoch()
            .block(creation_block_id)
            .call()
            .await?;
        let leader_activation_block = portal_contract
            .leaderActivationTempoBlock()
            .block(creation_block_id)
            .call()
            .await?;

        ensure!(
            sequencer_set_version == 0,
            "ZoneFactory initialized sequencer set version {sequencer_set_version}, expected 0"
        );
        ensure!(
            initial_leader == leader
                && leader_epoch == 1
                && leader_activation_block == creation_block,
            "ZoneFactory initialized leader snapshot ({initial_leader}, epoch {leader_epoch}, activation {leader_activation_block}), expected ({leader}, epoch 1, activation {creation_block})"
        );
        println!("Sequencer set version: {sequencer_set_version}");

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
            default_fee_token: self.initial_token,
            tempo_genesis_header_rlp: Some(header_rlp_hex),
            admin: self.admin,
            sequencer: Some(leader),
            specs_out: self.specs_out.clone(),
            with_createx: true,
            with_safe_deployer: true,
            with_create2_factory: true,
        };
        genesis_cmd.run().await?;

        // Write zone.json with deployment metadata for downstream tooling (e.g. `just zone-up`).
        let zone_json = serde_json::json!({
            "zoneId": zone_id,
            "chainId": chain_id,
            "portal": format!("{portal}"),
            "messenger": format!("{ZONE_MESSENGER_ADDRESS}"),
            "initialToken": format!("{}", self.initial_token),
            "accessMode": self.access_mode,
            "gatewayMode": self.gateway_mode,
            "zoneGateways": self.zone_gateways.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "allowedAccounts": self.allowed_accounts.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "admin": format!("{}", self.admin),
            "sequencer": format!("{leader}"),
            "sequencers": self.sequencers.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "sequencerThreshold": self.threshold,
            "sequencerSetVersion": sequencer_set_version,
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
        println!("  Messenger: {ZONE_MESSENGER_ADDRESS}");
        println!("  Initial Token: {}", self.initial_token);
        println!("  Access enforcement: {}", self.access_mode);
        println!("  Gateway enforcement: {}", self.gateway_mode);
        println!("  Admin: {}", self.admin);
        println!("  Sequencers: {:?}", self.sequencers);
        println!("  Threshold: {}", self.threshold);
        println!("  Sequencer set version: {sequencer_set_version}");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_params_install_the_requested_quorum_atomically() {
        let sequencers = vec![
            address!("0x1000000000000000000000000000000000000001"),
            address!("0x2000000000000000000000000000000000000002"),
            address!("0x3000000000000000000000000000000000000003"),
        ];
        let command = CreateZone {
            output: PathBuf::new(),
            l1_rpc_url: String::new(),
            zone_factory: Address::ZERO,
            initial_token: address!("0x4000000000000000000000000000000000000004"),
            access_mode: true,
            gateway_mode: true,
            zone_gateways: Vec::new(),
            allowed_accounts: Vec::new(),
            sequencers: sequencers.clone(),
            threshold: 2,
            admin: address!("0x5000000000000000000000000000000000000005"),
            rpc_url: String::new(),
            private_key: String::new(),
            base_fee_per_gas: 1,
            gas_limit: 30_000_000,
            specs_out: PathBuf::new(),
        };

        let params = command.factory_params();
        assert_eq!(params.sequencers, sequencers);
        assert_eq!(params.threshold, 2);
    }
}
