//! L1 policy event listener that keeps the [`SharedPolicyCache`] in sync with Tempo L1.
//!
//! Subscribes to L1 block headers and fetches TIP-403 policy events for each new block.
//! Events are decoded into [`PolicyEvent`]s and applied to the cache in a single batch
//! so the zone can evaluate transfer authorization without per-call RPC round-trips.

use super::{PolicyEvent, SharedPolicyCache, metrics::Tip403Metrics};
use alloy_consensus::BlockHeader as _;
use alloy_primitives::Address;
use alloy_provider::{DynProvider, Provider};
use alloy_rpc_types_eth::{Filter, Log};
use alloy_sol_types::{SolEvent, SolEventInterface};
use futures::StreamExt;
use tempo_alloy::TempoNetwork;
use tempo_contracts::precompiles::{
    ITIP20::TransferPolicyUpdate,
    ITIP403Registry::{
        BlacklistUpdated, CompoundPolicyCreated, ITIP403RegistryEvents, PolicyCreated,
        WhitelistUpdated,
    },
    TIP403_REGISTRY_ADDRESS,
};
use tracing::{debug, error, info, warn};

use crate::bindings::ZonePortal::TokenEnabled;

/// Configuration for the policy listener.
#[derive(Debug, Clone)]
pub struct PolicyListenerConfig {
    /// Pre-connected L1 provider for subscriptions and RPC calls.
    pub l1_provider: DynProvider<TempoNetwork>,
    /// ZonePortal contract address on L1, monitored for `TokenEnabled` events.
    pub portal_address: Address,
    /// Token addresses to monitor for transfer policy changes.
    /// New tokens discovered via `TokenEnabled` are appended at runtime.
    pub tracked_tokens: Vec<Address>,
}

/// Listener that watches L1 for TIP-403 policy events and updates the policy cache.
///
/// Subscribes to L1 block headers and, for each new block, fetches events from three sources:
/// `ZonePortal::TokenEnabled` (to discover newly bridged tokens),
/// `TIP403Registry::{BlacklistUpdated, WhitelistUpdated, PolicyCreated, CompoundPolicyCreated}`
/// (policy metadata and membership), and `TIP20::TransferPolicyUpdate` on every tracked token
/// (token-to-policy mapping changes).
///
/// `tracked_tokens` grows dynamically: whenever a `TokenEnabled` event is seen the new token
/// address is appended so its `TransferPolicyUpdate` events are monitored from that point on.
///
/// `last_l1_block` on the cache is advanced after every block, even when no events are found,
/// so downstream consumers know how far the cache has caught up.
///
/// Intended to be spawned via [`spawn_policy_listener`] as a critical task that automatically
/// reconnects on WebSocket failure.
pub struct PolicyListener {
    cache: SharedPolicyCache,
    config: PolicyListenerConfig,
    metrics: Tip403Metrics,
}

impl PolicyListener {
    pub fn new(cache: SharedPolicyCache, config: PolicyListenerConfig) -> Self {
        Self {
            cache,
            config,
            metrics: Tip403Metrics::default(),
        }
    }

    /// Subscribe to L1 block headers and process policy events for each new block.
    pub async fn run(&mut self) -> eyre::Result<()> {
        let mut stream = self
            .config
            .l1_provider
            .subscribe_blocks()
            .await?
            .into_stream();

        info!("Subscribed to L1 block headers for policy events");

        while let Some(header) = stream.next().await {
            let block_number = header.inner.number();

            if let Err(e) = self.process_block(block_number).await {
                warn!(block_number, error = %e, "Failed to process policy events for block");
            }
        }

        warn!("Policy listener block subscription ended");
        Ok(())
    }

