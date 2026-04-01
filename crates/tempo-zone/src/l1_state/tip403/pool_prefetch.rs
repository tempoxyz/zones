//! Pool transaction prefetch task for TIP-403 policy cache warming.
//!
//! Subscribes to new pending transactions from the transaction pool and
//! extracts sender/recipient addresses from TIP-20 transfer calls. For each
//! address, a [`ResolveAuthorization`](super::PolicyTaskMessage::ResolveAuthorization)
//! request is sent to the [`PolicyResolutionTask`](super::PolicyResolutionTask)
//! via the [`PolicyTaskHandle`](super::PolicyTaskHandle), warming the policy cache
//! before block building.

use alloy_primitives::TxKind;
use alloy_sol_types::SolCall;
use reth_transaction_pool::TransactionPool;
use tempo_contracts::precompiles::{DEFAULT_FEE_TOKEN, ITIP20};
use tempo_precompiles::tip20::is_tip20_prefix;
use tempo_revm::TempoTx;
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
/// 2. **Transfer sender** — for TIP-20 transfer calls, the sender is resolved
///    against the transfer token. For `transferFrom*`, this is the decoded `from`
///    address rather than the transaction sender.
/// 3. **Transfer recipient** — for `transfer`, `transferWithMemo`, `transferFrom`,
///    and `transferFromWithMemo` calls, the `to` address is decoded from calldata
///    and resolved as `AuthRole::Recipient`.
/// 4. **Batch calls** — Tempo AA transactions can include multiple top-level calls,
///    and each call is inspected independently.
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
    task_executor: reth_tasks::Runtime,
) where
    Pool: TransactionPool<Transaction = TempoPooledTransaction> + 'static,
{
    task_executor.spawn_task(Box::pin(async move {
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

        for (kind, input) in tx.transaction.inner().calls() {
            let TxKind::Call(token) = kind else {
                continue;
            };
            if !is_tip20_prefix(token) {
                continue;
            }

            let Some(selector) = input.first_chunk::<4>() else {
                continue;
            };
            let args = &input[4..];

            let (transfer_sender, recipient) = if *selector == ITIP20::transferCall::SELECTOR {
                let Ok(call) = ITIP20::transferCall::abi_decode_raw(args) else {
                    continue;
                };
                (sender, call.to)
            } else if *selector == ITIP20::transferWithMemoCall::SELECTOR {
                let Ok(call) = ITIP20::transferWithMemoCall::abi_decode_raw(args) else {
                    continue;
                };
                (sender, call.to)
            } else if *selector == ITIP20::transferFromCall::SELECTOR {
                let Ok(call) = ITIP20::transferFromCall::abi_decode_raw(args) else {
                    continue;
                };
                (call.from, call.to)
            } else if *selector == ITIP20::transferFromWithMemoCall::SELECTOR {
                let Ok(call) = ITIP20::transferFromWithMemoCall::abi_decode_raw(args) else {
                    continue;
                };
                (call.from, call.to)
            } else {
                continue;
            };

            if token != fee_token || transfer_sender != fee_payer {
                debug!(%token, %transfer_sender, "Pre-fetching TIP-403 sender authorization");
                let _ = handle.send_resolve_policy(token, transfer_sender, AuthRole::Sender);
            }

            debug!(%token, recipient = %recipient, "Pre-fetching TIP-403 recipient authorization");
            let _ = handle.send_resolve_policy(token, recipient, AuthRole::Recipient);
        }
    }

    debug!("Pool prefetch task shutting down");
}
