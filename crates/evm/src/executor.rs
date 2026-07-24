//! Zone block executor.
//!
//! A simplified block executor for zone nodes that wraps [`EthBlockExecutor`] directly.
//! Unlike the Tempo L1 `TempoBlockExecutor`, this executor does **not** enforce subblock
//! ordering, shared-gas accounting, or the end-of-block subblock metadata system transaction.

use alloy_consensus::transaction::TxHashRef;
use alloy_evm::{
    block::{
        BlockExecutionError, BlockExecutionResult, BlockExecutor, BlockValidationError,
        ExecutableTx, GasOutput, TxResult,
    },
    eth::{EthBlockExecutor, EthTxResult},
    Database, Evm, RecoveredTx,
};
use reth_evm::block::StateDB;
use reth_revm::{context::result::ResultAndState, Inspector};
use tempo_evm::{TempoBlockExecutionCtx, TempoReceiptBuilder};
use tempo_primitives::{TempoReceipt, TempoTxEnvelope, TempoTxType};
use tempo_revm::evm::TempoContext;
use zone_chainspec::ZoneChainSpec;
use zone_l1::state::L1StateProvider;
use zone_precompiles::{tx_context, L1StorageReader, ADVANCE_TEMPO_SELECTOR};
use zone_primitives::constants::ZONE_INBOX_ADDRESS;

use crate::{L1OverlayDB, ZoneEvm};

/// Zone transaction result with metadata identifying the first system transaction of each block.
#[derive(Debug)]
pub struct ZoneTxResult<H, T> {
    inner: EthTxResult<H, T>,
    is_advance_tempo: bool,
}

impl<H, T> TxResult for ZoneTxResult<H, T>
where
    H: Send + 'static,
    T: Send + 'static,
{
    type HaltReason = H;

    fn result(&self) -> &ResultAndState<Self::HaltReason> {
        self.inner.result()
    }

    fn into_result(self) -> ResultAndState<Self::HaltReason> {
        self.inner.into_result()
    }
}

/// Simplified block executor for zone nodes.
///
/// Enforces the single successful block-opening `advanceTempo` system transaction, then delegates
/// ordinary execution to [`EthBlockExecutor`] without Tempo subblock validation, gas-section
/// tracking, or end-of-block metadata requirements.
pub struct ZoneBlockExecutor<'a, DB: Database, I, L1: L1StorageReader = L1StateProvider> {
    inner: EthBlockExecutor<'a, ZoneEvm<DB, I, L1>, &'a ZoneChainSpec, TempoReceiptBuilder>,
    has_advanced_tempo: bool,
}

impl<'a, DB, I, L1> ZoneBlockExecutor<'a, DB, I, L1>
where
    DB: StateDB,
    L1: L1StorageReader,
    I: Inspector<TempoContext<L1OverlayDB<DB, L1>>>,
{
    /// Create a zone block executor for `evm` and the current block context.
    pub fn new(
        evm: ZoneEvm<DB, I, L1>,
        ctx: TempoBlockExecutionCtx<'a>,
        chain_spec: &'a ZoneChainSpec,
    ) -> Self {
        Self {
            inner: EthBlockExecutor::new(
                evm,
                ctx.inner,
                chain_spec,
                TempoReceiptBuilder::default(),
            ),
            has_advanced_tempo: false,
        }
    }
}

impl<'a, DB, I, L1> BlockExecutor for ZoneBlockExecutor<'a, DB, I, L1>
where
    DB: StateDB,
    L1: L1StorageReader,
    I: Inspector<TempoContext<L1OverlayDB<DB, L1>>>,
{
    type Transaction = TempoTxEnvelope;
    type Receipt = TempoReceipt;
    type Evm = ZoneEvm<DB, I, L1>;
    type Result = ZoneTxResult<<Self::Evm as Evm>::HaltReason, TempoTxType>;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        self.inner.apply_pre_execution_changes()
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<Self::Result, BlockExecutionError> {
        let (mut tx_env, recovered) = tx.into_parts();
        // Remove any prewarming-specific context that was added to the tx env.
        if let Some(tempo_tx_env) = tx_env.tempo_tx_env.as_mut() {
            tempo_tx_env.expiring_nonce_idx = None;
        }

        // ensure `advance_tempo` system transaction is the first in the block.
        let is_advance_tempo = validate_advance_tempo(self.has_advanced_tempo, recovered.tx())?;

        let _tx_context_guard = tx_context::set_current_transaction(
            *recovered.tx().tx_hash(),
            tx_env.fee_payer().unwrap_or(tx_env.caller),
        );
        let result = self
            .inner
            .execute_transaction_without_commit((tx_env, recovered));

        self.evm_mut().clear_l1_overlay_state();
        Ok(ZoneTxResult {
            inner: result?,
            is_advance_tempo,
        })
    }

    fn commit_transaction(&mut self, output: Self::Result) -> GasOutput {
        self.has_advanced_tempo |= output.is_advance_tempo;
        self.inner.commit_transaction(output.inner)
    }

    fn finish(
        self,
    ) -> Result<(Self::Evm, BlockExecutionResult<Self::Receipt>), BlockExecutionError> {
        if !self.has_advanced_tempo {
            return Err(BlockValidationError::msg(
                "zone block is missing its advanceTempo system transaction",
            )
            .into());
        }
        self.inner.finish()
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        self.inner.evm_mut()
    }

    fn evm(&self) -> &Self::Evm {
        self.inner.evm()
    }

    fn receipts(&self) -> &[Self::Receipt] {
        self.inner.receipts()
    }
}