    /// Fetch and process policy events for a single L1 block.
    ///
    /// Queries three event sources and applies them to the [`SharedPolicyCache`]:
    ///
    /// 1. **ZonePortal `TokenEnabled`** — discovers newly bridged tokens and adds
    ///    them to `tracked_tokens` so their policy changes are monitored going forward.
    /// 2. **TIP403Registry** — `BlacklistUpdated`, `WhitelistUpdated`, `PolicyCreated`,
    ///    and `CompoundPolicyCreated` events that update policy membership and metadata.
    /// 3. **TIP-20 `TransferPolicyUpdate`** — emitted when a token's transfer policy ID
    ///    changes, updating the token → policy mapping in the cache.
    ///
    /// Even when no events are found, the cache's block tracker is advanced so the
    /// resolution task queries L1 at a recent height.
    async fn process_block(&mut self, block_number: u64) -> eyre::Result<()> {
        let provider = &self.config.l1_provider;

        // Fetch TokenEnabled events from the portal contract.
        let portal_filter = Filter::new()
            .address(self.config.portal_address)
            .event_signature(vec![TokenEnabled::SIGNATURE_HASH])
            .select(block_number);

        let portal_logs = provider.get_logs(&portal_filter).await?;

        // Process portal logs first so newly enabled tokens are immediately tracked.
        for log in &portal_logs {
            match TokenEnabled::decode_log(&log.inner) {
                Ok(decoded) => {
                    let token = decoded.data.token;
                    if !self.config.tracked_tokens.contains(&token) {
                        info!(
                            %token,
                            name = decoded.data.name,
                            symbol = decoded.data.symbol,
                            "New token enabled on portal, adding to tracked tokens"
                        );
                        self.config.tracked_tokens.push(token);
                    }
                }
                Err(e) => {
                    warn!(
                        block_number,
                        address = %log.address(),
                        error = %e,
                        "Failed to decode TokenEnabled event from portal"
                    );
                }
            }
        }

        // Fetch all registry events — not filtered to this zone's policies.
        // The global registry is shared across all zones; pre-caching
        // everything avoids RPC lookups if a token switches policy.
        let registry_filter = Filter::new()
            .address(TIP403_REGISTRY_ADDRESS)
            .event_signature(vec![
                BlacklistUpdated::SIGNATURE_HASH,
                WhitelistUpdated::SIGNATURE_HASH,
                PolicyCreated::SIGNATURE_HASH,
                CompoundPolicyCreated::SIGNATURE_HASH,
            ])
            .select(block_number);

        let registry_logs = provider.get_logs(&registry_filter).await?;

        let tip20_logs = if !self.config.tracked_tokens.is_empty() {
            let tip20_filter = Filter::new()
                .address(self.config.tracked_tokens.clone())
                .event_signature(vec![TransferPolicyUpdate::SIGNATURE_HASH])
                .select(block_number);
            provider.get_logs(&tip20_filter).await?
        } else {
            vec![]
        };

        // Decode all logs into PolicyEvents outside the write lock.
        let events: Vec<_> = registry_logs
            .iter()
            .filter_map(|log| decode_registry_event(log, block_number))
            .chain(
                tip20_logs
                    .iter()
                    .filter_map(|log| decode_tip20_event(log, block_number)),
            )
            .collect();

        let event_count = events.len();
        if event_count > 0 {
            debug!(block_number, count = event_count, "Applying policy events");
        }

        // Always call apply_events to advance the block tracker, even when empty.
        {
            let mut cache = self.cache.write();
            cache.apply_events(block_number, &events);
            self.metrics
                .cached_policies
                .set(cache.policies().len() as f64);
            self.metrics
                .cached_token_policies
                .set(cache.num_token_policies() as f64);
        }

        if event_count > 0 {
            self.metrics
                .listener_events_applied
                .increment(event_count as u64);
        }

        Ok(())
    }
}

/// Query the current `transferPolicyId` for each tracked token and seed it
/// into the cache. This ensures the cache knows about tokens that have never
/// had a `TransferPolicyUpdate` event (i.e. still using the default policy).
///
/// Individual token failures are logged and skipped so one failing token
/// does not prevent the rest from being seeded.
pub async fn seed_token_policies(
    cache: &SharedPolicyCache,
    portal_address: Address,
    tracked_tokens: &[Address],
    provider: &DynProvider<TempoNetwork>,
) -> eyre::Result<()> {
    use tempo_contracts::precompiles::ITIP20;

    let block_number = cache.last_l1_block();

    let futs: Vec<_> = tracked_tokens
        .iter()
        .map(|token| {
            let tip20 = ITIP20::new(*token, provider);
            async move {
                let result = tip20
                    .transferPolicyId()
                    .block(alloy_rpc_types_eth::BlockId::number(block_number))
                    .call()
                    .await;
                (*token, result)
            }
        })
        .collect();

    let results = futures::future::join_all(futs).await;

    let mut w = cache.write();
    for (token, result) in results {
        match result {
            Ok(policy_id) => {
                info!(
                    %token,
                    policy_id,
                    block_number,
                    "Seeded token policy from L1"
                );
                w.set_token_policy(token, block_number, policy_id);
            }
            Err(e) => {
                warn!(
                    %token,
                    %portal_address,
                    error = %e,
                    "Failed to seed transferPolicyId, skipping token"
                );
            }
        }
    }

    Ok(())
}

