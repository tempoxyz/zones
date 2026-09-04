// The `sol!`generated `ZoneFactory` event/contract bindings expand to functions
// with more than 7 parameters, which trips `clippy::too_many_arguments`.
#![allow(clippy::too_many_arguments)]

use alloy::{
    network::{EthereumWallet, primitives::ReceiptResponse},
    primitives::{Address, address},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    sol_types::SolEvent,
};
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
use zone_sequencer::register_encryption_key;

use crate::{
    generate_zone_genesis::wait_for_finalized_pre_creation_anchor,
    zone_utils::{MODERATO_ZONE_FACTORY, write_owner_only},
};

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

    /// Admin address that controls token enablement and deposit pause/unpause.
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

    /// Optional shared sequencer private key (hex). When provided, registers its public key for
    /// encrypted deposits after creating the zone.
    #[arg(long, env = "SEQUENCER_KEY", hide_env_values = true)]
    sequencer_key: Option<String>,

    /// Optional portal admin or active sequencer private key that sends the encryption-key
    /// registration transaction. Defaults to the encryption key when it has either role.
    #[arg(
        long,
        env = "TRANSACTION_PRIVATE_KEY",
        hide_env_values = true,
        requires = "sequencer_key"
    )]
    transaction_private_key: Option<String>,

    /// Base fee per gas for the zone L2.
    #[arg(long, default_value_t = TEMPO_T0_BASE_FEE.into())]
    base_fee_per_gas: u128,

    /// Genesis block gas limit for the zone L2.
    #[arg(long, default_value_t = 30_000_000)]
    gas_limit: u64,
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

        let encryption_key_registration = parse_encryption_key_registration(
            self.sequencer_key.as_deref(),
            self.transaction_private_key.as_deref(),
            self.admin,
            &self.sequencers,
        )?;
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
        let parent_chain_id = provider.get_chain_id().await?;
        let chain_id = zone_chain_id(parent_chain_id, zone_id)?;

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

        println!("Waiting for creation block {creation_block} to finalize...");
        let anchor =
            wait_for_finalized_pre_creation_anchor(&provider, portal, creation_block).await?;
        println!(
            "Using pre-creation block {} (hash: {}) as genesis anchor",
            anchor.block_number, anchor.hash
        );

        let header_rlp_hex = const_hex::encode(&anchor.rlp);

        let genesis_cmd = crate::generate_zone_genesis::GenerateZoneGenesis {
            output: self.output.clone(),
            chain_id,
            base_fee_per_gas: self.base_fee_per_gas,
            gas_limit: self.gas_limit,
            tempo_portal: None,
            l1_rpc_url: None,
            default_fee_token: self.initial_token,
            tempo_genesis_header_rlp: Some(header_rlp_hex),
            admin: self.admin,
            sequencer: Some(leader),
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
            "tempoAnchorBlock": anchor.block_number,
            "zoneFactory": format!("{}", self.zone_factory),
            "rpcUrl": self.rpc_url,
        });
        let zone_json_path = self.output.join("zone.json");
        write_owner_only(
            &zone_json_path,
            serde_json::to_string_pretty(&zone_json).wrap_err("failed encoding zone.json")?,
        )
        .wrap_err("failed writing zone.json")?;

        if let Some((encryption_signer, portal_signer)) = encryption_key_registration {
            let sequencer_provider = ProviderBuilder::new_with_network::<TempoNetwork>()
                .wallet(EthereumWallet::from(portal_signer))
                .connect(&self.l1_rpc_url)
                .await
                .wrap_err("failed connecting to Tempo L1 RPC for encryption-key registration")?;
            println!("Registering sequencer encryption key on ZonePortal...");
            let tx_hash = register_encryption_key(&sequencer_provider, portal, &encryption_signer)
                .await
                .wrap_err("failed to register sequencer encryption key")?;
            println!("Encryption key registered  [tx: {tx_hash}]");
        }

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
        println!("  Tempo anchor block: {}", anchor.block_number);
        println!(
            "  Genesis written to: {}",
            self.output.join("genesis.json").display()
        );
        println!("  Zone metadata written to: {}", zone_json_path.display());

        Ok(())
    }
}

