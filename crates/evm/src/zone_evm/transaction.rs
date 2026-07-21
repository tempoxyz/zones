//! Zone transaction parsing and user-call policy.

use alloy_primitives::{Address, Bytes, TxKind};
use alloy_sol_types::SolCall;
use revm::context::Transaction;
use tempo_precompiles::tip20::{ITIP20, is_tip20_prefix};
use tempo_revm::{TempoInvalidTransaction, TempoTxEnv};

use super::contract_creation;

/// A transaction whose zone-level call policy has been parsed successfully.
///
/// Constructing this value proves that the transaction does not perform forbidden contract
/// creation and that every direct TIP-20 call is one of the user operations exposed by a zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedZoneTransaction {
    calls: Vec<ValidatedZoneCall>,
}

impl ValidatedZoneTransaction {
    /// Return the parsed calls in transaction order.
    pub fn calls(&self) -> &[ValidatedZoneCall] {
        &self.calls
    }
}

/// A direct call accepted by the zone transaction policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatedZoneCall {
    /// An allowed TIP-20 operation targeting `token`.
    Tip20 {
        token: Address,
        operation: AllowedTip20Operation,
    },
    /// A call outside the TIP-20 address space.
    Other { target: TxKind },
}

/// TIP-20 operations users may submit directly to a zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowedTip20Operation {
    TransferFrom,
    Approve,
}

/// Parse and enforce all stateless zone transaction invariants.
pub fn validate_transaction(
    tx: &TempoTxEnv,
    contract_deployer_allowlist: &[Address],
) -> Result<ValidatedZoneTransaction, TempoInvalidTransaction> {
    contract_creation::validate_transaction(tx, contract_deployer_allowlist)?;

    let calls = if let Some(aa) = tx.tempo_tx_env.as_ref() {
        aa.aa_calls
            .iter()
            .map(|call| parse_call(call.to, &call.input))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        vec![parse_call(tx.kind(), &tx.data)?]
    };

    Ok(ValidatedZoneTransaction { calls })
}

fn parse_call(target: TxKind, input: &Bytes) -> Result<ValidatedZoneCall, TempoInvalidTransaction> {
    let TxKind::Call(address) = target else {
        // Contract creation has already been checked against the deployer allowlist.
        return Ok(ValidatedZoneCall::Other { target });
    };

    if !is_tip20_prefix(address) {
        return Ok(ValidatedZoneCall::Other { target });
    }

    let operation = if input.starts_with(&ITIP20::transferFromCall::SELECTOR) {
        ITIP20::transferFromCall::abi_decode(input).map_err(|_| {
            TempoInvalidTransaction::CallsValidation("malformed TIP-20 transferFrom call")
        })?;
        AllowedTip20Operation::TransferFrom
    } else if input.starts_with(&ITIP20::approveCall::SELECTOR) {
        ITIP20::approveCall::abi_decode(input).map_err(|_| {
            TempoInvalidTransaction::CallsValidation("malformed TIP-20 approve call")
        })?;
        AllowedTip20Operation::Approve
    } else {
        return Err(TempoInvalidTransaction::CallsValidation(
            "TIP-20 operation is not allowed on a zone",
        ));
    };

    Ok(ValidatedZoneCall::Tip20 {
        token: address,
        operation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{U256, address};
    use tempo_primitives::transaction::Call;
    use tempo_revm::TempoBatchCallEnv;

    const TOKEN: Address = address!("0x20C0000000000000000000000000000000000001");

    fn call_tx(target: Address, input: Bytes) -> TempoTxEnv {
        TempoTxEnv {
            inner: revm::context::TxEnv {
                kind: TxKind::Call(target),
                data: input,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn parses_allowed_tip20_operations() {
        let transfer = ITIP20::transferFromCall {
            from: Address::repeat_byte(0x11),
            to: Address::repeat_byte(0x22),
            amount: U256::from(7),
        };
        let approve = ITIP20::approveCall {
            spender: Address::repeat_byte(0x33),
            amount: U256::from(9),
        };

        for (input, expected) in [
            (
                Bytes::from(transfer.abi_encode()),
                AllowedTip20Operation::TransferFrom,
            ),
            (
                Bytes::from(approve.abi_encode()),
                AllowedTip20Operation::Approve,
            ),
        ] {
            let parsed = validate_transaction(&call_tx(TOKEN, input), &[]).unwrap();
            assert_eq!(
                parsed.calls(),
                &[ValidatedZoneCall::Tip20 {
                    token: TOKEN,
                    operation: expected,
                }]
            );
        }
    }

    #[test]
    fn rejects_other_and_malformed_tip20_operations() {
        let transfer = ITIP20::transferCall {
            to: Address::repeat_byte(0x22),
            amount: U256::from(7),
        };

        assert!(validate_transaction(&call_tx(TOKEN, transfer.abi_encode().into()), &[]).is_err());
        assert!(
            validate_transaction(
                &call_tx(TOKEN, ITIP20::approveCall::SELECTOR.to_vec().into()),
                &[],
            )
            .is_err()
        );
    }

    #[test]
    fn parses_every_aa_call() {
        let allowed = ITIP20::approveCall {
            spender: Address::repeat_byte(0x33),
            amount: U256::from(9),
        };
        let forbidden = ITIP20::mintCall {
            to: Address::repeat_byte(0x44),
            amount: U256::from(1),
        };
        let tx = TempoTxEnv {
            tempo_tx_env: Some(Box::new(TempoBatchCallEnv {
                aa_calls: vec![
                    Call {
                        to: TxKind::Call(TOKEN),
                        value: U256::ZERO,
                        input: allowed.abi_encode().into(),
                    },
                    Call {
                        to: TxKind::Call(TOKEN),
                        value: U256::ZERO,
                        input: forbidden.abi_encode().into(),
                    },
                ],
                ..Default::default()
            })),
            ..Default::default()
        };

        assert!(validate_transaction(&tx, &[]).is_err());
    }

    #[test]
    fn permits_non_tip20_bridge_calls() {
        let target = Address::repeat_byte(0x1c);
        let parsed = validate_transaction(&call_tx(target, Bytes::new()), &[]).unwrap();
        assert_eq!(
            parsed.calls(),
            &[ValidatedZoneCall::Other {
                target: TxKind::Call(target),
            }]
        );
    }
}
