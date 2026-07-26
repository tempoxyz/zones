//! Zone-native precompiles and shared execution for Tempo precompiles on a Zone.
//!
//! All implementations use ordinary EVM storage. The Zone EVM installs an anchored database below
//! the revm journal, so TIP-20, TIP-403, and Zone fee-manager code transparently observe
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
//! - **Zone Inbox** ([`inbox`]) — advances Tempo state and processes the deposit queue.
//! - **TIP-403 Registry** ([`tip403_proxy`]) — upstream registry over finalized L1 state.
//! - **Zone TIP-20** ([`ztip20`]) — upstream TIP-20 with zone call rules.

#![cfg_attr(not(feature = "std"), no_std)]
#![allow(clippy::too_many_arguments)]

extern crate alloc;

pub mod error;
pub use error::{Result, ZonePrecompileError, ZoneResult};

pub mod aes_gcm;
pub mod chaum_pedersen;
pub mod ecies;
pub mod outbox;

/// Zone dispatch helpers: generic typed operations plus Tempo's concrete metadata helper.
pub mod dispatch {
    pub use tempo_precompiles::{
        dispatch::typed::{mutate, mutate_void, view},
        metadata,
    };
}

mod execution;
pub use execution::ZonePrecompileEnv;
pub mod inbox;
pub mod storage;
pub mod tempo_state;
pub mod tip403_proxy;
#[cfg(feature = "std")]
pub mod tx_context;
pub mod zone_fee_manager;
pub mod ztip20;

pub use aes_gcm::{AES_GCM_DECRYPT_ADDRESS, AesGcmDecrypt};
pub use chaum_pedersen::{CHAUM_PEDERSEN_VERIFY_ADDRESS, ChaumPedersenVerify};
pub use inbox::{ADVANCE_TEMPO_SELECTOR, ZoneInbox};
pub use outbox::ZoneOutbox;
pub use storage::{L1State, L1StateError, L1StorageReader};
pub use tempo_contracts::precompiles::TIP403_REGISTRY_ADDRESS;
pub use tempo_state::TempoState;
pub use zone_fee_manager::{ZONE_FEE_MANAGER_ADDRESS, ZoneFeeManager};

use alloy_evm::precompiles::DynPrecompile;
use alloy_primitives::Address;
use tempo_precompiles::{Precompile as _, tip20::TIP20Token, tip403_registry::TIP403Registry};

/// Creates the zone-native fee manager precompile.
pub fn create_zone_fee_manager_precompile(env: &ZonePrecompileEnv) -> DynPrecompile {
    execution::create_precompile(
        "ZoneFeeManager",
        env,
        execution::NoCallRules,
        |data, caller| ZoneFeeManager::new().call(data, caller),
    )
}

/// Creates the native ZoneOutbox over ordinary Zone storage and direct finalized portal reads.
#[cfg(feature = "std")]
pub fn create_outbox_precompile<P>(l1: L1State<P>, env: &ZonePrecompileEnv) -> DynPrecompile
where
    P: L1StorageReader,
{
    execution::create_precompile(
        "ZoneOutbox",
        env,
        execution::NoCallRules,
        move |data, caller| {
            let (tx_hash, fee_payer) =
                tx_context::current_transaction().unwrap_or((Default::default(), caller));
            ZoneOutbox::new().call_with_transaction(&l1, data, caller, tx_hash, fee_payer)
        },
    )
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
pub fn create_tip20_precompile<P>(
    address: Address,
    env: &ZonePrecompileEnv,
    l1: L1State<P>,
) -> DynPrecompile
where
    P: L1StorageReader,
{
    execution::create_precompile(
        "TIP20Token",
        env,
        ztip20::TIP20Rules::new(l1),
        move |data, caller| TIP20Token::from_address_unchecked(address).call(data, caller),
    )
}

#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub mod test_utils;
