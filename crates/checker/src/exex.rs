//! Reth ExEx integration for acquiring and verifying canonical Zone blocks.

use std::{
    collections::BTreeSet,
    future::Future,
    io::ErrorKind,
    sync::Arc,
    time::{Duration, Instant},
};

use alloy_consensus::BlockHeader as _;
use alloy_primitives::{Address, B256, U256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use futures::TryStreamExt;
use reth_chainspec::EthChainSpec as _;
use reth_execution_types::Chain;
use reth_exex::{ExExContext, ExExNotification};
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_storage_api::{
    BlockHashReader, BlockNumReader, BlockReader, StateProviderFactory, TransactionVariant,
};
use tempo_alloy::TempoNetwork;
use tempo_primitives::{Block, TempoPrimitives, TempoReceipt};

use crate::{
    CheckerBlockedReason, CheckerConfig,
    adapter::{AuthenticatedObservation, adapt},
    failure::Failure,
    kernel::{State, TokenPhase, apply_imported},
    metrics::{CheckerMetrics, CheckerState},
    observe::{
        L2BlockObservation, ZonePostStateOutputs, acquire_portal_token_balance,
        acquire_zone_post_state, observe_l1_range, observe_l2_block_with_context,
    },
    persistence::{BlockNumHash, Identity, Persistence, Snapshot},
    runtime::{
        AuthenticatedBlock, AuthenticationFailure, AuthenticationRequest, Runtime, RuntimeAction,
    },
};

/// Run the checker ExEx without allowing checker-local failures to finish it.
pub(super) async fn run<Node>(config: CheckerConfig, mut ctx: ExExContext<Node>) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    Node::Provider:
        BlockReader<Block = Block, Receipt = TempoReceipt> + BlockNumReader + StateProviderFactory,
    Node::Types: NodeTypes<Primitives = TempoPrimitives>,
{
    let metrics = CheckerMetrics::default();
    let mut failures = 0;
    loop {
        match start_and_run(&config, &mut ctx, &metrics).await {
            Ok(()) => std::future::pending::<()>().await,
            Err(error) => {
                let delay = retry_delay(failures);
                failures = failures.saturating_add(1);
                metrics.set_state(CheckerState::Retrying);
                tracing::error!(target: "zone::checker", %error, ?delay, "checker recovery attempt failed");
                if let Err(error) =
                    await_while_draining_notifications(&mut ctx, tokio::time::sleep(delay)).await
                {
                    tracing::error!(target: "zone::checker", %error, "checker notification stream closed while waiting to retry");
                    std::future::pending::<()>().await;
                }
            }
        }
    }
}

/// Open checker state and drive it until a checker-local failure blocks progress.
async fn start_and_run<Node>(
    config: &CheckerConfig,
    ctx: &mut ExExContext<Node>,
    metrics: &CheckerMetrics,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    Node::Provider:
        BlockReader<Block = Block, Receipt = TempoReceipt> + BlockNumReader + StateProviderFactory,
    Node::Types: NodeTypes<Primitives = TempoPrimitives>,
{
    metrics.set_state(CheckerState::Opening);
    eyre::ensure!(
        !config.acquisition_timeout.is_zero(),
        "checker acquisition timeout must not be zero"
    );
    let path = config.database_path.as_path();
    bootstrap_if_missing_while_draining(config, ctx, metrics).await?;
    let (identity, store, snapshot) = match open_checkpoint(config, ctx.config.chain.chain().id()) {
        Ok(opened) => opened,
        Err(error) => {
            metrics.set_state(CheckerState::Unavailable);
            tracing::error!(target: "zone::checker", %error, path = %path.display(), "checker database cannot be used");
            return drain_notifications(ctx).await;
        }
    };
    let mut runtime = Runtime::new(snapshot, metrics.clone());

    // The local node retains the replay journal; ExEx notifications only wake recovery.
    let l2_provider = ctx.provider().clone();
    refresh_and_publish_canonical_head(ctx, &mut runtime, &store, &l2_provider)?;
    if runtime.snapshot().meta.blocked.is_some() {
        tracing::error!(target: "zone::checker", "checker remains blocked from a previous run");
        return drain_notifications(ctx).await;
    }

    metrics.set_state(CheckerState::Connecting);
    let (l1_provider, actual_l1_chain_id) =
        connect_l1_while_draining(config, ctx, &store, &mut runtime, metrics).await?;
    runtime.publish_snapshot();
    if actual_l1_chain_id != identity.l1_chain_id {
        runtime.block(&store, CheckerBlockedReason::TempoChainMismatch)?;
        tracing::error!(target: "zone::checker", expected = identity.l1_chain_id, actual = actual_l1_chain_id, "Tempo chain ID does not match the checker checkpoint");
        return drain_notifications(ctx).await;
    }
    run_loop(config, ctx, &store, identity, &mut runtime, &l1_provider).await?;
    Ok(())
}

/// Validate and open an existing checker checkpoint as its sole writer.
fn open_checkpoint(
    config: &CheckerConfig,
    zone_chain_id: u64,
) -> eyre::Result<(Identity, Persistence, Snapshot)> {
    let identity = Persistence::inspect_identity(&config.database_path)?;
    validate_checkpoint_identity(config, zone_chain_id, identity)?;
    let (store, snapshot) = Persistence::open(&config.database_path, identity)?;
    Ok((identity, store, snapshot))
}

/// Build a checkpoint only when no durable checker database exists yet.
async fn bootstrap_if_missing_while_draining<Node>(
    config: &CheckerConfig,
    ctx: &mut ExExContext<Node>,
    metrics: &CheckerMetrics,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    Node::Provider:
        BlockReader<Block = Block, Receipt = TempoReceipt> + BlockNumReader + StateProviderFactory,
    Node::Types: NodeTypes<Primitives = TempoPrimitives>,
{
    let provider = ctx.provider().clone();
    let zone_chain_id = ctx.config.chain.chain().id();
    let mut failures = 0;
    loop {
        if !checkpoint_is_missing(&config.database_path)? {
            return Ok(());
        }

        metrics.set_state(CheckerState::Bootstrapping);
        tracing::info!(target: "zone::checker", path = %config.database_path.display(), "building missing checker checkpoint");
        match await_while_draining_notifications(
            ctx,
            crate::build_checkpoint(config.clone(), zone_chain_id, &provider),
        )
        .await?
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                let delay = retry_delay(failures);
                failures = failures.saturating_add(1);
                metrics.set_state(CheckerState::Retrying);
                tracing::warn!(target: "zone::checker", %error, ?delay, "checker bootstrap failed; retrying");
                await_while_draining_notifications(ctx, tokio::time::sleep(delay)).await?;
            }
        }
    }
}

/// Await startup work without applying notification-channel backpressure to the node.
async fn await_while_draining_notifications<Node, F, T>(
    ctx: &mut ExExContext<Node>,
    future: F,
) -> eyre::Result<T>
where
    Node: FullNodeComponents,
    Node::Types: NodeTypes<Primitives = TempoPrimitives>,
    F: Future<Output = T>,
{
    tokio::pin!(future);
    loop {
        tokio::select! {
            output = &mut future => return Ok(output),
            notification = ctx.notifications.try_next() => match notification {
                Ok(Some(_)) => {}
                Ok(None) => eyre::bail!("checker notification stream closed"),
                Err(error) => {
                    tracing::error!(target: "zone::checker", %error, "checker notification stream failed; resuming direct delivery");
                    ctx.set_notifications_without_head();
                }
            }
        }
    }
}

/// Delay repeated startup failures without busy-looping on an unavailable dependency.
fn retry_delay(failures: u32) -> Duration {
    Duration::from_secs((1_u64 << failures.min(5)).min(30))
}

/// Return whether the checkpoint path is absent without treating invalid paths as missing.
fn checkpoint_is_missing(path: &std::path::Path) -> eyre::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}

