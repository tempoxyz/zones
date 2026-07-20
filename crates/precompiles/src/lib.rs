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
pub use execution::ZonePrecompileEnv;
pub mod storage;
pub mod tempo_state;
pub mod tip20_factory;
pub mod tip403_proxy;
pub mod ztip20;

pub use aes_gcm::{AES_GCM_DECRYPT_ADDRESS, AesGcmDecrypt};
pub use chaum_pedersen::{CHAUM_PEDERSEN_VERIFY_ADDRESS, ChaumPedersenVerify};
pub use storage::{L1State, L1StateError, L1StorageReader};
pub use tempo_contracts::precompiles::TIP403_REGISTRY_ADDRESS;
pub use tempo_state::TempoState;
pub use tip20_factory::{ZONE_TIP20_FACTORY_ADDRESS, ZoneTokenFactory};
pub use ztip20::SequencerExt;

use alloc::{rc::Rc, sync::Arc};
use core::cell::RefCell;

use alloy_evm::precompiles::{DynPrecompile, PrecompilesMap};
use revm::context::CfgEnv;
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

/// Registers Zone-native and supported Tempo precompiles.
///
/// Every Zone wrapper receives the same [`ZonePrecompileEnv`]. Storage remains ordinary EVM
/// storage: the Zone database adapter transparently resolves mirrored TIP-20/TIP-403 reads at the
/// active Tempo anchor, while all other slots remain Zone-local. `TempoState` keeps its explicit L1
/// reader for the system-only arbitrary-storage read ABI.
pub fn extend_zone_precompiles<L1: L1StorageReader>(
    precompiles: &mut PrecompilesMap,
    cfg: &CfgEnv<TempoHardfork>,
    l1: L1State<L1>,
    sequencer: Arc<dyn SequencerExt>,
    actions: StorageActions,
    non_creditable_slots: Rc<RefCell<NonCreditableSlots>>,
) {
    let env = ZonePrecompileEnv::new(cfg, actions.clone(), non_creditable_slots.clone());
    let tempo_env = PrecompileEnv::new(cfg, actions, non_creditable_slots);

    precompiles.apply_precompile(&TEMPO_STATE_ADDRESS, |_| {
        Some(TempoState::create(l1.clone(), &env))
    });
    precompiles.apply_precompile(&CHAUM_PEDERSEN_VERIFY_ADDRESS, |_| {
        Some(ChaumPedersenVerify::create(&env))
    });
    precompiles.apply_precompile(&AES_GCM_DECRYPT_ADDRESS, |_| {
        Some(AesGcmDecrypt::create(&env))
    });
    precompiles.apply_precompile(&ZONE_TIP20_FACTORY_ADDRESS, |_| {
        Some(ZoneTokenFactory::create(&env))
    });

    let tip403_env = env.clone();
    precompiles.apply_precompile(&TIP403_REGISTRY_ADDRESS, move |_| {
        Some(create_tip403_precompile(&tip403_env))
    });

    // Static zone entries above take priority. Dynamic TIP-20 entries use upstream execution with
    // zone privacy, bridge-authorization, fixed-gas, and anchored policy-storage rules.
    precompiles.set_precompile_lookup(move |address: &alloy_primitives::Address| {
        if is_tip20_prefix(*address) {
            Some(create_tip20_precompile(*address, &env, sequencer.clone()))
        } else if *address == TIP_FEE_MANAGER_ADDRESS {
            Some(execution::create_precompile(
                "TipFeeManager",
                &env,
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
    /// Creates the AES-GCM precompile with the shared zone execution environment.
    pub fn create(env: &ZonePrecompileEnv) -> DynPrecompile {
        execution::create_precompile(
            "AesGcmDecrypt",
            env,
            execution::NoCallRules,
            |data, caller| Self.call(data, caller),
        )
    }
}

impl ChaumPedersenVerify {
    /// Creates the Chaum-Pedersen precompile with the shared zone execution environment.
    pub fn create(env: &ZonePrecompileEnv) -> DynPrecompile {
        execution::create_precompile(
            "ChaumPedersenVerify",
            env,
            execution::NoCallRules,
            |data, caller| Self.call(data, caller),
        )
    }
}

impl TempoState {
    /// Creates the direct-call-only `TempoState` precompile with checkpoint storage.
    ///
    /// System-only arbitrary L1 storage reads are delegated through `l1` at the stored checkpoint.
    pub fn create<P: L1StorageReader>(l1: L1State<P>, env: &ZonePrecompileEnv) -> DynPrecompile {
        execution::create_precompile(
            "TempoState",
            env,
            execution::NoCallRules,
            move |data, caller| Self::new().call_with_l1_state(&l1, data, caller),
        )
    }
}

impl ZoneTokenFactory {
    /// Creates the direct-call-only token factory with zone-local storage and execution.
    pub fn create(env: &ZonePrecompileEnv) -> DynPrecompile {
        execution::create_precompile(
            "ZoneTokenFactory",
            env,
            execution::NoCallRules,
            |data, caller| Self::new().call(data, caller),
        )
    }
}

/// Creates upstream TIP-403 execution with zone read-only rules and adapter-backed L1 reads.
pub(crate) fn create_tip403_precompile(env: &ZonePrecompileEnv) -> DynPrecompile {
    execution::create_precompile(
        "ZoneTip403Registry",
        env,
        tip403_proxy::Tip403Rules,
        |data, caller| TIP403Registry::new().call(data, caller),
    )
}

/// Creates upstream TIP-20 execution with zone rules and adapter-backed L1 policy reads.
pub(crate) fn create_tip20_precompile(
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
