//! Zone transaction pool construction and admission policy.

use crate::node::ZoneNode;
use alloy_primitives::{Address, TxKind};
use alloy_sol_types::SolCall;
use reth_chainspec::EthChainSpec;
use reth_node_api::FullNodeTypes;
use reth_node_builder::{
    BuilderContext,
    components::{PoolBuilder, spawn_maintenance_tasks},
};
use reth_storage_api::{StateProvider, StateProviderFactory};
use reth_transaction_pool::{
    Pool, PoolTransaction, TransactionOrigin, TransactionValidationTaskExecutor,
    blobstore::InMemoryBlobStore, error::InvalidPoolTransactionError,
};
use std::sync::Arc;
use tempo_contracts::precompiles::ITIP20;
use tempo_evm::TempoInvalidTransaction;
use tempo_node::DEFAULT_AA_VALID_AFTER_MAX_SECS;
use tempo_precompiles::tip20::TIP20Token;
use tempo_primitives::{TempoTxType, is_tip20_prefix};
use tempo_transaction_pool::{
    AA2dPool, AA2dPoolConfig, TempoTransactionPool,
    amm::AmmLiquidityCache,
    ordering::TempoTipOrdering,
    transaction::{TempoPoolTransactionError, TempoPooledTransaction},
    validator::{DEFAULT_MAX_TEMPO_AUTHORIZATIONS, TempoTransactionValidator},
};
use tempo_zone_contracts::{IZoneInbox, IZoneOutbox, ZONE_INBOX_ADDRESS, ZONE_OUTBOX_ADDRESS};
use tracing::{debug, info, warn};
use zone_evm::ZoneEvmConfig;
use zone_l1::state::EnabledTokenRegistry;

/// Shared filter for call-target calldata admitted by the Zone transaction pool.
pub type CalldataFilter =
    Arc<dyn Fn(Address, &[u8]) -> Result<(), InvalidPoolTransactionError> + Send + Sync + 'static>;
type CalldataFilterRef<'a> =
    &'a (dyn Fn(Address, &[u8]) -> Result<(), InvalidPoolTransactionError> + Send + Sync + 'a);

/// Transaction pool builder for Zone - uses Tempo pool with defaults.
#[derive(Clone, Default)]
#[non_exhaustive]
pub struct ZonePoolBuilder {
    enabled_tokens: EnabledTokenRegistry,
    calldata_filter: Option<CalldataFilter>,
}

impl std::fmt::Debug for ZonePoolBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZonePoolBuilder")
            .field("enabled_tokens", &self.enabled_tokens)
            .field(
                "calldata_filter",
                &self.calldata_filter.as_ref().map(|_| "..."),
            )
            .finish()
    }
}

impl ZonePoolBuilder {
    /// Create a pool builder using the shared enabled-token registry.
    pub fn new(enabled_tokens: EnabledTokenRegistry) -> Self {
        Self {
            enabled_tokens,
            ..Default::default()
        }
    }

    /// Sets the filter applied to each call target and calldata during Zone pool admission.
    pub fn with_calldata_filter<F>(mut self, filter: F) -> Self
    where
        F: Fn(Address, &[u8]) -> Result<(), InvalidPoolTransactionError> + Send + Sync + 'static,
    {
        self.calldata_filter = Some(Arc::new(filter));
        self
    }

    /// Sets or clears a shared calldata filter.
    pub fn with_calldata_filter_fn_opt(mut self, filter: Option<CalldataFilter>) -> Self {
        self.calldata_filter = filter;
        self
    }
}

