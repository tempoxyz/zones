//! Durable notification processing and observe-only failure isolation.

use std::{collections::BTreeSet, future::Future, time::Duration};

use alloy_consensus::BlockHeader as _;
use alloy_eips::{BlockHashOrNumber, BlockNumHash};
use alloy_provider::{DynProvider, Provider as _, ProviderBuilder};
use alloy_rpc_client::{ConnectionConfig, RpcClient, WebSocketConfig};
use alloy_transport::{TransportError, TransportErrorKind, TransportFut};
use futures::{Stream, StreamExt as _, TryStreamExt as _, future};
use reth_chainspec::ChainSpecProvider;
use reth_exex::{ExExContext, ExExNotification};
use reth_node_api::{BlockBody as _, FullNodeComponents, NodePrimitives};
use reth_primitives_traits::RecoveredBlock;
use reth_storage_api::{BlockNumReader, BlockReader, StateProviderFactory, TransactionVariant};
use tempo_alloy::TempoNetwork;
use tempo_chainspec::spec::TempoHardforks;
use tower::{
    BoxError,
    timeout::TimeoutLayer,
    util::{MapErrLayer, MapFutureLayer},
};

use crate::{
    AttemptError, CheckerConfig,
    accounting::effects,
    bootstrap,
    l1::{L1ReadError, classify_rpc_error, collect_l1_block_at, portal_balances},
    l2::{AccountingStateError, collect_l2_block_evidence, read_accounting_state},
    persistence::{BlockRef, CandidateTransition, Finding, Snapshot, Status, Store},
    telemetry::{self, CheckerMetrics},
};

const RETRY_DELAY: Duration = Duration::from_secs(1);
/// Maximum delay between Tempo acquisition attempts.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const MAX_WS_FRAME_AND_MESSAGE_SIZE: usize = 128 * 1024 * 1024;
/// Keep Alloy's established WebSocket recovery active beyond the checker work deadline.
const WS_RECONNECT_MAX_RETRIES: u32 = 15;
const WS_RECONNECT_RETRY_INTERVAL: Duration = Duration::from_secs(3);
const CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TOTAL_TIMEOUT: Duration = Duration::from_secs(2 * 60);
/// Bound one established RPC acquisition so a missing response becomes retryable.
const RPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const BOOTSTRAP_TOTAL_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// The only bound on one block's retry loops, so it is sized to ride out a transient outage.
const BLOCK_VERIFICATION_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Runtime limits are private policy rather than node CLI surface.
#[derive(Debug, Clone, Copy)]
struct RuntimeLimits {
    retry_delay: Duration,
    max_retry_delay: Duration,
    connect_attempt_timeout: Duration,
    connect_total_timeout: Duration,
    rpc_request_timeout: Duration,
    bootstrap_total_timeout: Duration,
    block_verification_timeout: Duration,
}

impl RuntimeLimits {
    const PRODUCTION: Self = Self {
        retry_delay: RETRY_DELAY,
        max_retry_delay: MAX_RETRY_DELAY,
        connect_attempt_timeout: CONNECT_ATTEMPT_TIMEOUT,
        connect_total_timeout: CONNECT_TOTAL_TIMEOUT,
        rpc_request_timeout: RPC_REQUEST_TIMEOUT,
        bootstrap_total_timeout: BOOTSTRAP_TOTAL_TIMEOUT,
        block_verification_timeout: BLOCK_VERIFICATION_TIMEOUT,
    };
}

/// Notification delivery advances independently from durable checker verification.
#[derive(Debug, Clone, Copy)]
struct RuntimeProgress {
    last_delivered_tip: BlockNumHash,
}

impl RuntimeProgress {
    const fn new(node_head: BlockNumHash) -> Self {
        Self {
            last_delivered_tip: node_head,
        }
    }

