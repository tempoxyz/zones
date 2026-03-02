//! Background resolution task for pre-fetching TIP-403 authorization data from L1.
//!
//! The [`PolicyResolutionTask`] receives [`PolicyTaskMessage`]s via a channel and resolves
//! them concurrently using [`FuturesUnordered`]. Each request triggers a
//! [`PolicyProvider::is_authorized_async`] call which will populate the
//! [`SharedPolicyCache`] on cache miss, ensuring the payload builder hits the cache
//! at block-building time.
//!
//! Typical producers are the transaction pool (mempool) validation layer, which can
//! submit pre-fetch requests for sender/recipient addresses as transactions arrive.

use alloy_primitives::Address;
use alloy_provider::{DynProvider, Provider as _};
use futures::{FutureExt, StreamExt, stream::FuturesUnordered};
use tempo_alloy::TempoNetwork;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::{AuthRole, SharedPolicyCache, metrics::Tip403Metrics};
use crate::l1_state::PolicyProvider;

/// Background task that processes [`PolicyTaskMessage`]s concurrently.
///
/// Drains messages from the channel and resolves them via [`PolicyProvider::is_authorized_async`],
/// populating the shared cache. Results are logged but not returned — the sole purpose
/// is cache warming so the synchronous payload builder path avoids RPC round-trips.
pub struct PolicyResolutionTask {
    /// Cache-first, RPC-fallback provider used to resolve authorization queries.
    provider: PolicyProvider,
    /// Sending half of the task channel, used to create new [`PolicyTaskHandle`]s.
    tx: mpsc::Sender<PolicyTaskMessage>,
    /// Receiver for pre-fetch requests from the mempool / tx validation layer.
    rx: mpsc::Receiver<PolicyTaskMessage>,
    /// Maximum number of concurrent in-flight RPC resolutions.
    max_concurrent: usize,
    /// Concurrent resolution futures currently being polled.
    in_flight: FuturesUnordered<futures::future::BoxFuture<'static, ()>>,
    /// Metrics for tracking pre-fetch activity.
    metrics: Tip403Metrics,
}

impl PolicyResolutionTask {
    /// Create a new resolution task with an internal channel of the given capacity.
    pub fn new(
        cache: SharedPolicyCache,
        l1_provider: DynProvider<TempoNetwork>,
        max_concurrent: usize,
        channel_capacity: usize,
    ) -> Self {
        let (tx, rx) = mpsc::channel(channel_capacity);
        let provider = PolicyProvider::new(cache, l1_provider, tokio::runtime::Handle::current());
        Self {
            provider,
            tx,
            rx,
            max_concurrent,
            in_flight: FuturesUnordered::new(),
            metrics: Tip403Metrics::default(),
        }
    }

    /// Returns a new [`PolicyTaskHandle`] for sending pre-fetch requests to this task.
    pub fn handle(&self) -> PolicyTaskHandle {
        PolicyTaskHandle::new(self.tx.clone())
    }

    /// Run the resolution loop until the channel is closed.
    ///
    /// Processes requests concurrently up to `max_concurrent` in-flight futures.
    /// Errors are logged and swallowed — a failed resolution simply means the cache
    /// remains unpopulated and the builder will fall back to its own RPC path.
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                // Accept new messages when below the concurrency limit.
                Some(msg) = self.rx.recv(), if self.in_flight.len() < self.max_concurrent => {
                    self.on_message(msg);
                    self.metrics.prefetch_in_flight.set(self.in_flight.len() as f64);
                }
                // Drain completed futures.
                Some(()) = self.in_flight.next() => {
                    self.metrics.prefetch_in_flight.set(self.in_flight.len() as f64);
                }
                // Channel closed and all in-flight work drained.
                else => break,
            }
        }

        debug!("Policy resolution task shutting down");
    }

    /// Dispatch a message to the appropriate handler.
    fn on_message(&mut self, msg: PolicyTaskMessage) {
        match msg {
            PolicyTaskMessage::ResolveAuthorization { token, user, role } => {
                self.metrics.prefetch_requests_total.increment(1);
                let provider = self.provider.clone();
                let block_number = provider.cache().read().last_l1_block();
                let metrics = self.metrics.clone();
                self.in_flight.push(
                    async move {
                        match provider
                            .is_authorized_async(token, user, block_number, role)
                            .await
                        {
                            Ok(authorized) => {
                                metrics.prefetch_successes.increment(1);
                                debug!(
                                    %token, %user, block_number,
                                    ?role, authorized,
                                    "Pre-fetched policy authorization"
                                );
                            }
                            Err(e) => {
                                metrics.prefetch_failures.increment(1);
                                warn!(
                                    %token, %user, block_number,
                                    ?role, error = %e,
                                    "Policy pre-fetch failed"
                                );
                            }
                        }
                    }
                    .boxed(),
                );
            }
        }
    }
}