impl<Node> PoolBuilder<Node, ZoneEvmConfig> for ZonePoolBuilder
where
    Node: FullNodeTypes<Types = ZoneNode>,
{
    type Pool = TempoTransactionPool<Node::Provider, ZoneEvmConfig>;

    async fn build_pool(
        self,
        ctx: &BuilderContext<Node>,
        evm_config: ZoneEvmConfig,
    ) -> eyre::Result<Self::Pool> {
        // Zone blocks have no protocol base fee, so allow zero-fee transactions into the pool.
        let mut pool_config = ctx.pool_config().with_disabled_protocol_base_fee();
        pool_config.max_inflight_delegated_slot_limit = pool_config.max_account_slots;

        // this store is effectively a noop
        let blob_store = InMemoryBlobStore::default();
        let additional_tasks = ctx.config().txpool.additional_validation_tasks;
        let task_executor = ctx.task_executor().clone();
        let mut validator =
            TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone(), evm_config)
                .with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
                .with_local_transactions_config(pool_config.local_transactions_config.clone())
                .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
                .with_max_tx_gas_limit(ctx.config().txpool.max_tx_gas_limit)
                .set_block_gas_limit(ctx.chain_spec().genesis().gas_limit)
                .disable_balance_check()
                .with_minimum_priority_fee(ctx.config().txpool.minimum_priority_fee)
                .with_custom_tx_type(TempoTxType::AA as u8)
                .no_eip7702()
                .no_eip4844()
                .build::<TempoPooledTransaction, _>(blob_store.clone());

        let calldata_filter = self.calldata_filter;
        validator.set_additional_stateless_validation(move |origin, tx| {
            validate_zone_pool_transaction(origin, tx, calldata_filter.as_deref())
        });

        let provider = ctx.provider().clone();
        let enabled_tokens = self.enabled_tokens;
        validator.set_additional_stateful_validation(move |_origin, tx, _account_state| {
            validate_has_enabled_token_balance(&provider, &enabled_tokens, *tx.sender_ref())
        });

        let validator =
            TransactionValidationTaskExecutor::spawn(validator, &task_executor, additional_tasks);

        let aa_2d_config = AA2dPoolConfig {
            price_bump_config: pool_config.price_bumps,
            pending_limit: pool_config.pending_limit,
            queued_limit: pool_config.queued_limit,
            max_txs_per_sender: pool_config.max_account_slots,
        };
        let aa_2d_pool = AA2dPool::new(aa_2d_config);
        let amm_liquidity_cache = AmmLiquidityCache::new(ctx.provider())?;

        let validator = validator.map(move |v| {
            TempoTransactionValidator::new(
                v,
                DEFAULT_AA_VALID_AFTER_MAX_SECS,
                DEFAULT_MAX_TEMPO_AUTHORIZATIONS,
                amm_liquidity_cache.clone(),
            )
            // Zones collect the selected fee token directly and never route through FeeAMM.
            .with_disable_fee_amm_check(true)
        });
        let protocol_pool = Pool::new(
            validator,
            TempoTipOrdering::default(),
            blob_store,
            pool_config.clone(),
        );

        let transaction_pool = TempoTransactionPool::new(protocol_pool, aa_2d_pool);

        spawn_maintenance_tasks(ctx, transaction_pool.clone(), &pool_config)?;

        // Spawn unified Tempo pool maintenance task
        // This consolidates: expired AA txs, 2D nonce updates, AMM cache, and keychain revocations
        ctx.task_executor().spawn_critical_task(
            "txpool maintenance - tempo pool",
            tempo_transaction_pool::maintain::maintain_tempo_pool(transaction_pool.clone()),
        );

        info!(target: "reth::cli", "Transaction pool initialized");
        debug!(target: "reth::cli", "Spawned txpool maintenance task");

        Ok(transaction_pool)
    }
}

fn validate_has_enabled_token_balance(
    provider: &impl StateProviderFactory,
    enabled_tokens: &EnabledTokenRegistry,
    sender: Address,
) -> Result<(), InvalidPoolTransactionError> {
    let state = provider.latest().map_err(|err| {
        warn!(%err, "Failed to read latest state for zone token-balance admission check");
        InvalidPoolTransactionError::other(TempoPoolTransactionError::Evm(
            TempoInvalidTransaction::EthInvalidTransaction(
                "could not verify balance of an enabled zone token".into(),
            ),
        ))
    })?;

    for token in enabled_tokens.read().iter().copied() {
        let slot = TIP20Token::from_address_unchecked(token).balances[sender].slot();
        let balance = state.storage(token, slot.into()).map_err(|err| {
            warn!(%err, %sender, "Failed to read zone token balance during pool admission");
            InvalidPoolTransactionError::other(TempoPoolTransactionError::Evm(
                TempoInvalidTransaction::EthInvalidTransaction(
                    "could not verify balance of an enabled zone token".into(),
                ),
            ))
        })?;
        if balance.is_some_and(|balance| !balance.is_zero()) {
            return Ok(());
        }
    }

    Err(InvalidPoolTransactionError::other(
        TempoPoolTransactionError::Evm(TempoInvalidTransaction::EthInvalidTransaction(
            "sender must hold a nonzero balance of an enabled zone token".into(),
        )),
    ))
}