fn validate_advance_tempo(
    has_advanced_tempo: bool,
    tx: &TempoTxEnvelope,
) -> Result<bool, BlockExecutionError> {
    let is_advance_tempo = tx.is_system_tx()
        && tx.calls().any(|(kind, input)| {
            kind.to() == Some(&ZONE_INBOX_ADDRESS) && input.starts_with(&ADVANCE_TEMPO_SELECTOR)
        });

    match (has_advanced_tempo, is_advance_tempo) {
        (false, false) => Err(BlockValidationError::msg(
            "advanceTempo must be the first transaction in a zone block",
        )
        .into()),
        (true, true) => Err(BlockValidationError::msg(
            "advanceTempo must only execute once per zone block",
        )
        .into()),
        (false, true) | (true, false) => Ok(is_advance_tempo),
    }
}

#[cfg(test)]
mod tests {
    use super::{validate_advance_tempo, ADVANCE_TEMPO_SELECTOR, ZONE_INBOX_ADDRESS};

    use alloy_consensus::{Signed, TxLegacy};
    use alloy_primitives::{Address, Bytes, U256};
    use tempo_precompiles::{
        storage::{hashmap::HashMapStorageProvider, ContractStorage, Handler, StorageCtx},
        test_util::TIP20Setup,
        tip_fee_manager::{amm::PoolKey, TipFeeManager},
        DEFAULT_FEE_TOKEN, TIP_FEE_MANAGER_ADDRESS,
    };
    use tempo_primitives::{transaction::envelope::TEMPO_SYSTEM_TX_SIGNATURE, TempoTxEnvelope};
    use tempo_revm::{TempoBatchCallEnv, TempoTxEnv};

    fn system_tx(to: Address, input: Bytes) -> TempoTxEnvelope {
        TempoTxEnvelope::Legacy(Signed::new_unhashed(
            TxLegacy {
                to: to.into(),
                input,
                ..Default::default()
            },
            TEMPO_SYSTEM_TX_SIGNATURE,
        ))
    }

    #[test]
    fn discarded_advance_tempo_does_not_change_committed_ordering_state() {
        let advance_tempo = system_tx(
            ZONE_INBOX_ADDRESS,
            Bytes::copy_from_slice(&ADVANCE_TEMPO_SELECTOR),
        );
        let mut has_advanced_tempo = false;

        // Execution returns the marker but must not mutate committed state.
        assert!(validate_advance_tempo(has_advanced_tempo, &advance_tempo).unwrap());
        assert!(!has_advanced_tempo);

        // Only committing the result records the opening transaction.
        has_advanced_tempo |= validate_advance_tempo(has_advanced_tempo, &advance_tempo).unwrap();
        assert!(has_advanced_tempo);
    }

    #[test]
    fn advance_tempo_ordering_errors_are_reported() {
        let advance_tempo = system_tx(
            ZONE_INBOX_ADDRESS,
            Bytes::copy_from_slice(&ADVANCE_TEMPO_SELECTOR),
        );
        let ordinary = system_tx(Address::ZERO, Bytes::new());

        let missing_first = validate_advance_tempo(false, &ordinary).unwrap_err();
        assert_eq!(
            missing_first.to_string(),
            "advanceTempo must be the first transaction in a zone block"
        );

        let duplicate = validate_advance_tempo(true, &advance_tempo).unwrap_err();
        assert_eq!(
            duplicate.to_string(),
            "advanceTempo must only execute once per zone block"
        );
    }

    #[test]
    fn clears_only_prewarming_expiring_nonce_index() {
        let mut tx_env = TempoTxEnv {
            tempo_tx_env: Some(Box::new(TempoBatchCallEnv {
                valid_before: Some(123),
                expiring_nonce_idx: Some(3),
                ..Default::default()
            })),
            ..Default::default()
        };

        if let Some(tempo_tx_env) = tx_env.tempo_tx_env.as_mut() {
            tempo_tx_env.expiring_nonce_idx = None;
        }

        let tempo_tx_env = tx_env.tempo_tx_env.unwrap();
        assert_eq!(tempo_tx_env.expiring_nonce_idx, None);
        assert_eq!(tempo_tx_env.valid_before, Some(123));
    }

