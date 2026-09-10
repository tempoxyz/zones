//! L1 state cache and provider for reading Tempo L1 contract storage from the zone.
//!
//! This module provides:
//!
//! - [`L1StateCache`] — a shared in-memory cache of L1 contract storage slots.
//! - [`L1StateCacheInner`] — the block-versioned cache storage guarded by [`L1StateCache`].
//! - [`L1RpcClient`] — shared trust-neutral exact-block RPC transport.
//! - [`L1StateProvider`] — an ordinary cache-first, RPC-fallback reader.
//! - [`VerifiedL1StateCache`] — storage-root-keyed authenticated values.
//! - [`PayloadL1StateProvider`] — payload-local provisional reads with deferred verification.
//!
//! TIP-20 and TIP-403 policy semantics are evaluated by Tempo's upstream precompiles. This
//! module only supplies their exact-block raw L1 storage view.

pub mod cache;
pub mod enabled_tokens;
pub mod provider;
pub mod verified;

pub use cache::{L1StateCache, L1StateCacheInner};
pub use enabled_tokens::EnabledTokenRegistry;
pub use provider::{L1ProofTargets, L1RpcClient, L1StateProvider, L1StateProviderConfig};
pub use verified::{L1ReadValidationError, PayloadL1StateProvider, VerifiedL1StateCache};
