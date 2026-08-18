//! Durable notification processing and observe-only failure isolation.

use std::{collections::BTreeSet, path::Path, time::Duration};

use alloy_consensus::BlockHeader as _;
use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, U256};
use alloy_provider::{DynProvider, Provider as _, ProviderBuilder};
use futures::{StreamExt as _, TryStreamExt as _, stream};
use reth_exex::{ExExContext, ExExHead, ExExNotification};
use reth_node_api::{BlockBody as _, FullNodeComponents, NodePrimitives};
use reth_storage_api::{BlockNumReader, StateProviderFactory};
use tempo_alloy::TempoNetwork;

use crate::{
    CheckerConfig,
    accounting::effects,
    bootstrap,
    l1::{collect_l1_history, portal_balance},
    l2::{collect_l2_block_evidence, read_accounting_state},
    metrics::CheckerMetrics,
    model::{TokenEnablementModel, TokenModelResult},
    persistence::{BlockRef, Finding, PersistenceError, Snapshot, Status, Store},
};

const RETRY_DELAY: Duration = Duration::from_secs(1);
const COLLATERAL_CONCURRENCY: usize = 8;

/// Bootstrap or open durable state, recover from its verified tip, and follow notifications.
pub(crate) async fn run<Node>(
    config: CheckerConfig,
    ctx: &mut ExExContext<Node>,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    Node::Provider: BlockNumReader + StateProviderFactory,
    <Node::Types as reth_node_api::NodeTypes>::Primitives: NodePrimitives,
    <<Node::Types as reth_node_api::NodeTypes>::Primitives as NodePrimitives>::SignedTx:
        alloy_consensus::Transaction + alloy_consensus::transaction::TxHashRef,
    <<Node::Types as reth_node_api::NodeTypes>::Primitives as NodePrimitives>::Receipt:
        alloy_consensus::TxReceipt<Log = alloy_primitives::Log>,
{
    if let Err(error) = run_inner(config, ctx).await {
        tracing::error!(target: "zone::checker", %error, "checker disabled; Zone execution continues");
        while let Some(notification) = ctx.notifications.try_next().await? {
            ctx.send_finished_height(notification_tip(&notification))?;
        }
    }
    Ok(())
}

async fn run_inner<Node>(config: CheckerConfig, ctx: &mut ExExContext<Node>) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    Node::Provider: BlockNumReader + StateProviderFactory,
    <Node::Types as reth_node_api::NodeTypes>::Primitives: NodePrimitives,
    <<Node::Types as reth_node_api::NodeTypes>::Primitives as NodePrimitives>::SignedTx:
        alloy_consensus::Transaction + alloy_consensus::transaction::TxHashRef,
    <<Node::Types as reth_node_api::NodeTypes>::Primitives as NodePrimitives>::Receipt:
        alloy_consensus::TxReceipt<Log = alloy_primitives::Log>,
{
    tracing::info!(target: "zone::checker", "checker started");
    let metrics = CheckerMetrics::default();
    let l1 = connect(&config.l1_rpc_url).await;
    let checkpoint = loop {
        match bootstrap::checkpoint(
            ctx.provider(),
            &l1,
            config.portal_address,
            config.zone_id,
            config.zone_chain_id,
        )
        .await
        {
            Ok(checkpoint) => break checkpoint,
            Err(error) => {
                tracing::warn!(target: "zone::checker", %error, "checker bootstrap unavailable; retrying");
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
    };
    let (store, mut snapshot) = open_or_create(&config.database_path, &checkpoint)?;
    publish(&metrics, &snapshot);

    ctx.catch_up_notifications_with_head(ExExHead::new(snapshot.metadata.verified_zone.into()))?;
    ctx.send_finished_height(snapshot.metadata.verified_zone.into())?;

    while let Some(notification) = ctx.notifications.try_next().await? {
        let delivered_tip = notification_tip(&notification);
        let recoverable_finding = match snapshot.metadata.status {
            Status::Diverged {
                first_unchecked, ..
            } => notification_ancestor(&notification)
                .is_some_and(|ancestor| ancestor.number < first_unchecked.number),
            Status::Verifying => false,
        };
        if matches!(snapshot.metadata.status, Status::Diverged { .. }) && !recoverable_finding {
            snapshot = store.observe_diverged(&snapshot, delivered_tip.into())?;
            publish(&metrics, &snapshot);
            ctx.send_finished_height(delivered_tip)?;
            continue;
        }
        if !recoverable_finding {
            snapshot = store.observe(&snapshot, delivered_tip.into())?;
            publish(&metrics, &snapshot);
        }

        loop {
            match process_notification(
                &notification,
                ctx.provider(),
                &l1,
                &store,
                &snapshot,
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
                        .saturating_sub(snapshot.metadata.verified_zone.number);
                    snapshot = next;
                    metrics.verified_zone_blocks_total.increment(verified);
                    publish(&metrics, &snapshot);
                    ctx.send_finished_height(snapshot.metadata.verified_zone.into())?;
                    break;
                }
                Ok(Outcome::Rebuild) => {
                    snapshot =
                        store.reset(checkpoint.zone, checkpoint.tempo, checkpoint.state.clone())?;
                    metrics.recovery_rebuilds_total.increment(1);
                    publish(&metrics, &snapshot);
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
                    publish(&metrics, &snapshot);
                    ctx.send_finished_height(notification_tip(&notification))?;
                    break;
                }
                Err(BlockError::Retry(error)) => {
                    snapshot = store.load()?;
                    metrics.acquisition_retries_total.increment(1);
                    publish(&metrics, &snapshot);
                    tracing::warn!(target: "zone::checker", %error, "checker acquisition unavailable; retrying");
                    tokio::time::sleep(RETRY_DELAY).await;
                }
            }
        }
    }
    tracing::info!(target: "zone::checker", "checker notification stream closed");
    Ok(())
}

fn publish(metrics: &CheckerMetrics, snapshot: &Snapshot) {
    let verified = snapshot.metadata.verified_zone.number;
    let observed = snapshot.metadata.observed_zone.number;
    metrics.verified_zone_height.set(verified as f64);
    metrics
        .imported_tempo_height
        .set(snapshot.metadata.imported_tempo.number as f64);
    metrics.observed_zone_height.set(observed as f64);
    metrics
        .verification_lag_blocks
        .set(observed.saturating_sub(verified) as f64);
    metrics.divergence_active.set(
        if matches!(snapshot.metadata.status, Status::Diverged { .. }) {
            1.0
        } else {
            0.0
        },
    );
}

async fn connect(url: &str) -> DynProvider<TempoNetwork> {
    loop {
        match ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(url)
            .await
        {
            Ok(provider) => return provider.erased(),
            Err(error) => {
                tracing::warn!(target: "zone::checker", %error, "Tempo RPC unavailable; retrying");
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
    }
}

fn open_or_create(
    path: &Path,
    checkpoint: &bootstrap::Checkpoint,
) -> eyre::Result<(Store, Snapshot)> {
    if path.exists() {
        return Store::open(path, checkpoint.identity).map_err(Into::into);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Store::create_atomic(
        path,
        checkpoint.identity,
        checkpoint.zone,
        checkpoint.tempo,
        checkpoint.state.clone(),
    )?;
    Store::open(path, checkpoint.identity).map_err(Into::into)
}

enum Outcome {
    Applied(Box<Snapshot>),
    Rebuild,
}

enum BlockError {
    Retry(eyre::Report),
    Finding { zone: BlockRef, error: eyre::Report },
}

async fn process_notification<N, P>(
    notification: &ExExNotification<N>,
    provider: &P,
    l1: &DynProvider<TempoNetwork>,
    store: &Store,
    snapshot: &Snapshot,
    config: &CheckerConfig,
) -> Result<Outcome, BlockError>
where
    N: NodePrimitives,
    N::SignedTx: alloy_consensus::Transaction + alloy_consensus::transaction::TxHashRef,
    N::Receipt: alloy_consensus::TxReceipt<Log = alloy_primitives::Log>,
    P: StateProviderFactory,
{
    let mut current = snapshot.clone();
    match notification {
        ExExNotification::ChainCommitted { new } => {
            for (block, receipts) in new.blocks_and_receipts() {
                if already_applied(&current, block.header().number(), block.hash())? {
                    continue;
                }
                current = verify_block(
                    provider,
                    l1,
                    store,
                    &current,
                    config,
                    block.header().number(),
                    block.hash(),
                    block.header().parent_hash(),
                    block.body().transactions(),
                    block.senders(),
                    receipts,
                )
                .await?;
            }
        }
        ExExNotification::ChainReverted { old } => {
            let (&number, block) = old.blocks().iter().next().expect("non-empty notification");
            let ancestor = BlockRef::new(number.saturating_sub(1), block.header().parent_hash());
            current = match store.reorg(&current, ancestor) {
                Ok(snapshot) => snapshot,
                Err(PersistenceError::ReorgBeyondRetention { .. }) => return Ok(Outcome::Rebuild),
                Err(error) => return Err(BlockError::Retry(error.into())),
            };
        }
        ExExNotification::ChainReorged { old, new } => {
            let (&number, block) = old.blocks().iter().next().expect("non-empty notification");
            let ancestor = BlockRef::new(number.saturating_sub(1), block.header().parent_hash());
            current = match store.reorg(&current, ancestor) {
                Ok(snapshot) => snapshot,
                Err(PersistenceError::ReorgBeyondRetention { .. }) => return Ok(Outcome::Rebuild),
                Err(error) => return Err(BlockError::Retry(error.into())),
            };
            for (block, receipts) in new.blocks_and_receipts() {
                if already_applied(&current, block.header().number(), block.hash())? {
                    continue;
                }
                current = verify_block(
                    provider,
                    l1,
                    store,
                    &current,
                    config,
                    block.header().number(),
                    block.hash(),
                    block.header().parent_hash(),
                    block.body().transactions(),
                    block.senders(),
                    receipts,
                )
                .await?;
            }
        }
    }
    Ok(Outcome::Applied(Box::new(current)))
}

fn already_applied(
    snapshot: &Snapshot,
    number: u64,
    hash: alloy_primitives::B256,
) -> Result<bool, BlockError> {
    let tip = snapshot.metadata.verified_zone;
    if number > tip.number {
        return Ok(false);
    }
    if number == tip.number && hash != tip.hash {
        return Err(BlockError::Finding {
            zone: BlockRef::new(number, hash),
            error: eyre::eyre!("notification conflicts with the persisted verified tip"),
        });
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
async fn verify_block<P, T, R>(
    provider: &P,
    l1: &DynProvider<TempoNetwork>,
    store: &Store,
    prior: &Snapshot,
    config: &CheckerConfig,
    number: u64,
    hash: alloy_primitives::B256,
    parent_hash: alloy_primitives::B256,
    transactions: &[T],
    senders: &[Address],
    receipts: &[R],
) -> Result<Snapshot, BlockError>
where
    P: StateProviderFactory,
    T: alloy_consensus::Transaction + alloy_consensus::transaction::TxHashRef,
    R: alloy_consensus::TxReceipt<Log = alloy_primitives::Log>,
{
    let zone = BlockRef::new(number, hash);
    let fail = |error| BlockError::Finding { zone, error };
    let l2 = collect_l2_block_evidence(
        provider,
        transactions,
        senders,
        receipts,
        BlockNumHash::new(number, hash),
    )
    .map_err(fail)?;
    let tempo = BlockNumHash::new(l2.l1_anchor().block_number(), l2.l1_anchor().block_hash());
    let tempo_parent = BlockNumHash::from(prior.metadata.imported_tempo);
    let history = collect_l1_history(l1, config.portal_address, tempo_parent, tempo)
        .await
        .map_err(BlockError::Retry)?;
    if history.is_empty() {
        return Err(fail(eyre::eyre!("Zone block did not advance Tempo")));
    }
    match TokenEnablementModel::evaluate_history(&history, &l2) {
        TokenModelResult::Pass { token_count } => {
            tracing::debug!(target: "zone::checker", token_count, "token enablement verified");
        }
        TokenModelResult::Violations(violations) => {
            return Err(fail(eyre::eyre!(
                "token enablement mismatch: {violations:?}"
            )));
        }
    }

    let mut block_effects = history
        .iter()
        .flat_map(|block| effects::from_tempo(block.portal_events()))
        .collect::<Vec<_>>();
    block_effects
        .extend(effects::from_zone(l2.bridge_events()).map_err(|error| fail(error.into()))?);
    let mut candidate = prior.state.as_ref().clone();
    candidate
        .apply(&block_effects)
        .map_err(|error| fail(error.into()))?;
    let mut accounts = l2.accounting_candidates();
    let tokens = candidate
        .tokens()
        .map(|(token, _)| token)
        .collect::<BTreeSet<_>>();
    for token in &tokens {
        accounts.entry(*token).or_default();
    }
    let observed = read_accounting_state(provider, &accounts, BlockNumHash::new(number, hash))
        .map_err(BlockError::Retry)?;
    candidate
        .verify_zone_state(
            observed.iter().flat_map(|token| {
                token.balances.iter().map(move |(&account, &balance)| {
                    (
                        crate::accounting::AccountKey::new(token.token, account),
                        balance,
                    )
                })
            }),
            observed
                .iter()
                .map(|token| (token.token, token.total_supply)),
        )
        .map_err(|error| fail(error.into()))?;

    let balances = stream::iter(tokens.into_iter().map(|token| async move {
        portal_balance(l1, token, config.portal_address, tempo.hash)
            .await
            .map(|balance| (token, balance))
    }))
    .buffer_unordered(COLLATERAL_CONCURRENCY)
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .collect::<eyre::Result<Vec<(Address, U256)>>>()
    .map_err(BlockError::Retry)?;
    candidate
        .verify_portal_balances(balances)
        .map_err(|error| fail(error.into()))?;

    store
        .apply(
            prior,
            zone,
            BlockRef::new(number.saturating_sub(1), parent_hash),
            tempo.into(),
            prior.metadata.imported_tempo,
            &block_effects,
        )
        .map_err(|error| BlockError::Retry(error.into()))
}

fn notification_tip<N: NodePrimitives>(notification: &ExExNotification<N>) -> BlockNumHash {
    match notification {
        ExExNotification::ChainCommitted { new } | ExExNotification::ChainReorged { new, .. } => {
            let tip = new.tip();
            BlockNumHash::new(tip.header().number(), tip.hash())
        }
        ExExNotification::ChainReverted { old } => {
            let (&number, block) = old.blocks().iter().next().expect("non-empty notification");
            BlockNumHash::new(number.saturating_sub(1), block.header().parent_hash())
        }
    }
}

fn notification_ancestor<N: NodePrimitives>(
    notification: &ExExNotification<N>,
) -> Option<BlockRef> {
    let old = match notification {
        ExExNotification::ChainReverted { old } | ExExNotification::ChainReorged { old, .. } => old,
        ExExNotification::ChainCommitted { .. } => return None,
    };
    let (&number, block) = old.blocks().iter().next().expect("non-empty notification");
    Some(BlockRef::new(
        number.saturating_sub(1),
        block.header().parent_hash(),
    ))
}

impl From<BlockRef> for BlockNumHash {
    fn from(value: BlockRef) -> Self {
        Self::new(value.number, value.hash)
    }
}
