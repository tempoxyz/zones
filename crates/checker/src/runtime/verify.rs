//! Verifying one canonical Zone block against authenticated Tempo evidence and exact Zone state.
//!
//! Blocks are loaded straight from the local provider rather than from notification payloads, so
//! each one must be shown to extend the durable verified hash before it is applied.

use std::{collections::BTreeSet, time::Duration};

use alloy_consensus::BlockHeader as _;
use alloy_eips::BlockNumHash;
use alloy_primitives::Sealable as _;
use alloy_provider::DynProvider;
use reth_chainspec::ChainSpecProvider;
use reth_node_api::BlockBody as _;
use reth_primitives_traits::Block as _;
use reth_storage_api::{BlockNumReader, BlockReader, StateProviderFactory};
use tempo_alloy::TempoNetwork;
use tempo_chainspec::spec::TempoHardforks;

use super::{AppendOnlyViolation, Backoff, CheckedPrimitives, RuntimeLimits};
use crate::{
    CheckerConfig,
    accounting::effects,
    l1::{L1ReadError, collect_l1_block_at, portal_balances},
    l2::{AccountingStateError, collect_l2_block_evidence, read_accounting_state},
    persistence::{BlockRef, CandidateTransition, Snapshot, Store},
    telemetry::{self, CheckerMetrics},
};

/// Failure processing one block.
pub(super) enum BlockError {
    /// Authenticated divergence to persist as a finding.
    Finding { zone: BlockRef, error: eyre::Report },
    /// Checker-local failure that must stop verification.
    Disable(eyre::Report),
}

/// What every block's verification needs, threaded as one parameter.
pub(super) struct VerificationContext<'a> {
    pub(super) config: &'a CheckerConfig,
    pub(super) metrics: &'a CheckerMetrics,
    pub(super) limits: RuntimeLimits,
}

/// Load and verify exactly one canonical block after the durable verified tip.
pub(super) async fn process_next_block<N, P>(
    number: u64,
    provider: &P,
    l1: &DynProvider<TempoNetwork>,
    store: &Store,
    snapshot: Snapshot,
    context: &VerificationContext<'_>,
) -> Result<Snapshot, BlockError>
where
    N: CheckedPrimitives,
    P: BlockReader<Block = N::Block, Receipt = N::Receipt>
        + ChainSpecProvider
        + StateProviderFactory,
    P::ChainSpec: TempoHardforks,
{
    let unavailable = |part: &str| {
        BlockError::Disable(eyre::eyre!(
            "canonical Zone block {number} {part} is unavailable; checker restart history may have been pruned"
        ))
    };
    let read = |error| BlockError::Disable(eyre::Report::new(error));
    let block = provider
        .block(number.into())
        .map_err(read)?
        .ok_or_else(|| unavailable("body"))?;
    let receipts = provider
        .receipts_by_block(number.into())
        .map_err(read)?
        .ok_or_else(|| unavailable("receipts"))?;
    let hash = block.header().hash_slow();

    if block.header().number() != number {
        return Err(BlockError::Disable(eyre::eyre!(
            "canonical Zone block {number} resolved to block {} ({hash})",
            block.header().number()
        )));
    }
    let verified = snapshot.metadata.verified_zone;
    if block.header().parent_hash() != verified.hash {
        return Err(BlockError::Disable(
            AppendOnlyViolation::new(
                BlockNumHash::new(number, hash),
                format!(
                    "canonical block {number} ({hash}) has parent {}, expected verified block {} ({})",
                    block.header().parent_hash(), verified.number, verified.hash
                ),
            )
            .into(),
        ));
    }

    verify_block::<N, _>(provider, l1, store, snapshot, context, &block, &receipts).await
}

/// Bound one block's verification, whatever it is waiting on.
pub(super) async fn with_block_timeout<T, F>(
    future: F,
    number: u64,
    duration: Duration,
) -> Result<T, BlockError>
where
    F: Future<Output = Result<T, BlockError>>,
{
    tokio::time::timeout(duration, future).await.map_err(|_| {
        BlockError::Disable(eyre::eyre!(
            "Zone block {number} verification timed out after {:?}",
            duration
        ))
    })?
}

