//! TIP-403 policy event decoding helpers and token policy seeding.
//!
//! Contains helpers for decoding TIP-403 registry events and TIP-20
//! `TransferPolicyUpdate` events from L1 logs, plus the [`seed_token_policies`]
//! startup function. The actual event listening is handled by the unified
//! [`L1Subscriber`](crate::l1::L1Subscriber) which extracts events from
//! `eth_getBlockReceipts`.

use super::{PolicyEvent, SharedPolicyCache};
use alloy_primitives::Address;
use alloy_provider::DynProvider;
use alloy_rpc_types_eth::Log;
use alloy_sol_types::{SolEvent, SolEventInterface};
use tempo_alloy::TempoNetwork;
use tempo_contracts::precompiles::{
    ITIP20::TransferPolicyUpdate,
    ITIP403Registry::{
        BlacklistUpdated, CompoundPolicyCreated, ITIP403RegistryEvents, PolicyCreated,
        WhitelistUpdated,
    },
};
use tracing::{debug, info, warn};

/// Query the current `transferPolicyId` for each tracked token and seed it
/// into the cache. This ensures the cache knows about tokens that have never
/// had a `TransferPolicyUpdate` event (i.e. still using the default policy).
///
/// Fails if any token's `transferPolicyId` cannot be resolved — all enabled
/// tokens must be seeded for the zone to enforce policies correctly.
pub async fn seed_token_policies(
    cache: &SharedPolicyCache,
    portal_address: Address,
    tracked_tokens: &[Address],
    provider: &DynProvider<TempoNetwork>,
) -> eyre::Result<()> {
    use tempo_contracts::precompiles::ITIP20;

    let block_number = cache.last_l1_block();

    let seeded = futures::future::join_all(tracked_tokens.iter().map(|token| {
        let tip20 = ITIP20::new(*token, provider);
        async move {
            let policy_id = tip20
                .transferPolicyId()
                .block(alloy_rpc_types_eth::BlockId::number(block_number))
                .call()
                .await
                .map_err(|e| {
                    eyre::eyre!(
                        "failed to seed transferPolicyId for token {token} \
                         (portal {portal_address}): {e}"
                    )
                })?;
            Ok::<_, eyre::Report>((*token, policy_id))
        }
    }))
    .await
    .into_iter()
    .collect::<eyre::Result<Vec<_>>>()?;

    let mut w = cache.write();
    for (token, policy_id) in seeded {
        info!(%token, policy_id, block_number, "Seeded token policy from L1");
        w.set_token_policy(token, block_number, policy_id);
    }

    Ok(())
}

/// Decode a single TIP-403 registry log into a [`PolicyEvent`], if applicable.
pub(crate) fn decode_registry_event(log: &Log, block_number: u64) -> Option<PolicyEvent> {
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
pub(crate) fn decode_tip20_event(log: &Log, block_number: u64) -> Option<PolicyEvent> {
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
