//! Durable notification processing and observe-only failure isolation.

use std::{collections::BTreeSet, future::Future, time::Duration};

use alloy_consensus::BlockHeader as _;
use alloy_eips::BlockNumHash;
use alloy_provider::{DynProvider, Provider as _, ProviderBuilder};
use futures::{StreamExt as _, TryStreamExt as _, future};
use reth_chainspec::ChainSpecProvider;
use reth_exex::{ExExContext, ExExHead, ExExNotification};
use reth_node_api::{BlockBody as _, FullNodeComponents, NodePrimitives};
use reth_primitives_traits::RecoveredBlock;
use reth_storage_api::{BlockNumReader, StateProviderFactory};
use tempo_alloy::TempoNetwork;
use tempo_chainspec::spec::TempoHardforks;

use crate::{
    AttemptError, CheckerConfig,
    accounting::effects,
    bootstrap,
    l1::{L1ReadError, classify_rpc_error, collect_l1_block_at, portal_balances},
    l2::{AccountingStateError, collect_l2_block_evidence, read_accounting_state},
    persistence::{
        AppliedStatus, BlockRef, CandidateTransition, Finding, PersistenceError, Snapshot, Status,
        Store,
    },
    telemetry::{self, CheckerMetrics},
};

const RETRY_DELAY: Duration = Duration::from_secs(1);

/// `NodePrimitives` whose transaction and receipt types the checker can process.
pub(crate) trait CheckedPrimitives: NodePrimitives {}

impl<N> CheckedPrimitives for N
where
    N: NodePrimitives,
    N::SignedTx: alloy_consensus::transaction::TxHashRef,
    N::Receipt: alloy_consensus::TxReceipt<Log = alloy_primitives::Log>,
{
}

