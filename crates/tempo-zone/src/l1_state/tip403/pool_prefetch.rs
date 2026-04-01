//! Pool transaction prefetch task for TIP-403 policy cache warming.
//!
//! Subscribes to new pending transactions from the transaction pool and
//! extracts sender/recipient addresses from TIP-20 transfer calls, including
//! batched Tempo AA calls. For each address, a
//! [`ResolveAuthorization`](super::PolicyTaskMessage::ResolveAuthorization)
//! request is sent to the [`PolicyResolutionTask`](super::PolicyResolutionTask)
//! via the [`PolicyTaskHandle`](super::PolicyTaskHandle), warming the policy
//! cache before block building.

use std::collections::HashSet;

use alloy_primitives::{Address, Bytes, TxKind};
use alloy_sol_types::SolCall;
use reth_transaction_pool::TransactionPool;
use tempo_contracts::precompiles::{DEFAULT_FEE_TOKEN, ITIP20};
use tempo_precompiles::tip20::is_tip20_prefix;
use tempo_revm::TempoTx;
use tempo_transaction_pool::transaction::TempoPooledTransaction;
use tracing::debug;

use super::{AuthRole, task::PolicyTaskHandle};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct PrefetchRequest {
    token: Address,
    user: Address,
    role: AuthRole,
}

impl PrefetchRequest {
    const fn new(token: Address, user: Address, role: AuthRole) -> Self {
        Self { token, user, role }
    }
}

/// Spawns a background task that watches for new pool transactions and
/// pre-fetches TIP-403 authorization data for sender/recipient addresses.
///
/// For every incoming transaction the task warms three categories of cache entries:
///
/// 1. **Fee payer** — the address paying gas fees, resolved as `AuthRole::Sender`
///    against the transaction's fee token (defaults to pathUSD). AA transactions
///    may specify a different fee token or delegate fee payment to another address.
/// 2. **Transfer sender** — for TIP-20 transfer calls, the actual transfer sender is
///    resolved against the transfer token. For `transferFrom*`, this is the decoded
///    `from` address rather than the transaction sender.
/// 3. **Transfer recipient** — for `transfer`, `transferWithMemo`, `transferFrom`,
///    and `transferFromWithMemo` calls, the `to` address is decoded from calldata
///    and resolved as `AuthRole::Recipient`.
/// 4. **Batch calls** — Tempo AA transactions can include multiple top-level calls;
///    each call is inspected independently so later batched transfers are warmed too.
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

        for request in
            collect_prefetch_requests(sender, fee_payer, fee_token, tx.transaction.inner().calls())
        {
            debug!(
                %request.token,
                %request.user,
                ?request.role,
                "Pre-fetching TIP-403 authorization"
            );
            let _ = handle.send_resolve_policy(request.token, request.user, request.role);
        }
    }

    debug!("Pool prefetch task shutting down");
}

