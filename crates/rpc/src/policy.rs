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
use tempo_contracts::precompiles::{ACCOUNT_KEYCHAIN_ADDRESS, account_keychain::IAccountKeychain};
use tempo_primitives::TempoTxEnvelope;
use tempo_zone_contracts::{ZONE_INBOX_ADDRESS, ZoneInbox};
use zone_primitives::constants::CONTRACT_DEPLOYER_ALLOWLIST;

use crate::{auth::AuthContext, types::JsonRpcError};

/// Enforce all private RPC authorization rules for simulation-style requests.
///
/// The sequencer check is lazy: it is awaited only for calls that try to read
/// another account's private state through a non-caller-scoped getter (currently
/// `ZoneInbox.refunds(token, owner)` and the `AccountKeychain` view getters).
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
    enforce_private_read_scoping(request, auth, is_sequencer).await
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

/// Reject `eth_call`/`eth_estimateGas` requests that read another account's private
/// state through a getter that is not scoped by `msg.sender` on-chain, unless the
/// authenticated caller is the sequencer (which is allowed full visibility).
///
/// Covers `ZoneInbox.refunds(token, owner)` and the `AccountKeychain` view getters.
/// The sequencer future is awaited at most once, only when a cross-account read is
/// actually detected.
async fn enforce_private_read_scoping<F>(
    request: &TempoTransactionRequest,
    auth: &AuthContext,
    is_sequencer: F,
) -> Result<(), JsonRpcError>
where
    F: Future<Output = Result<bool, JsonRpcError>>,
{
    let reads_other_account = zone_inbox_refunds_mismatched_owner(request, auth.caller).is_some()
        || account_keychain_mismatched_account(request, auth.caller).is_some();

    if !reads_other_account {
        return Ok(());
    }

    if is_sequencer.await? {
        return Ok(());
    }

    Err(JsonRpcError::account_mismatch())
}

/// `AccountKeychain` view getters that take a queried `account` as their first
/// argument and are **not** scoped by `msg.sender` on-chain. Reading any of these
/// for an account other than the caller would expose that account's keys, spending
/// limits, and allowed-call lists.
const KEYCHAIN_ACCOUNT_SCOPED_SELECTORS: [[u8; 4]; 6] = [
    IAccountKeychain::getKeyCall::SELECTOR,
    IAccountKeychain::getRemainingLimitCall::SELECTOR,
    IAccountKeychain::getRemainingLimitWithPeriodCall::SELECTOR,
    IAccountKeychain::getAllowedCallsCall::SELECTOR,
    IAccountKeychain::isKeyAuthorizationWitnessBurnedCall::SELECTOR,
    IAccountKeychain::isAdminKeyCall::SELECTOR,
];

/// Finds a direct or nested `AccountKeychain` view read whose queried `account` is
/// not the authenticated caller.
///
/// Every selector in [`KEYCHAIN_ACCOUNT_SCOPED_SELECTORS`] takes `address account`
/// as its first ABI word, so the account is decoded from the low 20 bytes of that
/// word. Other calls, contract creations, and malformed calldata are ignored here.
fn account_keychain_mismatched_account(
    request: &TempoTransactionRequest,
    caller: Address,
) -> Option<Address> {
    let keychain_account_mismatch = |to: Option<Address>, input: Option<&Bytes>| {
        if to != Some(ACCOUNT_KEYCHAIN_ADDRESS) {
            return None;
        }

        let input = input?;
        // selector (4 bytes) + one 32-byte ABI word for `account`.
        if input.len() < 36 {
            return None;
        }
        let selector: [u8; 4] = input[..4].try_into().ok()?;
        if !KEYCHAIN_ACCOUNT_SCOPED_SELECTORS.contains(&selector) {
            return None;
        }

        let account = Address::from_slice(&input[16..36]);
        (account != caller).then_some(account)
    };

    if let Some(account) = keychain_account_mismatch(
        TransactionBuilder::to(request),
        TransactionBuilder::input(request),
    ) {
        return Some(account);
    }

    request.calls.iter().find_map(|call| {
        let to = match call.to {
            TxKind::Call(to) => Some(to),
            TxKind::Create => None,
        };
        keychain_account_mismatch(to, Some(&call.input))
    })
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
    use tempo_contracts::precompiles::{ACCOUNT_KEYCHAIN_ADDRESS, account_keychain::IAccountKeychain};
    use tempo_primitives::transaction::Call;
    use tempo_zone_contracts::{ZONE_INBOX_ADDRESS, ZONE_TOKEN_ADDRESS, ZoneInbox};

    use super::{
        account_keychain_mismatched_account, enforce_contract_creation,
        enforce_contract_creation_with_allowlist, zone_inbox_refunds_mismatched_owner,
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

    fn account_keychain_get_key_request(account: Address) -> TempoTransactionRequest {
        TempoTransactionRequest {
            inner: TransactionRequest {
                to: Some(TxKind::Call(ACCOUNT_KEYCHAIN_ADDRESS)),
                input: TransactionInput::new(
                    IAccountKeychain::getKeyCall {
                        account,
                        keyId: Address::repeat_byte(0x09),
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
    fn account_keychain_detects_other_account_read() {
        let caller = Address::repeat_byte(0x11);
        let victim = Address::repeat_byte(0x22);
        let request = account_keychain_get_key_request(victim);

        assert_eq!(
            account_keychain_mismatched_account(&request, caller),
            Some(victim)
        );
    }

    #[test]
    fn account_keychain_allows_own_read() {
        let caller = Address::repeat_byte(0x11);
        let request = account_keychain_get_key_request(caller);

        assert_eq!(account_keychain_mismatched_account(&request, caller), None);
    }

    #[test]
    fn account_keychain_detects_nested_other_account_read() {
        let caller = Address::repeat_byte(0x11);
        let victim = Address::repeat_byte(0x22);
        let mut request = call_request(Some(TxKind::Call(Address::repeat_byte(0x33))));
        request.calls.push(Call {
            to: TxKind::Call(ACCOUNT_KEYCHAIN_ADDRESS),
            value: U256::ZERO,
            input: IAccountKeychain::getKeyCall {
                account: victim,
                keyId: Address::repeat_byte(0x09),
            }
            .abi_encode()
            .into(),
        });

        assert_eq!(
            account_keychain_mismatched_account(&request, caller),
            Some(victim)
        );
    }

    #[test]
    fn account_keychain_ignores_non_keychain_target() {
        let caller = Address::repeat_byte(0x11);
        let mut request = account_keychain_get_key_request(Address::repeat_byte(0x22));
        request.inner.to = Some(TxKind::Call(Address::repeat_byte(0x33)));

        assert_eq!(account_keychain_mismatched_account(&request, caller), None);
    }
}