/// Additional stateless validation hook for Zone transaction-pool admission.
///
/// # Scope
///
/// This policy runs only when the pool admits a transaction; it does not affect consensus
/// validation, EVM execution, payload conversion, or the validity of transactions in imported
/// blocks. `TransactionOrigin` is intentionally ignored, so origin metadata cannot grant or bypass
/// the contract-deployer allowlist. The allowlist is passed to the base transaction validator and
/// only exempts contract creation; configured calldata filtering still applies to every call.
/// Legitimate Zone system transactions are synthesized and handled outside pool admission.
///
/// After base transaction validation, the configured calldata filter receives every call target
/// and input from the direct transaction or AA batch. Contract creation remains the responsibility
/// of `zone_evm::validate_transaction`; no calldata filter is invoked for create calls.
///
/// # Errors
///
/// Base-validation failures are wrapped in `TempoPoolTransactionError::Evm`; a configured calldata
/// filter returns its own `InvalidPoolTransactionError`.
pub(crate) fn validate_zone_pool_transaction(
    _origin: TransactionOrigin,
    tx: &TempoPooledTransaction,
    calldata_filter: Option<CalldataFilterRef<'_>>,
) -> Result<(), InvalidPoolTransactionError> {
    validate_zone_pool_transaction_with_allowlist(
        tx,
        zone_primitives::constants::CONTRACT_DEPLOYER_ALLOWLIST,
        calldata_filter,
    )
}

fn validate_zone_pool_transaction_with_allowlist(
    tx: &TempoPooledTransaction,
    contract_deployer_allowlist: &[Address],
    calldata_filter: Option<CalldataFilterRef<'_>>,
) -> Result<(), InvalidPoolTransactionError> {
    let tx = tx.tx_env();
    zone_evm::validate_transaction(tx, contract_deployer_allowlist)
        .map_err(|err| InvalidPoolTransactionError::other(TempoPoolTransactionError::Evm(err)))?;

    if let Some(calldata_filter) = calldata_filter {
        for (target, input) in tx.calls() {
            if let TxKind::Call(address) = *target {
                calldata_filter(address, input)?;
            }
        }
    }

    Ok(())
}

fn validation_error(message: &'static str) -> InvalidPoolTransactionError {
    InvalidPoolTransactionError::other(TempoPoolTransactionError::Evm(message.into()))
}

