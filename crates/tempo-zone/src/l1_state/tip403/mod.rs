//! TIP-403 policy cache, provider, and resolution task for the zone sequencer.
//!
//! This module tracks TIP-403 transfer policy state from Tempo L1:
//!
//! - [`PolicyCache`] — block-versioned in-memory cache of policy data.
//! - [`PolicyProvider`] — cache-first, RPC-fallback authorization provider.
//! - [`PolicyResolutionTask`] — background task for pre-fetching authorization data.
//!
//! # Data flow
//!
//! ```text
//!                        L1
//!      (TIP403Registry, TIP-20 tokens, ZonePortal)
//!                  |                  ^
//!           events |                  | RPC fallback
//!                  |                  |
//!            L1Subscriber        PolicyProvider
//!                  |              |          |
//!            write |         read |          |
//!                  v              |          |
//!           SharedPolicyCache-----+     engine + EVM
//!                  ^                        ^
//!                  |                        | pre-fetch
//!             seed (startup)    pool_prefetch + ResolutionTask
//! ```
//!
//! The [`L1Subscriber`](crate::l1::L1Subscriber) extracts policy events from
//! `eth_getBlockReceipts` and applies them (creates, membership updates,
//! compound configs) to the [`SharedPolicyCache`].
//! [`PolicyProvider`] serves authorization queries from cache, falling back to L1
//! RPC on miss and writing the result back for future lookups.
//!
//! # Startup sequence
//!
//! 1. [`seed_token_policies`] — bulk-fetch current policy state from L1 for all
//!    tracked tokens and populate the cache baseline.
//! 2. [`spawn_policy_resolution_task`] — start the background resolution task
//!    (processes pre-fetch requests from the pool and other callers).
//! 3. [`spawn_pool_prefetch_task`] — watch incoming pool transactions and submit
//!    sender/recipient addresses for cache warming.
//! 4. Create [`PolicyProvider`] instances — one for the engine payload builder,
//!    one for the EVM precompile, both backed by the same [`SharedPolicyCache`].
//!
//! # Cache miss resolution
//!
//! The zone advances in lockstep with L1, so the L1Subscriber captures every
//! policy change for enabled tokens from the moment it starts. Cache misses only
//! occur for state that predates the subscriber — either because the zone was
//! created at an arbitrary L1 height or because the sequencer restarted with a
//! cold cache.
//!
//! On a miss, [`PolicyProvider::is_authorized`] falls back to an RPC call against
//! L1 at the zone's current height and writes the result into the cache. This is
//! safe because L1 is authoritative and the zone never runs ahead of it: the
//! queried state is final for the current block, and the subscriber will apply any
//! future changes that supersede it.
//!
//! # Key invariants
//!
//! - **Only the engine drives `advance()`**: the L1Subscriber writes events via
//!   `apply_events` but never advances the cache baseline. The engine calls
//!   `SharedPolicyCache::advance()` after processing each L1 block, ensuring the
//!   cache never runs ahead of the engine's view.

mod cache;
mod listener;
mod metrics;
mod pool_prefetch;
pub mod provider;
pub mod task;

pub use cache::{
    AuthRole, CachedPolicy, CompoundData, MembershipSet, PolicyCache, PolicyEvent,
    SharedPolicyCache,
};
pub(crate) use cache::{FIRST_USER_POLICY, POLICY_ALLOW_ALL, POLICY_REJECT_ALL};
pub use listener::seed_token_policies;
pub(crate) use listener::{decode_registry_event, decode_tip20_event};
pub use metrics::Tip403Metrics;
pub use pool_prefetch::spawn_pool_prefetch_task;
pub use provider::PolicyProvider;
pub use task::{
    PolicyResolutionTask, PolicyTaskHandle, PolicyTaskMessage, spawn_policy_resolution_task,
};