/// Bootstrap or open durable state, recover from its verified tip, and follow notifications.
pub(crate) async fn run<Node>(
    config: CheckerConfig,
    ctx: &mut ExExContext<Node>,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    Node::Provider: BlockNumReader + ChainSpecProvider + StateProviderFactory,
    <Node::Provider as ChainSpecProvider>::ChainSpec: TempoHardforks,
    <Node::Types as reth_node_api::NodeTypes>::Primitives: CheckedPrimitives,
{
    let metrics = CheckerMetrics::default();
    metrics.disabled.set(0.0);
    let result = run_inner(config, ctx, &metrics).await;
    metrics.disabled.set(1.0);
    match result {
        Ok(()) => tracing::error!(
            target: "zone::checker",
            "checker stopped unexpectedly; Zone execution continues"
        ),
        Err(error) => tracing::error!(
            target: "zone::checker",
            %error,
            "checker disabled; Zone execution continues"
        ),
    }
    // After verification stops, drain and acknowledge notifications so the ExEx cannot block the node.
    while let Some(notification) = ctx.notifications.next().await {
        let result = notification
            .and_then(|notification| {
                notification_tip(&notification)
                    .ok_or_else(|| eyre::eyre!("received an empty ExEx notification"))
            })
            .and_then(|tip| ctx.send_finished_height(tip).map_err(Into::into));
        if let Err(error) = result {
            tracing::error!(
                target: "zone::checker",
                %error,
                "checker cannot drain notifications; parking ExEx"
            );
            break;
        }
    }
    future::pending().await
}

/// Bootstrap or open durable state, then verify or recover from each notification in turn.
///
/// Transient acquisition failures are retried. An authenticated divergence is
/// persisted until a recoverable reorg clears it, while deterministic checker
/// failures return so the outer runtime can disable verification and drain.
async fn run_inner<Node>(
    config: CheckerConfig,
    ctx: &mut ExExContext<Node>,
    metrics: &CheckerMetrics,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    Node::Provider: BlockNumReader + ChainSpecProvider + StateProviderFactory,
    <Node::Provider as ChainSpecProvider>::ChainSpec: TempoHardforks,
    <Node::Types as reth_node_api::NodeTypes>::Primitives: CheckedPrimitives,
{
    tracing::info!(target: "zone::checker", "checker started");
    let l1 = connect(&config.l1_rpc_url).await?;
    let checkpoint = retry_transient(
        || bootstrap::build(ctx.provider(), &l1, &config),
        "checker bootstrap failed",
    )
    .await?;
    let (store, mut snapshot) = Store::open_or_create(&config.database_path, &checkpoint)?;
    metrics.update(&snapshot);

    ctx.catch_up_notifications_with_head(ExExHead::new(snapshot.metadata.verified_zone.into()))?;
    ctx.send_finished_height(snapshot.metadata.verified_zone.into())?;

    while let Some(notification) = ctx.notifications.try_next().await? {
        let delivered_tip = notification_tip(&notification)
            .ok_or_else(|| eyre::eyre!("received an empty ExEx notification"))?;
        let recoverable_finding = match &snapshot.metadata.status {
            Status::Diverged { finding } => notification_ancestor(&notification)?
                .is_some_and(|ancestor| ancestor.number < finding.zone.number),
            Status::Verifying => false,
        };
        if matches!(&snapshot.metadata.status, Status::Diverged { .. }) && !recoverable_finding {
            snapshot = store.observe(&snapshot, delivered_tip.into())?;
            metrics.update(&snapshot);
            ctx.send_finished_height(delivered_tip)?;
            continue;
        }
        if !recoverable_finding {
            snapshot = store.observe(&snapshot, delivered_tip.into())?;
            metrics.update(&snapshot);
        }

        loop {
            let previous_verified = snapshot.metadata.verified_zone.number;
            match process_notification(
                &notification,
                ctx.provider(),
                &l1,
                &store,
                snapshot,
                &config,
            )
            .await
            {
                Ok(Outcome::Applied(next)) => {
                    let next = *next;
                    let verified = next
                        .metadata
                        .verified_zone
                        .number
                        .saturating_sub(previous_verified);
                    snapshot = next;
                    metrics.verified_zone_blocks_total.increment(verified);
                    metrics.update(&snapshot);
                    ctx.send_finished_height(snapshot.metadata.verified_zone.into())?;
                    break;
                }
                Ok(Outcome::Rebuild) => {
                    snapshot = store.reset(&checkpoint)?;
                    metrics.recovery_rebuilds_total.increment(1);
                    metrics.update(&snapshot);
                    ctx.catch_up_notifications_with_head(ExExHead::new(checkpoint.zone.into()))?;
                    tracing::warn!(target: "zone::checker", "rebuilding after reorg beyond retained history");
                    break;
                }
                Err(BlockError::Finding { zone, error }) => {
                    snapshot = store.load()?;
                    tracing::error!(target: "zone::checker", %error, zone_block = zone.number, zone_hash = %zone.hash, "checker divergence");
                    snapshot = store.record_finding(
                        &snapshot,
                        Finding {
                            zone,
                            summary: error.to_string(),
                        },
                    )?;
                    metrics.update(&snapshot);
                    ctx.send_finished_height(delivered_tip)?;
                    break;
                }
                Err(BlockError::Retry(error)) => {
                    snapshot = store.load()?;
                    metrics.acquisition_retries_total.increment(1);
                    metrics.update(&snapshot);
                    tracing::warn!(target: "zone::checker", %error, "checker block processing failed; retrying");
                    tokio::time::sleep(RETRY_DELAY).await;
                }
                Err(BlockError::Disable(error)) => return Err(error),
            }
        }
    }
    tracing::info!(target: "zone::checker", "checker notification stream closed");
    Ok(())
}

async fn connect(url: &str) -> eyre::Result<DynProvider<TempoNetwork>> {
    let provider = retry_transient(
        || async {
            ProviderBuilder::new_with_network::<TempoNetwork>()
                .connect(url)
                .await
                .map_err(classify_rpc_error)
        },
        "Tempo RPC unavailable",
    )
    .await?;
    Ok(provider.erased())
}

/// Retry transient failure and return failures that disable the checker.
async fn retry_transient<T, F, Fut>(mut attempt: F, message: &str) -> eyre::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, AttemptError>>,
{
    loop {
        match attempt().await {
            Ok(value) => return Ok(value),
            Err(AttemptError::Retry(error)) => {
                tracing::warn!(target: "zone::checker", %error, "{message}; retrying");
                tokio::time::sleep(RETRY_DELAY).await;
            }
            Err(AttemptError::Disable(error)) => return Err(error),
        }
    }
}

/// Return the notification's new canonical tip, or the reverted parent for a `ChainReverted`.
fn notification_tip<N: NodePrimitives>(notification: &ExExNotification<N>) -> Option<BlockNumHash> {
    match notification {
        ExExNotification::ChainCommitted { new } | ExExNotification::ChainReorged { new, .. } => {
            let (&number, block) = new.blocks().iter().next_back()?;
            Some(BlockNumHash::new(number, block.hash()))
        }
        ExExNotification::ChainReverted { old } => {
            let (&number, block) = old.blocks().iter().next()?;
            Some(BlockNumHash::new(
                number.saturating_sub(1),
                block.header().parent_hash(),
            ))
        }
    }
}

/// Return the reorg's new parent for a reverted or reorged chain, or `None` for a plain commit.
fn notification_ancestor<N: NodePrimitives>(
    notification: &ExExNotification<N>,
) -> eyre::Result<Option<BlockRef>> {
    let old = match notification {
        ExExNotification::ChainReverted { old } | ExExNotification::ChainReorged { old, .. } => old,
        ExExNotification::ChainCommitted { .. } => return Ok(None),
    };
    let (&number, block) = old
        .blocks()
        .iter()
        .next()
        .ok_or_else(|| eyre::eyre!("received an empty reverted chain"))?;
    Ok(Some(BlockRef::new(
        number.saturating_sub(1),
        block.header().parent_hash(),
    )))
}

/// Result of processing one notification's blocks.
enum Outcome {
    Applied(Box<Snapshot>),
    /// Reorg exceeded retained history; state must be rebuilt from the checkpoint.
    Rebuild,
}

/// Failure processing one block.
enum BlockError {
    /// Transient failure; retry without advancing.
    Retry(eyre::Report),
    /// Authenticated divergence to persist as a finding.
    Finding { zone: BlockRef, error: eyre::Report },
    /// Deterministic checker failure that cannot be resolved by retrying.
    Disable(eyre::Report),
}

impl From<AccountingStateError> for BlockError {
    fn from(error: AccountingStateError) -> Self {
        match error {
            AccountingStateError::Unavailable(error) => Self::Retry(error),
            AccountingStateError::Disable(error) => Self::Disable(error),
        }
    }
}

/// Verify or recover from one notification's blocks.
///
/// Returns `Outcome::Rebuild` if a reorg unwinds past the retained delta
/// history instead of an error, since that case has a recovery path.
async fn process_notification<N, P>(
    notification: &ExExNotification<N>,
    provider: &P,
    l1: &DynProvider<TempoNetwork>,
    store: &Store,
    snapshot: Snapshot,
    config: &CheckerConfig,
) -> Result<Outcome, BlockError>
where
    N: CheckedPrimitives,
    P: ChainSpecProvider + StateProviderFactory,
    P::ChainSpec: TempoHardforks,
{
    let mut current = snapshot;
    if let Some(ancestor) = notification_ancestor(notification).map_err(BlockError::Disable)? {
        current = match store.reorg(&current, ancestor) {
            Ok(snapshot) => snapshot,
            Err(PersistenceError::ReorgBeyondRetention { .. }) => return Ok(Outcome::Rebuild),
            Err(error) => return Err(BlockError::Disable(error.into())),
        };
    }
    let new = match notification {
        ExExNotification::ChainCommitted { new } | ExExNotification::ChainReorged { new, .. } => {
            Some(new)
        }
        ExExNotification::ChainReverted { .. } => None,
    };
    if let Some(new) = new {
        for (block, receipts) in new.blocks_and_receipts() {
            if already_applied(&current, block.header().number(), block.hash())? {
                continue;
            }
            current =
                verify_block::<N, _>(provider, l1, store, current, config, block, receipts).await?;
        }
    }
    Ok(Outcome::Applied(Box::new(current)))
}

fn already_applied(
    snapshot: &Snapshot,
    number: u64,
    hash: alloy_primitives::B256,
) -> Result<bool, BlockError> {
    match snapshot.metadata.classify(number, hash) {
        AppliedStatus::New => Ok(false),
        AppliedStatus::Applied => Ok(true),
        AppliedStatus::Conflicts => Err(BlockError::Finding {
            zone: BlockRef::new(number, hash),
            error: eyre::eyre!("notification conflicts with the persisted verified tip"),
        }),
    }
}

/// Verify one Zone block's bridge accounting against Tempo history and Portal
/// collateral, persisting the result on success.
async fn verify_block<N, P>(
    provider: &P,
    l1: &DynProvider<TempoNetwork>,
    store: &Store,
    prior: Snapshot,
    config: &CheckerConfig,
    block: &RecoveredBlock<N::Block>,
    receipts: &[N::Receipt],
) -> Result<Snapshot, BlockError>
where
    N: CheckedPrimitives,
    P: ChainSpecProvider + StateProviderFactory,
    P::ChainSpec: TempoHardforks,
{
    let number = block.number();
    let hash = block.hash();
    let parent_hash = block.parent_hash();
    let zone = BlockRef::new(number, hash);
    let fail = |error| BlockError::Finding { zone, error };
    let l2 = collect_l2_block_evidence(block.body().transactions(), receipts, zone.into())
        .map_err(fail)?;
    let anchor = l2.l1_anchor();
    let tempo = BlockNumHash::new(anchor.block_number(), anchor.block_hash());
    let tempo_parent = BlockNumHash::from(prior.metadata.imported_tempo);
    validate_tempo_advance(tempo_parent.number, tempo.number).map_err(fail)?;
    let tempo_block = collect_l1_block_at(l1, config.portal_address, tempo_parent, tempo)
        .await
        .map_err(|error| classify_block_l1_error(error, zone))?;
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
    let observed =
        read_accounting_state(provider, &accounts, zone.into(), spec).map_err(BlockError::from)?;
    state
        .verify_zone_state(
            observed
                .into_iter()
                .map(|evidence| (evidence.token, evidence.total_supply, evidence.balances)),
        )
        .map_err(|error| fail(error.into()))?;

    let balances = portal_balances(l1, config.portal_address, tokens, tempo.hash)
        .await
        .map_err(|error| classify_block_l1_error(error, zone))?;
    state
        .verify_portal_balances(balances)
        .map_err(|error| fail(error.into()))?;

    let next = store
        .apply(candidate)
        .map_err(|error| BlockError::Disable(error.into()))?;
    telemetry::log_verified_activity(&tempo_block, &l2, zone);
    Ok(next)
}

fn classify_block_l1_error(error: L1ReadError, zone: BlockRef) -> BlockError {
    match error {
        L1ReadError::Unavailable(error) => BlockError::Retry(error),
        L1ReadError::Finding(error) => BlockError::Finding { zone, error },
        L1ReadError::Disable(error) => BlockError::Disable(error),
    }
}

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

    #[tokio::test]
    async fn disable_error_is_not_retried() {
        let attempts = std::cell::Cell::new(0);
        let result = retry_transient(
            || {
                attempts.set(attempts.get() + 1);
                future::ready(Err::<(), _>(AttemptError::Disable(eyre::eyre!(
                    "invalid genesis"
                ))))
            },
            "operation failed",
        )
        .await;

        assert!(result.is_err());
        assert_eq!(attempts.get(), 1);
    }

    #[test]
    fn tempo_advance_requires_the_exact_successor() {
        assert!(validate_tempo_advance(10, 11).is_ok());
        for tip in [9, 10, 12, u64::MAX] {
            assert!(validate_tempo_advance(10, tip).is_err());
        }
        assert!(validate_tempo_advance(u64::MAX, u64::MAX).is_err());
    }
}
