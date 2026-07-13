//! Zone-native precompiles and shared execution for Tempo precompiles on a zone.
//!
//! Zone-native implementations execute against ordinary local EVM storage. Tempo implementations
//! that require finalized L1 state execute through a storage overlay anchored at the block recorded
//! in `TempoState`. Zone admission, delegate-call, fixed-gas, and privacy rules remain outside the
//! forwarded business logic.
//!
//! [`extend_zone_precompiles`] centralizes registration while the progressive policy migration is
//! underway. The legacy zone TIP-20 and TIP-403 implementations remain active until their dedicated
//! cutovers; `TipFeeManager` is currently the low-risk Tempo wrapper using anchored L1 execution.
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

// Required by the `#[contract]` proc macro expansion (references `crate::error`).
pub(crate) use tempo_precompiles::error;

pub mod aes_gcm;
pub mod chaum_pedersen;
pub mod ecies;
mod execution;
pub mod policy;
pub mod storage;
pub mod tempo_state;
pub mod tip20_factory;
pub mod tip403_proxy;
pub mod ztip20;

pub use aes_gcm::{AES_GCM_DECRYPT_ADDRESS, AesGcmDecrypt};
pub use chaum_pedersen::{CHAUM_PEDERSEN_VERIFY_ADDRESS, ChaumPedersenVerify};
pub use storage::L1StorageReader;
pub use tempo_state::TempoState;
pub use tip20_factory::{ZONE_TIP20_FACTORY_ADDRESS, ZoneTokenFactory};
pub use tip403_proxy::{ZONE_TIP403_PROXY_ADDRESS, ZoneTip403ProxyRegistry};
pub use ztip20::{SequencerExt, ZoneTip20Token};

use alloc::{rc::Rc, sync::Arc};
use core::cell::RefCell;

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use revm::{context::CfgEnv, precompile::PrecompileError};
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_precompiles::{
    ACCOUNT_KEYCHAIN_ADDRESS, NONCE_PRECOMPILE_ADDRESS, Precompile as _, PrecompileEnv,
    STABLECOIN_DEX_ADDRESS, TIP_FEE_MANAGER_ADDRESS, account_keychain::AccountKeychain,
    nonce::NonceManager, storage::actions::StorageActions, storage_credits::NonCreditableSlots,
    tip_fee_manager::TipFeeManager, tip20::is_tip20_prefix,
};
use zone_primitives::constants::TEMPO_STATE_ADDRESS;

use crate::policy::PolicyCheck;

/// Register zone-native and currently supported Tempo precompiles.
///
/// - **Local and legacy execution:** AES-GCM, Chaum-Pedersen, `TempoState`, and the zone token
///   factory use shared local execution; nonce and account-keychain retain Tempo's ordinary
///   environment; and the policy-cache-backed [`ZoneTip20Token`] and
///   [`ZoneTip403ProxyRegistry`] remain active until migrated.
/// - **Anchored L1 execution:** `TipFeeManager` uses the exact finalized Tempo anchor through
///   [`storage::ZonePrecompileStorageProvider`].
pub fn extend_zone_precompiles<L1, Policy>(
    precompiles: &mut PrecompilesMap,
    cfg: &CfgEnv<TempoHardfork>,
    l1_reader: L1,
    policy_provider: Option<Policy>,
    sequencer: Arc<dyn SequencerExt>,
    actions: StorageActions,
    non_creditable_slots: Rc<RefCell<NonCreditableSlots>>,
) where
    L1: L1StorageReader,
    Policy: PolicyCheck + Clone + Send + Sync + 'static,
{
    let l1_env = execution::L1BackedPrecompileEnv::new(
        cfg,
        l1_reader.clone(),
        actions.clone(),
        non_creditable_slots.clone(),
    );
    let tempo_env = PrecompileEnv::new(cfg, actions, non_creditable_slots);

    precompiles.apply_precompile(&TEMPO_STATE_ADDRESS, |_| {
        Some(TempoState::create(l1_reader.clone(), cfg))
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

    if let Some(provider) = policy_provider.clone() {
        precompiles.apply_precompile(&ZONE_TIP403_PROXY_ADDRESS, |_| {
            Some(ZoneTip403ProxyRegistry::create(provider.clone(), cfg))
        });
    }
    let registry = policy_provider.map(ZoneTip403ProxyRegistry::new);

    // Static zone entries above take priority. The dynamic lookup preserves the legacy token
    // wrapper while sharing registration for the remaining Tempo precompiles.
    let zone_cfg = cfg.clone();
    precompiles.set_precompile_lookup(move |address: &alloy_primitives::Address| {
        if is_tip20_prefix(*address) {
            Some(ZoneTip20Token::create(
                *address,
                &zone_cfg,
                registry.clone(),
                sequencer.clone(),
            ))
        } else if *address == TIP_FEE_MANAGER_ADDRESS {
            Some(execution::create_l1_backed_precompile(
                "TipFeeManager",
                l1_env.clone(),
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
    pub fn create<P: L1StorageReader>(reader: P, cfg: &CfgEnv<TempoHardfork>) -> DynPrecompile {
        execution::create_local_precompile(
            "TempoState",
            cfg,
            execution::DirectCallOnly,
            move |data, caller| Self::new().call_with_provider(&reader, data, caller),
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