/// Decode a single TIP-403 registry log into a [`PolicyEvent`], if applicable.
fn decode_registry_event(log: &Log, block_number: u64) -> Option<PolicyEvent> {
    let event = match ITIP403RegistryEvents::decode_log(&log.inner) {
        Ok(decoded) => decoded.data,
        Err(e) => {
            warn!(block_number, error = %e, "Failed to decode TIP-403 event");
            return None;
        }
    };

    match event {
        ITIP403RegistryEvents::BlacklistUpdated(BlacklistUpdated {
            policyId,
            account,
            restricted,
            ..
        }) => {
            info!(
                policy_id = policyId,
                account = %account,
                restricted,
                "Decoded BlacklistUpdated"
            );
            Some(PolicyEvent::MembershipChanged {
                policy_id: policyId,
                account,
                in_set: restricted,
            })
        }
        ITIP403RegistryEvents::WhitelistUpdated(WhitelistUpdated {
            policyId,
            account,
            allowed,
            ..
        }) => {
            info!(
                policy_id = policyId,
                account = %account,
                allowed,
                "Decoded WhitelistUpdated"
            );
            Some(PolicyEvent::MembershipChanged {
                policy_id: policyId,
                account,
                in_set: allowed,
            })
        }
        ITIP403RegistryEvents::PolicyCreated(PolicyCreated {
            policyId,
            policyType,
            ..
        }) => {
            info!(
                policy_id = policyId,
                policy_type = ?policyType,
                "New policy created on L1"
            );
            Some(PolicyEvent::PolicyCreated {
                policy_id: policyId,
                policy_type: policyType,
            })
        }
        ITIP403RegistryEvents::CompoundPolicyCreated(CompoundPolicyCreated {
            policyId,
            senderPolicyId,
            recipientPolicyId,
            mintRecipientPolicyId,
            ..
        }) => {
            info!(
                policy_id = policyId,
                sender_policy_id = senderPolicyId,
                recipient_policy_id = recipientPolicyId,
                mint_recipient_policy_id = mintRecipientPolicyId,
                "Compound policy created on L1"
            );
            Some(PolicyEvent::CompoundPolicyCreated {
                policy_id: policyId,
                sender_policy_id: senderPolicyId,
                recipient_policy_id: recipientPolicyId,
                mint_recipient_policy_id: mintRecipientPolicyId,
            })
        }
        ITIP403RegistryEvents::PolicyAdminUpdated(event) => {
            debug!(
                policy_id = event.policyId,
                admin = %event.admin,
                "Policy admin updated on L1"
            );
            None
        }
    }
}

/// Decode a TIP-20 `TransferPolicyUpdate` log into a [`PolicyEvent`].
fn decode_tip20_event(log: &Log, block_number: u64) -> Option<PolicyEvent> {
    let event = match TransferPolicyUpdate::decode_log(&log.inner) {
        Ok(decoded) => decoded.data,
        Err(e) => {
            warn!(block_number, error = %e, "Failed to decode TIP-20 TransferPolicyUpdate");
            return None;
        }
    };

    let token = log.address();
    info!(
        token = %token,
        new_policy_id = event.newPolicyId,
        updater = %event.updater,
        "Decoded TransferPolicyUpdate"
    );
    Some(PolicyEvent::TokenPolicyChanged {
        token,
        policy_id: event.newPolicyId,
    })
}

/// Spawn the policy listener as a critical background task with automatic reconnection.
pub fn spawn_policy_listener(
    config: PolicyListenerConfig,
    cache: SharedPolicyCache,
    task_executor: impl reth_ethereum::tasks::TaskSpawner,
) {
    let mut listener = PolicyListener::new(cache, config);

    task_executor.spawn_critical(
        "l1-policy-listener",
        Box::pin(async move {
            loop {
                if let Err(e) = listener.run().await {
                    error!(error = %e, "Policy listener failed, reconnecting in 5s");
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }),
    );
}
