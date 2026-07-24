//! Shared L1 reader fixtures for precompile and EVM integration tests.
use crate::{L1StateError, L1StorageReader};
use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_sol_types::SolValue;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tempo_precompiles::{
    storage::{Handler, PrecompileStorageProvider, StorageCtx, hashmap::HashMapStorageProvider},
    tip403_registry::{CompoundPolicyData, PolicyData, TIP403Registry},
};

pub type L1Slot = (Address, B256, u64);
type Shared<T> = Arc<Mutex<T>>;

/// In-memory exact-block L1 reader shared by EVM and precompile tests.
#[derive(Clone)]
pub struct MockL1Reader {
    slots: Shared<HashMap<L1Slot, B256>>,
    registry_storage: Shared<HashMapStorageProvider>,
    storage_requests: Shared<Vec<L1Slot>>,
    storage_state_roots: Shared<Vec<Option<B256>>>,
    fallback: B256,
    fail_storage: bool,
}

impl Default for MockL1Reader {
    fn default() -> Self {
        Self {
            slots: Default::default(),
            registry_storage: Arc::new(Mutex::new(HashMapStorageProvider::new(1))),
            storage_requests: Default::default(),
            storage_state_roots: Default::default(),
            fallback: B256::ZERO,
            fail_storage: false,
        }
    }
}

impl MockL1Reader {
    pub fn insert(&self, address: Address, slot: U256, anchor: u64, value: U256) {
        self.set_u256(address, slot, anchor, value);
    }

    pub fn returning(value: B256) -> Self {
        Self {
            fallback: value,
            ..Default::default()
        }
    }

    pub fn failing_storage() -> Self {
        Self {
            fail_storage: true,
            ..Default::default()
        }
    }

    pub fn set_u256(&self, address: Address, slot: U256, block: u64, value: U256) {
        self.slots.lock().unwrap().insert(
            (address, B256::from(slot.to_be_bytes()), block),
            B256::from(value.to_be_bytes()),
        );
    }

    pub fn storage_requests(&self) -> Vec<L1Slot> {
        self.storage_requests.lock().unwrap().clone()
    }

    pub fn storage_state_roots(&self) -> Vec<Option<B256>> {
        self.storage_state_roots.lock().unwrap().clone()
    }

    pub fn seed_active_sequencer(
        &self,
        portal_address: Address,
        block_number: u64,
        account: Address,
    ) {
        let slot = keccak256(
            (
                account,
                zone_primitives::constants::PORTAL_IS_SEQUENCER_SLOT,
            )
                .abi_encode(),
        );
        self.set_u256(
            portal_address,
            U256::from_be_bytes(slot.0),
            block_number,
            U256::ONE,
        );
    }
    pub fn seed_simple_policy(
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

    pub fn seed_blacklist_policy(
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

    pub fn seed_compound_policy(
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
        state_root: Option<B256>,
    ) -> Result<B256, L1StateError> {
        self.storage_requests
            .lock()
            .unwrap()
            .push((account, slot, block_number));
        self.storage_state_roots.lock().unwrap().push(state_root);
        if self.fail_storage {
            return Err(L1StateError::StorageUnavailable {
                account,
                slot,
                block_number,
                reason: "RPC unavailable".into(),
            });
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
            .map_err(|err| L1StateError::StorageUnavailable {
                account,
                slot,
                block_number,
                reason: err.to_string(),
            })?;
        if value.is_zero() {
            Ok(self.fallback)
        } else {
            Ok(B256::from(value.to_be_bytes()))
        }
    }
}
