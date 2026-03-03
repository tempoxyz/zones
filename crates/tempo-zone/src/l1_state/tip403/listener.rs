//! L1 policy event listener that keeps the [`SharedPolicyCache`] in sync with Tempo L1.
//!
//! Subscribes to L1 block headers and fetches TIP-403 policy events for each new block.
//! Events are decoded into [`PolicyEvent`]s and applied to the cache in a single batch
//! so the zone can evaluate transfer authorization without per-call RPC round-trips.

use super::{PolicyEvent, SharedPolicyCache};
use alloy_primitives::Address;
use alloy_provider::{Provider, ProviderBuilder, WsConnect};
use alloy_rpc_types_eth::{Filter, Log};
use alloy_sol_types::{SolEvent, SolEventInterface};
use futures::StreamExt;
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
    /// WebSocket URL of the L1 node.
    pub l1_ws_url: String,
    /// ZonePortal contract address on L1, monitored for `TokenEnabled` events.
    pub portal_address: Address,
    /// Token addresses to monitor for transfer policy changes.
    /// New tokens discovered via `TokenEnabled` are appended at runtime.
    pub tracked_tokens: Vec<Address>,
}

/// Listener that watches L1 for TIP-403 policy events and updates the policy cache.
pub struct PolicyListener {
    cache: SharedPolicyCache,
    config: PolicyListenerConfig,
}

impl PolicyListener {
    pub fn new(cache: SharedPolicyCache, config: PolicyListenerConfig) -> Self {
        Self { cache, config }
    }

    /// Connect to the L1 node via WebSocket and subscribe to new block headers.
    ///
    /// On each new block, TIP-403 policy events are fetched and applied to the cache.
    pub async fn run(&mut self) -> eyre::Result<()> {
        info!(url = %self.config.l1_ws_url, "Connecting to L1 for policy listener");

        let ws = WsConnect::new(&self.config.l1_ws_url);
        let provider = ProviderBuilder::new().connect_ws(ws).await?;

        info!("Policy listener connected, subscribing to block headers");

        let mut stream = provider.subscribe_blocks().await?.into_stream();

        info!("Subscribed to L1 block headers for policy events");

        while let Some(header) = stream.next().await {
            let block_number = header.number;

            if let Err(e) = self.process_block(&provider, block_number).await {
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
    async fn process_block(
        &mut self,
        provider: &impl Provider,
        block_number: u64,
    ) -> eyre::Result<()> {
        // Fetch TokenEnabled events from the portal contract.
        let portal_filter = Filter::new()
            .address(self.config.portal_address)
            .event_signature(vec![TokenEnabled::SIGNATURE_HASH])
            .select(block_number);

        let portal_logs = provider.get_logs(&portal_filter).await?;

        // Process portal logs first so newly enabled tokens are immediately tracked.
        for log in &portal_logs {
            if let Ok(decoded) = TokenEnabled::decode_log(&log.inner) {
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
        }

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

        if registry_logs.is_empty() && tip20_logs.is_empty() {
            // No policy events in this block, but still advance the block tracker
            // so the resolution task queries L1 at a recent height.
            self.cache.write().apply_events(block_number, &[]);
            return Ok(());
        }

        debug!(
            block_number,
            registry = registry_logs.len(),
            tip20 = tip20_logs.len(),
            "Processing policy events"
        );

        // Decode all logs into PolicyEvents outside the write lock.
        let mut events = Vec::new();
        for log in &registry_logs {
            if let Some(event) = decode_registry_event(log, block_number) {
                events.push(event);
            }
        }
        for log in &tip20_logs {
            if let Some(event) = decode_tip20_event(log, block_number) {
                events.push(event);
            }
        }

        if !events.is_empty() {
            self.cache.write().apply_events(block_number, &events);
        }

        Ok(())
    }
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
