//! Durable notification processing and observe-only failure isolation.

mod finished_height;
mod notifications;
#[cfg(test)]
mod tests;
mod verify;

use std::{future::Future, time::Duration};

use alloy_eips::BlockNumHash;
use alloy_provider::{DynProvider, Provider as _, ProviderBuilder};
use alloy_rpc_client::{ConnectionConfig, RpcClient, WebSocketConfig};
use alloy_transport::{TransportError, TransportErrorKind, TransportFut};
use futures::future;
use reth_chainspec::ChainSpecProvider;
use reth_exex::ExExContext;
use reth_node_api::{FullNodeComponents, NodePrimitives, PrimitivesTy};
use reth_storage_api::{
    BlockNumReader, BlockReader, PruneCheckpointReader, StageCheckpointReader, StateProviderFactory,
};
use tempo_alloy::TempoNetwork;
use tempo_chainspec::spec::TempoHardforks;
use tower::{
    BoxError,
    timeout::TimeoutLayer,
    util::{MapErrLayer, MapFutureLayer},
};

use self::{
    notifications::{
        DeliveredNotification, DriveError, Wake, drive_exex_work, drive_eyre_error,
        ensure_append_only, next_wake,
    },
    verify::{BlockError, VerificationContext, process_next_block, with_block_timeout},
};
use crate::{
    AttemptError, CheckerConfig, bootstrap,
    l1::classify_rpc_error,
    persistence::{Finding, Snapshot, Status, Store},
    telemetry::CheckerMetrics,
};

/// Internal provider surface shared by the runtime and verification worker.
pub(super) trait CheckerProvider<N: NodePrimitives>:
    BlockReader<Block = N::Block, Receipt = N::Receipt>
    + ChainSpecProvider<ChainSpec: TempoHardforks>
    + PruneCheckpointReader
    + StageCheckpointReader
    + StateProviderFactory
    + Clone
{
}

impl<N, P> CheckerProvider<N> for P
where
    N: NodePrimitives,
    P: BlockReader<Block = N::Block, Receipt = N::Receipt>
        + ChainSpecProvider<ChainSpec: TempoHardforks>
        + PruneCheckpointReader
        + StageCheckpointReader
        + StateProviderFactory
        + Clone,
{
}

/// Internal primitive constraints shared by the runtime and verification worker.
pub(super) trait CheckedPrimitives: NodePrimitives {}

impl<N> CheckedPrimitives for N
where
    N: NodePrimitives,
    N::SignedTx: alloy_consensus::transaction::TxHashRef,
    N::Receipt: alloy_consensus::TxReceipt<Log = alloy_primitives::Log>,
{
}

/// Tempo blocks with full Portal logs exceed tungstenite's default frame limit.
const MAX_WS_FRAME_AND_MESSAGE_SIZE: usize = 128 * 1024 * 1024;
/// Keep Alloy's established WebSocket recovery active beyond the checker work deadline.
const WS_RECONNECT_MAX_RETRIES: u32 = 15;
/// Base interval for Alloy's reconnect backoff.
const WS_RECONNECT_RETRY_INTERVAL: Duration = Duration::from_secs(3);
/// How often to re-check Reth's committed frontier while waiting on it.
const PERSISTENCE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Runtime limits are private policy rather than node CLI surface.
#[derive(Debug, Clone, Copy)]
struct RuntimeLimits {
    retry_delay: Duration,
    /// Maximum delay between Tempo acquisition attempts.
    max_retry_delay: Duration,
    connect_attempt_timeout: Duration,
    connect_total_timeout: Duration,
    /// Bound one established RPC acquisition; expiry disables this provider as untrustworthy.
    rpc_request_timeout: Duration,
    bootstrap_total_timeout: Duration,
    /// The only bound on one block's retry loops, so it is sized to ride out a transient outage.
    block_verification_timeout: Duration,
}

impl RuntimeLimits {
    const PRODUCTION: Self = Self {
        retry_delay: Duration::from_secs(1),
        max_retry_delay: Duration::from_secs(30),
        connect_attempt_timeout: Duration::from_secs(15),
        connect_total_timeout: Duration::from_secs(2 * 60),
        rpc_request_timeout: Duration::from_secs(60),
        bootstrap_total_timeout: Duration::from_secs(5 * 60),
        block_verification_timeout: Duration::from_secs(5 * 60),
    };
}

/// Notification delivery advances independently from durable checker verification.
#[derive(Debug, Clone, Copy)]
struct RuntimeProgress {
    last_delivered_tip: BlockNumHash,
    last_finished_tip: Option<BlockNumHash>,
}

impl RuntimeProgress {
    const fn new(node_head: BlockNumHash) -> Self {
        Self {
            last_delivered_tip: node_head,
            last_finished_tip: None,
        }
    }

