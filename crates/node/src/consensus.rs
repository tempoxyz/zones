//! Zone node admission rules on top of deterministic Tempo execution validation.
//!
//! Tempo gates future timestamps in its consensus voting layer, which Zones do not run.
//! Keep the Zone clock allowance here, outside stateless/proof execution, and mark clock
//! failures transient so the engine can retry rather than permanently blacklist a block.

use alloy_evm::block::BlockExecutionResult;
use alloy_primitives::B256;
use reth_consensus::{Consensus, ConsensusError, FullConsensus, HeaderValidator, ReceiptRootBloom};
use reth_primitives_traits::{RecoveredBlock, SealedBlock, SealedHeader};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tempo_chainspec::TempoHardforks as _;
use tempo_evm::consensus::TempoConsensus;
use tempo_primitives::{Block, BlockBody, TempoHeader, TempoPrimitives, TempoReceipt};
use zone_chainspec::ZoneChainSpec;

const ALLOWED_FUTURE_BLOCK_TIME_MILLIS: u64 = 100;

/// Tempo validation with equal millisecond timestamps and Zone-local clock admission.
#[derive(Debug, Clone)]
pub struct ZoneConsensus {
    inner: TempoConsensus<ZoneChainSpec>,
}

impl ZoneConsensus {
    /// Create the validator shared by the node builder and CLI import commands.
    pub fn new(chain_spec: Arc<ZoneChainSpec>) -> Self {
        Self {
            inner: TempoConsensus::new(chain_spec).with_allow_equal_timestamps(true),
        }
    }

    fn validate_header_at(
        &self,
        header: &SealedHeader<TempoHeader>,
        present_timestamp: u64,
    ) -> Result<(), ConsensusError> {
        self.inner.validate_header(header)?;
        let timestamp = header.timestamp_millis();
        if timestamp > present_timestamp.saturating_add(ALLOWED_FUTURE_BLOCK_TIME_MILLIS) {
            return Err(ConsensusError::TimestampIsInFuture {
                timestamp,
                present_timestamp,
            });
        }
        Ok(())
    }
}

impl HeaderValidator<TempoHeader> for ZoneConsensus {
    fn validate_header(&self, header: &SealedHeader<TempoHeader>) -> Result<(), ConsensusError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(ConsensusError::other)?
            .as_millis()
            .try_into()
            .map_err(ConsensusError::other)?;
        self.validate_header_at(header, now)
    }

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader<TempoHeader>,
        parent: &SealedHeader<TempoHeader>,
    ) -> Result<(), ConsensusError> {
        self.inner.validate_header_against_parent(header, parent)
    }
}

impl Consensus<Block> for ZoneConsensus {
    fn validate_body_against_header(
        &self,
        body: &BlockBody,
        header: &SealedHeader<TempoHeader>,
    ) -> Result<(), ConsensusError> {
        self.inner.validate_body_against_header(body, header)
    }

    fn validate_block_pre_execution(
        &self,
        block: &SealedBlock<Block>,
    ) -> Result<(), ConsensusError> {
        self.inner.validate_block_pre_execution(block)
    }

    fn is_transient_error(&self, error: &ConsensusError) -> bool {
        matches!(error, ConsensusError::TimestampIsInFuture { .. })
            || self.inner.is_transient_error(error)
    }
}

impl FullConsensus<TempoPrimitives> for ZoneConsensus {
    fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock<Block>,
        result: &BlockExecutionResult<TempoReceipt>,
        receipt_root_bloom: Option<ReceiptRootBloom>,
        block_access_list_hash: Option<B256>,
    ) -> Result<(), ConsensusError> {
        self.inner.validate_block_post_execution(
            block,
            result,
            receipt_root_bloom,
            block_access_list_hash,
        )
    }
}