/// Message sent to the [`PolicyResolutionTask`].
#[derive(Debug, Clone)]
pub enum PolicyTaskMessage {
    /// Pre-fetch and cache authorization data for a (token, user) pair.
    ///
    /// The resolution task queries L1 at the latest block number — callers
    /// don't need to specify one.
    ResolveAuthorization {
        /// The TIP-20 token whose transfer policy to check.
        token: Address,
        /// The address to check authorization for.
        user: Address,
        /// Which role to resolve (sender, recipient, mint recipient, or full transfer).
        role: AuthRole,
    },
}

/// Handle for sending pre-fetch requests to the [`PolicyResolutionTask`].
///
/// Wraps an `mpsc::Sender<PolicyTaskMessage>` so callers don't need to
/// construct messages manually or carry around the raw channel sender.
#[derive(Debug, Clone)]
pub struct PolicyTaskHandle {
    tx: mpsc::Sender<PolicyTaskMessage>,
}

impl PolicyTaskHandle {
    /// Create a new handle from the sending half of the task channel.
    pub fn new(tx: mpsc::Sender<PolicyTaskMessage>) -> Self {
        Self { tx }
    }

    /// Request pre-fetch of authorization data for a (token, user) pair.
    ///
    /// The resolution task will query L1 at the latest block number.
    /// Returns `Ok(())` if the message was queued, or `Err` if the
    /// resolution task has shut down.
    pub async fn resolve_authorization(
        &self,
        token: Address,
        user: Address,
        role: AuthRole,
    ) -> Result<(), mpsc::error::SendError<PolicyTaskMessage>> {
        self.tx
            .send(PolicyTaskMessage::ResolveAuthorization { token, user, role })
            .await
    }

    /// Non-blocking version of [`resolve_authorization`](Self::resolve_authorization).
    ///
    /// Returns `Err` if the channel is full or the task has shut down.
    pub fn send_resolve_policy(
        &self,
        token: Address,
        user: Address,
        role: AuthRole,
    ) -> Result<(), mpsc::error::TrySendError<PolicyTaskMessage>> {
        self.tx
            .try_send(PolicyTaskMessage::ResolveAuthorization { token, user, role })
    }
}

/// Spawn the policy resolution task as a critical background task.
///
/// Returns a [`PolicyTaskHandle`] for sending pre-fetch requests.
pub fn spawn_policy_resolution_task(
    cache: SharedPolicyCache,
    l1_rpc_url: String,
    max_concurrent: usize,
    channel_capacity: usize,
    task_executor: impl reth_ethereum::tasks::TaskSpawner,
) -> PolicyTaskHandle {
    let (tx, rx) = mpsc::channel(channel_capacity);
    let handle = PolicyTaskHandle::new(tx.clone());

    task_executor.spawn_critical(
        "l1-policy-resolution",
        Box::pin(async move {
            let l1_provider =
                match alloy_provider::ProviderBuilder::new_with_network::<TempoNetwork>()
                    .connect(&l1_rpc_url)
                    .await
                {
                    Ok(p) => p.erased(),
                    Err(e) => {
                        warn!(error = %e, "Failed to connect L1 provider for policy resolution task");
                        return;
                    }
                };

            let task = PolicyResolutionTask {
                provider: PolicyProvider::new(
                    cache,
                    l1_provider,
                    tokio::runtime::Handle::current(),
                ),
                tx,
                rx,
                max_concurrent,
                in_flight: FuturesUnordered::new(),
                metrics: Tip403Metrics::default(),
            };
            task.run().await;
        }),
    );

    handle
}