    fn accept_delivery(
        &mut self,
        delivery: DeliveredNotification,
    ) -> Result<(), AppendOnlyViolation> {
        ensure_append_only(delivery)?;
        if delivery.tip.number == self.last_delivered_tip.number
            && delivery.tip.hash != self.last_delivered_tip.hash
        {
            return Err(AppendOnlyViolation::new(
                delivery.tip,
                format!(
                    "delivered block {} changed from {} to {}",
                    delivery.tip.number, self.last_delivered_tip.hash, delivery.tip.hash
                ),
            ));
        }
        if delivery.tip.number > self.last_delivered_tip.number {
            self.last_delivered_tip = delivery.tip;
        }
        Ok(())
    }
}

/// A Zone history change. Carries its block so the runtime can persist a durable finding.
#[derive(Debug, thiserror::Error)]
#[error("Zone append-only invariant violated: {summary}")]
struct AppendOnlyViolation {
    zone: BlockNumHash,
    summary: String,
}

impl AppendOnlyViolation {
    fn new(zone: BlockNumHash, summary: String) -> Self {
        Self { zone, summary }
    }
}

/// Exponential backoff between acquisition attempts, uncapped in count: the surrounding
/// deadline decides when to give up.
struct Backoff {
    attempts: u32,
    delay: Duration,
    max_delay: Duration,
}

impl Backoff {
    const fn new(delay: Duration, max_delay: Duration) -> Self {
        Self {
            attempts: 0,
            delay,
            max_delay,
        }
    }

    fn attempted(&mut self) -> u32 {
        self.attempts = self.attempts.saturating_add(1);
        self.attempts
    }

    /// Wait, then increase the next delay.
    async fn wait(&mut self) {
        tokio::time::sleep(self.delay).await;
        self.delay = self.delay.saturating_mul(2).min(self.max_delay);
    }
}

