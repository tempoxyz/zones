//! Zone chain specification.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

use alloy_eips::{eip1559::BaseFeeParams, eip7840::BlobParams};
use alloy_evm::eth::spec::EthExecutorSpec;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, U256};
use reth_chainspec::{
    Chain, ChainSpec, DepositContract, EthChainSpec, EthereumHardfork, EthereumHardforks,
    ForkCondition, ForkFilter, ForkId, Hardfork, Hardforks, Head,
};
use reth_network_peers::NodeRecord;
use std::fmt::Display;
use tempo_chainspec::{TempoChainSpec, hardfork::TempoHardfork, spec::TempoHardforks};
use tempo_primitives::TempoHeader;

/// Chain specification for a Tempo Zone.
///
/// Zone behavior delegates to Tempo by default. Zone-specific protocol rules can be overridden
/// here without changing the parent Tempo chain specification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneChainSpec {
    tempo: TempoChainSpec,
}

impl ZoneChainSpec {
    /// Creates a Zone chain specification from a Tempo chain specification.
    pub const fn new(tempo: TempoChainSpec) -> Self {
        Self { tempo }
    }

    /// Converts a genesis configuration into a Zone chain specification.
    pub fn from_genesis(genesis: Genesis) -> Self {
        Self::new(TempoChainSpec::from_genesis(genesis))
    }

    /// Returns the underlying Tempo chain specification.
    pub const fn as_tempo(&self) -> &TempoChainSpec {
        &self.tempo
    }

    /// Applies Tempo hardfork activations from the parent chain.
    pub fn with_tempo_hardforks_from(mut self, parent: &impl TempoHardforks) -> Self {
        for &hardfork in TempoHardfork::VARIANTS {
            self.tempo
                .inner
                .hardforks
                .insert(hardfork, parent.tempo_fork_activation(hardfork));
        }
        self
    }

    /// Consumes this value and returns the underlying Tempo chain specification.
    pub fn into_tempo(self) -> TempoChainSpec {
        self.tempo
    }
}

impl From<TempoChainSpec> for ZoneChainSpec {
    fn from(value: TempoChainSpec) -> Self {
        Self::new(value)
    }
}

impl From<ChainSpec> for ZoneChainSpec {
    fn from(value: ChainSpec) -> Self {
        Self::new(value.into())
    }
}

impl Hardforks for ZoneChainSpec {
    fn fork<H: Hardfork>(&self, fork: H) -> ForkCondition {
        self.tempo.fork(fork)
    }

    fn forks_iter(&self) -> impl Iterator<Item = (&dyn Hardfork, ForkCondition)> {
        self.tempo.forks_iter()
    }

    fn fork_id(&self, head: &Head) -> ForkId {
        self.tempo.fork_id(head)
    }

    fn latest_fork_id(&self) -> ForkId {
        self.tempo.latest_fork_id()
    }

    fn fork_filter(&self, head: Head) -> ForkFilter {
        self.tempo.fork_filter(head)
    }
}

impl EthChainSpec for ZoneChainSpec {
    type Header = TempoHeader;

    fn chain(&self) -> Chain {
        self.tempo.chain()
    }

    fn base_fee_params_at_timestamp(&self, timestamp: u64) -> BaseFeeParams {
        self.tempo.base_fee_params_at_timestamp(timestamp)
    }

    fn blob_params_at_timestamp(&self, timestamp: u64) -> Option<BlobParams> {
        self.tempo.blob_params_at_timestamp(timestamp)
    }

    fn deposit_contract(&self) -> Option<&DepositContract> {
        self.tempo.deposit_contract()
    }

    fn genesis_hash(&self) -> B256 {
        self.tempo.genesis_hash()
    }

    fn prune_delete_limit(&self) -> usize {
        self.tempo.prune_delete_limit()
    }

    fn display_hardforks(&self) -> Box<dyn Display> {
        self.tempo.display_hardforks()
    }

    fn genesis_header(&self) -> &Self::Header {
        self.tempo.genesis_header()
    }

    fn genesis(&self) -> &Genesis {
        self.tempo.genesis()
    }

    fn bootnodes(&self) -> Option<Vec<NodeRecord>> {
        self.tempo.bootnodes()
    }

    fn final_paris_total_difficulty(&self) -> Option<U256> {
        self.tempo.final_paris_total_difficulty()
    }

    fn next_block_base_fee(&self, parent: &TempoHeader, target_timestamp: u64) -> Option<u64> {
        self.tempo.next_block_base_fee(parent, target_timestamp)
    }
}

impl EthereumHardforks for ZoneChainSpec {
    fn ethereum_fork_activation(&self, fork: EthereumHardfork) -> ForkCondition {
        self.tempo.ethereum_fork_activation(fork)
    }
}

impl TempoHardforks for ZoneChainSpec {
    fn tempo_fork_activation(&self, fork: TempoHardfork) -> ForkCondition {
        self.tempo.tempo_fork_activation(fork)
    }
}

impl EthExecutorSpec for ZoneChainSpec {
    fn deposit_contract_address(&self) -> Option<Address> {
        self.tempo.deposit_contract_address()
    }
}

/// Zone chain specification parser.
#[cfg(feature = "cli")]
#[derive(Debug, Clone, Default)]
pub struct ZoneChainSpecParser;

#[cfg(feature = "cli")]
impl reth_cli::chainspec::ChainSpecParser for ZoneChainSpecParser {
    type ChainSpec = ZoneChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = tempo_chainspec::spec::SUPPORTED_CHAINS;

    fn parse(s: &str) -> eyre::Result<std::sync::Arc<Self::ChainSpec>> {
        tempo_chainspec::spec::chain_value_parser(s)
            .map(|spec| std::sync::Arc::new(spec.as_ref().clone().into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "cli")]
    use reth_cli::chainspec::ChainSpecParser;
    use tempo_chainspec::spec::DEV;

    #[test]
    fn delegates_tempo_chain_behavior() {
        let zone = ZoneChainSpec::new(DEV.as_ref().clone());
        let parent = zone.genesis_header();
        let timestamp = parent.inner.timestamp;

        assert_eq!(zone.chain(), DEV.chain());
        assert_eq!(zone.genesis_hash(), DEV.genesis_hash());
        assert_eq!(
            zone.next_block_base_fee(parent, timestamp),
            DEV.next_block_base_fee(parent, timestamp)
        );
        for &hardfork in TempoHardfork::VARIANTS {
            assert_eq!(
                zone.tempo_fork_activation(hardfork),
                DEV.tempo_fork_activation(hardfork)
            );
        }
    }

    #[cfg(feature = "cli")]
    #[test]
    fn parser_wraps_tempo_chain_spec() {
        let zone = ZoneChainSpecParser::parse("dev").expect("valid development chain spec");

        assert_eq!(zone.chain(), DEV.chain());
        assert_eq!(zone.genesis_hash(), DEV.genesis_hash());
    }
}