/// Keep a blocked checker from applying notification-channel backpressure to the node.
async fn drain_notifications<Node>(ctx: &mut ExExContext<Node>) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    Node::Provider: BlockHashReader + BlockNumReader,
    Node::Types: NodeTypes<Primitives = TempoPrimitives>,
{
    let provider = ctx.provider().clone();
    publish_canonical_finished_height(ctx, &provider)?;
    loop {
        match ctx.notifications.try_next().await {
            Ok(Some(_)) => publish_canonical_finished_height(ctx, &provider)?,
            Ok(None) => eyre::bail!("checker notification stream closed"),
            Err(error) => {
                tracing::error!(target: "zone::checker", %error, "checker notification stream failed; resuming direct delivery");
                ctx.set_notifications_without_head();
                publish_canonical_finished_height(ctx, &provider)?;
            }
        }
    }
}

/// Publish the current canonical head after the checker has stopped semantic processing.
fn publish_canonical_finished_height<Node, P>(
    ctx: &ExExContext<Node>,
    provider: &P,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    Node::Types: NodeTypes<Primitives = TempoPrimitives>,
    P: BlockHashReader + BlockNumReader + ?Sized,
{
    ctx.send_finished_height(canonical_head(provider)?.into())?;
    Ok(())
}

/// Read the node's current canonical tip as one coherent ExEx coordinate.
fn canonical_head<P>(provider: &P) -> eyre::Result<BlockNumHash>
where
    P: BlockHashReader + BlockNumReader + ?Sized,
{
    let number = provider.best_block_number()?;
    let hash = provider
        .block_hash(number)?
        .ok_or_else(|| eyre::eyre!("canonical Zone block {number} is unavailable"))?;
    Ok(BlockNumHash { number, hash })
}

/// Await checker work while consuming notification wakeups from the node.
async fn await_with_notifications<Node, F, T>(
    ctx: &mut ExExContext<Node>,
    store: &Persistence,
    runtime: &mut Runtime,
    future: F,
) -> eyre::Result<Option<T>>
where
    Node: FullNodeComponents,
    Node::Provider:
        BlockReader<Block = Block, Receipt = TempoReceipt> + BlockNumReader + StateProviderFactory,
    Node::Types: NodeTypes<Primitives = TempoPrimitives>,
    F: Future<Output = T>,
{
    if runtime.snapshot().meta.blocked.is_some() {
        return Ok(None);
    }
    let provider = ctx.provider().clone();
    tokio::pin!(future);
    loop {
        tokio::select! {
            output = &mut future => return Ok(Some(output)),
            next = ctx.notifications.try_next() => {
                handle_notification(ctx, runtime, store, &provider, next)?;
                if runtime.snapshot().meta.blocked.is_some() {
                    return Ok(None);
                }
            }
        }
    }
}

/// Restore the newest retained canonical checkpoint and return its verified height.
fn reconcile_canonical_head<P>(
    runtime: &mut Runtime,
    store: &Persistence,
    provider: &P,
) -> eyre::Result<Option<BlockNumHash>>
where
    P: BlockHashReader + BlockNumReader + ?Sized,
{
    let verified = runtime.snapshot().meta.verified_zone_tip;
    if provider.block_hash(verified.number)? == Some(verified.hash) {
        if let Some(finding) = runtime.snapshot().meta.active_finding
            && provider.block_hash(finding.zone.number)? != Some(finding.zone.hash)
        {
            runtime.reorg(store, verified)?;
        }
        return Ok(Some(runtime.snapshot().meta.verified_zone_tip));
    }
    let head = provider.best_block_number()?;
    for retained in store.retained_zone_coordinates()?.into_iter().rev() {
        if retained.number > head {
            continue;
        }
        if provider.block_hash(retained.number)? == Some(retained.hash) {
            runtime.reorg(store, retained)?;
            return Ok(Some(runtime.snapshot().meta.verified_zone_tip));
        }
    }
    tracing::error!(target: "zone::checker", verified_block = verified.number, local_head = head, "Zone reorg exceeds retained checker history");
    runtime.block(store, CheckerBlockedReason::DeepReorgBeyondRetention)?;
    Ok(None)
}

/// Reconcile verified history and record the current local canonical head.
///
/// Returns `None` when no retained checkpoint remains canonical.
fn refresh_canonical_head<P>(
    runtime: &mut Runtime,
    store: &Persistence,
    provider: &P,
) -> eyre::Result<Option<BlockNumHash>>
where
    P: BlockHashReader + BlockNumReader + ?Sized,
{
    let verified = reconcile_canonical_head(runtime, store, provider)?;
    if runtime.snapshot().meta.blocked.is_some() {
        return Ok(verified);
    }
    let number = provider.best_block_number()?;
    let hash = provider
        .block_hash(number)?
        .ok_or_else(|| eyre::eyre!("canonical Zone block {number} is unavailable"))?;
    runtime.observe_tip(store, BlockNumHash { number, hash })?;
    Ok(verified)
}

/// Reconcile local history and publish the canonical verified height to Reth.
fn refresh_and_publish_canonical_head<Node, P>(
    ctx: &ExExContext<Node>,
    runtime: &mut Runtime,
    store: &Persistence,
    provider: &P,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    Node::Types: NodeTypes<Primitives = TempoPrimitives>,
    P: BlockHashReader + BlockNumReader + ?Sized,
{
    if let Some(verified) = refresh_canonical_head(runtime, store, provider)? {
        ctx.send_finished_height(verified.into())?;
    }
    Ok(())
}

/// Connect to Tempo once and authenticate its chain identity.
async fn connect_l1(config: &CheckerConfig) -> eyre::Result<(DynProvider<TempoNetwork>, u64)> {
    tokio::time::timeout(config.acquisition_timeout, async {
        let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&config.l1_rpc_url)
            .await?
            .erased();
        let chain_id = provider.get_chain_id().await?;
        Ok((provider, chain_id))
    })
    .await
    .map_err(|_| eyre::eyre!("Tempo connection attempt timed out"))?
}

/// Connect to Tempo while continuing to consume and coalesce Zone notifications.
async fn connect_l1_while_draining<Node>(
    config: &CheckerConfig,
    ctx: &mut ExExContext<Node>,
    store: &Persistence,
    runtime: &mut Runtime,
    metrics: &CheckerMetrics,
) -> eyre::Result<(DynProvider<TempoNetwork>, u64)>
where
    Node: FullNodeComponents,
    Node::Provider:
        BlockReader<Block = Block, Receipt = TempoReceipt> + BlockNumReader + StateProviderFactory,
    Node::Types: NodeTypes<Primitives = TempoPrimitives>,
{
    loop {
        metrics.set_state(CheckerState::Connecting);
        let Some(result) =
            await_with_notifications(ctx, store, runtime, connect_l1(config)).await?
        else {
            eyre::bail!("checker blocked while connecting to Tempo");
        };
        let error = match result {
            Ok(connection) => return Ok(connection),
            Err(error) => error,
        };
        metrics.set_state(CheckerState::Retrying);
        tracing::warn!(target: "zone::checker", %error, "checker could not connect to Tempo; retrying");
        if await_with_notifications(
            ctx,
            store,
            runtime,
            tokio::time::sleep(Duration::from_secs(1)),
        )
        .await?
        .is_none()
        {
            eyre::bail!("checker blocked while connecting to Tempo");
        }
    }
}

/// Drive checker work after startup without allowing a checker failure to finish the ExEx.
async fn run_loop<Node, P>(
    config: &CheckerConfig,
    ctx: &mut ExExContext<Node>,
    store: &Persistence,
    identity: Identity,
    runtime: &mut Runtime,
    l1_provider: &P,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    Node::Provider:
        BlockReader<Block = Block, Receipt = TempoReceipt> + BlockNumReader + StateProviderFactory,
    Node::Types: NodeTypes<Primitives = TempoPrimitives>,
    P: Provider<TempoNetwork>,
{
    loop {
        if runtime.snapshot().meta.blocked.is_some() {
            return drain_notifications(ctx).await;
        }
        let action = runtime.next_action(Instant::now());
        match action {
            RuntimeAction::Authenticate(request) => {
                let Some(result) = authenticate_while_draining(
                    config,
                    ctx,
                    store,
                    runtime,
                    request,
                    identity.creation_block,
                    l1_provider,
                )
                .await?
                else {
                    continue;
                };
                if let Some(verified) = runtime.complete_authentication(
                    store,
                    identity,
                    request,
                    result,
                    Instant::now(),
                )? {
                    ctx.send_finished_height(verified.into())?;
                }
                continue;
            }
            RuntimeAction::RetryAt(deadline) => {
                await_with_notifications(
                    ctx,
                    store,
                    runtime,
                    tokio::time::sleep_until(deadline.into()),
                )
                .await?;
                continue;
            }
            RuntimeAction::AwaitNotification => {}
        }

        let next = ctx.notifications.try_next().await;
        let l2_state_provider = ctx.provider().clone();
        handle_notification(ctx, runtime, store, &l2_state_provider, next)?;
    }
}

/// Authenticate one canonical block while continuing to consume notification wakeups.
async fn authenticate_while_draining<Node, P>(
    config: &CheckerConfig,
    ctx: &mut ExExContext<Node>,
    store: &Persistence,
    runtime: &mut Runtime,
    request: AuthenticationRequest,
    creation_block: B256,
    l1_provider: &P,
) -> eyre::Result<Option<Result<AuthenticatedBlock, AuthenticationFailure>>>
where
    Node: FullNodeComponents,
    Node::Provider:
        BlockReader<Block = Block, Receipt = TempoReceipt> + BlockNumReader + StateProviderFactory,
    Node::Types: NodeTypes<Primitives = TempoPrimitives>,
    P: Provider<TempoNetwork>,
{
    let started_at = Instant::now();
    let provider = ctx.provider().clone();
    let parent = runtime.snapshot().meta.verified_zone_tip;
    let authentication = tokio::time::timeout(
        config.acquisition_timeout,
        authenticate_canonical_zone_block(
            &provider,
            request.height(),
            l1_provider,
            Arc::clone(&runtime.snapshot().state),
            creation_block,
            config.zone_id,
        ),
    );
    let Some(result) = await_with_notifications(ctx, store, runtime, authentication).await? else {
        return Ok(None);
    };
    runtime.record_authentication(started_at.elapsed());
    let result = result.unwrap_or_else(|_| {
        Err(AuthenticationFailure::unlocated(Failure::retry(
            "checker acquisition timed out",
        )))
    });
    refresh_and_publish_canonical_head(ctx, runtime, store, &provider)?;
    if runtime.snapshot().meta.blocked.is_some()
        || runtime.snapshot().meta.verified_zone_tip != parent
    {
        return Ok(None);
    }
    let coordinate = match &result {
        Ok(block) => Some(block.zone),
        Err(failure) => failure.coordinate(),
    };
    if let Some(coordinate) = coordinate
        && provider.block_hash(coordinate.number)? != Some(coordinate.hash)
    {
        refresh_and_publish_canonical_head(ctx, runtime, store, &provider)?;
        return Ok(None);
    }
    Ok(Some(result))
}

/// Ensure the persisted checker identity matches the active node configuration.
fn validate_checkpoint_identity(
    config: &CheckerConfig,
    zone_chain_id: u64,
    identity: Identity,
) -> eyre::Result<()> {
    if identity.zone_chain_id != zone_chain_id
        || identity.zone_id != config.zone_id
        || identity.portal != config.portal_address
    {
        eyre::bail!("checker checkpoint identity does not match the node configuration");
    }
    Ok(())
}

/// Handle one notification-stream result and apply its ExEx lifecycle outcome.
fn handle_notification<Node, P, E>(
    ctx: &mut ExExContext<Node>,
    runtime: &mut Runtime,
    store: &Persistence,
    l2_state_provider: &P,
    next: Result<Option<ExExNotification<TempoPrimitives>>, E>,
) -> eyre::Result<()>
where
    Node: FullNodeComponents,
    Node::Types: NodeTypes<Primitives = TempoPrimitives>,
    P: BlockNumReader + ?Sized,
    E: core::fmt::Display,
{
    let notification = match next {
        Ok(Some(notification)) => notification,
        Ok(None) => eyre::bail!("checker notification stream closed"),
        Err(error) => {
            tracing::error!(target: "zone::checker", %error, "checker notification stream failed; resuming direct notification delivery");
            ctx.set_notifications_without_head();
            refresh_and_publish_canonical_head(ctx, runtime, store, l2_state_provider)?;
            return Ok(());
        }
    };
    if let Err(error) = validate_notification(&notification) {
        tracing::error!(target: "zone::checker", message = %error.message, "checker received an invalid notification");
        runtime.block(store, CheckerBlockedReason::InvalidNotificationSequence)?;
        return Ok(());
    }
    refresh_and_publish_canonical_head(ctx, runtime, store, l2_state_provider)?;
    Ok(())
}

/// Validate the internal continuity of one ExEx notification.
fn validate_notification(notification: &ExExNotification<TempoPrimitives>) -> Result<(), Failure> {
    match notification {
        ExExNotification::ChainCommitted { new } => {
            validate_fragment(new, "committed")?;
        }
        ExExNotification::ChainReverted { old } => {
            validate_fragment(old, "reverted")?;
        }
        ExExNotification::ChainReorged { old, new } => {
            let reverted_parent = validate_fragment(old, "reverted")?;
            let applied_parent = validate_fragment(new, "replacement")?;
            if reverted_parent != applied_parent {
                return Err(Failure::terminal(
                    "reorg fragments have different common ancestors",
                ));
            }
        }
    }
    Ok(())
}

/// Validate a contiguous chain fragment and return its parent.
fn validate_fragment(chain: &Chain<TempoPrimitives>, kind: &str) -> Result<BlockNumHash, Failure> {
    let first = chain
        .blocks()
        .values()
        .next()
        .ok_or_else(|| Failure::terminal(format!("empty {kind} fragment")))?;
    let parent = BlockNumHash {
        number: first
            .number()
            .checked_sub(1)
            .ok_or_else(|| Failure::terminal("fragment starts at genesis"))?,
        hash: first.parent_hash(),
    };
    let mut previous: Option<BlockNumHash> = None;
    for block in chain.blocks().values() {
        if let Some(previous) = previous
            && (previous.number.checked_add(1) != Some(block.number())
                || block.parent_hash() != previous.hash)
        {
            return Err(Failure::terminal(format!(
                "{kind} fragment is not contiguous"
            )));
        }
        previous = Some(BlockNumHash {
            number: block.number(),
            hash: block.hash(),
        });
    }
    Ok(parent)
}

/// Acquire a canonical Zone block directly from the local node's retained history.
async fn authenticate_canonical_zone_block<P, S>(
    l2_provider: &S,
    height: u64,
    l1_provider: &P,
    parent_state: Arc<State>,
    portal_creation_block_hash: B256,
    zone_id: u32,
) -> Result<AuthenticatedBlock, AuthenticationFailure>
where
    P: Provider<TempoNetwork>,
    S: BlockReader<Block = Block, Receipt = TempoReceipt> + StateProviderFactory + ?Sized,
{
    let block = l2_provider
        .recovered_block(height.into(), TransactionVariant::WithHash)
        .map_err(|error| AuthenticationFailure::unlocated(Failure::retry(error.to_string())))?
        .ok_or_else(|| {
            AuthenticationFailure::unlocated(Failure::retry(format!(
                "canonical Zone block {height} is unavailable"
            )))
        })?;
    let zone = BlockNumHash {
        number: block.number(),
        hash: block.hash(),
    };
    let parent = BlockNumHash {
        number: block.number().checked_sub(1).ok_or_else(|| {
            AuthenticationFailure::at(
                zone,
                zone,
                Failure::terminal("cannot recover Zone genesis as a child block"),
            )
        })?,
        hash: block.parent_hash(),
    };
    let canonical = l2_provider.block_hash(height).map_err(|error| {
        AuthenticationFailure::at(zone, parent, Failure::retry(error.to_string()))
    })?;
    if canonical != Some(zone.hash) {
        return Err(AuthenticationFailure::at(
            zone,
            parent,
            Failure::retry("Zone block changed during checker acquisition"),
        ));
    }
    let receipts = l2_provider
        .receipts_by_block(zone.hash.into())
        .map_err(|error| {
            AuthenticationFailure::at(zone, parent, Failure::retry(error.to_string()))
        })?
        .ok_or_else(|| {
            AuthenticationFailure::at(
                zone,
                parent,
                Failure::retry("local Zone receipt set is unavailable"),
            )
        })?;
    let observation = observe_l2_block_with_context(&block, &receipts).map_err(|failure| {
        AuthenticationFailure::at(zone, parent, Failure::from(failure.into_parts().0))
    })?;
    authenticate_zone_observation(
        l1_provider,
        l2_provider,
        parent_state,
        observation,
        portal_creation_block_hash,
        zone_id,
    )
    .await
    .map_err(|failure| AuthenticationFailure::at(zone, parent, failure))
}

/// Authenticate a Zone observation against its imported Tempo block and post-state.
async fn authenticate_zone_observation<P, S>(
    l1_provider: &P,
    l2_state_provider: &S,
    parent_state: Arc<State>,
    l2_observation: L2BlockObservation,
    portal_creation_block_hash: B256,
    zone_id: u32,
) -> Result<AuthenticatedBlock, Failure>
where
    P: Provider<TempoNetwork>,
    S: StateProviderFactory + ?Sized,
{
    let imported_l1_header = l2_observation.inputs().advance_tempo().imported_header();
    let portal_address = parent_state
        .portal()
        .map(|portal| portal.identity().portal)
        .ok_or_else(|| Failure::terminal("checker state has no portal identity"))?;
    let l1_observations = observe_l1_range(
        l1_provider,
        core::slice::from_ref(imported_l1_header),
        portal_address,
    )
    .await
    .map_err(Failure::from)?;

    let l2_post_state = acquire_l2_post_state(l2_state_provider, &parent_state, &l2_observation)?;

    let authenticated_block = adapt(&AuthenticatedObservation {
        l2: l2_observation,
        l1: l1_observations,
        state: l2_post_state,
        portal_creation_block_hash,
        zone_id,
    })?;
    verify_l1_collateral(
        l1_provider,
        &parent_state,
        &authenticated_block,
        portal_address,
    )
    .await?;
    Ok(authenticated_block)
}

/// Read the post-block Zone state needed to verify the observation.
fn acquire_l2_post_state<S>(
    l2_state_provider: &S,
    parent_state: &State,
    l2_observation: &L2BlockObservation,
) -> Result<ZonePostStateOutputs, Failure>
where
    S: StateProviderFactory + ?Sized,
{
    // Include tokens enabled by this import in the post-block supply reads.
    let mut tokens_to_query = parent_state
        .tokens()
        .filter_map(|(token, state)| (state.phase == TokenPhase::ZoneEnabled).then_some(token))
        .collect::<BTreeSet<_>>();
    tokens_to_query.extend(
        l2_observation
            .inputs()
            .advance_tempo()
            .enabled_tokens()
            .iter()
            .map(|token| token.token),
    );
    let supply_tokens = tokens_to_query.into_iter().collect::<Vec<_>>();
    acquire_zone_post_state(
        l2_state_provider,
        l2_observation.block_hash(),
        &supply_tokens,
    )
    .map_err(Failure::from)
}
/// Verify portal collateral at the authenticated imported Tempo block.
async fn verify_l1_collateral<P>(
    l1_provider: &P,
    parent_state: &State,
    authenticated_block: &AuthenticatedBlock,
    portal_address: Address,
) -> Result<(), Failure>
where
    P: Provider<TempoNetwork>,
{
    let post_l1_import_state = apply_imported(parent_state, &authenticated_block.imported)
        .map_err(|error| {
            Failure::authenticated_divergence(
                error.to_string(),
                crate::kernel::Finding::coded(
                    crate::kernel::FindingCategory::Invariant,
                    2,
                    crate::kernel::FindingLocation::Block,
                ),
            )
        })?;
    // Collateral belongs to the exact post-import/pre-Zone cut. Zone
    // processing may burn or mint and therefore cannot select this set.
    let expected_l1_accounting = post_l1_import_state
        .expected_accounting()
        .map_err(|error| {
            Failure::authenticated_divergence(
                error.to_string(),
                crate::kernel::Finding::coded(
                    crate::kernel::FindingCategory::CollateralMismatch,
                    3,
                    crate::kernel::FindingLocation::Block,
                ),
            )
        })?;
    for (token, accounting) in expected_l1_accounting {
        let collateral_balance = acquire_portal_token_balance(
            l1_provider,
            token,
            portal_address,
            authenticated_block.tempo.hash,
        )
        .await
        .map_err(Failure::from)?;
        let required = accounting.collateral().unwrap_or(U256::ZERO);
        if collateral_balance < required {
            return Err(Failure::authenticated_divergence(
                "imported collateral is insufficient",
                crate::kernel::Finding {
                    category: crate::kernel::FindingCategory::CollateralMismatch,
                    code: 4,
                    location: Some(crate::kernel::FindingLocation::State(
                        crate::kernel::StateKey::Token(token),
                    )),
                    expected: Some(crate::kernel::Datum::U256(required)),
                    actual: Some(crate::kernel::Datum::U256(collateral_balance)),
                },
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_primitives::{Address, B256};
    use reth_provider::test_utils::MockEthProvider;
    use tempo_primitives::{TempoHeader, TempoPrimitives};

    use super::{canonical_head, checkpoint_is_missing, refresh_canonical_head, retry_delay};
    use crate::{
        kernel::{PortalIdentity, State, StateDelta},
        metrics::CheckerMetrics,
        persistence::{BlockNumHash, ChainCut, Coverage, Identity, JournalEntry, Persistence},
        runtime::Runtime,
    };

    fn block(number: u64, byte: u8) -> BlockNumHash {
        BlockNumHash {
            number,
            hash: B256::repeat_byte(byte),
        }
    }

    fn add_header(provider: &MockEthProvider<TempoPrimitives>, coordinate: BlockNumHash) {
        let mut header = TempoHeader::default();
        header.inner.number = coordinate.number;
        provider.add_header(coordinate.hash, header);
    }

    #[test]
    fn only_an_absent_checkpoint_path_is_missing() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("checker");
        assert!(checkpoint_is_missing(&missing).unwrap());

        std::fs::create_dir(&missing).unwrap();
        assert!(!checkpoint_is_missing(&missing).unwrap());
    }

    #[test]
    fn startup_retry_delay_is_bounded() {
        assert_eq!(retry_delay(0), Duration::from_secs(1));
        assert_eq!(retry_delay(4), Duration::from_secs(16));
        assert_eq!(retry_delay(5), Duration::from_secs(30));
        assert_eq!(retry_delay(u32::MAX), Duration::from_secs(30));
    }

    #[test]
    fn canonical_head_uses_the_provider_tip() {
        let provider = MockEthProvider::<TempoPrimitives>::new();
        add_header(&provider, block(1, 0x11));
        add_header(&provider, block(2, 0x12));

        assert_eq!(canonical_head(&provider).unwrap(), block(2, 0x12));
    }

    #[test]
    fn reorg_refresh_returns_the_lower_verified_height() {
        let directory = tempfile::tempdir().unwrap();
        let identity = Identity {
            l1_chain_id: 1,
            zone_chain_id: 2,
            zone_id: 7,
            portal: Address::repeat_byte(0x70),
            creation_block: B256::repeat_byte(0xc0),
            creation_height: 0,
        };
        let genesis = ChainCut {
            zone: block(0, 0x10),
            tempo: block(0, 0x20),
        };
        let state = State::awaiting(PortalIdentity {
            portal: identity.portal,
            zone_id: identity.zone_id,
            initial_token: Address::repeat_byte(0x11),
        });
        let (store, mut snapshot) =
            Persistence::create(directory.path(), identity, genesis, state).unwrap();

        for number in 1..=2 {
            let zone = block(number, 0x10 + number as u8);
            let tempo = block(number, 0x20 + number as u8);
            snapshot = store
                .apply(
                    &snapshot,
                    JournalEntry {
                        zone,
                        parent: snapshot.meta.verified_zone_tip,
                        imported_tempo: tempo,
                        imported_tempo_parent: snapshot.meta.imported_tempo_tip,
                        delta: StateDelta::default(),
                    },
                    zone,
                    Coverage::Complete,
                )
                .unwrap();
        }

        let replacement = block(2, 0x92);
        let provider = MockEthProvider::<TempoPrimitives>::new();
        add_header(&provider, genesis.zone);
        add_header(&provider, block(1, 0x11));
        add_header(&provider, replacement);
        let mut runtime = Runtime::new(snapshot, CheckerMetrics::default());

        let finished = refresh_canonical_head(&mut runtime, &store, &provider).unwrap();

        assert_eq!(finished, Some(block(1, 0x11)));
        assert_eq!(runtime.snapshot().meta.verified_zone_tip, block(1, 0x11));
        assert_eq!(runtime.snapshot().meta.observed_zone_tip, replacement);
    }
}