fn collect_prefetch_requests<'a, I>(
    sender: Address,
    fee_payer: Address,
    fee_token: Address,
    calls: I,
) -> Vec<PrefetchRequest>
where
    I: IntoIterator<Item = (TxKind, &'a Bytes)>,
{
    let mut seen = HashSet::new();
    let mut requests = Vec::new();

    push_prefetch_request(
        PrefetchRequest::new(fee_token, fee_payer, AuthRole::Sender),
        &mut seen,
        &mut requests,
    );

    for (kind, input) in calls {
        let TxKind::Call(token) = kind else {
            continue;
        };
        if !is_tip20_prefix(token) {
            continue;
        }

        let Some((transfer_sender, recipient)) = decode_transfer_participants(sender, input) else {
            continue;
        };

        push_prefetch_request(
            PrefetchRequest::new(token, transfer_sender, AuthRole::Sender),
            &mut seen,
            &mut requests,
        );
        push_prefetch_request(
            PrefetchRequest::new(token, recipient, AuthRole::Recipient),
            &mut seen,
            &mut requests,
        );
    }

    requests
}

fn push_prefetch_request(
    request: PrefetchRequest,
    seen: &mut HashSet<PrefetchRequest>,
    requests: &mut Vec<PrefetchRequest>,
) {
    if seen.insert(request) {
        requests.push(request);
    }
}

fn decode_transfer_participants(caller: Address, input: &[u8]) -> Option<(Address, Address)> {
    let selector = *input.first_chunk::<4>()?;
    let args = &input[4..];

    match selector {
        ITIP20::transferCall::SELECTOR => {
            let call = ITIP20::transferCall::abi_decode_raw(args).ok()?;
            Some((caller, call.to))
        }
        ITIP20::transferWithMemoCall::SELECTOR => {
            let call = ITIP20::transferWithMemoCall::abi_decode_raw(args).ok()?;
            Some((caller, call.to))
        }
        ITIP20::transferFromCall::SELECTOR => {
            let call = ITIP20::transferFromCall::abi_decode_raw(args).ok()?;
            Some((call.from, call.to))
        }
        ITIP20::transferFromWithMemoCall::SELECTOR => {
            let call = ITIP20::transferFromWithMemoCall::abi_decode_raw(args).ok()?;
            Some((call.from, call.to))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256, address};

    #[test]
    fn collects_transfer_from_sender_from_calldata() {
        let sender = Address::repeat_byte(0x11);
        let fee_payer = Address::repeat_byte(0x22);
        let fee_token = address!("0x20C0000000000000000000000000000000000003");
        let token = address!("0x20C0000000000000000000000000000000000001");
        let from = Address::repeat_byte(0x44);
        let to = Address::repeat_byte(0x55);
        let amount = U256::from(7_u64);
        let input: Bytes = ITIP20::transferFromCall { from, to, amount }
            .abi_encode()
            .into();
        let calls = [(TxKind::Call(token), input)];

        let requests = collect_prefetch_requests(
            sender,
            fee_payer,
            fee_token,
            calls.iter().map(|(kind, input)| (*kind, input)),
        );

        assert_eq!(
            requests,
            vec![
                PrefetchRequest::new(fee_token, fee_payer, AuthRole::Sender),
                PrefetchRequest::new(token, from, AuthRole::Sender),
                PrefetchRequest::new(token, to, AuthRole::Recipient),
            ]
        );
    }

    #[test]
    fn collects_all_batched_transfer_calls() {
        let sender = Address::repeat_byte(0x11);
        let fee_payer = Address::repeat_byte(0x22);
        let fee_token = address!("0x20C0000000000000000000000000000000000003");
        let token_a = address!("0x20C0000000000000000000000000000000000001");
        let token_b = address!("0x20C0000000000000000000000000000000000002");
        let to_a = Address::repeat_byte(0x44);
        let from_b = Address::repeat_byte(0x55);
        let to_b = Address::repeat_byte(0x66);
        let amount = U256::from(7_u64);
        let calls = [
            (
                TxKind::Call(token_a),
                Bytes::from(ITIP20::transferCall { to: to_a, amount }.abi_encode()),
            ),
            (
                TxKind::Call(token_b),
                Bytes::from(
                    ITIP20::transferFromCall {
                        from: from_b,
                        to: to_b,
                        amount,
                    }
                    .abi_encode(),
                ),
            ),
        ];

        let requests = collect_prefetch_requests(
            sender,
            fee_payer,
            fee_token,
            calls.iter().map(|(kind, input)| (*kind, input)),
        );

        assert_eq!(
            requests,
            vec![
                PrefetchRequest::new(fee_token, fee_payer, AuthRole::Sender),
                PrefetchRequest::new(token_a, sender, AuthRole::Sender),
                PrefetchRequest::new(token_a, to_a, AuthRole::Recipient),
                PrefetchRequest::new(token_b, from_b, AuthRole::Sender),
                PrefetchRequest::new(token_b, to_b, AuthRole::Recipient),
            ]
        );
    }

    #[test]
    fn keeps_transfer_sender_when_fee_payer_differs_on_same_token() {
        let sender = Address::repeat_byte(0x11);
        let fee_payer = Address::repeat_byte(0x22);
        let token = address!("0x20C0000000000000000000000000000000000001");
        let to = Address::repeat_byte(0x44);
        let amount = U256::from(7_u64);
        let input: Bytes = ITIP20::transferCall { to, amount }.abi_encode().into();
        let calls = [(TxKind::Call(token), input)];

        let requests = collect_prefetch_requests(
            sender,
            fee_payer,
            token,
            calls.iter().map(|(kind, input)| (*kind, input)),
        );

        assert_eq!(
            requests,
            vec![
                PrefetchRequest::new(token, fee_payer, AuthRole::Sender),
                PrefetchRequest::new(token, sender, AuthRole::Sender),
                PrefetchRequest::new(token, to, AuthRole::Recipient),
            ]
        );
    }
}