    fn ensure_no_conflicting_commit(&self, delivery: DeliveredNotification) -> eyre::Result<()> {
        if delivery.kind == NotificationKind::Committed
            && delivery.tip.number == self.last_delivered_tip.number
            && delivery.tip.hash != self.last_delivered_tip.hash
        {
            eyre::bail!(
                "Zone append-only invariant violated: delivered block {} changed from {} to {}",
                delivery.tip.number,
                self.last_delivered_tip.hash,
                delivery.tip.hash
            );
        }
        Ok(())
    }

    fn delivered(&mut self, delivery: DeliveredNotification) {
        if delivery.kind != NotificationKind::Committed
            || delivery.tip.number > self.last_delivered_tip.number
        {
            self.last_delivered_tip = delivery.tip;
        }
    }

    fn delivered_checked(&mut self, delivery: DeliveredNotification) -> eyre::Result<()> {
        self.ensure_no_conflicting_commit(delivery)?;
        ensure_append_only(delivery)?;
        self.delivered(delivery);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotificationKind {
    Committed,
    Reorged,
    Reverted,
}

#[derive(Debug, Clone, Copy)]
struct DeliveredNotification {
    tip: BlockNumHash,
    kind: NotificationKind,
}

#[derive(Debug)]
enum DriveError<E> {
    Work(E),
    Notifications(eyre::Report),
}

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
    Node::Provider: BlockReader<
            Block = <<Node::Types as reth_node_api::NodeTypes>::Primitives as NodePrimitives>::Block,
            Receipt = <<Node::Types as reth_node_api::NodeTypes>::Primitives as NodePrimitives>::Receipt,
        > + ChainSpecProvider
        + StateProviderFactory
        + Clone,
    <Node::Provider as ChainSpecProvider>::ChainSpec: TempoHardforks,
    <Node::Types as reth_node_api::NodeTypes>::Primitives: CheckedPrimitives,
{
    let limits = RuntimeLimits::PRODUCTION;
    let metrics = CheckerMetrics::default();
    let mut progress = RuntimeProgress::new(ctx.head);
    // The checker reconstructs canonical history itself and uses ExEx notifications only as
    // append-only tip signals. This avoids Reth backfill execution and private buffered state.
    ctx.set_notifications_without_head();
    metrics.disabled.set(0.0);
    let result = run_inner(config, ctx, &metrics, &mut progress, limits).await;
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
    // Delivery already releases ExEx channel backpressure. Keep draining without advancing
    // FinishedHeight beyond durable verification so restart history remains available.
    while let Some(notification) = ctx.notifications.next().await {
        match notification.and_then(|notification| classify_notification(&notification)) {
            Ok(delivery) => {
                if let Err(error) = progress.delivered_checked(delivery) {
                    tracing::error!(
                        target: "zone::checker",
                        %error,
                        "checker received invalid history while disabled; continuing to drain"
                    );
                }
            }
            Err(error) => {
                tracing::error!(
                    target: "zone::checker",
                    %error,
                    "checker received an invalid notification while disabled; continuing to drain"
                );
            }
        }
    }
    future::pending().await
}

/// Bootstrap or open durable state, then verify canonical Zone blocks in order.
///
/// Transient acquisition failures are retried until their enclosing deadline. An authenticated
/// divergence is persisted until the checker is rebuilt, while deterministic failures or expired
/// deadlines return so the outer runtime can disable and drain.
async fn run_inner<Node>(
    config: CheckerConfig,
    ctx: &mut ExExContext<Node>,
    metrics: &CheckerMetrics,
    progress: &mut RuntimeProgress,
    limits: RuntimeLimits,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    Node::Provider: BlockReader<
            Block = <<Node::Types as reth_node_api::NodeTypes>::Primitives as NodePrimitives>::Block,
            Receipt = <<Node::Types as reth_node_api::NodeTypes>::Primitives as NodePrimitives>::Receipt,
        > + ChainSpecProvider
        + StateProviderFactory
        + Clone,
    <Node::Provider as ChainSpecProvider>::ChainSpec: TempoHardforks,
    <Node::Types as reth_node_api::NodeTypes>::Primitives: CheckedPrimitives,
{
    tracing::info!(target: "zone::checker", "checker started");
    let provider = ctx.provider().clone();
    let initialization = initialize(&config, &provider, metrics, limits);
    let (store, mut snapshot, l1) = drive_exex_work(ctx, progress, initialization)
        .await
        .map_err(drive_eyre_error)?;

    snapshot = observe_tip(&provider, &store, snapshot, progress.last_delivered_tip)?;
    metrics.update(&snapshot);
    ctx.send_finished_height(snapshot.metadata.verified_zone.into())?;
    let verification_context = VerificationContext {
        config: &config,
        metrics,
        limits,
    };

    loop {
        snapshot = observe_tip(&provider, &store, snapshot, progress.last_delivered_tip)?;
        metrics.update(&snapshot);

        if matches!(&snapshot.metadata.status, Status::Diverged { .. })
            || snapshot.metadata.verified_zone.number >= progress.last_delivered_tip.number
        {
            let Some(notification) = ctx.notifications.try_next().await? else {
                break;
            };
            progress.delivered_checked(classify_notification(&notification)?)?;
            continue;
        }

        let number = snapshot
            .metadata
            .verified_zone
            .number
            .checked_add(1)
            .ok_or_else(|| eyre::eyre!("Zone block number overflow after verification"))?;
        let verification = with_block_timeout(
            process_next_block::<<Node::Types as reth_node_api::NodeTypes>::Primitives, _>(
                number,
                &provider,
                &l1,
                &store,
                snapshot,
                &verification_context,
            ),
            number,
            limits.block_verification_timeout,
        );
        match drive_exex_work(ctx, progress, verification).await {
            Ok(next) => {
                snapshot = next;
                metrics.verified_zone_blocks_total.increment(1);
                metrics.update(&snapshot);
                ctx.send_finished_height(snapshot.metadata.verified_zone.into())?;
            }
            Err(DriveError::Work(BlockError::Finding { zone, error })) => {
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
            }
            Err(DriveError::Work(BlockError::Disable(error))) => return Err(error),
            Err(DriveError::Notifications(error)) => return Err(error),
        }
    }
    tracing::info!(target: "zone::checker", "checker notification stream closed");
    Ok(())
}

fn observe_tip<P: BlockNumReader>(
    provider: &P,
    store: &Store,
    snapshot: Snapshot,
    tip: BlockNumHash,
) -> eyre::Result<Snapshot> {
    let observed = tip.into();
    if snapshot.metadata.observed_zone == observed {
        return Ok(snapshot);
    }
    ensure_canonical_tip(provider, tip)?;
    Ok(store.observe(snapshot, observed)?)
}

/// Load and verify exactly one canonical block after the durable verified tip.
async fn process_next_block<N, P>(
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
    let disable = |error| BlockError::Disable(error);
    let hash = provider
        .block_hash(number)
        .map_err(|error| disable(eyre::Report::new(error)))?
        .ok_or_else(|| {
            disable(eyre::eyre!(
                "canonical Zone block {number} is unavailable; checker restart history may have been pruned"
            ))
        })?;
    let id = BlockHashOrNumber::Hash(hash);
    let block = provider
        .recovered_block(id, TransactionVariant::WithHash)
        .map_err(|error| disable(eyre::Report::new(error)))?
        .ok_or_else(|| {
            disable(eyre::eyre!(
                "canonical Zone block {number} ({hash}) body is unavailable; checker restart history may have been pruned"
            ))
        })?;
    let receipts = provider
        .receipts_by_block(id)
        .map_err(|error| disable(eyre::Report::new(error)))?
        .ok_or_else(|| {
            disable(eyre::eyre!(
                "canonical Zone block {number} ({hash}) receipts are unavailable; checker restart history may have been pruned"
            ))
        })?;

    if block.number() != number || block.hash() != hash {
        return Err(disable(eyre::eyre!(
            "canonical Zone block {number} ({hash}) resolved to {} ({})",
            block.number(),
            block.hash()
        )));
    }
    let verified = snapshot.metadata.verified_zone;
    if block.parent_hash() != verified.hash {
        return Err(disable(eyre::eyre!(
            "Zone append-only invariant violated: canonical block {number} ({hash}) has parent {}, expected verified block {} ({})",
            block.parent_hash(),
            verified.number,
            verified.hash
        )));
    }

    verify_block::<N, _>(provider, l1, store, snapshot, context, &block, &receipts).await
}

async fn initialize<P>(
    config: &CheckerConfig,
    provider: &P,
    metrics: &CheckerMetrics,
    limits: RuntimeLimits,
) -> eyre::Result<(Store, Snapshot, DynProvider<TempoNetwork>)>
where
    P: BlockNumReader + ChainSpecProvider + StateProviderFactory,
    P::ChainSpec: TempoHardforks,
{
    let persisted_identity = config
        .database_path
        .exists()
        .then(|| Store::inspect_identity(&config.database_path))
        .transpose()?;
    let l1 = connect(&config.l1_rpc_url, limits).await?;
    let bootstrap = retry_transient(
        || async {
            match persisted_identity {
                Some(identity) => bootstrap::authenticate(provider, &l1, config, identity).await,
                None => bootstrap::discover(provider, &l1, config).await,
            }
        },
        "checker bootstrap",
        limits.bootstrap_total_timeout,
        limits,
    )
    .await?;
    let checkpoint = bootstrap.checkpoint();
    let (store, mut snapshot) = if persisted_identity.is_some() {
        Store::open(&config.database_path, bootstrap.identity())?
    } else {
        Store::open_or_create(&config.database_path, &checkpoint)?
    };
    let verified = snapshot.metadata.verified_zone;
    if provider.block_hash(verified.number)? != Some(verified.hash) {
        tracing::warn!(
            target: "zone::checker",
            zone_block = verified.number,
            zone_hash = %verified.hash,
            "checker tip is not in local Zone history; rebuilding"
        );
        snapshot = store.reset(&checkpoint)?;
        metrics.recovery_rebuilds_total.increment(1);
    }
    metrics.update(&snapshot);
    Ok((store, snapshot, l1))
}

async fn connect(url: &str, limits: RuntimeLimits) -> eyre::Result<DynProvider<TempoNetwork>> {
    retry_transient(
        || async {
            tokio::time::timeout(
                limits.connect_attempt_timeout,
                connect_once(url, limits.rpc_request_timeout),
            )
            .await
            .unwrap_or_else(|_| {
                Err(AttemptError::retry(eyre::eyre!(
                    "Tempo RPC connection attempt timed out after {:?}",
                    limits.connect_attempt_timeout
                )))
            })
        },
        "Tempo RPC connection",
        limits.connect_total_timeout,
        limits,
    )
    .await
}

async fn connect_once(
    url: &str,
    request_timeout: Duration,
) -> Result<DynProvider<TempoNetwork>, AttemptError> {
    // Adapt Tower's timeout error and future types back to Alloy's transport contract.
    let client = RpcClient::builder()
        .layer(MapFutureLayer::new(|future| -> TransportFut<'static> {
            Box::pin(future)
        }))
        .layer(MapErrLayer::new(move |error| {
            map_timeout_error(error, request_timeout)
        }))
        .layer(TimeoutLayer::new(request_timeout))
        .connect_with_config(url, rpc_connection_config())
        .await
        .map_err(classify_rpc_error)?;
    Ok(ProviderBuilder::new_with_network::<TempoNetwork>()
        .connect_client(client)
        .erased())
}

fn map_timeout_error(error: BoxError, request_timeout: Duration) -> TransportError {
    match error.downcast::<TransportError>() {
        Ok(error) => *error,
        Err(error) => match error.downcast::<tower::timeout::error::Elapsed>() {
            Ok(_) => TransportErrorKind::non_retryable_str(&format!(
                "Tempo RPC request timed out after {request_timeout:?}"
            )),
            Err(error) => TransportErrorKind::custom_str(&error.to_string()),
        },
    }
}

fn rpc_connection_config() -> ConnectionConfig {
    ConnectionConfig::new()
        .with_max_retries(WS_RECONNECT_MAX_RETRIES)
        .with_retry_interval(WS_RECONNECT_RETRY_INTERVAL)
        .with_ws_config(
            WebSocketConfig::default()
                .max_frame_size(Some(MAX_WS_FRAME_AND_MESSAGE_SIZE))
                .max_message_size(Some(MAX_WS_FRAME_AND_MESSAGE_SIZE)),
        )
}

/// Assert the append-only Zone history still contains the latest tip delivered to the checker.
fn ensure_canonical_tip<P: BlockNumReader>(provider: &P, tip: BlockNumHash) -> eyre::Result<()> {
    let canonical = provider.block_hash(tip.number)?;
    eyre::ensure!(
        canonical == Some(tip.hash),
        "Zone append-only invariant violated: delivered block {} ({}) is no longer canonical (local hash: {:?})",
        tip.number,
        tip.hash,
        canonical
    );
    Ok(())
}

async fn drive_exex_work<Node, F, T, E>(
    ctx: &mut ExExContext<Node>,
    progress: &mut RuntimeProgress,
    work: F,
) -> Result<T, DriveError<E>>
where
    Node: FullNodeComponents,
    F: Future<Output = Result<T, E>>,
{
    let notifications = ctx
        .notifications
        .by_ref()
        .map(|result| result.and_then(|notification| classify_notification(&notification)));
    drive_while_draining(work, notifications, progress).await
}

/// Drive `work` while retaining only lightweight delivered tips. The checker later walks the
/// canonical provider range itself, so notification payloads never need to be buffered or replayed.
async fn drive_while_draining<F, S, T, E>(
    work: F,
    mut notifications: S,
    progress: &mut RuntimeProgress,
) -> Result<T, DriveError<E>>
where
    F: Future<Output = Result<T, E>>,
    S: Stream<Item = eyre::Result<DeliveredNotification>> + Unpin,
{
    tokio::pin!(work);
    loop {
        tokio::select! {
            result = &mut work => return result.map_err(DriveError::Work),
            notification = notifications.next() => {
                let delivery = notification
                    .ok_or_else(|| eyre::eyre!("checker notification stream closed while work was pending"))
                    .and_then(|result| result)
                    .map_err(DriveError::Notifications)?;
                progress.delivered_checked(delivery).map_err(DriveError::Notifications)?;
            }
        }
    }
}

fn drive_eyre_error(error: DriveError<eyre::Report>) -> eyre::Report {
    match error {
        DriveError::Work(error) | DriveError::Notifications(error) => error,
    }
}

/// Retry transient failures until the acquisition deadline.
async fn retry_transient<T, F, Fut>(
    mut attempt: F,
    operation: &str,
    total_timeout: Duration,
    limits: RuntimeLimits,
) -> eyre::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, AttemptError>>,
{
    let retry = async {
        let mut backoff = Backoff::new(limits);
        loop {
            match attempt().await {
                Ok(value) => return Ok(value),
                Err(AttemptError::Retry(error)) => {
                    tracing::warn!(
                        target: "zone::checker",
                        %error,
                        operation,
                        attempt = backoff.attempted(),
                        ?total_timeout,
                        "checker acquisition failed; retrying"
                    );
                    backoff.wait().await;
                }
                Err(AttemptError::Disable(error)) => return Err(error),
            }
        }
    };
    tokio::time::timeout(total_timeout, retry)
        .await
        .map_err(|_| eyre::eyre!("{operation} deadline exhausted after {total_timeout:?}"))?
}

/// Classify one delivered notification and extract its lightweight tip for flow control.
fn classify_notification<N: NodePrimitives>(
    notification: &ExExNotification<N>,
) -> eyre::Result<DeliveredNotification> {
    // A revert reports the tip it rewinds to; the other two report their own new canonical tip.
    let (new, kind) = match notification {
        ExExNotification::ChainCommitted { new } => (new, NotificationKind::Committed),
        ExExNotification::ChainReorged { new, .. } => (new, NotificationKind::Reorged),
        ExExNotification::ChainReverted { old } => {
            let (&number, block) = old
                .blocks()
                .iter()
                .next()
                .ok_or_else(|| eyre::eyre!("received an empty reverted ExEx notification"))?;
            return Ok(DeliveredNotification {
                tip: BlockNumHash::new(number.saturating_sub(1), block.header().parent_hash()),
                kind: NotificationKind::Reverted,
            });
        }
    };
    let (&number, block) = new
        .blocks()
        .iter()
        .next_back()
        .ok_or_else(|| eyre::eyre!("received an empty {kind:?} ExEx notification"))?;
    Ok(DeliveredNotification {
        tip: BlockNumHash::new(number, block.hash()),
        kind,
    })
}

fn ensure_append_only(delivery: DeliveredNotification) -> eyre::Result<()> {
    match delivery.kind {
        NotificationKind::Committed => Ok(()),
        NotificationKind::Reorged => Err(eyre::eyre!(
            "Zone append-only invariant violated: received a reorg notification at {} ({})",
            delivery.tip.number,
            delivery.tip.hash
        )),
        NotificationKind::Reverted => Err(eyre::eyre!(
            "Zone append-only invariant violated: received a revert notification to {} ({})",
            delivery.tip.number,
            delivery.tip.hash
        )),
    }
}

/// Failure processing one block.
enum BlockError {
    /// Authenticated divergence to persist as a finding.
    Finding { zone: BlockRef, error: eyre::Report },
    /// Checker-local failure that must stop verification.
    Disable(eyre::Report),
}

struct VerificationContext<'a> {
    config: &'a CheckerConfig,
    metrics: &'a CheckerMetrics,
    limits: RuntimeLimits,
}

/// Exponential backoff between acquisition attempts, uncapped in count: the surrounding
/// deadline decides when to give up.
struct Backoff {
    attempts: u32,
    delay: Duration,
    max_delay: Duration,
}

impl Backoff {
    const fn new(limits: RuntimeLimits) -> Self {
        Self {
            attempts: 0,
            delay: limits.retry_delay,
            max_delay: limits.max_retry_delay,
        }
    }

