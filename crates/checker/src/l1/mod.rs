//! Collection of all temporary evidence for one anchored Tempo/L1 block.

mod events;

use alloy_consensus::BlockHeader as _;
use alloy_eips::{BlockId, BlockNumHash};
use alloy_network::{BlockResponse as _, primitives::HeaderResponse as _};
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{DynProvider, Provider};
use alloy_transport::{RpcError, TransportError, TransportErrorKind};
use futures::{StreamExt as _, TryStreamExt as _, stream};
use tempo_alloy::{TempoNetwork, rpc::TempoTransactionReceipt};
use tempo_contracts::precompiles::ITIP20;
use zone_l1::L1BlockTracker;

use crate::AttemptError;

pub(crate) use events::L1PortalEvent;
use events::{EventCollector, L1Events};

/// Bound on concurrent Portal balance reads for one token set.
const BALANCE_CONCURRENCY: usize = 8;

/// Failure acquiring or interpreting exact Tempo state.
#[derive(Debug)]
pub(crate) enum L1ReadError {
    /// Required RPC data is not currently available.
    Unavailable(eyre::Report),
    /// Authenticated protocol evidence cannot be verified.
    Finding(eyre::Report),
    /// Deterministic provider or checker failure prevents verification.
    Disable(eyre::Report),
}

impl From<AttemptError> for L1ReadError {
    fn from(error: AttemptError) -> Self {
        match error {
            AttemptError::Retry(error) => Self::Unavailable(error),
            AttemptError::Disable(error) => Self::Disable(error),
        }
    }
}

impl From<L1ReadError> for AttemptError {
    fn from(error: L1ReadError) -> Self {
        match error {
            L1ReadError::Unavailable(error) => Self::Retry(error),
            L1ReadError::Finding(error) | L1ReadError::Disable(error) => Self::Disable(error),
        }
    }
}

/// Recognized Portal events for one exact anchored L1 block.
#[derive(Debug)]
pub(crate) struct L1BlockEvidence {
    block: BlockNumHash,
    events: L1Events,
}

impl L1BlockEvidence {
    pub(crate) const fn block(&self) -> BlockNumHash {
        self.block
    }

    /// Return authenticated Portal events in receipt order.
    pub(crate) fn portal_events(&self) -> impl Iterator<Item = &L1PortalEvent> {
        self.events.events.iter()
    }
}

/// Fetch one canonical Tempo block that extends `parent`.
pub(crate) async fn collect_l1_block(
    provider: &DynProvider<TempoNetwork>,
    portal: Address,
    parent: BlockNumHash,
) -> Result<L1BlockEvidence, L1ReadError> {
    collect_l1_block_inner(provider, portal, parent, None).await
}

/// Fetch the exact anchored Tempo block that extends `parent`.
pub(crate) async fn collect_l1_block_at(
    provider: &DynProvider<TempoNetwork>,
    tracker: &L1BlockTracker,
    portal: Address,
    parent: BlockNumHash,
    expected: BlockNumHash,
) -> Result<L1BlockEvidence, L1ReadError> {
    if let Some(evidence) = tracker
        .authenticated_portal_logs(expected)
        .map_err(finding)?
    {
        return collect_tracked_l1_block_evidence(portal, parent, evidence);
    }
    collect_l1_block_inner(provider, portal, parent, Some(expected)).await
}

fn collect_tracked_l1_block_evidence(
    portal: Address,
    parent: BlockNumHash,
    evidence: zone_l1::AuthenticatedPortalLogs,
) -> Result<L1BlockEvidence, L1ReadError> {
    let expected_number = parent.number.checked_add(1).ok_or_else(|| {
        disable(eyre::eyre!(
            "Tempo block number overflow after {}",
            parent.number
        ))
    })?;
    if evidence.block.number != expected_number || evidence.parent_hash != parent.hash {
        return Err(finding(eyre::eyre!(
            "Tempo history is not contiguous at block {}",
            evidence.block.number
        )));
    }
    let mut collector = EventCollector::new(portal);
    for log in &evidence.logs {
        collector
            .extract_log(log, evidence.block.number)
            .map_err(finding)?;
    }
    Ok(L1BlockEvidence {
        block: evidence.block,
        events: collector.finish(),
    })
}

async fn collect_l1_block_inner(
    provider: &DynProvider<TempoNetwork>,
    portal: Address,
    parent: BlockNumHash,
    expected: Option<BlockNumHash>,
) -> Result<L1BlockEvidence, L1ReadError> {
    let number = parent.number.checked_add(1).ok_or_else(|| {
        disable(eyre::eyre!(
            "Tempo block number overflow after {}",
            parent.number
        ))
    })?;
    let block = provider
        .get_block_by_number(number.into())
        .hashes()
        .await
        .map_err(classify_rpc_error)?
        .ok_or_else(|| unavailable(eyre::eyre!("Tempo block {number} is unavailable")))?;
    if block.header().number() != number {
        return Err(disable(eyre::eyre!(
            "Tempo RPC returned block {} for requested block {number}",
            block.header().number()
        )));
    }
    if block.header().parent_hash() != parent.hash {
        return Err(finding(eyre::eyre!(
            "Tempo history is not contiguous at block {number}"
        )));
    }
    let coordinate = BlockNumHash::new(number, block.header().hash());
    if expected.is_some_and(|expected| coordinate != expected) {
        return Err(finding(eyre::eyre!(
            "Tempo history does not end at the Zone anchor"
        )));
    }
    let receipts = provider
        .get_block_receipts(BlockId::hash(coordinate.hash))
        .await
        .map_err(classify_rpc_error)?
        .ok_or_else(|| {
            unavailable(eyre::eyre!(
                "no receipts for L1 block {number} ({})",
                coordinate.hash
            ))
        })?;
    zone_l1::verify_receipts_against_header(
        coordinate,
        block.header().receipts_root(),
        block.header().logs_bloom(),
        &receipts,
    )
    .map_err(disable)?;
    collect_l1_block_evidence(portal, coordinate, &receipts)
}

