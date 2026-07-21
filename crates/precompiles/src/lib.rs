//! Zone-native precompiles and shared execution for Tempo precompiles on a Zone.
//!
//! All implementations use ordinary EVM storage. The Zone EVM installs an anchored database below
//! the revm journal, so upstream TIP-20, TIP-403, and fee-manager code transparently observe
//! finalized Tempo policy state. Zone admission, delegate-call, fixed-gas, and privacy rules remain
//! outside the forwarded business logic.
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
//! - **TIP-403 Registry** ([`tip403_proxy`]) — upstream registry over finalized L1 state.
//! - **Zone TIP-20** ([`ztip20`]) — upstream TIP-20 with zone call rules.
//! - **Zone Fee Manager** ([`zone_fee_manager`]) — direct multi-token fee settlement.

#![cfg_attr(not(feature = "std"), no_std)]
#![allow(clippy::too_many_arguments)]

extern crate alloc;

pub mod error;
pub use error::{Result, ZonePrecompileError, ZoneResult};

pub mod aes_gcm;
pub mod chaum_pedersen;
pub mod ecies;

/// Zone dispatch helpers: generic typed operations plus Tempo's concrete metadata helper.
pub mod dispatch {
    pub use tempo_precompiles::{
        dispatch::typed::{mutate, mutate_void, view},
        metadata,
    };
}

mod execution;
pub mod fee_policy;
pub mod policy;
pub use execution::ZonePrecompileEnv;
pub mod storage;
pub mod tempo_state;
pub mod tip20_factory;
pub mod tip403_proxy;
pub mod zone_fee_manager;
pub mod ztip20;

pub use aes_gcm::{AES_GCM_DECRYPT_ADDRESS, AesGcmDecrypt};
pub use chaum_pedersen::{CHAUM_PEDERSEN_VERIFY_ADDRESS, ChaumPedersenVerify};
pub use fee_policy::ZoneFeePolicy;
pub use storage::{L1State, L1StateError, L1StorageReader};
pub use tempo_contracts::precompiles::TIP403_REGISTRY_ADDRESS;
pub use tempo_state::TempoState;
pub use tip20_factory::{ZONE_TIP20_FACTORY_ADDRESS, ZoneTokenFactory};
pub use zone_fee_manager::{ZoneConfigReader, ZoneFeeManager};
pub use ztip20::SequencerExt;

use alloc::sync::Arc;

use alloy_evm::precompiles::DynPrecompile;
use revm::precompile::PrecompileError;
use tempo_precompiles::{Precompile as _, tip20::TIP20Token, tip403_registry::TIP403Registry};

const ZONE_RPC_ERROR_PREFIX: &str = "[zone rpc]";

/// Creates a fatal precompile error for a transient L1 RPC failure.
pub fn zone_rpc_error(msg: impl core::fmt::Display) -> PrecompileError {
    PrecompileError::Fatal(alloc::format!("{ZONE_RPC_ERROR_PREFIX} {msg}"))
}

/// Returns whether an error originated from a zone L1 RPC read.
pub fn is_zone_rpc_error(err: &str) -> bool {
    err.starts_with(ZONE_RPC_ERROR_PREFIX)
}

/// Creates upstream TIP-403 execution with zone read-only rules and adapter-backed L1 reads.
pub fn create_tip403_precompile(env: &ZonePrecompileEnv) -> DynPrecompile {
    execution::create_precompile(
        "ZoneTip403Registry",
        env,
        tip403_proxy::Tip403Rules,
        |data, caller| TIP403Registry::new().call(data, caller),
    )
}

/// Creates upstream TIP-20 execution with zone rules and adapter-backed L1 policy reads.
pub fn create_tip20_precompile(
    address: alloy_primitives::Address,
    env: &ZonePrecompileEnv,
    sequencer: Arc<dyn SequencerExt>,
) -> DynPrecompile {
    execution::create_precompile(
        "TIP20Token",
        env,
        ztip20::TIP20Rules::new(sequencer),
        move |data, caller| TIP20Token::from_address_unchecked(address).call(data, caller),
    )
}

#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub mod test_utils;
