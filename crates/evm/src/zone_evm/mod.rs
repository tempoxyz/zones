//! Zone runtime EVM and its private execution policies.

pub(crate) mod contract_creation;

use crate::TempoCtx;
use alloy_evm::{Database, Evm, EvmEnv, precompiles::PrecompilesMap, revm::Inspector};
use alloy_primitives::{Address, Bytes};
use revm::context::result::{EVMError, ResultAndState};
use tempo_evm::{TempoBlockEnv, TempoHaltReason, evm::TempoEvm};
use tempo_revm::{TempoInvalidTransaction, TempoTxEnv};
use zone_primitives::constants::CONTRACT_DEPLOYER_ALLOWLIST;

/// Zone runtime EVM.
///
/// Wraps Tempo (L1) EVM to enforce Zone-specific execution rules.
pub struct ZoneEvm<DB: Database, I> {
    inner: TempoEvm<DB, I>,
}

impl<DB: Database, I> ZoneEvm<DB, I> {
    /// Creates a new `ZoneEvm` with guarded `CREATE` and `CREATE2` opcodes.
    pub(super) fn new(mut evm: TempoEvm<DB, I>) -> Self {
        contract_creation::configure_runtime(&mut evm);
        Self { inner: evm }
    }

    /// Provides a reference to the EVM context.
    pub fn ctx(&self) -> &TempoCtx<DB> {
        self.inner.ctx()
    }

    /// Provides a mutable reference to the EVM context.
    pub fn ctx_mut(&mut self) -> &mut TempoCtx<DB> {
        self.inner.ctx_mut()
    }
}

impl<DB, I> Evm for ZoneEvm<DB, I>
where
    DB: Database,
    I: Inspector<TempoCtx<DB>>,
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
        self.inner.transact_raw(tx)
    }

    fn transact_system_call(
        &mut self,
        caller: Address,
        contract: Address,
        data: Bytes,
    ) -> Result<ResultAndState<Self::HaltReason>, Self::Error> {
        self.inner.transact_system_call(caller, contract, data)
    }

    fn finish(self) -> (Self::DB, EvmEnv<Self::Spec, Self::BlockEnv>) {
        self.inner.finish()
    }

    fn set_inspector_enabled(&mut self, enabled: bool) {
        self.inner.set_inspector_enabled(enabled);
    }

    fn components(&self) -> (&Self::DB, &Self::Inspector, &Self::Precompiles) {
        self.inner.components()
    }

    fn components_mut(&mut self) -> (&mut Self::DB, &mut Self::Inspector, &mut Self::Precompiles) {
        self.inner.components_mut()
    }
}