    /// Count one failed attempt and return the running total, for logs and metrics only.
    fn attempted(&mut self) -> u32 {
        self.attempts = self.attempts.saturating_add(1);
        self.attempts
    }

    /// Wait, then increase the next delay. The enclosing operation owns the deadline.
    async fn wait(&mut self) {
        tokio::time::sleep(self.delay).await;
        self.delay = self.delay.saturating_mul(2).min(self.max_delay);
    }
}

async fn with_block_timeout<T, F>(
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
    block: &RecoveredBlock<N::Block>,
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
    let tempo_block = collect_l1_block_with_retry(l1, tempo_parent, tempo, zone, context).await?;
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

    let mut balance_backoff = Backoff::new(*limits);
    let balances = loop {
        match portal_balances(
            l1,
            config.portal_address,
            tokens.iter().copied(),
            tempo.hash,
        )
        .await
        {
            Ok(balances) => break balances,
            Err(L1ReadError::Unavailable(error)) => {
                record_l1_retry(
                    metrics,
                    &mut balance_backoff,
                    &error,
                    "Portal balance acquisition",
                );
                balance_backoff.wait().await;
            }
            Err(error) => return Err(classify_block_l1_error(error, zone)),
        }
    };
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
        L1ReadError::Unavailable(error) => BlockError::Disable(error),
        L1ReadError::Finding(error) => BlockError::Finding { zone, error },
        L1ReadError::Disable(error) => BlockError::Disable(error),
    }
}

