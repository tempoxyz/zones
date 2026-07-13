//! Privacy policy enforcement helpers.
//!
//! Shared by [`ZoneRpcApi`] implementations.

use std::future::Future;

use alloy_consensus::transaction::SignerRecoverable;
use alloy_eips::eip2718::Decodable2718;
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, Bytes, TxKind};
use alloy_sol_types::SolCall;
use tempo_alloy::rpc::TempoTransactionRequest;
use tempo_primitives::TempoTxEnvelope;
use tempo_zone_contracts::{ZONE_INBOX_ADDRESS, ZoneInbox};
use zone_primitives::constants::CONTRACT_DEPLOYER_ALLOWLIST;

use crate::{auth::AuthContext, types::JsonRpcError};

/// Enforce all private RPC authorization rules for simulation-style requests.
///
/// The sequencer check is lazy: it is awaited only for calls that try to read
/// another account's `ZoneInbox.refunds(token, owner)` entry.
pub async fn enforce_authorized<F>(
    request: &mut TempoTransactionRequest,
    auth: &AuthContext,
    is_sequencer: F,
) -> Result<(), JsonRpcError>
where
    F: Future<Output = Result<bool, JsonRpcError>>,
{
    enforce_from(request, auth)?;
    enforce_contract_creation(request, auth.caller)?;
    enforce_zone_inbox_refund_call_privacy(request, auth, is_sequencer).await
}

