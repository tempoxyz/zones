//! TIP-403 and TIP-20 policy event decoding.
//!
//! The L1 subscriber decodes raw receipt logs into [`PolicyEvent`] values outside
//! the cache write lock. The cache then applies those events in block order.

use alloy_primitives::Address;
use alloy_sol_types::{SolEvent, SolEventInterface};
use tempo_contracts::precompiles::ITIP403Registry::PolicyType;

/// A decoded L1 policy event ready to be applied to the cache.
///
/// The [`L1Subscriber`](crate::l1::L1Subscriber) decodes raw logs into these events
/// outside the cache write lock, then applies them in batch via
/// [`PolicyCacheInner::apply_events`](super::PolicyCacheInner::apply_events).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PolicyEvent {
    /// A user's membership in a policy set changed (`WhitelistUpdated` / `BlacklistUpdated`).
    MembershipChanged {
        policy_id: u64,
        account: Address,
        in_set: bool,
    },
    /// A token's transfer policy ID changed (`TransferPolicyUpdate`).
    TokenPolicyChanged { token: Address, policy_id: u64 },
    /// A new simple policy was created on L1 (`PolicyCreated`).
    PolicyCreated {
        policy_id: u64,
        policy_type: PolicyType,
    },
    /// A new compound policy was created on L1 (`CompoundPolicyCreated`).
    CompoundPolicyCreated {
        policy_id: u64,
        sender_policy_id: u64,
        recipient_policy_id: u64,
        mint_recipient_policy_id: u64,
    },
}

impl PolicyEvent {
    /// Try to decode an `ITIP403Registry` log into a [`PolicyEvent`].
    ///
    /// Handles `WhitelistUpdated`, `BlacklistUpdated`, `PolicyCreated`, and
    /// `CompoundPolicyCreated` events. `PolicyAdminUpdated` is logged but ignored
    /// (returns `None`). Returns `None` for unrecognised logs.
    pub fn decode_registry(log: &alloy_rpc_types_eth::Log) -> Option<Self> {
        use tempo_contracts::precompiles::ITIP403Registry::{
            BlacklistUpdated, CompoundPolicyCreated, ITIP403RegistryEvents, PolicyCreated,
            WhitelistUpdated,
        };

        let event = match ITIP403RegistryEvents::decode_log(&log.inner) {
            Ok(decoded) => decoded.data,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to decode TIP-403 event");
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
                tracing::info!(
                    policy_id = policyId,
                    account = %account,
                    restricted,
                    "Decoded BlacklistUpdated"
                );
                Some(Self::MembershipChanged {
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
                tracing::info!(
                    policy_id = policyId,
                    account = %account,
                    allowed,
                    "Decoded WhitelistUpdated"
                );
                Some(Self::MembershipChanged {
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
                tracing::info!(
                    policy_id = policyId,
                    policy_type = ?policyType,
                    "New policy created on L1"
                );
                Some(Self::PolicyCreated {
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
                tracing::info!(
                    policy_id = policyId,
                    sender_policy_id = senderPolicyId,
                    recipient_policy_id = recipientPolicyId,
                    mint_recipient_policy_id = mintRecipientPolicyId,
                    "Compound policy created on L1"
                );
                Some(Self::CompoundPolicyCreated {
                    policy_id: policyId,
                    sender_policy_id: senderPolicyId,
                    recipient_policy_id: recipientPolicyId,
                    mint_recipient_policy_id: mintRecipientPolicyId,
                })
            }
            ITIP403RegistryEvents::PolicyAdminUpdated(event) => {
                tracing::debug!(
                    policy_id = event.policyId,
                    admin = %event.admin,
                    "Policy admin updated on L1"
                );
                None
            }
            ITIP403RegistryEvents::ReceivePolicyUpdated(event) => {
                tracing::debug!(
                    policy_id = ?event,
                    "Receive policy updated on L1 (ignored)"
                );
                None
            }
        }
    }

    /// Try to decode a TIP-20 `TransferPolicyUpdate` log into a
    /// [`PolicyEvent::TokenPolicyChanged`].
    ///
    /// The caller should pre-filter by topic hash before calling this; it will
    /// return `None` with a warning if the log does not match.
    pub fn decode_tip20(log: &alloy_rpc_types_eth::Log) -> Option<Self> {
        use tempo_contracts::precompiles::ITIP20::TransferPolicyUpdate;

        let event = match TransferPolicyUpdate::decode_log(&log.inner) {
            Ok(decoded) => decoded.data,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to decode TIP-20 TransferPolicyUpdate");
                return None;
            }
        };

        let token = log.address();
        tracing::info!(
            token = %token,
            new_policy_id = event.newPolicyId,
            updater = %event.updater,
            "Decoded TransferPolicyUpdate"
        );
        Some(Self::TokenPolicyChanged {
            token,
            policy_id: event.newPolicyId,
        })
    }
}
