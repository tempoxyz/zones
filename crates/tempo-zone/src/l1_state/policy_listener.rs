//! L1 policy event listener that keeps the [`SharedPolicyCache`] in sync with Tempo L1.
//!
//! Subscribes to L1 block headers and fetches TIP-403 policy events for each new block.
//! Events are parsed and applied to the cache so the zone can evaluate transfer
//! authorization without per-call RPC round-trips.

use crate::l1_state::{PolicyCache, SharedPolicyCache};
use alloy_primitives::Address;
use alloy_provider::{Provider, ProviderBuilder, WsConnect};
use alloy_rpc_types_eth::{Filter, Log};
use alloy_sol_types::{SolEvent, SolEventInterface};
use futures::StreamExt;
use tempo_contracts::precompiles::{
    ITIP20::TransferPolicyUpdate,
    ITIP403Registry::{
        BlacklistUpdated, ITIP403RegistryEvents, PolicyCreated, WhitelistUpdated,
    },
    TIP403_REGISTRY_ADDRESS,
};
use tracing::{debug, error, info, warn};

/// Configuration for the policy listener.
#[derive(Debug, Clone)]
pub struct PolicyListenerConfig {
    /// WebSocket URL of the L1 node.
    pub l1_ws_url: String,
    /// Token addresses to monitor for transfer policy changes.
    pub tracked_tokens: Vec<Address>,
}

/// Listener that watches L1 for TIP-403 policy events and updates the policy cache.
#[derive(Clone)]
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
    pub async fn run(&self) -> eyre::Result<()> {
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

    /// Fetch and process TIP-403 and TIP-20 policy events for a single block.
    async fn process_block(
        &self,
        provider: &impl Provider,
        block_number: u64,
    ) -> eyre::Result<()> {
        let registry_filter = Filter::new()
            .address(TIP403_REGISTRY_ADDRESS)
            .event_signature(vec![
                BlacklistUpdated::SIGNATURE_HASH,
                WhitelistUpdated::SIGNATURE_HASH,
                PolicyCreated::SIGNATURE_HASH,
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
            return Ok(());
        }

        debug!(
            block_number,
            registry = registry_logs.len(),
            tip20 = tip20_logs.len(),
            "Processing policy events"
        );

        let mut cache = self.cache.write();

        for log in &registry_logs {
            self.apply_registry_event(&mut cache, log, block_number);
        }

        for log in &tip20_logs {
            self.apply_tip20_event(&mut cache, log, block_number);
        }

        Ok(())
    }

    /// Apply a single TIP-403 registry event to the cache.
    fn apply_registry_event(&self, cache: &mut PolicyCache, log: &Log, block_number: u64) {
        let event = match ITIP403RegistryEvents::decode_log(&log.inner) {
            Ok(decoded) => decoded.data,
            Err(e) => {
                warn!(block_number, error = %e, "Failed to decode TIP-403 event");
                return;
            }
        };

        match event {
            ITIP403RegistryEvents::BlacklistUpdated(BlacklistUpdated {
                policyId,
                account,
                restricted,
                ..
            }) => {
                let count =
                    cache.update_policy_membership(policyId, account, block_number, restricted);
                info!(
                    policy_id = policyId,
                    account = %account,
                    restricted,
                    tokens_updated = count,
                    "Applied BlacklistUpdated"
                );
            }
            ITIP403RegistryEvents::WhitelistUpdated(WhitelistUpdated {
                policyId,
                account,
                allowed,
                ..
            }) => {
                let count =
                    cache.update_policy_membership(policyId, account, block_number, allowed);
                info!(
                    policy_id = policyId,
                    account = %account,
                    allowed,
                    tokens_updated = count,
                    "Applied WhitelistUpdated"
                );
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
            }
            ITIP403RegistryEvents::PolicyAdminUpdated(event) => {
                debug!(
                    policy_id = event.policyId,
                    admin = %event.admin,
                    "Policy admin updated on L1"
                );
            }
            _ => {
                debug!(block_number, "Unhandled TIP-403 event variant");
            }
        }
    }

    /// Apply a TIP-20 `TransferPolicyUpdate` event to the cache.
    fn apply_tip20_event(&self, cache: &mut PolicyCache, log: &Log, block_number: u64) {
        let event = match TransferPolicyUpdate::decode_log(&log.inner) {
            Ok(decoded) => decoded.data,
            Err(e) => {
                warn!(block_number, error = %e, "Failed to decode TIP-20 TransferPolicyUpdate event");
                return;
            }
        };

        let token_address = log.address();
        let new_policy_id = event.newPolicyId;
        cache.set_token_policy(token_address, block_number, new_policy_id);

        info!(
            token = %token_address,
            new_policy_id,
            block_number,
            updater = %event.updater,
            "Applied TransferPolicyUpdate"
        );
    }
}

/// Spawn the policy listener as a critical background task with automatic reconnection.
pub fn spawn_policy_listener(
    config: PolicyListenerConfig,
    cache: SharedPolicyCache,
    task_executor: impl reth_ethereum::tasks::TaskSpawner,
) {
    let listener = PolicyListener::new(cache, config);

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
