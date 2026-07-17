//! Zone runtime EVM and its private execution policies.

pub(crate) mod contract_creation;

use crate::{
    TempoCtx,
    database::{AnchoredZoneDb, AnchoredZoneDbError},
};
use alloy_evm::{Database, Evm, EvmEnv, precompiles::PrecompilesMap, revm::Inspector};
use alloy_primitives::{Address, Bytes};
use revm::context::{
    DBErrorMarker,
    result::{EVMError, ResultAndState},
};
use tempo_evm::{TempoBlockEnv, TempoHaltReason, evm::TempoEvm};
use tempo_revm::{TempoInvalidTransaction, TempoTxEnv};
use zone_l1::state::L1StateProvider;
use zone_precompiles::{L1AnchorController, L1StorageReader};
use zone_primitives::constants::CONTRACT_DEPLOYER_ALLOWLIST;

/// Zone runtime EVM.
///
/// Execution uses an anchored database adapter internally while the public [`Evm::DB`] remains the
/// exact database supplied by the caller.
pub struct ZoneEvm<DB: Database, I, L1: L1StorageReader = L1StateProvider> {
    inner: TempoEvm<AnchoredZoneDb<DB, L1>, I>,
    controller: L1AnchorController,
}

impl<DB: Database, I, L1: L1StorageReader> ZoneEvm<DB, I, L1> {
    /// Creates a new `ZoneEvm` with guarded `CREATE` and `CREATE2` opcodes.
    pub(super) fn new(
        mut evm: TempoEvm<AnchoredZoneDb<DB, L1>, I>,
        controller: L1AnchorController,
    ) -> Self {
        contract_creation::configure_runtime(&mut evm);
        Self {
            inner: evm,
            controller,
        }
    }

    /// Provides a reference to the EVM context.
    pub fn ctx(&self) -> &TempoCtx<AnchoredZoneDb<DB, L1>> {
        self.inner.ctx()
    }

    /// Provides a mutable reference to the EVM context.
    pub fn ctx_mut(&mut self) -> &mut TempoCtx<AnchoredZoneDb<DB, L1>> {
        self.inner.ctx_mut()
    }

    /// Returns the execution-local anchor controller.
    pub const fn anchor_controller(&self) -> &L1AnchorController {
        &self.controller
    }
}

impl<DB, I, L1> Evm for ZoneEvm<DB, I, L1>
where
    DB: Database,
    L1: L1StorageReader,
    I: Inspector<TempoCtx<AnchoredZoneDb<DB, L1>>>,
{
    type DB = DB;
    type Tx = TempoTxEnv;
    type Error = EVMError<DB::Error, TempoInvalidTransaction>;
    type HaltReason = TempoHaltReason;
    type Spec = tempo_chainspec::hardfork::TempoHardfork;
    type BlockEnv = TempoBlockEnv;
    type Precompiles = PrecompilesMap;
    type Inspector = I;

    fn block(&self) -> &Self::BlockEnv {
        self.inner.block()
    }

    fn cfg_env(&self) -> &revm::context::CfgEnv<Self::Spec> {
        self.inner.cfg_env()
    }

    fn chain_id(&self) -> u64 {
        self.inner.chain_id()
    }

    fn transact_raw(
        &mut self,
        tx: Self::Tx,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        contract_creation::validate_transaction(&tx, CONTRACT_DEPLOYER_ALLOWLIST)?;
        let snapshot = self.controller.phase();
        let mut result = match self.inner.transact_raw(tx) {
            Ok(result) => result,
            Err(err) => {
                self.controller.restore(snapshot);
                return Err(map_adapter_error(err));
            }
        };
        if !result.result.is_success() {
            self.controller.restore(snapshot);
            return Ok(result);
        }
        if let Err(err) = self.inner.db().sanitize_state(&mut result.state) {
            self.controller.restore(snapshot);
            return Err(err.into_evm_error());
        }
        Ok(result)
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        let snapshot = self.controller.phase();
        let mut result = match self.inner.transact_system_call(caller, contract, data) {
            Ok(result) => result,
            Err(err) => {
                self.controller.restore(snapshot);
                return Err(map_adapter_error(err));
            }
        };
        if !result.result.is_success() {
            self.controller.restore(snapshot);
            return Ok(result);
        }
        if let Err(err) = self.inner.db().sanitize_state(&mut result.state) {
            self.controller.restore(snapshot);
            return Err(err.into_evm_error());
        }
        Ok(result)
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec, Self::BlockEnv>) {
        let (db, env) = self.inner.finish();
        (db.into_inner(), env)
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inner.set_inspector_enabled(enabled);
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        let (db, inspector, precompiles) = self.inner.components();
        (db.inner(), inspector, precompiles)
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        let (db, inspector, precompiles) = self.inner.components_mut();
        (db.inner_mut(), inspector, precompiles)
    }
}

fn map_adapter_error<E: core::error::Error + DBErrorMarker>(
    error: EVMError<AnchoredZoneDbError<E>, TempoInvalidTransaction>,
) -> EVMError<E, TempoInvalidTransaction> {
    match error {
        EVMError::Transaction(error) => EVMError::Transaction(error),
        EVMError::Header(error) => EVMError::Header(error),
        EVMError::Database(error) => error.into_evm_error(),
        EVMError::Custom(error) => EVMError::Custom(error),
        EVMError::CustomAny(error) => EVMError::CustomAny(error),
    }
}