fn parse_private_key(key: &str) -> eyre::Result<PrivateKeySigner> {
    key.strip_prefix("0x")
        .unwrap_or(key)
        .parse()
        .map_err(Into::into)
}

fn parse_encryption_key_registration(
    encryption_key: Option<&str>,
    transaction_private_key: Option<&str>,
    admin: Address,
    sequencers: &[Address],
) -> eyre::Result<Option<(PrivateKeySigner, PrivateKeySigner)>> {
    let Some(encryption_key) = encryption_key else {
        ensure!(
            transaction_private_key.is_none(),
            "TRANSACTION_PRIVATE_KEY requires SEQUENCER_KEY"
        );
        return Ok(None);
    };
    let encryption_signer =
        parse_private_key(encryption_key).wrap_err("SEQUENCER_KEY is not a valid private key")?;
    let (portal_signer, portal_signer_source) = match transaction_private_key {
        Some(key) => (
            parse_private_key(key)
                .wrap_err("TRANSACTION_PRIVATE_KEY is not a valid private key")?,
            "TRANSACTION_PRIVATE_KEY",
        ),
        None => (encryption_signer.clone(), "SEQUENCER_KEY"),
    };
    ensure!(
        portal_signer.address() == admin || sequencers.contains(&portal_signer.address()),
        "{portal_signer_source} resolves to {}, but that address is neither the portal admin nor in the configured sequencer set; set TRANSACTION_PRIVATE_KEY to an authorized portal signer",
        portal_signer.address(),
    );
    Ok(Some((encryption_signer, portal_signer)))
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
            sequencer_key: None,
            transaction_private_key: None,
            base_fee_per_gas: 1,
            gas_limit: 30_000_000,
        };

        let params = command.factory_params();
        assert_eq!(params.sequencers, sequencers);
        assert_eq!(params.threshold, 2);
    }

    #[test]
    fn defaults_portal_signer_to_encryption_key() {
        let key = "0000000000000000000000000000000000000000000000000000000000000001";
        let signer = parse_private_key(key).unwrap();
        let (encryption_signer, portal_signer) =
            parse_encryption_key_registration(Some(key), None, Address::ZERO, &[signer.address()])
                .unwrap()
                .unwrap();

        assert_eq!(encryption_signer.address(), signer.address());
        assert_eq!(portal_signer.address(), signer.address());
    }

    #[test]
    fn supports_distinct_encryption_and_portal_signers() {
        let encryption_key = "0000000000000000000000000000000000000000000000000000000000000001";
        let portal_key = "0000000000000000000000000000000000000000000000000000000000000002";
        let portal_signer = parse_private_key(portal_key).unwrap();
        let (encryption_signer, parsed_portal_signer) = parse_encryption_key_registration(
            Some(encryption_key),
            Some(portal_key),
            Address::ZERO,
            &[portal_signer.address()],
        )
        .unwrap()
        .unwrap();

        assert_ne!(encryption_signer.address(), portal_signer.address());
        assert_eq!(parsed_portal_signer.address(), portal_signer.address());
    }

    #[test]
    fn accepts_portal_admin_as_transaction_signer() {
        let encryption_key = "0000000000000000000000000000000000000000000000000000000000000001";
        let admin_key = "0000000000000000000000000000000000000000000000000000000000000002";
        let admin = parse_private_key(admin_key).unwrap();
        let (_, portal_signer) = parse_encryption_key_registration(
            Some(encryption_key),
            Some(admin_key),
            admin.address(),
            &[address!("0x2000000000000000000000000000000000000002")],
        )
        .unwrap()
        .unwrap();

        assert_eq!(portal_signer.address(), admin.address());
    }

    #[test]
    fn rejects_portal_signer_outside_the_sequencer_set() {
        let error = parse_encryption_key_registration(
            Some("0000000000000000000000000000000000000000000000000000000000000001"),
            Some("0000000000000000000000000000000000000000000000000000000000000002"),
            address!("0x3000000000000000000000000000000000000003"),
            &[address!("0x2000000000000000000000000000000000000002")],
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("TRANSACTION_PRIVATE_KEY resolves to")
        );
    }
}
