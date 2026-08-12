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
use zone_primitives::constants::{ZoneChainIdError, decode_l1_chain_id};

/// Chain specification for a Tempo Zone.
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneChainSpec {
    /// Underlying Tempo chain specification.
    pub inner: Arc<TempoChainSpec>,
}

impl ZoneChainSpec {
    /// Converts a genesis configuration into a Zone chain specification.
    pub fn from_genesis(genesis: Genesis) -> Result<Self, ZoneChainSpecError> {
        let mut zone = Arc::new(TempoChainSpec::from_genesis(genesis));
        let parent_chain_id = decode_l1_chain_id(zone.chain().id())?;
        let parent = tempo_chain_spec_for_l1(parent_chain_id)
            .ok_or(ZoneChainSpecError::UnsupportedParent(parent_chain_id))?;

        let inner = Arc::make_mut(&mut zone);
        for &hardfork in EthereumHardfork::VARIANTS {
            inner
                .inner
                .hardforks
                .insert(hardfork, parent.ethereum_fork_activation(hardfork));
        }
        for &hardfork in TempoHardfork::VARIANTS {
            inner
                .inner
                .hardforks
                .insert(hardfork, parent.tempo_fork_activation(hardfork));
        }

        Ok(Self { inner: zone })
    }
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
        for &hardfork in TempoHardfork::VARIANTS {
            assert_eq!(
                zone.tempo_fork_activation(hardfork),
                DEV.tempo_fork_activation(hardfork)
            );
        }
    }

    #[test]
    fn genesis_uses_parent_hardforks_everywhere() {
        let mut genesis = DEV.genesis().clone();
        genesis.config.chain_id = zone_chain_id(MODERATO.chain().id(), 7).unwrap();
        genesis.config.osaka_time = Some(123);
        let raw = TempoChainSpec::from_genesis(genesis.clone());
        let zone = ZoneChainSpec::from_genesis(genesis).unwrap();

        assert_eq!(zone.chain().id(), raw.chain().id());
        assert_eq!(zone.genesis_hash(), raw.genesis_hash());
        assert_ne!(
            raw.ethereum_fork_activation(EthereumHardfork::Osaka),
            MODERATO.ethereum_fork_activation(EthereumHardfork::Osaka)
        );
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
    fn next_block_base_fee_is_zero() {
        let zone = dev_zone_spec(2);
        let parent = zone.genesis_header();
        let timestamp = parent.inner.timestamp;

        assert_ne!(DEV.next_block_base_fee(parent, timestamp), Some(0));
        assert_eq!(zone.next_block_base_fee(parent, timestamp), Some(0));
    }

    #[test]
    fn consensus_gas_limits_disable_tempo_gas_sections() {
        let zone = ZoneChainSpec::from(DEV.clone());

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
