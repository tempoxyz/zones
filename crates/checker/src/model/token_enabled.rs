//! Pure token-enablement spec model.
//!
//! Derives expected Zone L2 outputs from the canonical L1 input — ordered
//! `ZonePortal.TokenEnabled` events from the exact anchored L1 block — and
//! compares them against observed L2 evidence (calldata, events, and
//! exact-block state).
//!
//! The model is pure: no I/O, no logging, no external mutation. It depends
//! only on supplied evidence and returns deterministic typed output.

use alloy_primitives::Address;
use tempo_primitives::transaction::envelope::TEMPO_SYSTEM_TX_SENDER;

use crate::l1::L1BlockEvidence;
use crate::l2::{L2BlockEvidence, L2TokenStateEvidence};
use crate::model::{
    CalldataMismatch, StateMismatch, TokenModelResult, TokenModelViolation, TokenSpec,
};

// ---------------------------------------------------------------------------

/// Pure per-block model of the token-enablement transition.
pub(crate) struct TokenEnablementModel;

impl TokenEnablementModel {
    /// Evaluate every Tempo block imported by one Zone transition.
    pub(crate) fn evaluate_history(
        l1_history: &[L1BlockEvidence],
        l2_evidence: &L2BlockEvidence,
    ) -> TokenModelResult {
        let expected = l1_history
            .iter()
            .flat_map(L1BlockEvidence::token_enabled_specs)
            .collect::<Vec<_>>();
        let mut violations = Vec::new();

        check_duplicate_l1_tokens(&expected, &mut violations);

        check_l2_calldata(
            l2_evidence.advance_tempo_call_count(),
            &l2_evidence.successful_advance_tempo_provenance(),
            &l2_evidence.advance_tempo_enabled_token_specs(),
            &expected,
            &mut violations,
        );

        check_l2_events(
            &expected,
            &l2_evidence.token_enabled_specs(),
            &mut violations,
        );

        check_l2_state(&expected, l2_evidence.token_states(), &mut violations);

        result(expected.len(), violations)
    }
}

/// Reject duplicate canonical L1 token inputs within one block.
fn check_duplicate_l1_tokens(expected: &[TokenSpec], violations: &mut Vec<TokenModelViolation>) {
    let mut first_seen = std::collections::HashMap::new();
    for (index, spec) in expected.iter().enumerate() {
        if let Some(&first_index) = first_seen.get(&spec.token) {
            violations.push(TokenModelViolation::DuplicateL1Token {
                token: spec.token,
                first_index,
                duplicate_index: index,
            });
        } else {
            first_seen.insert(spec.token, index);
        }
    }
}

/// Verify L2 `advanceTempo` calldata against expected tokens.
///
/// Protocol enforces exactly one successful `advanceTempo` per block (the
/// first transaction must be the system `advanceTempo`, a second is rejected,
/// and finishing without one is rejected).
fn check_l2_calldata(
    call_count: usize,
    successful_calls: &[(u32, Address)],
    calldata_tokens: &[TokenSpec],
    expected: &[TokenSpec],
    violations: &mut Vec<TokenModelViolation>,
) {
    if successful_calls.is_empty() {
        violations.push(TokenModelViolation::L2CalldataMismatch {
            reason: if call_count == 0 {
                CalldataMismatch::Missing
            } else {
                CalldataMismatch::Failed
            },
            expected: expected.to_vec(),
            observed: vec![],
        });
        return;
    }
    if successful_calls.len() > 1 {
        violations.push(TokenModelViolation::L2CalldataMismatch {
            reason: CalldataMismatch::Multiple,
            expected: expected.to_vec(),
            observed: calldata_tokens.to_vec(),
        });
        return;
    }
    if successful_calls[0] != (0, TEMPO_SYSTEM_TX_SENDER) {
        violations.push(TokenModelViolation::L2CalldataMismatch {
            reason: CalldataMismatch::InvalidProvenance,
            expected: expected.to_vec(),
            observed: calldata_tokens.to_vec(),
        });
    }
    if calldata_tokens != expected {
        violations.push(TokenModelViolation::L2CalldataMismatch {
            reason: CalldataMismatch::TokenSequence,
            expected: expected.to_vec(),
            observed: calldata_tokens.to_vec(),
        });
    }
}

