//! Zone-specific precompile implementations.
//!
//! This crate is `no_std` compatible so these precompiles can run inside the
//! SP1 prover guest (RISC-V) as well as in the zone node.
//!
//! ## Crypto precompiles
//!
//! - **Chaum-Pedersen Verify** ([`chaum_pedersen`]) — verifies DLOG equality proofs
//!   for ECDH shared secret derivation.
//! - **AES-256-GCM Decrypt** ([`aes_gcm`]) — decrypts ECIES ciphertext and verifies
//!   the GCM authentication tag.
//! - **ECIES** ([`ecies`]) — sequencer-side ECIES decryption logic.
//!
//! ## Policy/token precompiles
//!
//! - **TIP-20 Factory** ([`tip20_factory`]) — zone-side TIP-20 token factory.
//! - **TIP-403 Proxy** ([`tip403_proxy`]) — read-only TIP-403 registry proxy.
//! - **Zone TIP-20** ([`ztip20`]) — policy-aware TIP-20 wrapper.

#![cfg_attr(not(feature = "std"), no_std)]
#![allow(clippy::too_many_arguments)]

extern crate alloc;

// Required by the `#[contract]` proc macro expansion (references `crate::storage` / `crate::error`).
pub(crate) use tempo_precompiles::{error, storage};

pub mod aes_gcm;
pub mod chaum_pedersen;
pub mod ecies;
pub mod policy;
pub mod tip20_factory;
pub mod tip403_proxy;
pub mod ztip20;

pub use aes_gcm::{AES_GCM_DECRYPT_ADDRESS, AesGcmDecrypt};
pub use chaum_pedersen::{CHAUM_PEDERSEN_VERIFY_ADDRESS, ChaumPedersenVerify};
pub use tip20_factory::{ZONE_TIP20_FACTORY_ADDRESS, ZoneTokenFactory};
pub use tip403_proxy::{ZONE_TIP403_PROXY_ADDRESS, ZoneTip403ProxyRegistry};
pub use ztip20::ZoneTip20Token;

use revm::precompile::PrecompileError;

const ZONE_RPC_ERROR_PREFIX: &str = "[zone rpc]";

/// Create a [`PrecompileError::Fatal`] for transient L1 RPC errors.
///
/// Fatal errors propagate out of the EVM as `Err` (instead of a revert),
/// allowing the builder to skip the pool transaction rather than charging gas.
pub fn zone_rpc_error(msg: impl core::fmt::Display) -> PrecompileError {
    PrecompileError::Fatal(alloc::format!("{ZONE_RPC_ERROR_PREFIX} {msg}"))
}

/// Returns `true` if the error string was produced by [`zone_rpc_error`].
pub fn is_zone_rpc_error(err: &str) -> bool {
    err.starts_with(ZONE_RPC_ERROR_PREFIX)
}

#[cfg(test)]
mod test_utils;
