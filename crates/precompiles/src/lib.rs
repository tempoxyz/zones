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
pub mod storage;
pub mod tempo_state;
pub mod tip20_factory;
pub mod tip403_proxy;
pub mod ztip20;

pub use aes_gcm::{AES_GCM_DECRYPT_ADDRESS, AesGcmDecrypt};
pub use chaum_pedersen::{CHAUM_PEDERSEN_VERIFY_ADDRESS, ChaumPedersenVerify};
pub use storage::{L1AnchorController, L1StorageReader};
pub use tempo_contracts::precompiles::TIP403_REGISTRY_ADDRESS;
pub use tempo_state::TempoState;
pub use tip20_factory::{ZONE_TIP20_FACTORY_ADDRESS, ZoneTokenFactory};
pub use tip403_proxy::ZONE_TIP403_PROXY_ADDRESS;
pub use ztip20::SequencerExt;

use alloc::{rc::Rc, sync::Arc};
use core::cell::RefCell;

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use revm::{context::CfgEnv, precompile::PrecompileError};
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_precompiles::{
    ACCOUNT_KEYCHAIN_ADDRESS, NONCE_PRECOMPILE_ADDRESS, Precompile as _, PrecompileEnv,
    STABLECOIN_DEX_ADDRESS, TIP_FEE_MANAGER_ADDRESS,
    account_keychain::AccountKeychain,
    nonce::NonceManager,
    storage::actions::StorageActions,
    storage_credits::NonCreditableSlots,
    tip_fee_manager::TipFeeManager,
    tip20::{TIP20Token, is_tip20_prefix},
    tip403_registry::TIP403Registry,
};
use zone_primitives::constants::TEMPO_STATE_ADDRESS;

/// Register zone-native and currently supported Tempo precompiles.
///
/// - **Local execution:** AES-GCM, Chaum-Pedersen, `TempoState`, and the zone token factory use
///   shared local execution; nonce and account-keychain retain Tempo's ordinary environment.
/// - **Anchored L1 execution:** TIP-20, TIP-403, and `TipFeeManager` use ordinary EVM storage;
///   the Zone EVM context resolves mirrored reads through its anchored database adapter.
pub fn extend_zone_precompiles<L1: L1StorageReader>(
    precompiles: &mut PrecompilesMap,
    cfg: &CfgEnv<TempoHardfork>,
    l1_reader: L1,
    sequencer: Arc<dyn SequencerExt>,
    actions: StorageActions,
    non_creditable_slots: Rc<RefCell<NonCreditableSlots>>,
    controller: L1AnchorController,
) {
    let protocol_env =
        execution::ProtocolPrecompileEnv::new(cfg, actions.clone(), non_creditable_slots.clone());
    let tempo_env = PrecompileEnv::new(cfg, actions, non_creditable_slots);

    precompiles.apply_precompile(&TEMPO_STATE_ADDRESS, |_| {
        Some(TempoState::create(
            l1_reader.clone(),
            controller.clone(),
            cfg,
        ))
    });
    precompiles.apply_precompile(&CHAUM_PEDERSEN_VERIFY_ADDRESS, |_| {
        Some(ChaumPedersenVerify::create(cfg))
    });
    precompiles.apply_precompile(&AES_GCM_DECRYPT_ADDRESS, |_| {
        Some(AesGcmDecrypt::create(cfg))
    });
    precompiles.apply_precompile(&ZONE_TIP20_FACTORY_ADDRESS, |_| {
        Some(ZoneTokenFactory::create(cfg))
    });

    let tip403_env = protocol_env.clone();
    precompiles.apply_precompile(&ZONE_TIP403_PROXY_ADDRESS, move |_| {
        Some(create_tip403_precompile(&tip403_env))
    });

    // Static zone entries above take priority. Dynamic TIP-20 entries use upstream execution with
    // zone privacy, bridge-authorization, fixed-gas, and anchored policy-storage rules.
    precompiles.set_precompile_lookup(move |address: &alloy_primitives::Address| {
        if is_tip20_prefix(*address) {
            Some(create_tip20_precompile(
                *address,
                &protocol_env,
                sequencer.clone(),
            ))
        } else if *address == TIP_FEE_MANAGER_ADDRESS {
            Some(execution::create_protocol_precompile(
                "TipFeeManager",
                protocol_env.clone(),
                execution::NoCallRules,
                |data, caller| TipFeeManager::new().call(data, caller),
            ))
        } else if *address == STABLECOIN_DEX_ADDRESS {
            None
        } else if *address == NONCE_PRECOMPILE_ADDRESS {
            Some(NonceManager::create_precompile(&tempo_env))
        } else if *address == ACCOUNT_KEYCHAIN_ADDRESS {
            Some(AccountKeychain::create_precompile(&tempo_env))
        } else {
            None
        }
    });
}

