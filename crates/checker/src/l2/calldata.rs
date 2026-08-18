//! Strict `advanceTempo` calldata evidence extraction.

use alloy_consensus::{Transaction, TxReceipt, transaction::TxHashRef};
use alloy_primitives::{Address, B256, Bytes, Log};
use alloy_sol_types::SolCall;
use eyre::WrapErr as _;
use tempo_zone_contracts::{IZoneInbox, ZONE_INBOX_ADDRESS};

/// One decoded `advanceTempo` call and its canonical transaction provenance.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct L2AdvanceTempoEvidence {
    pub(crate) transaction_hash: B256,
    pub(crate) transaction_index: u32,
    pub(crate) sender: Address,
    pub(crate) target: Address,
    pub(crate) success: bool,
    pub(crate) raw_input: Bytes,
    pub(crate) header: Bytes,
    pub(crate) deposits: Vec<IZoneInbox::QueuedDeposit>,
    pub(crate) decryptions: Vec<IZoneInbox::DecryptionData>,
    pub(crate) enabled_tokens: Vec<IZoneInbox::EnabledToken>,
}

/// Decode `advanceTempo` from one transaction, ignoring unrelated calls.
pub(super) fn extract<T, R>(
    transaction_index: usize,
    transaction: &T,
    sender: Address,
    receipt: &R,
    block: u64,
) -> eyre::Result<Option<L2AdvanceTempoEvidence>>
where
    T: Transaction + TxHashRef,
    R: TxReceipt<Log = Log>,
{
    let Some(target) = transaction.to() else {
        return Ok(None);
    };
    if target != ZONE_INBOX_ADDRESS {
        return Ok(None);
    }
    let input = transaction.input();
    if input.len() < 4 || input[..4] != IZoneInbox::advanceTempoCall::SELECTOR {
        return Ok(None);
    }
    let call = IZoneInbox::advanceTempoCall::abi_decode(input.as_ref())
        .wrap_err_with(|| format!("malformed advanceTempo calldata in block {block}"))?;
    eyre::ensure!(
        call.abi_encode() == input.as_ref(),
        "non-canonical advanceTempo encoding in block {block}"
    );
    Ok(Some(L2AdvanceTempoEvidence {
        transaction_hash: *transaction.tx_hash(),
        transaction_index: transaction_index as u32,
        sender,
        target,
        success: receipt.status(),
        raw_input: input.clone(),
        header: call.header,
        deposits: call.deposits,
        decryptions: call.decryptions,
        enabled_tokens: call.enabledTokens,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{SignableTransaction as _, TxLegacy, TxType};
    use alloy_primitives::{Signature, TxKind, U256, address};
    use reth_ethereum_primitives::Receipt;

    const TOKEN_A: Address = address!("0x20C00000000000000000000000000000000000a1");
    const TOKEN_B: Address = address!("0x20C00000000000000000000000000000000000b2");

    fn tx_to(target: Address, input: Bytes) -> alloy_consensus::Signed<TxLegacy> {
        TxLegacy {
            to: TxKind::Call(target),
            input,
            ..Default::default()
        }
        .into_signed(Signature::new(U256::from(1), U256::from(1), false))
    }

    fn tx(input: Bytes) -> alloy_consensus::Signed<TxLegacy> {
        tx_to(ZONE_INBOX_ADDRESS, input)
    }

    fn receipt(success: bool) -> Receipt {
        Receipt {
            tx_type: TxType::Legacy,
            success,
            cumulative_gas_used: 0,
            logs: vec![],
        }
    }

    fn call(tokens: &[Address]) -> IZoneInbox::advanceTempoCall {
        IZoneInbox::advanceTempoCall {
            header: Bytes::from_static(b"header"),
            deposits: vec![],
            decryptions: vec![],
            enabledTokens: tokens
                .iter()
                .map(|token| IZoneInbox::EnabledToken {
                    token: *token,
                    name: "Token".into(),
                    symbol: "T".into(),
                    currency: "USD".into(),
                })
                .collect(),
        }
    }

    fn collect<T, R>(
        transactions: &[T],
        senders: &[Address],
        receipts: &[R],
        block: u64,
    ) -> eyre::Result<Vec<L2AdvanceTempoEvidence>>
    where
        T: Transaction + TxHashRef,
        R: TxReceipt<Log = Log>,
    {
        transactions
            .iter()
            .zip(senders)
            .zip(receipts)
            .enumerate()
            .filter_map(|(index, ((transaction, sender), receipt))| {
                extract(index, transaction, *sender, receipt, block).transpose()
            })
            .collect()
    }

    #[test]
    fn strict_decode_retains_failed_call() {
        let call = call(&[]);
        let evidence = collect(
            &[tx(call.abi_encode().into())],
            &[Address::ZERO],
            &[receipt(false)],
            1,
        )
        .unwrap();
        assert_eq!(evidence.len(), 1);
        assert!(!evidence[0].success);
        let mut trailing = call.abi_encode();
        trailing.extend([0; 32]);
        assert!(
            collect(
                &[tx(trailing.into())],
                &[Address::ZERO],
                &[receipt(true)],
                1
            )
            .unwrap_err()
            .to_string()
            .contains("non-canonical")
        );
    }

    #[test]
    fn preserves_complete_call_and_canonical_order() {
        let first = tx(call(&[TOKEN_A, TOKEN_B]).abi_encode().into());
        let second = tx(call(&[TOKEN_B]).abi_encode().into());
        let senders = [Address::repeat_byte(1), Address::repeat_byte(2)];
        let evidence = collect(
            &[first.clone(), second],
            &senders,
            &[receipt(true), receipt(true)],
            1,
        )
        .unwrap();

        assert_eq!(evidence.len(), 2);
        assert_eq!(evidence[0].transaction_hash, *first.tx_hash());
        assert_eq!(evidence[0].transaction_index, 0);
        assert_eq!(evidence[0].sender, senders[0]);
        assert_eq!(evidence[0].target, ZONE_INBOX_ADDRESS);
        assert_eq!(evidence[0].header, Bytes::from_static(b"header"));
        assert_eq!(evidence[0].enabled_tokens[0].token, TOKEN_A);
        assert_eq!(evidence[0].enabled_tokens[1].token, TOKEN_B);
        assert_eq!(evidence[1].transaction_index, 1);
        assert_eq!(evidence[1].enabled_tokens[0].token, TOKEN_B);
        assert_eq!(evidence[0].raw_input, first.input().clone());
    }

    #[test]
    fn malformed_known_call_fails_and_unrelated_calls_are_ignored() {
        let mut malformed = call(&[]).abi_encode();
        malformed[10] = 0xff;
        assert!(
            collect(
                &[tx(malformed.into())],
                &[Address::ZERO],
                &[receipt(true)],
                1,
            )
            .unwrap_err()
            .to_string()
            .contains("malformed advanceTempo")
        );

        let evidence = collect(
            &[
                tx_to(Address::repeat_byte(9), call(&[]).abi_encode().into()),
                tx(Bytes::from_static(b"unknown selector")),
            ],
            &[Address::ZERO, Address::ZERO],
            &[receipt(true), receipt(true)],
            1,
        )
        .unwrap();
        assert!(evidence.is_empty());
    }
}
