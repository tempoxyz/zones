//! Pool transaction prefetch task for TIP-403 policy cache warming.
//!
//! Subscribes to new pending transactions from the transaction pool and
//! extracts sender/recipient addresses from TIP-20 transfer calls. For each
//! address, a [`ResolveAuthorization`](super::PolicyTaskMessage::ResolveAuthorization)
//! request is sent to the [`PolicyResolutionTask`](super::PolicyResolutionTask)
//! via the [`PolicyTaskHandle`](super::PolicyTaskHandle), warming the policy cache
//! before block building.

use alloy_consensus::Transaction;
use alloy_sol_types::SolCall;
use reth_transaction_pool::TransactionPool;
use tempo_contracts::precompiles::{DEFAULT_FEE_TOKEN, ITIP20};
use tempo_transaction_pool::transaction::TempoPooledTransaction;
use tracing::debug;

use super::{AuthRole, task::PolicyTaskHandle};

/// Spawns a background task that watches for new pool transactions and
/// pre-fetches TIP-403 authorization data for sender/recipient addresses.
///
/// For every incoming transaction the task warms three categories of cache entries:
///
/// 1. **Fee payer** — the address paying gas fees, resolved as `AuthRole::Sender`
///    against the transaction's fee token (defaults to pathUSD). AA transactions
///    may specify a different fee token or delegate fee payment to another address.
/// 2. **Transfer sender** — for TIP-20 payment transactions, the sender is resolved
///    against the transfer token if it differs from the fee token.
/// 3. **Transfer recipient** — for `transfer`, `transferWithMemo`, `transferFrom`,
///    and `transferFromWithMemo` calls, the `to` address is decoded from calldata
///    and resolved as `AuthRole::Recipient`.
///
/// The resolution task fetches the latest L1 block number for each request,
/// so callers don't need to track block heights.
///
/// The task is spawned as a non-critical background task — if it stops, block
/// building still works but may incur more synchronous RPC round-trips on cache
/// misses.
pub fn spawn_pool_prefetch_task<Pool>(
    pool: Pool,
    handle: PolicyTaskHandle,
    task_executor: impl reth_ethereum::tasks::TaskSpawner,
) where
    Pool: TransactionPool<Transaction = TempoPooledTransaction> + 'static,
{
    task_executor.spawn(Box::pin(async move {
        run_pool_prefetch(pool, handle).await;
    }));
}

async fn run_pool_prefetch<Pool>(pool: Pool, handle: PolicyTaskHandle)
where
    Pool: TransactionPool<Transaction = TempoPooledTransaction>,
{
    let mut new_txs = pool.new_transactions_listener();

    while let Some(tx_event) = new_txs.recv().await {
        let tx = &tx_event.transaction;
        let sender = tx.sender();

        // Resolve the fee token for this transaction (AA txs may specify one,
        // otherwise falls back to DEFAULT_FEE_TOKEN / pathUSD).
        let fee_token = tx
            .transaction
            .inner()
            .fee_token()
            .unwrap_or(DEFAULT_FEE_TOKEN);

        // Resolve the fee payer (may differ from sender for AA txs with delegated fees)
        let fee_payer = tx.transaction.inner().fee_payer(sender).unwrap_or(sender);

        // Pre-fetch fee payer authorization for the fee token (every tx pays fees)
        debug!(%fee_token, %fee_payer, "Pre-fetching TIP-403 fee token authorization");
        let _ = handle.send_resolve_policy(fee_token, fee_payer, AuthRole::Sender);

        // For TIP-20 payment transactions, also pre-fetch transfer-specific data
        if tx.transaction.is_payment() {
            let Some(token) = tx.to() else {
                continue;
            };

            // Pre-fetch sender for the transfer token (may differ from fee token)
            if token != fee_token {
                let _ = handle.send_resolve_policy(token, sender, AuthRole::Sender);
            }

            // Pre-fetch recipient authorization for all transfer variants
            let recipient = if tx.transaction.function_selector()
                == Some(&ITIP20::transferCall::SELECTOR.into())
                && let Ok(call) = ITIP20::transferCall::abi_decode_raw(&tx.transaction.input()[4..])
            {
                Some(call.to)
            } else if tx.transaction.function_selector()
                == Some(&ITIP20::transferWithMemoCall::SELECTOR.into())
                && let Ok(call) =
                    ITIP20::transferWithMemoCall::abi_decode_raw(&tx.transaction.input()[4..])
            {
                Some(call.to)
            } else if tx.transaction.function_selector()
                == Some(&ITIP20::transferFromCall::SELECTOR.into())
                && let Ok(call) =
                    ITIP20::transferFromCall::abi_decode_raw(&tx.transaction.input()[4..])
            {
                Some(call.to)
            } else if tx.transaction.function_selector()
                == Some(&ITIP20::transferFromWithMemoCall::SELECTOR.into())
                && let Ok(call) =
                    ITIP20::transferFromWithMemoCall::abi_decode_raw(&tx.transaction.input()[4..])
            {
                Some(call.to)
            } else {
                None
            };

            if let Some(to) = recipient {
                debug!(%token, recipient = %to, "Pre-fetching TIP-403 recipient authorization");
                let _ = handle.send_resolve_policy(token, to, AuthRole::Recipient);
            }
        }
    }

    debug!("Pool prefetch task shutting down");
}