    /// Simulates the zone executor's per-tx validator token override and runs
    /// the full fee lifecycle across multiple TIP-20 tokens, verifying:
    ///
    /// 1. Default validator token is PATH_USD (no explicit preference set).
    /// 2. No FeeAMM liquidity exists for any token pair.
    /// 3. Paying fees in betaUSD, gammaUSD, and pathUSD all succeed when the
    ///    validator token is overridden per-tx.
    /// 4. Fees are credited in the user's token (no conversion).
    /// 5. FeeAMM pool reserves remain zero throughout.
    #[test]
    fn multi_token_fees_with_validator_override() -> eyre::Result<()> {
        let mut storage = HashMapStorageProvider::new(1);
        let admin = Address::random();
        let user = Address::random();
        let sequencer = Address::random();

        StorageCtx::enter(&mut storage, || {
            // Deploy three tokens.
            let path_usd = TIP20Setup::create("PathUSD", "pUSD", admin)
                .with_issuer(admin)
                .with_mint(user, U256::from(10_000_000u64))
                .with_approval(user, TIP_FEE_MANAGER_ADDRESS, U256::MAX)
                .apply()?;
            let beta_usd = TIP20Setup::create("BetaUSD", "bUSD", admin)
                .with_issuer(admin)
                .with_mint(user, U256::from(10_000_000u64))
                .with_approval(user, TIP_FEE_MANAGER_ADDRESS, U256::MAX)
                .apply()?;
            let gamma_usd = TIP20Setup::create("GammaUSD", "gUSD", admin)
                .with_issuer(admin)
                .with_mint(user, U256::from(10_000_000u64))
                .with_approval(user, TIP_FEE_MANAGER_ADDRESS, U256::MAX)
                .apply()?;

            let fee_manager = TipFeeManager::new();

            // 1. Validator token defaults to PATH_USD.
            assert_eq!(
                fee_manager.get_validator_token(sequencer)?,
                DEFAULT_FEE_TOKEN
            );

            // 2. No FeeAMM pools exist.
            for (a, b) in [
                (beta_usd.address(), DEFAULT_FEE_TOKEN),
                (gamma_usd.address(), DEFAULT_FEE_TOKEN),
                (beta_usd.address(), gamma_usd.address()),
            ] {
                let pool = fee_manager.pools[PoolKey::new(a, b).get_id()].read()?;
                assert_eq!(pool.reserve_user_token, 0);
                assert_eq!(pool.reserve_validator_token, 0);
            }

            // 3. Three transactions, each paying in a different token.
            let txs = [
                (
                    beta_usd.address(),
                    U256::from(5_000u64),
                    U256::from(3_000u64),
                ),
                (
                    gamma_usd.address(),
                    U256::from(8_000u64),
                    U256::from(7_000u64),
                ),
                (
                    path_usd.address(),
                    U256::from(4_000u64),
                    U256::from(2_000u64),
                ),
            ];

            let mut fee_manager = TipFeeManager::new();
            for (token, max, used) in &txs {
                // Zone executor override: validatorTokens[sequencer] = fee_token.
                fee_manager.validator_tokens[sequencer].write(*token)?;

                fee_manager.collect_fee_pre_tx(user, *token, *max, sequencer, false)?;
                fee_manager.collect_fee_post_tx(user, *used, *max - *used, *token, sequencer)?;
            }

            // 4. Fees credited per-token — no conversion happened.
            for (token, _, used) in &txs {
                let collected = fee_manager.collected_fees[sequencer][*token].read()?;
                assert_eq!(collected, *used, "fees should be credited in {token}");
            }

            // 5. FeeAMM pools still empty — never touched.
            for (a, b) in [
                (beta_usd.address(), DEFAULT_FEE_TOKEN),
                (gamma_usd.address(), DEFAULT_FEE_TOKEN),
                (beta_usd.address(), gamma_usd.address()),
            ] {
                let pool = fee_manager.pools[PoolKey::new(a, b).get_id()].read()?;
                assert_eq!(
                    pool.reserve_user_token, 0,
                    "pool {a}-{b} user reserve should be 0"
                );
                assert_eq!(
                    pool.reserve_validator_token, 0,
                    "pool {a}-{b} validator reserve should be 0"
                );
            }

            Ok(())
        })
    }

    /// Validator token slot computation is deterministic and the storage
    /// write produces the expected value when read back via TipFeeManager.
    #[test]
    fn validator_token_slot_roundtrip() -> eyre::Result<()> {
        let mut storage = HashMapStorageProvider::new(1);
        let sequencer = Address::random();
        let token = Address::random();

        StorageCtx::enter(&mut storage, || {
            let mut fee_manager = TipFeeManager::new();

            // Write via the Mapping handler (what the executor does via journal sstore).
            fee_manager.validator_tokens[sequencer].write(token)?;

            // Read back via TipFeeManager API.
            let read_back = fee_manager.get_validator_token(sequencer)?;
            assert_eq!(read_back, token);

            Ok(())
        })
    }
}