/// Verify one Zone block's bridge accounting against Tempo history and Portal
/// collateral, persisting the result on success.
async fn verify_block<N, P>(
    provider: &P,
    l1: &DynProvider<TempoNetwork>,
    store: &Store,
    prior: Snapshot,
    context: &VerificationContext<'_>,
    block: &N::Block,
    receipts: &[N::Receipt],
) -> Result<Snapshot, BlockError>
where
    N: CheckedPrimitives,
    P: BlockNumReader + ChainSpecProvider + StateProviderFactory,
    P::ChainSpec: TempoHardforks,
{
    let VerificationContext {
        config,
        metrics,
        limits,
    } = context;
    let header = block.header();
    let number = header.number();
    let hash = header.hash_slow();
    let parent_hash = header.parent_hash();
    let zone = BlockRef::new(number, hash);
    let fail = |error| BlockError::Finding { zone, error };
    let l2 = collect_l2_block_evidence(block.body().transactions(), receipts, zone.into())
        .map_err(fail)?;
    let anchor = l2.l1_anchor();
    let tempo = BlockNumHash::new(anchor.block_number(), anchor.block_hash());
    let tempo_parent = BlockNumHash::from(prior.metadata.imported_tempo);
    validate_tempo_advance(tempo_parent.number, tempo.number).map_err(fail)?;
    let tempo_block = retry_l1_read("Tempo block acquisition", zone, context, || {
        collect_l1_block_at(
            l1,
            &config.l1_block_tracker,
            config.portal_address,
            tempo_parent,
            tempo,
        )
    })
    .await?;
    let mut block_effects = effects::from_tempo(&tempo_block);
    block_effects.extend(effects::from_zone(&l2));
    let candidate = CandidateTransition::derive(
        prior,
        zone,
        BlockRef::new(number.saturating_sub(1), parent_hash),
        tempo.into(),
        &block_effects,
    )
    .map_err(|error| fail(error.into()))?;
    let state = candidate.state();
    let mut accounts = l2.accounting_candidates();
    let tokens = state
        .tokens()
        .map(|(token, _)| token)
        .collect::<BTreeSet<_>>();
    for token in &tokens {
        accounts.entry(*token).or_default();
    }
    let spec = provider
        .chain_spec()
        .tempo_hardfork_at(block.header().timestamp());
    let mut state_attempts = 0u32;
    let observed = loop {
        match read_accounting_state(provider, &accounts, zone.into(), spec) {
            Ok(observed) => break observed,
            Err(AccountingStateError::Unavailable(error)) => {
                metrics.acquisition_retries_total.increment(1);
                state_attempts = state_attempts.saturating_add(1);
                tracing::warn!(
                    target: "zone::checker",
                    %error,
                    zone_block = zone.number,
                    zone_hash = %zone.hash,
                    attempt = state_attempts,
                    "checker local state unavailable (unpruned state is required for notified blocks); retrying"
                );
                tokio::time::sleep(limits.retry_delay).await;
            }
            Err(AccountingStateError::Disable(error)) => {
                return Err(BlockError::Disable(error));
            }
        }
    };
    state
        .verify_zone_state(
            observed
                .into_iter()
                .map(|evidence| (evidence.token, evidence.total_supply, evidence.balances)),
        )
        .map_err(|error| fail(error.into()))?;

    let balances = retry_l1_read("Portal balance acquisition", zone, context, || {
        portal_balances(
            l1,
            config.portal_address,
            tokens.iter().copied(),
            tempo.hash,
        )
    })
    .await?;
    state
        .verify_portal_balances(balances)
        .map_err(|error| fail(error.into()))?;

    let next = store
        .apply(candidate)
        .map_err(|error| BlockError::Disable(error.into()))?;
    telemetry::log_verified_activity(&tempo_block, &l2, zone);
    Ok(next)
}

/// Split a Tempo read failure into a durable finding or a checker-local stop.
fn classify_block_l1_error(error: L1ReadError, zone: BlockRef) -> BlockError {
    match error {
        L1ReadError::Unavailable(error) => BlockError::Disable(error),
        L1ReadError::Finding(error) => BlockError::Finding { zone, error },
        L1ReadError::Disable(error) => BlockError::Disable(error),
    }
}

/// Retry one Tempo read while it is merely unavailable.
async fn retry_l1_read<T, F, Fut>(
    operation: &str,
    zone: BlockRef,
    context: &VerificationContext<'_>,
    mut read: F,
) -> Result<T, BlockError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, L1ReadError>>,
{
    let mut backoff = Backoff::new(context.limits.retry_delay, context.limits.max_retry_delay);
    loop {
        match read().await {
            Ok(value) => return Ok(value),
            Err(L1ReadError::Unavailable(error)) => {
                context.metrics.acquisition_retries_total.increment(1);
                tracing::warn!(
                    target: "zone::checker",
                    %error,
                    operation,
                    attempt = backoff.attempted(),
                    "checker L1 acquisition failed; retrying"
                );
                backoff.wait().await;
            }
            Err(error) => return Err(classify_block_l1_error(error, zone)),
        }
    }
}

/// A Zone block may import exactly one new Tempo block.
fn validate_tempo_advance(parent: u64, tip: u64) -> eyre::Result<()> {
    let expected = parent
        .checked_add(1)
        .ok_or_else(|| eyre::eyre!("Tempo block number overflow after {parent}"))?;
    eyre::ensure!(
        tip == expected,
        "Zone advanced Tempo from block {parent} to {tip}; expected {expected}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tempo_advance_requires_the_exact_successor() {
        assert!(validate_tempo_advance(10, 11).is_ok());
        for tip in [9, 10, 12, u64::MAX] {
            assert!(validate_tempo_advance(10, tip).is_err());
        }
        assert!(validate_tempo_advance(u64::MAX, u64::MAX).is_err());
    }
}