impl AesGcmDecrypt {
    /// Create the AES-GCM precompile with ordinary zone-local execution.
    pub fn create(cfg: &CfgEnv<TempoHardfork>) -> DynPrecompile {
        execution::create_local_precompile(
            "AesGcmDecrypt",
            cfg,
            execution::NoCallRules,
            |data, caller| Self.call(data, caller),
        )
    }
}

impl ChaumPedersenVerify {
    /// Create the Chaum-Pedersen precompile with ordinary zone-local execution.
    pub fn create(cfg: &CfgEnv<TempoHardfork>) -> DynPrecompile {
        execution::create_local_precompile(
            "ChaumPedersenVerify",
            cfg,
            execution::NoCallRules,
            |data, caller| Self.call(data, caller),
        )
    }
}

impl TempoState {
    /// Create the `TempoState` precompile with local storage and direct-call-only execution.
    ///
    /// Storage-slot RPC reads are delegated to `reader` at the checkpoint recorded in local state.
    pub fn create<P: L1StorageReader>(
        reader: P,
        controller: L1AnchorController,
        cfg: &CfgEnv<TempoHardfork>,
    ) -> DynPrecompile {
        execution::create_local_precompile(
            "TempoState",
            cfg,
            execution::DirectCallOnly,
            move |data, caller| Self::new().call_with_provider(&reader, &controller, data, caller),
        )
    }
}

impl ZoneTokenFactory {
    /// Create the zone token factory with local storage and direct-call-only execution.
    pub fn create(cfg: &CfgEnv<TempoHardfork>) -> DynPrecompile {
        execution::create_local_precompile(
            "ZoneTokenFactory",
            cfg,
            execution::DirectCallOnly,
            |data, caller| Self::new().call(data, caller),
        )
    }
}

/// Create upstream TIP-403 execution with zone read-only rules and finalized L1 state.
pub(crate) fn create_tip403_precompile(env: &execution::ProtocolPrecompileEnv) -> DynPrecompile {
    execution::create_protocol_precompile(
        "ZoneTip403Registry",
        env.clone(),
        tip403_proxy::Tip403Rules,
        |data, caller| TIP403Registry::new().call(data, caller),
    )
}

/// Create upstream TIP-20 execution with zone rules and finalized L1 policy reads.
pub(crate) fn create_tip20_precompile(
    address: alloy_primitives::Address,
    env: &execution::ProtocolPrecompileEnv,
    sequencer: Arc<dyn SequencerExt>,
) -> DynPrecompile {
    execution::create_protocol_precompile(
        "TIP20Token",
        env.clone(),
        ztip20::TIP20Rules::new(sequencer),
        move |data, caller| TIP20Token::from_address_unchecked(address).call(data, caller),
    )
}

const ZONE_RPC_ERROR_PREFIX: &str = "[zone rpc]";

/// Create a [`PrecompileError::Fatal`] for transient L1 RPC errors.
///
/// Fatal errors propagate out of the EVM as `Err` (instead of a revert),
/// allowing the builder to skip the pool transaction rather than charging gas.
pub fn zone_rpc_error(msg: impl core::fmt::Display) -> PrecompileError {
    PrecompileError::Fatal(alloc::format!("{ZONE_RPC_ERROR_PREFIX} {msg}"))
}

/// Returns `true` if the error chain contains a failure produced by [`zone_rpc_error`].
pub fn is_zone_rpc_error(err: &str) -> bool {
    err.contains(ZONE_RPC_ERROR_PREFIX)
}

#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub mod test_utils;
