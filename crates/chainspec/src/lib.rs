//! Zone chain specification.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

use alloy_eips::{eip1559::BaseFeeParams, eip7840::BlobParams};
use alloy_evm::eth::spec::EthExecutorSpec;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, U256};
use reth_chainspec::{
    Chain, DepositContract, EthChainSpec, EthereumHardfork, EthereumHardforks, ForkCondition,
    ForkFilter, ForkId, Hardfork, Hardforks, Head,
};
use reth_network_peers::NodeRecord;
use std::{fmt::Display, sync::Arc};
use tempo_chainspec::{
    TempoChainSpec, TempoConsensusSpec,
    hardfork::TempoHardfork,
    spec::{DEV, TempoHardforks, chainspec_from_chain_id},
};
use tempo_primitives::TempoHeader;
pub use zone_hardfork::ZoneHardfork;
use zone_primitives::constants::{ZoneChainIdError, decode_l1_chain_id, decode_zone_chain_id};

/// Chain specification for a Tempo Zone.
///
/// Zone, Tempo, and Ethereum activations all live in the underlying canonical hardfork schedule;
/// the typed query traits keep the protocol axes independently addressable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneChainSpec {
    /// Underlying Tempo chain specification, extended with Zone hardfork activations.
    pub inner: Arc<TempoChainSpec>,
}

/// Typed queries for Zone-owned hardfork activations.
pub trait ZoneHardforks: TempoHardforks {
    /// Returns the activation condition for a Zone-owned hardfork.
    fn zone_fork_activation(&self, fork: ZoneHardfork) -> ForkCondition;

    /// Returns the Zone-owned hardfork active at `timestamp`.
    fn zone_hardfork_at(&self, timestamp: u64) -> ZoneHardfork {
        ZoneHardfork::VARIANTS
            .iter()
            .rev()
            .copied()
            .find(|&fork| {
                self.zone_fork_activation(fork)
                    .active_at_timestamp(timestamp)
            })
            .unwrap_or(ZoneHardfork::Z0)
    }
}

impl ZoneChainSpec {
    /// Converts a genesis configuration into a Zone chain specification.
    pub fn from_genesis(genesis: Genesis) -> Result<Self, ZoneChainSpecError> {
        let parent_chain_id = decode_l1_chain_id(genesis.config.chain_id)?;
        let parent = tempo_chain_spec_for_l1(parent_chain_id)
            .ok_or(ZoneChainSpecError::UnsupportedParent(parent_chain_id))?;
        Self::from_genesis_with_l1(genesis, parent.as_ref())
    }

    /// Converts a genesis configuration using an already-resolved L1 chain specification.
    ///
    /// This supports custom L1 chains whose hardfork schedule is not globally registered.
    pub fn from_genesis_with_l1(
        mut genesis: Genesis,
        l1: &TempoChainSpec,
    ) -> Result<Self, ZoneChainSpecError> {
        decode_l1_chain_id(genesis.config.chain_id)?;
        inherit_parent_fork_activations(&mut genesis, l1)?;
        let mut zone = TempoChainSpec::from_genesis(genesis);
        insert_zone_fork_activations(&mut zone);

        Ok(Self {
            inner: Arc::new(zone),
        })
    }

    /// Zone identifier encoded in the genesis chain ID.
    pub fn zone_id(&self) -> u32 {
        decode_zone_chain_id(self.inner.genesis().config.chain_id)
            .expect("ZoneChainSpec validates its chain ID during construction")
            .1
    }
}

fn insert_zone_fork_activations(spec: &mut TempoChainSpec) {
    let z1_time = spec
        .genesis()
        .config
        .extra_fields
        .get("z1Time")
        .and_then(parse_activation_timestamp);

    spec.inner
        .hardforks
        .insert(ZoneHardfork::Z0, ForkCondition::Timestamp(0));
    if let Some(timestamp) = z1_time {
        spec.inner
            .hardforks
            .insert(ZoneHardfork::Z1, ForkCondition::Timestamp(timestamp));
    }
}

fn parse_activation_timestamp(value: &serde_json::Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value.as_str().and_then(|value| {
            value.strip_prefix("0x").map_or_else(
                || value.parse().ok(),
                |hex| u64::from_str_radix(hex, 16).ok(),
            )
        })
    })
}