/// Verify L2 token events against the canonical L1 inputs.
fn check_l2_events(
    expected: &[TokenSpec],
    observed: &[TokenSpec],
    violations: &mut Vec<TokenModelViolation>,
) {
    if observed != expected {
        violations.push(TokenModelViolation::L2EventMismatch {
            expected: expected.to_vec(),
            observed: observed.to_vec(),
        });
    }
}

/// Verify L2 token state for each expected token. Report unexpected observations.
fn check_l2_state(
    expected: &[TokenSpec],
    states: &[L2TokenStateEvidence],
    violations: &mut Vec<TokenModelViolation>,
) {
    for spec in expected {
        match states.iter().find(|s| s.token() == spec.token) {
            None => violations.push(TokenModelViolation::L2StateMismatch {
                token: spec.token,
                mismatch: StateMismatch::MissingObservation,
            }),
            Some(L2TokenStateEvidence::Present {
                initialized,
                name,
                symbol,
                currency,
                inbox_has_issuer_role,
                outbox_has_issuer_role,
                ..
            }) => {
                if !*initialized {
                    violations.push(TokenModelViolation::L2StateMismatch {
                        token: spec.token,
                        mismatch: StateMismatch::NotInitialized,
                    });
                }
                push_metadata_violations(
                    spec,
                    name,
                    symbol,
                    currency,
                    spec.token,
                    |m, token| TokenModelViolation::L2StateMismatch { token, mismatch: m },
                    violations,
                );
                if !*inbox_has_issuer_role {
                    violations.push(TokenModelViolation::L2StateMismatch {
                        token: spec.token,
                        mismatch: StateMismatch::MissingInboxRole,
                    });
                }
                if !*outbox_has_issuer_role {
                    violations.push(TokenModelViolation::L2StateMismatch {
                        token: spec.token,
                        mismatch: StateMismatch::MissingOutboxRole,
                    });
                }
            }
            Some(L2TokenStateEvidence::Absent { .. }) => {
                violations.push(TokenModelViolation::L2StateMismatch {
                    token: spec.token,
                    mismatch: StateMismatch::NotEnabled,
                });
            }
        }
    }
    for state in states {
        if !expected.iter().any(|s| s.token == state.token()) {
            violations.push(TokenModelViolation::L2StateMismatch {
                token: state.token(),
                mismatch: StateMismatch::UnexpectedObservation,
            });
        }
    }
}

/// Compare token metadata fields, pushing a `MetadataMismatch` violation for
/// each field that differs.
fn push_metadata_violations(
    spec: &TokenSpec,
    name: &str,
    symbol: &str,
    currency: &str,
    token: Address,
    make_violation: impl Fn(StateMismatch, Address) -> TokenModelViolation,
    violations: &mut Vec<TokenModelViolation>,
) {
    if name != spec.name {
        violations.push(make_violation(
            StateMismatch::MetadataMismatch {
                field: "name",
                expected: spec.name.clone(),
                observed: name.to_string(),
            },
            token,
        ));
    }
    if symbol != spec.symbol {
        violations.push(make_violation(
            StateMismatch::MetadataMismatch {
                field: "symbol",
                expected: spec.symbol.clone(),
                observed: symbol.to_string(),
            },
            token,
        ));
    }
    if currency != spec.currency {
        violations.push(make_violation(
            StateMismatch::MetadataMismatch {
                field: "currency",
                expected: spec.currency.clone(),
                observed: currency.to_string(),
            },
            token,
        ));
    }
}