/// Read Portal custody for one token at an exact canonical Tempo block.
pub(crate) async fn portal_balance(
    provider: &DynProvider<TempoNetwork>,
    token: Address,
    portal: Address,
    block: B256,
) -> Result<U256, L1ReadError> {
    ITIP20::new(token, provider)
        .balanceOf(portal)
        .block(BlockId::hash_canonical(block))
        .call()
        .await
        .map_err(classify_contract_error)
}

/// Read Portal custody for every token, concurrently, at one exact canonical Tempo block.
pub(crate) async fn portal_balances(
    provider: &DynProvider<TempoNetwork>,
    portal: Address,
    tokens: impl IntoIterator<Item = Address>,
    block: B256,
) -> Result<Vec<(Address, U256)>, L1ReadError> {
    stream::iter(tokens.into_iter().map(|token| async move {
        portal_balance(provider, token, portal, block)
            .await
            .map(|balance| (token, balance))
    }))
    .buffer_unordered(BALANCE_CONCURRENCY)
    .try_collect()
    .await
}

/// Fetch receipts and collect Portal events for one authenticated L1 block.
fn collect_l1_block_evidence(
    portal: Address,
    block: BlockNumHash,
    receipts: &[TempoTransactionReceipt],
) -> Result<L1BlockEvidence, L1ReadError> {
    let number = block.number;
    let mut event_collector = EventCollector::new(portal);
    for receipt in receipts {
        event_collector
            .extract_receipt(receipt, number)
            .map_err(finding)?;
    }
    let events = event_collector.finish();
    Ok(L1BlockEvidence { block, events })
}

fn classify_contract_error(error: alloy_contract::Error) -> L1ReadError {
    if error.as_revert_data().is_some() {
        return finding(error);
    }
    match error {
        alloy_contract::Error::TransportError(error) => classify_rpc_error(error).into(),
        error @ (alloy_contract::Error::ContractNotDeployed
        | alloy_contract::Error::ZeroData(..)
        | alloy_contract::Error::AbiError(_)) => finding(error),
        error => disable(error),
    }
}

/// Classify one provider RPC failure without relying on its display text.
pub(crate) fn classify_rpc_error(error: TransportError) -> AttemptError {
    let retryable = match &error {
        RpcError::ErrorResp(error) => error.is_retry_err(),
        RpcError::UnsupportedFeature(_)
        | RpcError::LocalUsageError(_)
        | RpcError::SerError(_)
        | RpcError::DeserError { .. }
        | RpcError::Transport(TransportErrorKind::NonRetryable(_)) => false,
        _ => true,
    };
    if retryable {
        AttemptError::retry(error)
    } else {
        AttemptError::disable(error)
    }
}

fn unavailable(error: impl Into<eyre::Report>) -> L1ReadError {
    L1ReadError::Unavailable(error.into())
}

fn finding(error: impl Into<eyre::Report>) -> L1ReadError {
    L1ReadError::Finding(error.into())
}

fn disable(error: impl Into<eyre::Report>) -> L1ReadError {
    L1ReadError::Disable(error.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eips::NumHash;
    use alloy_primitives::{B256, Log};
    use alloy_sol_types::SolEvent;
    use tempo_zone_contracts::ZonePortal;

    const BLOCK: u64 = 100;
    const HASH: B256 = B256::repeat_byte(0x10);

    #[test]
    fn tracked_evidence_preserves_accounting_events_and_parent_link() {
        let parent = BlockNumHash::new(BLOCK - 1, B256::repeat_byte(0x09));
        let portal = Address::repeat_byte(0x20);
        let token = Address::repeat_byte(0x21);
        let log = Log {
            address: portal,
            data: ZonePortal::TokenEnabled {
                token,
                name: "Test".into(),
                symbol: "TST".into(),
                currency: "USD".into(),
            }
            .encode_log_data(),
        };
        let tracked = zone_l1::AuthenticatedPortalLogs {
            block: NumHash::new(BLOCK, HASH),
            parent_hash: parent.hash,
            logs: vec![log],
        };

        let evidence = collect_tracked_l1_block_evidence(portal, parent, tracked).unwrap();
        assert_eq!(evidence.block(), BlockNumHash::new(BLOCK, HASH));
        assert!(matches!(
            evidence.portal_events().next(),
            Some(L1PortalEvent::TokenEnabled { token: observed }) if *observed == token
        ));
    }

    #[test]
    fn tracked_evidence_rejects_non_contiguous_parent() {
        let tracked = zone_l1::AuthenticatedPortalLogs {
            block: NumHash::new(BLOCK, HASH),
            parent_hash: B256::repeat_byte(0xff),
            logs: vec![],
        };
        let result = collect_tracked_l1_block_evidence(
            Address::ZERO,
            BlockNumHash::new(BLOCK - 1, B256::repeat_byte(0x09)),
            tracked,
        );
        assert!(matches!(result, Err(L1ReadError::Finding(_))));
    }

    #[test]
    fn classifies_retryable_and_terminal_transport_failures() {
        let retryable = alloy_transport::TransportErrorKind::backend_gone();
        assert!(matches!(
            classify_rpc_error(retryable),
            AttemptError::Retry(_)
        ));

        let terminal = alloy_transport::TransportErrorKind::non_retryable_str("invalid request");
        assert!(matches!(
            classify_rpc_error(terminal),
            AttemptError::Disable(_)
        ));
    }
}