/// Fills missing fork activation fields in the Zone genesis from its parent.
///
/// Chain config serializes Ethereum and Tempo activations as camelCase `*Block` and `*Time`
/// fields. Composing them before constructing the chain spec ensures that its cached genesis
/// header is built with the inherited activations. Explicit Zone activations take precedence.
fn inherit_parent_fork_activations(
    zone_genesis: &mut Genesis,
    l1: &TempoChainSpec,
) -> Result<(), serde_json::Error> {
    let mut zone_config = serde_json::to_value(&zone_genesis.config)?;
    let l1_config = serde_json::to_value(&l1.genesis().config)?;
    let zone_fields = zone_config
        .as_object_mut()
        .expect("ChainConfig must serialize as a JSON object");
    let l1_fields = l1_config
        .as_object()
        .expect("ChainConfig must serialize as a JSON object");

    for (name, value) in l1_fields {
        if name.ends_with("Block") || name.ends_with("Time") {
            zone_fields
                .entry(name.clone())
                .or_insert_with(|| value.clone());
        }
    }

    zone_genesis.config = serde_json::from_value(zone_config)?;
    Ok(())
}

impl Hardforks for ZoneChainSpec {
    fn fork<H: Hardfork>(&self, fork: H) -> ForkCondition {
        self.inner.fork(fork)
    }

    fn forks_iter(&self) -> impl Iterator<Item = (&dyn Hardfork, ForkCondition)> {
        self.inner.forks_iter()
    }

    fn fork_id(&self, head: &Head) -> ForkId {
        self.inner.fork_id(head)
    }

    fn latest_fork_id(&self) -> ForkId {
        self.inner.latest_fork_id()
    }

    fn fork_filter(&self, head: Head) -> ForkFilter {
        self.inner.fork_filter(head)
    }
}

impl EthChainSpec for ZoneChainSpec {
    type Header = TempoHeader;

    fn chain(&self) -> Chain {
        self.inner.chain()
    }

    fn base_fee_params_at_timestamp(&self, timestamp: u64) -> BaseFeeParams {
        self.inner.base_fee_params_at_timestamp(timestamp)
    }

    fn blob_params_at_timestamp(&self, timestamp: u64) -> Option<BlobParams> {
        self.inner.blob_params_at_timestamp(timestamp)
    }

    fn deposit_contract(&self) -> Option<&DepositContract> {
        self.inner.deposit_contract()
    }

    fn genesis_hash(&self) -> B256 {
        self.inner.genesis_hash()
    }

    fn prune_delete_limit(&self) -> usize {
        self.inner.prune_delete_limit()
    }

    fn display_hardforks(&self) -> Box<dyn Display> {
        self.inner.display_hardforks()
    }

    fn genesis_header(&self) -> &Self::Header {
        self.inner.genesis_header()
    }

    fn genesis(&self) -> &Genesis {
        self.inner.genesis()
    }

    fn bootnodes(&self) -> Option<Vec<NodeRecord>> {
        self.inner.bootnodes()
    }

    fn final_paris_total_difficulty(&self) -> Option<U256> {
        self.inner.final_paris_total_difficulty()
    }

    fn next_block_base_fee(&self, _parent: &TempoHeader, _target_timestamp: u64) -> Option<u64> {
        Some(0)
    }
}

impl EthereumHardforks for ZoneChainSpec {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        self.inner.ethereum_fork_activation(fork)
    }
}

impl TempoHardforks for ZoneChainSpec {
    fn tempo_fork_activation(&self, fork: TempoHardfork) -> ForkCondition {
        self.inner.tempo_fork_activation(fork)
    }
}

impl ZoneHardforks for ZoneChainSpec {
    fn zone_fork_activation(&self, fork: ZoneHardfork) -> ForkCondition {
        self.fork(fork)
    }
}

impl TempoConsensusSpec for ZoneChainSpec {
    fn shared_gas_limit_at(&self, _timestamp: u64, _gas_limit: u64) -> u64 {
        0
    }

    fn general_gas_limit_at(
        &self,
        _timestamp: u64,
        _gas_limit: u64,
        _shared_gas_limit: u64,
    ) -> u64 {
        0
    }
}

impl EthExecutorSpec for ZoneChainSpec {
    fn deposit_contract_address(&self) -> Option<Address> {
        self.inner.deposit_contract_address()
    }
}