/// Production calldata policy for Zone transaction-pool admission.
///
/// State-changing Zone system operations are rejected because they must be synthesized as system
/// transactions. TIP-20 targets accept only fully decodable `transferFrom`, `transferWithMemo`,
/// `transferFromWithMemo`, and `approve` calls.
pub(crate) fn validate_zone_pool_calldata(
    target: Address,
    input: &[u8],
) -> Result<(), InvalidPoolTransactionError> {
    let is_zone_system_operation = match (target, input.get(..4)) {
        (ZONE_INBOX_ADDRESS, Some(selector)) => selector == IZoneInbox::advanceTempoCall::SELECTOR,
        (ZONE_OUTBOX_ADDRESS, Some(selector)) => {
            selector == IZoneOutbox::finalizeWithdrawalBatchCall::SELECTOR
        }
        _ => false,
    };

    if is_zone_system_operation {
        return Err(validation_error(
            "zone system operations require a system transaction",
        ));
    }

    if !is_tip20_prefix(target) {
        return Ok(());
    }

    if input.starts_with(&ITIP20::transferFromCall::SELECTOR) {
        ITIP20::transferFromCall::abi_decode(input)
            .map_err(|_| validation_error("malformed TIP-20 transferFrom call"))?;
    } else if input.starts_with(&ITIP20::transferWithMemoCall::SELECTOR) {
        ITIP20::transferWithMemoCall::abi_decode(input)
            .map_err(|_| validation_error("malformed TIP-20 transferWithMemo call"))?;
    } else if input.starts_with(&ITIP20::transferFromWithMemoCall::SELECTOR) {
        ITIP20::transferFromWithMemoCall::abi_decode(input)
            .map_err(|_| validation_error("malformed TIP-20 transferFromWithMemo call"))?;
    } else if input.starts_with(&ITIP20::approveCall::SELECTOR) {
        ITIP20::approveCall::abi_decode(input)
            .map_err(|_| validation_error("malformed TIP-20 approve call"))?;
    } else {
        return Err(validation_error("TIP-20 operation is not allowed on zones"));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Signed, TxEip1559};
    use alloy_primitives::{Address, B256, Bytes, Signature, U256, address};
    use reth_primitives_traits::Recovered;
    use tempo_primitives::{
        TempoTxEnvelope,
        transaction::{AASigned, Call, PrimitiveSignature, TempoSignature, TempoTransaction},
    };

    const TOKEN: Address = address!("0x20C0000000000000000000000000000000000001");

    fn pooled_transaction(envelope: TempoTxEnvelope, sender: Address) -> TempoPooledTransaction {
        TempoPooledTransaction::new(Recovered::new_unchecked(envelope, sender))
    }

    fn aa_transaction(sender: Address, calls: Vec<Call>) -> TempoPooledTransaction {
        let transaction = TempoTransaction {
            calls,
            ..Default::default()
        };
        let signature =
            TempoSignature::Primitive(PrimitiveSignature::Secp256k1(Signature::test_signature()));
        pooled_transaction(
            AASigned::new_unhashed(transaction, signature).into(),
            sender,
        )
    }

    fn call_transaction(sender: Address, target: Address, input: Bytes) -> TempoPooledTransaction {
        pooled_transaction(
            TempoTxEnvelope::Eip1559(Signed::new_unhashed(
                TxEip1559 {
                    to: TxKind::Call(target),
                    input,
                    ..Default::default()
                },
                Signature::test_signature(),
            )),
            sender,
        )
    }

    fn create_transaction(sender: Address) -> TempoPooledTransaction {
        pooled_transaction(
            TempoTxEnvelope::Eip1559(Signed::new_unhashed(
                TxEip1559 {
                    to: TxKind::Create,
                    ..Default::default()
                },
                Signature::test_signature(),
            )),
            sender,
        )
    }

    fn validate_pool_transaction(
        origin: TransactionOrigin,
        transaction: &TempoPooledTransaction,
    ) -> Result<(), InvalidPoolTransactionError> {
        validate_zone_pool_transaction(origin, transaction, Some(&validate_zone_pool_calldata))
    }

    #[test]
    fn pool_policy_allowlist_only_exempts_contract_creation() {
        let sender = Address::repeat_byte(0x11);
        let create = create_transaction(sender);
        let reject_all_calls =
            |_target, _input: &[u8]| Err(validation_error("custom calldata rejection"));

        assert!(
            validate_zone_pool_transaction_with_allowlist(
                &create,
                &[sender],
                Some(&reject_all_calls),
            )
            .is_ok()
        );
        assert!(
            validate_zone_pool_transaction_with_allowlist(&create, &[], Some(&reject_all_calls),)
                .is_err()
        );

        let call = call_transaction(sender, Address::repeat_byte(0x22), Bytes::new());
        let error = validate_zone_pool_transaction_with_allowlist(
            &call,
            &[sender],
            Some(&reject_all_calls),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "custom calldata rejection");
    }

    #[test]
    fn pool_policy_rejects_create_in_non_first_aa_call() {
        let transaction = aa_transaction(
            Address::repeat_byte(0x11),
            vec![
                Call {
                    to: TxKind::Call(Address::repeat_byte(0x22)),
                    value: U256::ZERO,
                    input: Bytes::new(),
                },
                Call {
                    to: TxKind::Create,
                    value: U256::ZERO,
                    input: Bytes::new(),
                },
            ],
        );

        assert!(validate_pool_transaction(TransactionOrigin::External, &transaction).is_err());
    }

    #[test]
    fn pool_policy_restricts_tip20_operations() {
        let sender = Address::repeat_byte(0x11);
        let transfer_from = ITIP20::transferFromCall {
            from: sender,
            to: Address::repeat_byte(0x22),
            amount: U256::from(7),
        };
        let approve = ITIP20::approveCall {
            spender: Address::repeat_byte(0x33),
            amount: U256::from(9),
        };
        let transfer_with_memo = ITIP20::transferWithMemoCall {
            to: Address::repeat_byte(0x44),
            amount: U256::from(11),
            memo: B256::with_last_byte(12),
        };
        let transfer_from_with_memo = ITIP20::transferFromWithMemoCall {
            from: sender,
            to: Address::repeat_byte(0x55),
            amount: U256::from(13),
            memo: B256::with_last_byte(14),
        };

        for input in [
            transfer_from.abi_encode(),
            transfer_with_memo.abi_encode(),
            transfer_from_with_memo.abi_encode(),
            approve.abi_encode(),
        ] {
            let transaction = call_transaction(sender, TOKEN, input.into());
            assert!(validate_pool_transaction(TransactionOrigin::External, &transaction).is_ok());
        }

        let transfer = ITIP20::transferCall {
            to: Address::repeat_byte(0x22),
            amount: U256::from(7),
        };
        let transaction = call_transaction(sender, TOKEN, transfer.abi_encode().into());
        let error =
            validate_pool_transaction(TransactionOrigin::External, &transaction).unwrap_err();
        assert_eq!(
            error.to_string(),
            "TIP-20 operation is not allowed on zones"
        );

        for (selector, expected_error) in [
            (
                ITIP20::transferFromCall::SELECTOR,
                "malformed TIP-20 transferFrom call",
            ),
            (
                ITIP20::transferWithMemoCall::SELECTOR,
                "malformed TIP-20 transferWithMemo call",
            ),
            (
                ITIP20::transferFromWithMemoCall::SELECTOR,
                "malformed TIP-20 transferFromWithMemo call",
            ),
            (
                ITIP20::approveCall::SELECTOR,
                "malformed TIP-20 approve call",
            ),
        ] {
            let transaction = call_transaction(sender, TOKEN, selector.to_vec().into());
            let error =
                validate_pool_transaction(TransactionOrigin::External, &transaction).unwrap_err();
            assert_eq!(error.to_string(), expected_error);
        }
    }

    #[test]
    fn pool_policy_validates_every_tip20_call_in_aa_batch() {
        let sender = Address::repeat_byte(0x11);
        let transaction = aa_transaction(
            sender,
            vec![
                Call {
                    to: TxKind::Call(TOKEN),
                    value: U256::ZERO,
                    input: ITIP20::approveCall {
                        spender: Address::repeat_byte(0x33),
                        amount: U256::from(9),
                    }
                    .abi_encode()
                    .into(),
                },
                Call {
                    to: TxKind::Call(TOKEN),
                    value: U256::ZERO,
                    input: ITIP20::mintCall {
                        to: Address::repeat_byte(0x44),
                        amount: U256::from(1),
                    }
                    .abi_encode()
                    .into(),
                },
            ],
        );

        let error =
            validate_pool_transaction(TransactionOrigin::External, &transaction).unwrap_err();
        assert_eq!(
            error.to_string(),
            "TIP-20 operation is not allowed on zones"
        );
    }

    #[test]
    fn pool_policy_uses_configured_calldata_filter() {
        let sender = Address::repeat_byte(0x11);
        let target = Address::repeat_byte(0x44);
        let transaction = call_transaction(sender, target, Bytes::from_static(b"custom"));

        assert!(
            validate_zone_pool_transaction(TransactionOrigin::External, &transaction, None).is_ok()
        );

        let filter = |filtered_target: Address, input: &[u8]| {
            assert_eq!(filtered_target, target);
            assert_eq!(input, b"custom");
            Err(validation_error("custom calldata rejection"))
        };
        let error = validate_zone_pool_transaction(
            TransactionOrigin::External,
            &transaction,
            Some(&filter),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "custom calldata rejection");
    }

    #[test]
    fn pool_policy_preserves_non_tip20_calls_and_rejects_system_operations() {
        let sender = Address::repeat_byte(0x11);
        let non_tip20 = call_transaction(sender, Address::repeat_byte(0x44), Bytes::new());
        for origin in [
            TransactionOrigin::Local,
            TransactionOrigin::External,
            TransactionOrigin::Private,
        ] {
            assert!(validate_pool_transaction(origin, &non_tip20).is_ok());
        }

        for (target, selector) in [
            (ZONE_INBOX_ADDRESS, IZoneInbox::advanceTempoCall::SELECTOR),
            (
                ZONE_OUTBOX_ADDRESS,
                IZoneOutbox::finalizeWithdrawalBatchCall::SELECTOR,
            ),
        ] {
            let system_operation = call_transaction(sender, target, selector.to_vec().into());
            for origin in [
                TransactionOrigin::Local,
                TransactionOrigin::External,
                TransactionOrigin::Private,
            ] {
                let error = validate_pool_transaction(origin, &system_operation).unwrap_err();
                assert_eq!(
                    error.to_string(),
                    "zone system operations require a system transaction"
                );
            }
        }
    }
}