/// Enforce that `from` matches the authenticated caller.
///
/// - If `from` is omitted, sets it to `auth.caller`.
/// - If present and mismatched, returns `-32004 Account mismatch`.
pub fn enforce_from(
    request: &mut TempoTransactionRequest,
    auth: &AuthContext,
) -> Result<(), JsonRpcError> {
    match TransactionBuilder::from(request as &TempoTransactionRequest) {
        Some(from) if from != auth.caller => Err(JsonRpcError::account_mismatch()),
        None => {
            request.set_from(auth.caller);
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Apply the protocol contract-deployer allowlist to create-style transaction requests.
///
/// Plain Ethereum-style create requests (`to = null`) and Tempo AA calls to `TxKind::Create`
/// are rejected with `-32602 Invalid params` unless the caller is a protocol-allowed deployer.
pub fn enforce_contract_creation(
    request: &TempoTransactionRequest,
    caller: Address,
) -> Result<(), JsonRpcError> {
    enforce_contract_creation_with_allowlist(request, caller, CONTRACT_DEPLOYER_ALLOWLIST)
}

fn enforce_contract_creation_with_allowlist(
    request: &TempoTransactionRequest,
    caller: Address,
    allowlist: &[Address],
) -> Result<(), JsonRpcError> {
    if allowlist.contains(&caller) {
        return Ok(());
    }

    let outer_create = request.inner.to.is_some_and(|to| to.is_create());
    let implicit_plain_create = request.calls.is_empty() && request.inner.to.is_none();
    let tempo_create = request.calls.iter().any(|call| call.to.is_create());
    if outer_create || implicit_plain_create || tempo_create {
        return Err(JsonRpcError::invalid_params(
            "contract creation not supported on zones",
        ));
    }

    Ok(())
}

async fn enforce_zone_inbox_refund_call_privacy<F>(
    request: &TempoTransactionRequest,
    auth: &AuthContext,
    is_sequencer: F,
) -> Result<(), JsonRpcError>
where
    F: Future<Output = Result<bool, JsonRpcError>>,
{
    if zone_inbox_refunds_mismatched_owner(request, auth.caller).is_none() {
        return Ok(());
    }

    if is_sequencer.await? {
        return Ok(());
    }

    Err(JsonRpcError::account_mismatch())
}

/// Finds a direct or nested `ZoneInbox.refunds(token, owner)` read where
/// `owner` is not the authenticated caller.
///
/// Other calls, contract creations, and malformed calldata are ignored here.
fn zone_inbox_refunds_mismatched_owner(
    request: &TempoTransactionRequest,
    caller: Address,
) -> Option<Address> {
    let refunds_owner_mismatch = |to: Option<Address>, input: Option<&Bytes>| {
        if to != Some(ZONE_INBOX_ADDRESS) {
            return None;
        }

        let input = input?;
        if !input.starts_with(&ZoneInbox::refundsCall::SELECTOR) {
            return None;
        }

        let owner = ZoneInbox::refundsCall::abi_decode(input).ok()?.owner;
        (owner != caller).then_some(owner)
    };

    if let Some(owner) = refunds_owner_mismatch(
        TransactionBuilder::to(request),
        TransactionBuilder::input(request),
    ) {
        return Some(owner);
    }

    request.calls.iter().find_map(|call| {
        let to = match call.to {
            TxKind::Call(to) => Some(to),
            TxKind::Create => None,
        };
        refunds_owner_mismatch(to, Some(&call.input))
    })
}

/// Decode a raw transaction and verify the recovered sender matches the
/// authenticated caller. Returns `-32003 Transaction rejected` on mismatch.
pub fn verify_raw_tx_sender(data: &[u8], auth: &AuthContext) -> Result<(), JsonRpcError> {
    let tx = TempoTxEnvelope::decode_2718_exact(data)
        .map_err(|_| JsonRpcError::invalid_params("failed to decode transaction"))?;

    let sender = tx
        .recover_signer()
        .map_err(|_| JsonRpcError::invalid_params("invalid transaction signature"))?;

    if sender != auth.caller {
        return Err(JsonRpcError::transaction_rejected());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, Bytes, TxKind, U256};
    use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
    use alloy_sol_types::SolCall;
    use tempo_alloy::rpc::TempoTransactionRequest;
    use tempo_primitives::transaction::Call;
    use tempo_zone_contracts::{ZONE_INBOX_ADDRESS, ZONE_TOKEN_ADDRESS, ZoneInbox};

    use super::{
        enforce_contract_creation, enforce_contract_creation_with_allowlist,
        zone_inbox_refunds_mismatched_owner,
    };

    fn call_target(byte: u8) -> TxKind {
        TxKind::Call(Address::repeat_byte(byte))
    }

    fn call_request(to: Option<TxKind>) -> TempoTransactionRequest {
        TempoTransactionRequest {
            inner: TransactionRequest {
                to,
                input: TransactionInput::new(Bytes::default()),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn zone_inbox_refunds_request(owner: Address) -> TempoTransactionRequest {
        TempoTransactionRequest {
            inner: TransactionRequest {
                to: Some(TxKind::Call(ZONE_INBOX_ADDRESS)),
                input: TransactionInput::new(
                    ZoneInbox::refundsCall {
                        token: ZONE_TOKEN_ADDRESS,
                        owner,
                    }
                    .abi_encode()
                    .into(),
                ),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn contract_creation_policy_allows_standard_call_request() {
        let request = call_request(Some(call_target(0x11)));
        assert!(enforce_contract_creation(&request, Address::repeat_byte(0x01)).is_ok());
    }

    #[test]
    fn contract_creation_policy_rejects_plain_create_request() {
        let request = call_request(None);
        let err = enforce_contract_creation(&request, Address::repeat_byte(0x01)).unwrap_err();
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "contract creation not supported on zones");
    }

    #[test]
    fn contract_creation_policy_rejects_explicit_outer_create_request() {
        let request = call_request(Some(TxKind::Create));
        let err = enforce_contract_creation(&request, Address::repeat_byte(0x01)).unwrap_err();
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "contract creation not supported on zones");
    }

    #[test]
    fn contract_creation_policy_allows_tempo_calls_without_outer_to() {
        let mut request = call_request(None);
        request.calls = vec![Call {
            to: call_target(0x22),
            value: U256::ZERO,
            input: Bytes::default(),
        }];

        assert!(enforce_contract_creation(&request, Address::repeat_byte(0x01)).is_ok());
    }

    #[test]
    fn contract_creation_policy_rejects_tempo_create_call() {
        let mut request = call_request(None);
        request.calls = vec![Call {
            to: TxKind::Create,
            value: U256::ZERO,
            input: Bytes::default(),
        }];

        let err = enforce_contract_creation(&request, Address::repeat_byte(0x01)).unwrap_err();
        assert_eq!(err.code, -32602);
        assert_eq!(err.message, "contract creation not supported on zones");
    }

    #[test]
    fn contract_creation_policy_allows_designated_deployer() {
        let caller = Address::repeat_byte(0x11);
        let request = call_request(None);

        assert!(enforce_contract_creation_with_allowlist(&request, caller, &[]).is_err());
        assert!(enforce_contract_creation_with_allowlist(&request, caller, &[caller]).is_ok());
    }

    #[test]
    fn zone_inbox_refunds_mismatched_owner_detects_outer_call() {
        let caller = Address::repeat_byte(0x11);
        let owner = Address::repeat_byte(0x22);
        let request = zone_inbox_refunds_request(owner);

        assert_eq!(
            zone_inbox_refunds_mismatched_owner(&request, caller),
            Some(owner)
        );
    }

    #[test]
    fn zone_inbox_refunds_mismatched_owner_allows_own_outer_call() {
        let caller = Address::repeat_byte(0x11);
        let request = zone_inbox_refunds_request(caller);

        assert_eq!(zone_inbox_refunds_mismatched_owner(&request, caller), None);
    }

    #[test]
    fn zone_inbox_refunds_mismatched_owner_detects_nested_tempo_call() {
        let caller = Address::repeat_byte(0x11);
        let owner = Address::repeat_byte(0x22);
        let mut request = TempoTransactionRequest {
            inner: TransactionRequest {
                to: Some(TxKind::Call(Address::repeat_byte(0x33))),
                ..Default::default()
            },
            ..Default::default()
        };
        request.calls.push(Call {
            to: TxKind::Call(ZONE_INBOX_ADDRESS),
            value: U256::ZERO,
            input: ZoneInbox::refundsCall {
                token: ZONE_TOKEN_ADDRESS,
                owner,
            }
            .abi_encode()
            .into(),
        });

        assert_eq!(
            zone_inbox_refunds_mismatched_owner(&request, caller),
            Some(owner)
        );
    }

    #[test]
    fn zone_inbox_refunds_mismatched_owner_ignores_other_calls() {
        let caller = Address::repeat_byte(0x11);
        let mut request = zone_inbox_refunds_request(Address::repeat_byte(0x22));
        request.inner.to = Some(TxKind::Call(Address::repeat_byte(0x33)));

        assert_eq!(zone_inbox_refunds_mismatched_owner(&request, caller), None);
    }
}
