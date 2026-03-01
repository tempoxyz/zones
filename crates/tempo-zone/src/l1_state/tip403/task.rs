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
    provider: PolicyProvider,
    rx: mpsc::Receiver<PolicyTaskMessage>,
    /// Maximum number of concurrent in-flight RPC resolutions.
    max_concurrent: usize,
    metrics: Tip403Metrics,
}

impl PolicyResolutionTask {
    /// Create a new resolution task.
    pub fn new(
        cache: SharedPolicyCache,
        l1_provider: DynProvider<TempoNetwork>,
        rx: mpsc::Receiver<PolicyTaskMessage>,
        max_concurrent: usize,
    ) -> Self {
        let provider = PolicyProvider::new(cache, l1_provider, tokio::runtime::Handle::current());
        Self {
            provider,
            rx,
            max_concurrent,
            metrics: Tip403Metrics::default(),
        }
    }

    /// Run the resolution loop until the channel is closed.
    ///
    /// Processes requests concurrently up to `max_concurrent` in-flight futures.
    /// Errors are logged and swallowed — a failed resolution simply means the cache
    /// remains unpopulated and the builder will fall back to its own RPC path.
    pub async fn run(mut self) {
        let mut in_flight = FuturesUnordered::new();

        loop {
            tokio::select! {
                // Accept new messages when below the concurrency limit.
                Some(msg) = self.rx.recv(), if in_flight.len() < self.max_concurrent => {
                    self.handle_message(msg, &mut in_flight);
                    self.metrics.prefetch_in_flight.set(in_flight.len() as f64);
                }
                // Drain completed futures.
                Some(()) = in_flight.next() => {
                    self.metrics.prefetch_in_flight.set(in_flight.len() as f64);
                }
                // Channel closed and all in-flight work drained.
                else => break,
            }
        }

        debug!("Policy resolution task shutting down");
    }

    /// Dispatch a message to the appropriate handler.
    fn handle_message(
        &self,
        msg: PolicyTaskMessage,
        in_flight: &mut FuturesUnordered<futures::future::BoxFuture<'static, ()>>,
    ) {
        match msg {
            PolicyTaskMessage::ResolveAuthorization {
                token,
                user,
                block_number,
                role,
            } => {
                self.metrics.prefetch_requests_total.increment(1);
                let provider = self.provider.clone();
                let metrics = self.metrics.clone();
                in_flight.push(
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
    ResolveAuthorization {
        /// The TIP-20 token whose transfer policy to check.
        token: Address,
        /// The address to check authorization for.
        user: Address,
        /// The L1 block number to query at.
        block_number: u64,
        /// Which role to resolve (sender, recipient, mint recipient, or full transfer).
        role: AuthRole,
    },
}

/// Spawn the policy resolution task as a critical background task.
pub fn spawn_policy_resolution_task(
    cache: SharedPolicyCache,
    l1_rpc_url: String,
    rx: mpsc::Receiver<PolicyTaskMessage>,
    max_concurrent: usize,
    task_executor: impl reth_ethereum::tasks::TaskSpawner,
) {
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

            let task = PolicyResolutionTask::new(cache, l1_provider, rx, max_concurrent);
            task.run().await;
        }),
    );
}