async fn collect_l1_block_with_retry(
    l1: &DynProvider<TempoNetwork>,
    parent: BlockNumHash,
    expected: BlockNumHash,
    zone: BlockRef,
    context: &VerificationContext<'_>,
) -> Result<crate::l1::L1BlockEvidence, BlockError> {
    let VerificationContext {
        config,
        metrics,
        limits,
    } = context;
    let mut backoff = Backoff::new(*limits);
    loop {
        match collect_l1_block_at(
            l1,
            &config.l1_block_tracker,
            config.portal_address,
            parent,
            expected,
        )
        .await
        {
            Ok(block) => return Ok(block),
            Err(L1ReadError::Unavailable(error)) => {
                record_l1_retry(metrics, &mut backoff, &error, "Tempo block acquisition");
                backoff.wait().await;
            }
            Err(error) => return Err(classify_block_l1_error(error, zone)),
        }
    }
}

/// Report one retryable Tempo failure. The caller's block deadline decides when to stop.
fn record_l1_retry(
    metrics: &CheckerMetrics,
    backoff: &mut Backoff,
    error: &eyre::Report,
    operation: &str,
) {
    metrics.acquisition_retries_total.increment(1);
    tracing::warn!(
        target: "zone::checker",
        %error,
        operation,
        attempt = backoff.attempted(),
        "checker L1 acquisition failed; retrying"
    );
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
    use futures::{SinkExt as _, channel::mpsc, stream};

    use super::*;

    fn test_limits() -> RuntimeLimits {
        RuntimeLimits {
            retry_delay: Duration::from_millis(1),
            max_retry_delay: Duration::from_millis(4),
            connect_attempt_timeout: Duration::from_millis(10),
            connect_total_timeout: Duration::from_millis(100),
            rpc_request_timeout: Duration::from_millis(10),
            bootstrap_total_timeout: Duration::from_millis(100),
            block_verification_timeout: Duration::from_millis(100),
        }
    }

    fn delivery(number: u64, kind: NotificationKind) -> DeliveredNotification {
        DeliveredNotification {
            tip: BlockNumHash::new(number, alloy_primitives::B256::repeat_byte(number as u8)),
            kind,
        }
    }

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
            "operation",
            Duration::from_secs(10),
            test_limits(),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(attempts.get(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn hung_acquisition_ends_at_the_total_deadline() {
        let attempts = std::cell::Cell::new(0);
        let limits = test_limits();
        let started = tokio::time::Instant::now();
        let result = retry_transient(
            || {
                attempts.set(attempts.get() + 1);
                async {
                    tokio::time::timeout(
                        limits.rpc_request_timeout,
                        future::pending::<Result<(), AttemptError>>(),
                    )
                    .await
                    .unwrap_or_else(|_| {
                        Err(AttemptError::retry(eyre::eyre!(
                            "hung acquisition timed out"
                        )))
                    })
                }
            },
            "hung acquisition",
            Duration::from_secs(1),
            limits,
        )
        .await;

        assert!(attempts.get() > 1);
        assert_eq!(started.elapsed(), Duration::from_secs(1));
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("deadline exhausted")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_backoff_cannot_exceed_total_deadline() {
        let mut limits = test_limits();
        limits.retry_delay = Duration::from_secs(1);
        limits.max_retry_delay = Duration::from_secs(2);
        let started = tokio::time::Instant::now();
        let result = retry_transient(
            || future::ready(Err::<(), _>(AttemptError::retry(eyre::eyre!("offline")))),
            "bounded acquisition",
            Duration::from_millis(1500),
            limits,
        )
        .await;

        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("deadline exhausted")
        );
        assert_eq!(started.elapsed(), Duration::from_millis(1500));
    }

    #[tokio::test]
    async fn busy_work_drains_notifications_and_tracks_latest_tip() {
        let (sender_guard, receiver) = mpsc::channel(1);
        let mut sender = sender_guard.clone();
        let (release, released) = futures::channel::oneshot::channel();
        let producer = async move {
            for number in 1..=4 {
                sender
                    .send(Ok(delivery(number, NotificationKind::Committed)))
                    .await
                    .unwrap();
            }
            release.send(()).unwrap();
        };
        let work = async move {
            released.await.unwrap();
            Ok::<_, eyre::Report>(())
        };
        let mut progress = RuntimeProgress::new(BlockNumHash::default());

        let ((), result) = tokio::join!(
            producer,
            drive_while_draining(work, receiver, &mut progress)
        );

        result.unwrap();
        drop(sender_guard);
        assert!(progress.last_delivered_tip.number >= 3);
    }

    #[tokio::test]
    async fn busy_work_rejects_reorg_as_an_append_only_violation() {
        let notifications = stream::iter([Ok(delivery(7, NotificationKind::Reorged))]);
        let mut progress = RuntimeProgress::new(BlockNumHash::default());
        let error = drive_while_draining(
            future::pending::<Result<(), eyre::Report>>(),
            notifications,
            &mut progress,
        )
        .await
        .expect_err("a Zone reorg must disable the checker");

        assert!(matches!(error, DriveError::Notifications(_)));
        assert_eq!(progress.last_delivered_tip.number, 0);
    }

    #[test]
    fn every_non_commit_notification_violates_append_only_history() {
        for kind in [NotificationKind::Reorged, NotificationKind::Reverted] {
            let error = ensure_append_only(delivery(7, kind))
                .expect_err("a non-commit notification must disable the checker");
            assert!(error.to_string().contains("append-only invariant violated"));
        }
    }

    #[tokio::test]
    async fn transient_acquisition_recovers_within_budget() {
        let attempts = std::cell::Cell::new(0);
        let limits = test_limits();
        let result = retry_transient(
            || {
                attempts.set(attempts.get() + 1);
                future::ready(if attempts.get() == 1 {
                    Err(AttemptError::retry(eyre::eyre!("disconnected")))
                } else {
                    Ok(42)
                })
            },
            "recovering acquisition",
            limits.connect_total_timeout,
            limits,
        )
        .await;

        assert_eq!(result.unwrap(), 42);
        assert_eq!(attempts.get(), 2);
    }

    #[test]
    fn replayed_commits_do_not_regress_or_conflict_with_the_delivered_tip() {
        let mut progress = RuntimeProgress::new(BlockNumHash::default());
        progress.delivered(delivery(10, NotificationKind::Committed));
        progress.delivered(delivery(5, NotificationKind::Committed));
        assert_eq!(progress.last_delivered_tip.number, 10);

        assert!(
            progress
                .ensure_no_conflicting_commit(DeliveredNotification {
                    tip: BlockNumHash::new(10, alloy_primitives::B256::repeat_byte(99)),
                    kind: NotificationKind::Committed,
                })
                .is_err()
        );
        assert_eq!(
            progress.last_delivered_tip,
            delivery(10, NotificationKind::Committed).tip
        );

        progress.delivered(delivery(7, NotificationKind::Reverted));
        assert_eq!(progress.last_delivered_tip.number, 7);
    }

    #[test]
    fn tempo_advance_requires_the_exact_successor() {
        assert!(validate_tempo_advance(10, 11).is_ok());
        for tip in [9, 10, 12, u64::MAX] {
            assert!(validate_tempo_advance(10, tip).is_err());
        }
        assert!(validate_tempo_advance(u64::MAX, u64::MAX).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn hung_acquisition_ends_at_the_block_deadline() {
        let limits = test_limits();
        let started = tokio::time::Instant::now();

        let result = with_block_timeout(
            future::pending::<Result<(), BlockError>>(),
            3,
            limits.block_verification_timeout,
        )
        .await;

        assert!(matches!(result, Err(BlockError::Disable(_))));
        assert_eq!(started.elapsed(), limits.block_verification_timeout);
    }
}
