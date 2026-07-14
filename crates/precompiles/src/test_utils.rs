//! Shared test utilities for precompile tests.

use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::{Arc, Mutex},
};

use alloy_evm::{
    EvmInternals,
    precompiles::{DynPrecompile, Precompile as _, PrecompileInput},
};
use alloy_primitives::{Address, B256, U256};
use k256::{
    AffinePoint, ProjectivePoint, Scalar,
    elliptic_curve::{ops::Reduce, sec1::ToEncodedPoint},
};
use revm::{
    Context,
    context::{BlockEnv, CfgEnv, TxEnv},
    database::{CacheDB, EmptyDB},
    precompile::{PrecompileError, PrecompileResult},
};
use tempo_chainspec::hardfork::TempoHardfork;
use tempo_precompiles::{
    storage::{
        Handler, PrecompileStorageProvider, StorageCtx, actions::StorageActions,
        evm::EvmPrecompileStorageProvider, hashmap::HashMapStorageProvider,
    },
    storage_credits::NonCreditableSlots,
    tip403_registry::{CompoundPolicyData, PolicyData, TIP403Registry},
};

use crate::{
    L1StorageReader,
    chaum_pedersen::{challenge_hash, recover_point},
    ecies::DecryptedDeposit,
    execution::L1BackedPrecompileEnv,
};

pub(crate) use crate::ecies::{build_plaintext, compressed_x_and_parity, encrypt_plaintext};

/// EVM context used by precompile tests.
pub(crate) type TestContext = Context<BlockEnv, TxEnv, CfgEnv<TempoHardfork>, CacheDB<EmptyDB>>;
type L1Slot = (Address, B256, u64);
type Shared<T> = Arc<Mutex<T>>;

/// Create an empty test EVM context at the default Tempo hardfork.
pub(crate) fn test_context() -> TestContext {
    Context::new(CacheDB::new(EmptyDB::new()), TempoHardfork::default())
}

/// Create an EVM-backed precompile storage provider over `ctx`.
pub(crate) fn test_storage_provider(
    ctx: &mut TestContext,
    gas_limit: u64,
    is_static: bool,
) -> EvmPrecompileStorageProvider<'_> {
    let cfg = ctx.cfg.clone();
    EvmPrecompileStorageProvider::new(
        EvmInternals::from_context(ctx),
        gas_limit,
        0,
        cfg.spec,
        cfg.enable_amsterdam_eip8037,
        is_static,
        cfg.gas_params,
    )
}

/// Create the shared finalized-L1 execution environment for a precompile test.
pub(crate) fn test_l1_env<P: L1StorageReader>(
    ctx: &TestContext,
    l1_reader: P,
) -> L1BackedPrecompileEnv<P> {
    L1BackedPrecompileEnv::new(
        &ctx.cfg,
        l1_reader,
        StorageActions::disabled(),
        Rc::new(RefCell::new(NonCreditableSlots::empty())),
    )
}

/// Call a dynamic precompile with test defaults for value and reservoir.
pub(crate) fn call_precompile(
    ctx: &mut TestContext,
    precompile: &DynPrecompile,
    caller: Address,
    data: &[u8],
    gas: u64,
    is_static: bool,
    target: Address,
    bytecode_address: Address,
) -> PrecompileResult {
    precompile.call(PrecompileInput {
        data,
        gas,
        reservoir: 0,
        caller,
        value: U256::ZERO,
        target_address: target,
        is_static,
        bytecode_address,
        internals: EvmInternals::from_context(ctx),
    })
}

// TODO(rusowsky): Remove once Tempo L1 stores transfer policy IDs in the TIP403 precompile.
fn pack_transfer_policy_id(policy_id: u64) -> U256 {
    U256::from(policy_id) << (tempo_precompiles::tip20::tip20_slots::TRANSFER_POLICY_ID_OFFSET * 8)
}

/// In-memory exact-block L1 reader shared by precompile tests.
#[derive(Clone)]
pub(crate) struct MockL1Reader {
    slots: Shared<HashMap<L1Slot, B256>>,
    registry_storage: Shared<HashMapStorageProvider>,
    storage_requests: Shared<Vec<L1Slot>>,
    hardfork_requests: Shared<Vec<u64>>,
    fallback: B256,
    policy_id: u64,
    fail_storage: bool,
    fail_hardfork: bool,
}

impl Default for MockL1Reader {
    fn default() -> Self {
        Self {
            slots: Default::default(),
            registry_storage: Arc::new(Mutex::new(HashMapStorageProvider::new(1))),
            storage_requests: Default::default(),
            hardfork_requests: Default::default(),
            fallback: B256::ZERO,
            policy_id: 0,
            fail_storage: false,
            fail_hardfork: false,
        }
    }
}

impl MockL1Reader {
    pub(crate) fn allow_all() -> Self {
        Self::with_policy_id(1)
    }