/// Bootstrap or open durable state, recover from its verified tip, and follow notifications.
pub(super) async fn run<Node>(
    config: CheckerConfig,
    ctx: &mut ExExContext<Node>,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    Node::Provider: CheckerProvider<PrimitivesTy<Node::Types>>,
    PrimitivesTy<Node::Types>: CheckedPrimitives,
{
    let limits = RuntimeLimits::PRODUCTION;
    let metrics = CheckerMetrics::default();
    let mut progress = RuntimeProgress::new(ctx.head);
    // The checker reconstructs canonical history itself and uses ExEx notifications only as
    // append-only tip signals. This avoids Reth backfill execution and private buffered state.
    ctx.set_notifications_without_head();
    metrics.disabled.set(0.0);
    let result = run_inner(&config, ctx, &metrics, &mut progress, limits).await;
    metrics.disabled.set(1.0);
    match &result {
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
    drain_disabled(config, ctx, &metrics, &mut progress, result.err()).await;
    future::pending().await
}

/// Keep acknowledging valid delivered commits after verification stops. Observe mode prohibits
/// pruning replay-required provider data, so WAL finalization no longer depends on provider reads.
async fn drain_disabled<Node>(
    config: CheckerConfig,
    ctx: &mut ExExContext<Node>,
    metrics: &CheckerMetrics,
    progress: &mut RuntimeProgress,
    disabled_by: Option<eyre::Report>,
) where
    Node: FullNodeComponents,
{
    let mut durable = match open_durable_snapshot(&config) {
        Ok(durable) => {
            metrics.update(&durable.1);
            Some(durable)
        }
        Err(error) => {
            tracing::warn!(
                target: "zone::checker",
                %error,
                "checker durable state is unavailable; requiring replay history from genesis"
            );
            None
        }
    };
    if let Some(violation) = disabled_by.as_ref().and_then(append_only_violation) {
        persist_append_only_violation(durable.as_mut(), metrics, violation);
    }

    let delivered = progress.last_delivered_tip;
    if let Err(error) = finished_height::emit_finished_height(ctx, progress, delivered) {
        report_disabled_error(durable.as_mut(), metrics, &error);
    }
    loop {
        let outcome = match next_wake(ctx, false).await {
            Wake::Delivered(delivery) => delivery.and_then(|delivery| {
                progress
                    .accept_delivery(delivery)
                    .map_err(eyre::Report::new)?;
                let delivered = progress.last_delivered_tip;
                finished_height::emit_finished_height(ctx, progress, delivered)
            }),
            Wake::Poll => unreachable!("disabled draining does not poll persistence"),
            Wake::Closed => break,
        };
        if let Err(error) = outcome {
            report_disabled_error(durable.as_mut(), metrics, &error);
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
    config: &CheckerConfig,
    ctx: &mut ExExContext<Node>,
    metrics: &CheckerMetrics,
    progress: &mut RuntimeProgress,
    limits: RuntimeLimits,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    Node::Provider: CheckerProvider<PrimitivesTy<Node::Types>>,
    PrimitivesTy<Node::Types>: CheckedPrimitives,
{
    tracing::info!(target: "zone::checker", "checker started");
    let provider = ctx.provider().clone();
    let initialization = initialize(config, &provider, metrics, limits);
    let (store, mut snapshot, l1) = drive_exex_work(ctx, progress, initialization)
        .await
        .map_err(drive_eyre_error)?;

    let verification_context = VerificationContext {
        config,
        metrics,
        limits,
    };

    loop {
        let verified = snapshot.metadata.verified_zone;
        let resolved = finished_height::resolve_verifying(&provider, progress, verified)?;
        snapshot = finished_height::observe_tip(
            &provider,
            &store,
            snapshot,
            progress.last_delivered_tip,
            resolved.persisted,
        )?;
        metrics.update(&snapshot);
        if let Some(acknowledgement) = resolved.acknowledgement {
            finished_height::emit_finished_height(ctx, progress, acknowledgement)?;
        }
        let persisted = resolved.persisted;
        let verification_target = progress.last_delivered_tip.number.min(persisted.number);

        if matches!(&snapshot.metadata.status, Status::Diverged { .. })
            || verified.number >= verification_target
        {
            let poll_persistence =
                finished_height::can_advance(persisted, progress.last_delivered_tip, verified);
            match next_wake(ctx, poll_persistence).await {
                Wake::Delivered(delivery) => {
                    progress
                        .accept_delivery(delivery?)
                        .map_err(eyre::Report::new)?;
                }
                Wake::Poll => {}
                Wake::Closed => break,
            }
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

/// Recover an append-only violation from anywhere in the report's cause chain.
fn append_only_violation(error: &eyre::Report) -> Option<&AppendOnlyViolation> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<AppendOnlyViolation>())
}

/// Persist append-only evidence and keep an isolated checker failure visible while draining.
fn report_disabled_error(
    durable: Option<&mut (Store, Snapshot)>,
    metrics: &CheckerMetrics,
    error: &eyre::Report,
) {
    if let Some(violation) = append_only_violation(error) {
        persist_append_only_violation(durable, metrics, violation);
    }
    tracing::error!(
        target: "zone::checker",
        %error,
        "checker received invalid history while disabled; continuing to drain"
    );
}

/// Open the checker database using the identity it already records.
fn open_durable_snapshot(config: &CheckerConfig) -> eyre::Result<(Store, Snapshot)> {
    let identity = Store::inspect_identity(&config.database_path)?;
    Ok(Store::open(&config.database_path, identity)?)
}

/// Persist append-only evidence when durable checker state is available.
fn persist_append_only_violation(
    durable: Option<&mut (Store, Snapshot)>,
    metrics: &CheckerMetrics,
    violation: &AppendOnlyViolation,
) {
    let Some((store, snapshot)) = durable else {
        return;
    };
    let result = if matches!(snapshot.metadata.status, Status::Verifying) {
        store
            .record_finding(
                snapshot,
                Finding {
                    zone: violation.zone.into(),
                    summary: violation.to_string(),
                },
            )
            .map(|next| *snapshot = next)
    } else {
        Ok(())
    };
    match result {
        Ok(()) => {
            metrics.update(snapshot);
        }
        Err(error) => {
            tracing::error!(
                target: "zone::checker",
                %error,
                "failed to persist Zone append-only violation"
            );
        }
    }
}

/// Connect to Tempo, authenticate the bootstrap identity, and open durable state.
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
        Backoff::new(limits.retry_delay, limits.max_retry_delay),
    )
    .await?;
    let checkpoint = bootstrap.checkpoint();
    let (store, snapshot) = if persisted_identity.is_some() {
        Store::open(&config.database_path, bootstrap.identity())?
    } else {
        Store::open_or_create(&config.database_path, &checkpoint)?
    };
    metrics.update(&snapshot);
    Ok((store, snapshot, l1))
}

/// Connect to Tempo, retrying until the connection deadline.
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
        Backoff::new(limits.retry_delay, limits.max_retry_delay),
    )
    .await
}

/// One connection attempt, with every request through it bounded.
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

/// Adapt Tower's timeout error back to Alloy's transport contract.
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

/// Frame limits and reconnect budget for the long-lived Tempo socket.
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

/// Retry transient failures until the acquisition deadline.
async fn retry_transient<T, F, Fut>(
    mut attempt: F,
    operation: &str,
    total_timeout: Duration,
    mut backoff: Backoff,
) -> eyre::Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, AttemptError>>,
{
    let retry = async {
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