/// Match the producer's T13+ readiness rule on import as well as local production.
///
/// Use observed finalized L1, not just the embedded anchor: checkpoint catch-up deliberately
/// imports pre-fork headers under newer Zone rules after L1 has activated those rules.
pub(crate) fn zone_hardfork_ready(
    chain_spec: &ZoneChainSpec,
    zone_timestamp: u64,
    finalized_l1_timestamp: u64,
) -> bool {
    let zone_hardfork = chain_spec.tempo_hardfork_at(zone_timestamp);
    !zone_hardfork.is_t13() || zone_hardfork <= chain_spec.tempo_hardfork_at(finalized_l1_timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_chainspec::EthChainSpec as _;
    use tempo_chainspec::hardfork::TempoHardfork;
    use zone_chainspec::test_utils::set_tempo_fork;
    use zone_primitives::constants::zone_chain_id;

    fn spec() -> Arc<ZoneChainSpec> {
        let mut genesis = crate::genesis::genesis_template().unwrap();
        genesis.config.chain_id = zone_chain_id(1337, 1).unwrap();
        set_tempo_fork(&mut genesis, TempoHardfork::T13, 0);
        Arc::new(ZoneChainSpec::from_genesis(genesis).unwrap())
    }

    fn header(spec: &ZoneChainSpec, timestamp: u64) -> SealedHeader<TempoHeader> {
        let mut header = spec.genesis_header().clone();
        header.inner.number = 1;
        header.inner.timestamp = timestamp / 1000;
        header.inner.nonce = Default::default();
        header.inner.base_fee_per_gas = Some(0);
        header.timestamp_millis_part = timestamp % 1000;
        header.shared_gas_limit = 0;
        header.general_gas_limit = 0;
        SealedHeader::seal_slow(header)
    }

    #[test]
    fn future_clock_allowance_is_inclusive_and_retryable() {
        let spec = spec();
        let consensus = ZoneConsensus::new(spec.clone());
        let now = 10_000;
        for timestamp in [now - 1, now, now + 100] {
            consensus
                .validate_header_at(&header(&spec, timestamp), now)
                .unwrap();
        }
        let future = header(&spec, now + 101);
        let error = consensus.validate_header_at(&future, now).unwrap_err();
        assert!(matches!(
            error,
            ConsensusError::TimestampIsInFuture {
                timestamp: 10_101,
                present_timestamp: 10_000,
            }
        ));
        assert!(consensus.is_transient_error(&error));
        consensus.validate_header_at(&future, now + 1).unwrap();
    }

    #[test]
    fn rejects_far_future_header_that_upstream_accepts() {
        let spec = spec();
        let consensus = ZoneConsensus::new(spec.clone());
        let future = header(&spec, 4_102_444_800_000);
        consensus.inner.validate_header(&future).unwrap();
        assert!(matches!(
            consensus.validate_header_at(&future, 10_000),
            Err(ConsensusError::TimestampIsInFuture { .. })
        ));
        assert!(matches!(
            consensus.validate_header(&future),
            Err(ConsensusError::TimestampIsInFuture { .. })
        ));
    }

    #[test]
    fn preserves_equal_timestamps_and_rejects_regression() {
        let spec = spec();
        let consensus = ZoneConsensus::new(spec.clone());
        let parent = header(&spec, 10_000);
        let mut child = parent.clone().unseal();
        child.inner.number += 1;
        child.inner.parent_hash = parent.hash();
        consensus
            .validate_header_against_parent(&SealedHeader::seal_slow(child.clone()), &parent)
            .unwrap();
        child.inner.timestamp -= 1;
        let error = consensus
            .validate_header_against_parent(&SealedHeader::seal_slow(child), &parent)
            .unwrap_err();
        assert!(matches!(error, ConsensusError::TimestampIsInPast { .. }));
        assert!(!consensus.is_transient_error(&error));
    }

    #[test]
    fn hardfork_readiness_waits_for_finalized_l1() {
        for fork in [TempoHardfork::T13, TempoHardfork::T14] {
            let mut genesis = crate::genesis::genesis_template().unwrap();
            genesis.config.chain_id = zone_chain_id(1337, 1).unwrap();
            set_tempo_fork(&mut genesis, fork, 100);
            let spec = ZoneChainSpec::from_genesis(genesis).unwrap();
            assert!(zone_hardfork_ready(&spec, 99, 99));
            assert!(!zone_hardfork_ready(&spec, 100, 99));
            assert!(zone_hardfork_ready(&spec, 100, 100));
            // A delayed Zone block may be behind finalized L1.
            assert!(zone_hardfork_ready(&spec, 99, 100));
        }
    }
}
