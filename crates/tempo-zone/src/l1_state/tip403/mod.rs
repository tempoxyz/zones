//! TIP-403 policy cache, listener, provider, and resolution task for the zone sequencer.
//!
//! This module tracks TIP-403 transfer policy state from Tempo L1:
//!
//! - [`PolicyCache`] — block-versioned in-memory cache of policy data.
//! - [`PolicyListener`] — L1 event listener that keeps the cache in sync.
//! - [`PolicyProvider`] — cache-first, RPC-fallback authorization provider.
//! - [`PolicyResolutionTask`] — background task for pre-fetching authorization data.

mod cache;
mod listener;
mod metrics;
pub mod provider;
pub mod task;

pub use cache::{
    AuthRole, CachedPolicy, CompoundData, MembershipSet, PolicyCache, PolicyEvent,
    SharedPolicyCache,
};
pub(crate) use cache::{FIRST_USER_POLICY, POLICY_ALLOW_ALL, POLICY_REJECT_ALL};
pub use listener::{PolicyListener, PolicyListenerConfig, spawn_policy_listener};
pub use metrics::Tip403Metrics;
pub use provider::PolicyProvider;
pub use task::{PolicyResolutionTask, PolicyTaskMessage, spawn_policy_resolution_task};