    pub(crate) fn failing() -> Self {
        Self {
            fail_storage: true,
            ..Self::allow_all()
        }
    }

    pub(crate) fn with_policy_id(policy_id: u64) -> Self {
        Self {
            policy_id,
            ..Default::default()
        }
    }

    pub(crate) fn returning(value: B256) -> Self {
        Self {
            fallback: value,
            ..Default::default()
        }
    }

    pub(crate) fn failing_storage() -> Self {
        Self {
            fail_storage: true,
            ..Default::default()
        }
    }

    pub(crate) fn failing_hardfork() -> Self {
        Self {
            fail_hardfork: true,
            ..Default::default()
        }
    }

    pub(crate) fn set_u256(&self, address: Address, slot: U256, block: u64, value: U256) {
        self.slots.lock().unwrap().insert(
            (address, B256::from(slot.to_be_bytes()), block),
            B256::from(value.to_be_bytes()),
        );
    }

    pub(crate) fn storage_requests(&self) -> Vec<L1Slot> {
        self.storage_requests.lock().unwrap().clone()
    }

    pub(crate) fn hardfork_requests(&self) -> Vec<u64> {
        self.hardfork_requests.lock().unwrap().clone()
    }

    pub(crate) fn seed_transfer_policy_id(&self, token: Address, block_number: u64) {
        let packed = pack_transfer_policy_id(self.policy_id);
        self.set_u256(
            token,
            tempo_precompiles::tip20::tip20_slots::TRANSFER_POLICY_ID,
            block_number,
            packed,
        );
    }

    pub(crate) fn seed_simple_policy(
        &self,
        policy_id: u64,
        policy_type: tempo_contracts::precompiles::ITIP403Registry::PolicyType,
        accounts: &[Address],
    ) -> tempo_precompiles::Result<()> {
        let mut storage = self.registry_storage.lock().unwrap();
        StorageCtx::enter(&mut *storage, || {
            let mut registry = TIP403Registry::new();
            let next_policy_id = registry.policy_id_counter()?.max(policy_id + 1);
            registry.policy_id_counter.write(next_policy_id)?;
            registry.policy_records[policy_id].base.write(PolicyData {
                policy_type: policy_type as u8,
                admin: Address::ZERO,
            })?;
            for account in accounts {
                registry.policy_set[policy_id][*account].write(true)?;
            }
            Ok(())
        })
    }

    pub(crate) fn seed_blacklist_policy(
        &self,
        policy_id: u64,
        accounts: &[Address],
    ) -> tempo_precompiles::Result<()> {
        self.seed_simple_policy(
            policy_id,
            tempo_contracts::precompiles::ITIP403Registry::PolicyType::BLACKLIST,
            accounts,
        )
    }

    pub(crate) fn seed_compound_policy(
        &self,
        policy_id: u64,
        sender_policy_id: u64,
        recipient_policy_id: u64,
        mint_recipient_policy_id: u64,
    ) -> tempo_precompiles::Result<()> {
        let mut storage = self.registry_storage.lock().unwrap();
        StorageCtx::enter(&mut *storage, || {
            let mut registry = TIP403Registry::new();
            let next_policy_id = registry.policy_id_counter()?.max(policy_id + 1);
            registry.policy_id_counter.write(next_policy_id)?;
            registry.policy_records[policy_id].base.write(PolicyData {
                policy_type: tempo_contracts::precompiles::ITIP403Registry::PolicyType::COMPOUND
                    as u8,
                admin: Address::ZERO,
            })?;
            registry.policy_records[policy_id]
                .compound
                .write(CompoundPolicyData {
                    sender_policy_id,
                    recipient_policy_id,
                    mint_recipient_policy_id,
                })?;
            Ok(())
        })
    }
}

impl L1StorageReader for MockL1Reader {
    fn read_l1_storage(
        &self,
        account: Address,
        slot: B256,
        block_number: u64,
    ) -> Result<B256, PrecompileError> {
        self.storage_requests
            .lock()
            .unwrap()
            .push((account, slot, block_number));
        if self.fail_storage {
            return Err(crate::zone_rpc_error("RPC unavailable"));
        }
        if let Some(value) = self
            .slots
            .lock()
            .unwrap()
            .get(&(account, slot, block_number))
            .copied()
        {
            return Ok(value);
        }

        let key = U256::from_be_bytes(slot.0);
        let value = self
            .registry_storage
            .lock()
            .unwrap()
            .sload(account, key)
            .map_err(|err| PrecompileError::Fatal(err.to_string()))?;
        if value.is_zero() {
            Ok(self.fallback)
        } else {
            Ok(B256::from(value.to_be_bytes()))
        }
    }