/// Build the model result after all checks run.
fn result(token_count: usize, violations: Vec<TokenModelViolation>) -> TokenModelResult {
    if violations.is_empty() {
        TokenModelResult::Pass { token_count }
    } else {
        TokenModelResult::Violations(violations)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eips::BlockNumHash;
    use alloy_primitives::{Address, B256};

    const TOKEN_A: Address = Address::repeat_byte(0xa1);
    const TOKEN_B: Address = Address::repeat_byte(0xb2);
    const BLOCK: BlockNumHash = BlockNumHash::new(1, B256::repeat_byte(1));

    fn spec(token: Address) -> TokenSpec {
        TokenSpec {
            token,
            name: "T".into(),
            symbol: "T".into(),
            currency: "USD".into(),
        }
    }
    fn spec_with(token: Address, name: &str, symbol: &str, currency: &str) -> TokenSpec {
        TokenSpec {
            token,
            name: name.into(),
            symbol: symbol.into(),
            currency: currency.into(),
        }
    }
    fn l2_present(token: Address) -> L2TokenStateEvidence {
        L2TokenStateEvidence::Present {
            token,
            block: BLOCK,
            initialized: true,
            name: "T".into(),
            symbol: "T".into(),
            currency: "USD".into(),
            inbox_has_issuer_role: true,
            outbox_has_issuer_role: true,
        }
    }
    fn l2_present_meta(
        token: Address,
        name: &str,
        symbol: &str,
        currency: &str,
        inbox: bool,
        outbox: bool,
    ) -> L2TokenStateEvidence {
        L2TokenStateEvidence::Present {
            token,
            block: BLOCK,
            initialized: true,
            name: name.into(),
            symbol: symbol.into(),
            currency: currency.into(),
            inbox_has_issuer_role: inbox,
            outbox_has_issuer_role: outbox,
        }
    }
    fn l2_absent(token: Address) -> L2TokenStateEvidence {
        L2TokenStateEvidence::Absent {
            token,
            block: BLOCK,
        }
    }
    fn l2_uninitialized(token: Address) -> L2TokenStateEvidence {
        let mut state = l2_present(token);
        let L2TokenStateEvidence::Present { initialized, .. } = &mut state else {
            unreachable!()
        };
        *initialized = false;
        state
    }

    fn eval_l2(
        expected: &[TokenSpec],
        call_count: usize,
        success_count: usize,
        calldata: &[TokenSpec],
        events: &[TokenSpec],
        l2_states: &[L2TokenStateEvidence],
    ) -> TokenModelResult {
        let successful_calls = vec![(0, TEMPO_SYSTEM_TX_SENDER); success_count];
        let mut violations = Vec::new();
        check_l2_calldata(
            call_count,
            &successful_calls,
            calldata,
            expected,
            &mut violations,
        );
        check_l2_events(expected, events, &mut violations);
        check_l2_state(expected, l2_states, &mut violations);
        result(expected.len(), violations)
    }

    fn eval_duplicates(expected: &[TokenSpec]) -> TokenModelResult {
        let mut violations = Vec::new();
        check_duplicate_l1_tokens(expected, &mut violations);
        result(expected.len(), violations)
    }

    fn has_violation(
        result: &TokenModelResult,
        pred: impl Fn(&TokenModelViolation) -> bool,
    ) -> bool {
        matches!(result, TokenModelResult::Violations(vs) if vs.iter().any(pred))
    }
    fn is_pass(result: &TokenModelResult) -> bool {
        matches!(result, TokenModelResult::Pass { .. })
    }

    // -- Passing --

    #[test]
    fn pass_no_tokens() {
        assert!(is_pass(&eval_l2(&[], 1, 1, &[], &[], &[])));
    }

    #[test]
    fn pass_one_token() {
        assert!(is_pass(&eval_l2(
            &[spec(TOKEN_A)],
            1,
            1,
            &[spec(TOKEN_A)],
            &[spec(TOKEN_A)],
            &[l2_present(TOKEN_A)]
        )));
    }

    #[test]
    fn pass_multiple_tokens_in_order() {
        let specs = vec![spec(TOKEN_A), spec(TOKEN_B)];
        assert!(is_pass(&eval_l2(
            &specs,
            1,
            1,
            &specs.clone(),
            &specs.clone(),
            &[l2_present(TOKEN_A), l2_present(TOKEN_B)]
        )));
    }

    // -- Duplicate L1 token input --

    #[test]
    fn duplicate_l1_token_is_violation() {
        assert!(has_violation(
            &eval_duplicates(&[spec(TOKEN_A), spec(TOKEN_A)]),
            |v| matches!(
                v,
                TokenModelViolation::DuplicateL1Token {
                    token: TOKEN_A,
                    first_index: 0,
                    duplicate_index: 1,
                }
            )
        ));
    }

    // -- L2 calldata mismatch --

    #[test]
    fn l2_missing_advance_tempo() {
        assert!(has_violation(
            &eval_l2(
                &[spec(TOKEN_A)],
                0,
                0,
                &[],
                &[spec(TOKEN_A)],
                &[l2_present(TOKEN_A)]
            ),
            |v| matches!(
                v,
                TokenModelViolation::L2CalldataMismatch {
                    reason: CalldataMismatch::Missing,
                    ..
                }
            )
        ));
    }

    #[test]
    fn l2_failed_advance_tempo() {
        assert!(has_violation(
            &eval_l2(
                &[spec(TOKEN_A)],
                1,
                0,
                &[],
                &[spec(TOKEN_A)],
                &[l2_present(TOKEN_A)]
            ),
            |v| matches!(
                v,
                TokenModelViolation::L2CalldataMismatch {
                    reason: CalldataMismatch::Failed,
                    ..
                }
            )
        ));
    }

    #[test]
    fn l2_multiple_successful() {
        assert!(has_violation(
            &eval_l2(
                &[spec(TOKEN_A)],
                2,
                2,
                &[spec(TOKEN_A)],
                &[spec(TOKEN_A)],
                &[l2_present(TOKEN_A)]
            ),
            |v| matches!(
                v,
                TokenModelViolation::L2CalldataMismatch {
                    reason: CalldataMismatch::Multiple,
                    ..
                }
            )
        ));
    }

    #[test]
    fn l2_successful_call_must_be_opening_system_call() {
        let expected = [spec(TOKEN_A)];
        let mut violations = Vec::new();
        check_l2_calldata(
            1,
            &[(1, Address::repeat_byte(1))],
            &expected,
            &expected,
            &mut violations,
        );
        assert!(has_violation(
            &result(expected.len(), violations),
            |v| matches!(
                v,
                TokenModelViolation::L2CalldataMismatch {
                    reason: CalldataMismatch::InvalidProvenance,
                    ..
                }
            )
        ));
    }

    #[test]
    fn l2_calldata_missing_token() {
        assert!(has_violation(
            &eval_l2(
                &[spec(TOKEN_A)],
                1,
                1,
                &[],
                &[spec(TOKEN_A)],
                &[l2_present(TOKEN_A)]
            ),
            |v| matches!(
                v,
                TokenModelViolation::L2CalldataMismatch {
                    reason: CalldataMismatch::TokenSequence,
                    ..
                }
            )
        ));
    }

    #[test]
    fn l2_calldata_reversed() {
        assert!(has_violation(
            &eval_l2(
                &[spec(TOKEN_A), spec(TOKEN_B)],
                1,
                1,
                &[spec(TOKEN_B), spec(TOKEN_A)],
                &[spec(TOKEN_A), spec(TOKEN_B)],
                &[l2_present(TOKEN_A), l2_present(TOKEN_B)]
            ),
            |v| matches!(
                v,
                TokenModelViolation::L2CalldataMismatch {
                    reason: CalldataMismatch::TokenSequence,
                    ..
                }
            )
        ));
    }

    #[test]
    fn l2_calldata_metadata_mismatch() {
        assert!(has_violation(
            &eval_l2(
                &[spec(TOKEN_A)],
                1,
                1,
                &[spec_with(TOKEN_A, "X", "T", "USD")],
                &[spec(TOKEN_A)],
                &[l2_present(TOKEN_A)]
            ),
            |v| matches!(
                v,
                TokenModelViolation::L2CalldataMismatch {
                    reason: CalldataMismatch::TokenSequence,
                    ..
                }
            )
        ));
    }

    // -- L2 event mismatch --

    #[test]
    fn l2_event_missing() {
        assert!(has_violation(
            &eval_l2(
                &[spec(TOKEN_A)],
                1,
                1,
                &[spec(TOKEN_A)],
                &[],
                &[l2_present(TOKEN_A)]
            ),
            |v| matches!(v, TokenModelViolation::L2EventMismatch { .. })
        ));
    }

    #[test]
    fn l2_event_unexpected() {
        assert!(has_violation(
            &eval_l2(&[], 1, 1, &[], &[spec(TOKEN_A)], &[]),
            |v| matches!(v, TokenModelViolation::L2EventMismatch { .. })
        ));
    }

    #[test]
    fn l2_event_reversed() {
        assert!(has_violation(
            &eval_l2(
                &[spec(TOKEN_A), spec(TOKEN_B)],
                1,
                1,
                &[spec(TOKEN_A), spec(TOKEN_B)],
                &[spec(TOKEN_B), spec(TOKEN_A)],
                &[l2_present(TOKEN_A), l2_present(TOKEN_B)]
            ),
            |v| matches!(v, TokenModelViolation::L2EventMismatch { .. })
        ));
    }

    #[test]
    fn l2_event_metadata_mismatch() {
        assert!(has_violation(
            &eval_l2(
                &[spec(TOKEN_A)],
                1,
                1,
                &[spec(TOKEN_A)],
                &[spec_with(TOKEN_A, "X", "T", "USD")],
                &[l2_present(TOKEN_A)]
            ),
            |v| matches!(v, TokenModelViolation::L2EventMismatch { .. })
        ));
    }

    // -- L2 state mismatch --

    #[test]
    fn l2_state_absent() {
        assert!(has_violation(
            &eval_l2(
                &[spec(TOKEN_A)],
                1,
                1,
                &[spec(TOKEN_A)],
                &[spec(TOKEN_A)],
                &[l2_absent(TOKEN_A)]
            ),
            |v| matches!(
                v,
                TokenModelViolation::L2StateMismatch {
                    mismatch: StateMismatch::NotEnabled,
                    ..
                }
            )
        ));
    }

    #[test]
    fn l2_state_uninitialized() {
        assert!(has_violation(
            &eval_l2(
                &[spec(TOKEN_A)],
                1,
                1,
                &[spec(TOKEN_A)],
                &[spec(TOKEN_A)],
                &[l2_uninitialized(TOKEN_A)]
            ),
            |v| matches!(
                v,
                TokenModelViolation::L2StateMismatch {
                    mismatch: StateMismatch::NotInitialized,
                    ..
                }
            )
        ));
    }

    #[test]
    fn l2_state_missing() {
        assert!(has_violation(
            &eval_l2(
                &[spec(TOKEN_A)],
                1,
                1,
                &[spec(TOKEN_A)],
                &[spec(TOKEN_A)],
                &[]
            ),
            |v| matches!(
                v,
                TokenModelViolation::L2StateMismatch {
                    mismatch: StateMismatch::MissingObservation,
                    ..
                }
            )
        ));
    }

    #[test]
    fn l2_state_metadata_mismatch() {
        assert!(has_violation(
            &eval_l2(
                &[spec(TOKEN_A)],
                1,
                1,
                &[spec(TOKEN_A)],
                &[spec(TOKEN_A)],
                &[l2_present_meta(TOKEN_A, "X", "T", "USD", true, true)]
            ),
            |v| matches!(
                v,
                TokenModelViolation::L2StateMismatch {
                    mismatch: StateMismatch::MetadataMismatch { field: "name", .. },
                    ..
                }
            )
        ));
    }

    #[test]
    fn l2_state_missing_inbox_role() {
        assert!(has_violation(
            &eval_l2(
                &[spec(TOKEN_A)],
                1,
                1,
                &[spec(TOKEN_A)],
                &[spec(TOKEN_A)],
                &[l2_present_meta(TOKEN_A, "T", "T", "USD", false, true)]
            ),
            |v| matches!(
                v,
                TokenModelViolation::L2StateMismatch {
                    mismatch: StateMismatch::MissingInboxRole,
                    ..
                }
            )
        ));
    }

    #[test]
    fn l2_state_missing_outbox_role() {
        assert!(has_violation(
            &eval_l2(
                &[spec(TOKEN_A)],
                1,
                1,
                &[spec(TOKEN_A)],
                &[spec(TOKEN_A)],
                &[l2_present_meta(TOKEN_A, "T", "T", "USD", true, false)]
            ),
            |v| matches!(
                v,
                TokenModelViolation::L2StateMismatch {
                    mismatch: StateMismatch::MissingOutboxRole,
                    ..
                }
            )
        ));
    }

    #[test]
    fn l2_state_unexpected() {
        assert!(has_violation(
            &eval_l2(&[], 1, 1, &[], &[], &[l2_present(TOKEN_A)]),
            |v| matches!(
                v,
                TokenModelViolation::L2StateMismatch {
                    mismatch: StateMismatch::UnexpectedObservation,
                    ..
                }
            )
        ));
    }
}