/// Zone chain specification parser.
#[cfg(feature = "cli")]
#[derive(Debug, Clone, Default)]
pub struct ZoneChainSpecParser;

#[cfg(feature = "cli")]
impl reth_cli::chainspec::ChainSpecParser for ZoneChainSpecParser {
    type ChainSpec = ZoneChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = &[];

    fn parse(s: &str) -> eyre::Result<std::sync::Arc<Self::ChainSpec>> {
        let genesis = reth_cli::chainspec::parse_genesis(s)?;
        Ok(Arc::new(ZoneChainSpec::from_genesis(genesis)?))
    }
}

/// Failure to resolve the canonical chain specification for a zone.
#[derive(Debug, thiserror::Error)]
pub enum ZoneChainSpecError {
    /// The zone chain ID is not a valid encoding of its parent and zone IDs.
    #[error(transparent)]
    InvalidChainId(#[from] ZoneChainIdError),
    /// The parent Tempo hardfork schedule is unknown.
    #[error("unsupported parent Tempo chain ID {0}")]
    UnsupportedParent(u64),
    /// The inherited parent fork activations could not be applied to the Zone genesis.
    #[error("failed to compose Zone genesis config: {0}")]
    InvalidGenesisConfig(#[from] serde_json::Error),
}

/// Returns the Tempo chain specification whose hardfork schedule a parent uses.
///
/// Tempo Anvil uses chain ID 31337 and the Tempo DEV schedule. Additional
/// dev-schedule chain IDs can be listed in `ZONE_L1_DEV_CHAIN_IDS`.
pub fn tempo_chain_spec_for_l1(chain_id: u64) -> Option<Arc<TempoChainSpec>> {
    chainspec_from_chain_id(chain_id).or_else(|| match chain_id {
        1_337 | 31_337 => Some(DEV.clone()),
        _ => std::env::var("ZONE_L1_DEV_CHAIN_IDS")
            .ok()?
            .split(',')
            .any(|id| id.trim().parse() == Ok(chain_id))
            .then(|| DEV.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "cli")]
    use reth_cli::chainspec::ChainSpecParser;
    use tempo_chainspec::spec::{DEV, MODERATO};
    use zone_primitives::constants::zone_chain_id;

    fn dev_zone_spec(zone_id: u32) -> ZoneChainSpec {
        let mut genesis = DEV.genesis().clone();
        genesis.config.chain_id = zone_chain_id(DEV.chain().id(), zone_id).unwrap();
        ZoneChainSpec::from_genesis(genesis).unwrap()
    }

    #[test]
    fn delegates_tempo_chain_behavior() {
        let zone = dev_zone_spec(1);

        assert_eq!(
            zone.chain().id(),
            zone_chain_id(DEV.chain().id(), 1).unwrap()
        );
        assert_eq!(zone.fork(ZoneHardfork::Z0), ForkCondition::Timestamp(0));
        for &hardfork in TempoHardfork::VARIANTS {
            assert_eq!(
                zone.tempo_fork_activation(hardfork),
                DEV.tempo_fork_activation(hardfork)
            );
        }
    }

    #[test]
    fn zone_schedule_defaults_to_z0() {
        let zone = dev_zone_spec(4);

        assert_eq!(
            zone.zone_fork_activation(ZoneHardfork::Z0),
            ForkCondition::Timestamp(0)
        );
        assert_eq!(
            zone.zone_fork_activation(ZoneHardfork::Z1),
            ForkCondition::Never
        );
        assert_eq!(zone.zone_hardfork_at(u64::MAX), ZoneHardfork::Z0);
    }

    #[test]
    fn parses_z1_timestamp_and_activates_at_boundary() {
        let mut genesis = DEV.genesis().clone();
        genesis.config.chain_id = zone_chain_id(DEV.chain().id(), 5).unwrap();
        genesis
            .config
            .extra_fields
            .insert_value("z1Time".into(), 100)
            .unwrap();
        let zone = ZoneChainSpec::from_genesis(genesis).unwrap();

        assert_eq!(zone.zone_hardfork_at(99), ZoneHardfork::Z0);
        assert_eq!(zone.zone_hardfork_at(100), ZoneHardfork::Z1);
    }

    #[test]
    fn genesis_inherits_missing_parent_hardforks_everywhere() {
        let mut genesis = MODERATO.genesis().clone();
        genesis.config.chain_id = zone_chain_id(MODERATO.chain().id(), 7).unwrap();
        genesis.config.london_block = None;
        genesis.config.shanghai_time = None;
        genesis.config.cancun_time = None;
        genesis.config.prague_time = None;
        let raw = TempoChainSpec::from_genesis(genesis.clone());
        let zone = ZoneChainSpec::from_genesis(genesis).unwrap();

        assert_eq!(zone.chain().id(), raw.chain().id());
        assert_ne!(zone.genesis_hash(), raw.genesis_hash());
        assert!(raw.genesis_header().inner.base_fee_per_gas.is_none());
        assert!(raw.genesis_header().inner.withdrawals_root.is_none());
        assert!(
            raw.genesis_header()
                .inner
                .parent_beacon_block_root
                .is_none()
        );
        assert!(raw.genesis_header().inner.requests_hash.is_none());
        assert!(zone.genesis_header().inner.base_fee_per_gas.is_some());
        assert!(zone.genesis_header().inner.withdrawals_root.is_some());
        assert!(
            zone.genesis_header()
                .inner
                .parent_beacon_block_root
                .is_some()
        );
        assert!(zone.genesis_header().inner.requests_hash.is_some());
        for &hardfork in EthereumHardfork::VARIANTS {
            assert_eq!(
                zone.ethereum_fork_activation(hardfork),
                MODERATO.ethereum_fork_activation(hardfork)
            );
        }
        for &hardfork in TempoHardfork::VARIANTS {
            assert_eq!(
                zone.tempo_fork_activation(hardfork),
                MODERATO.tempo_fork_activation(hardfork)
            );
        }
    }

    #[test]
    fn genesis_accepts_an_already_resolved_custom_l1_spec() {
        const CUSTOM_L1_CHAIN_ID: u64 = 31_318;

        let mut l1_genesis = DEV.genesis().clone();
        l1_genesis.config.chain_id = CUSTOM_L1_CHAIN_ID;
        l1_genesis.config.osaka_time = Some(456);
        let l1 = TempoChainSpec::from_genesis(l1_genesis);

        let mut zone_genesis = DEV.genesis().clone();
        zone_genesis.config.chain_id = zone_chain_id(CUSTOM_L1_CHAIN_ID, 8).unwrap();
        zone_genesis.config.osaka_time = Some(123);
        zone_genesis
            .config
            .extra_fields
            .insert_value("zoneForkTime".to_string(), 789)
            .unwrap();
        let zone = ZoneChainSpec::from_genesis_with_l1(zone_genesis, &l1).unwrap();

        assert_eq!(
            zone.ethereum_fork_activation(EthereumHardfork::Osaka),
            ForkCondition::Timestamp(123)
        );
        assert_eq!(
            zone.genesis()
                .config
                .extra_fields
                .get_deserialized::<u64>("zoneForkTime")
                .unwrap()
                .unwrap(),
            789
        );
    }

    #[test]
    fn next_block_base_fee_is_zero() {
        let zone = dev_zone_spec(2);
        let parent = zone.genesis_header();
        let timestamp = parent.inner.timestamp;

        assert_ne!(DEV.next_block_base_fee(parent, timestamp), Some(0));
        assert_eq!(zone.next_block_base_fee(parent, timestamp), Some(0));
    }

    #[test]
    fn consensus_gas_limits_disable_tempo_gas_sections() {
        let zone = dev_zone_spec(3);

        assert_eq!(zone.shared_gas_limit_at(0, 30_000_000), 0);
        assert_eq!(zone.general_gas_limit_at(0, 30_000_000, 0), 0);
    }

    #[cfg(feature = "cli")]
    #[test]
    fn parser_parses_zone_genesis_json() {
        let mut genesis = DEV.genesis().clone();
        let chain_id = zone_chain_id(DEV.chain().id(), 9).unwrap();
        genesis.config.chain_id = chain_id;
        let json = serde_json::to_string(&genesis).unwrap();
        let zone = ZoneChainSpecParser::parse(&json).expect("valid Zone genesis JSON");

        assert_eq!(zone.chain().id(), chain_id);
    }

    #[cfg(feature = "cli")]
    #[test]
    fn parser_rejects_named_tempo_chain() {
        assert!(ZoneChainSpecParser::parse("dev").is_err());
    }
}