    fn hardfork_at(&self, block_number: u64) -> Result<TempoHardfork, PrecompileError> {
        self.hardfork_requests.lock().unwrap().push(block_number);
        if self.fail_hardfork {
            Err(PrecompileError::Fatal("hardfork unavailable".into()))
        } else {
            Ok(TempoHardfork::T8)
        }
    }
}

/// Assert that the Chaum-Pedersen proof inside a [`DecryptedDeposit`] is valid.
pub(crate) fn assert_cp_proof_valid(
    dec: &DecryptedDeposit,
    ephemeral_pub: &AffinePoint,
    sequencer_pub: &AffinePoint,
) {
    let s = <Scalar as Reduce<k256::U256>>::reduce_bytes(&dec.proof.cp_proof_s.0.into());
    let c = <Scalar as Reduce<k256::U256>>::reduce_bytes(&dec.proof.cp_proof_c.0.into());
    let shared_pt =
        recover_point(&dec.proof.shared_secret.0, dec.proof.shared_secret_y_parity).unwrap();

    let r1 = ProjectivePoint::GENERATOR * s - ProjectivePoint::from(*sequencer_pub) * c;
    let r2 = ProjectivePoint::from(*ephemeral_pub) * s - ProjectivePoint::from(shared_pt) * c;

    let c_prime = challenge_hash(
        ephemeral_pub,
        sequencer_pub,
        &shared_pt,
        &r1.to_affine(),
        &r2.to_affine(),
    );
    assert_eq!(c, c_prime, "Chaum-Pedersen proof must verify");
}

/// Pre-computed encrypted deposit for testing.
/// All fields are deterministic (derived from fixed seed keys).
pub(crate) struct EncryptedDepositFixture {
    pub seq_key: k256::SecretKey,
    pub seq_pub: AffinePoint,
    pub eph_pub: AffinePoint,
    pub eph_pub_x: B256,
    pub eph_pub_y_parity: u8,
    pub portal: Address,
    pub key_index: U256,
    pub to: Address,
    pub memo: B256,
    pub ciphertext: Vec<u8>,
    pub nonce: [u8; 12],
    pub tag: [u8; 16],
}

impl EncryptedDepositFixture {
    /// Create a fixture with deterministic keys for reproducible tests.
    pub(crate) fn new() -> Self {
        use sha2::{Digest, Sha256};

        // Deterministic sequencer key
        let seq_bytes: [u8; 32] = Sha256::digest(b"test-sequencer-key").into();
        let seq_key = k256::SecretKey::from_slice(&seq_bytes).expect("valid key");
        let seq_scalar: Scalar = *seq_key.to_nonzero_scalar();
        let seq_pub = AffinePoint::from(ProjectivePoint::GENERATOR * seq_scalar);

        // Deterministic ephemeral key
        let eph_bytes: [u8; 32] = Sha256::digest(b"test-ephemeral-key").into();
        let eph_key = k256::SecretKey::from_slice(&eph_bytes).expect("valid key");
        let eph_scalar: Scalar = *eph_key.to_nonzero_scalar();
        let eph_pub = AffinePoint::from(ProjectivePoint::GENERATOR * eph_scalar);
        let (eph_pub_x, eph_pub_y_parity) = compressed_x_and_parity(&eph_pub);

        // ECDH (depositor side)
        let shared_proj = ProjectivePoint::from(seq_pub) * eph_scalar;
        let shared_affine = AffinePoint::from(shared_proj);
        let ss_enc = shared_affine.to_encoded_point(true);
        let shared_secret_x: [u8; 32] = ss_enc.x().unwrap().as_slice().try_into().unwrap();

        let portal = Address::repeat_byte(0xAA);
        let key_index = U256::from(42u64);

        // HKDF key derivation
        let info = crate::ecies::hkdf_info(&portal, &key_index, &eph_pub_x);
        let aes_key = crate::ecies::hkdf_sha256(&shared_secret_x, b"ecies-aes-key", &info);

        // Build and encrypt plaintext
        let to = Address::repeat_byte(0xBB);
        let memo = B256::repeat_byte(0xCC);
        let plaintext = build_plaintext(&to, &memo);
        let (ciphertext, nonce, tag) = encrypt_plaintext(&aes_key, &plaintext);

        Self {
            seq_key,
            seq_pub,
            eph_pub,
            eph_pub_x,
            eph_pub_y_parity,
            portal,
            key_index,
            to,
            memo,
            ciphertext,
            nonce,
            tag,
        }
    }

    /// Decrypt using the fixture's sequencer key.
    pub(crate) fn decrypt(&self) -> Option<DecryptedDeposit> {
        crate::ecies::decrypt_deposit(
            &self.seq_key,
            &self.eph_pub_x,
            self.eph_pub_y_parity,
            &self.ciphertext,
            &self.nonce,
            &self.tag,
            self.portal,
            self.key_index,
        )
    }
}
