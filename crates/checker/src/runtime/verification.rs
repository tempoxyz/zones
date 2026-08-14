//! Verifies authenticated block observations against checker state.

use crate::{
    failure::{Failure, divergence},
    kernel::{
        Datum, Effect, FindingCategory, FindingLocation, State, TransitionCandidate,
        apply_imported, apply_zone,
    },
    persistence::Identity,
};
use alloy_primitives::keccak256;

use super::AuthenticatedBlock;

/// Verify one authenticated block against the current checker state.
pub(super) fn verify_block(
    identity: Identity,
    state: &State,
    block: &AuthenticatedBlock,
) -> Result<TransitionCandidate, Failure> {
    validate_creation_coordinate(identity, state, block)?;
    let candidate = apply_imported(state, &block.imported)
        .and_then(|imported| apply_zone(imported, &block.zone_facts))
        .map_err(|error| {
            divergence(
                FindingCategory::Invariant,
                1,
                Some(FindingLocation::Block),
                None,
                Some(Datum::Code(1)),
                error.to_string(),
            )
        })?;
    verify_outputs(block, &candidate)?;
    Ok(candidate)
}

/// Compare checker-derived effects, state commitments, and supply with observed output.
fn verify_outputs(
    block: &AuthenticatedBlock,
    candidate: &crate::kernel::TransitionCandidate,
) -> Result<(), Failure> {
    let effects = &candidate.expected_effects;
    let supplies = candidate
        .expected_accounting
        .iter()
        .map(|(token, a)| (*token, a.supply))
        .collect();
    if block.outputs.effects.as_slice() != effects.as_slice() {
        let index = block
            .outputs
            .effects
            .iter()
            .zip(effects)
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| block.outputs.effects.len().min(effects.len()));
        let expected_kind = effects.get(index).map_or("Missing", Effect::kind);
        let actual_kind = block
            .outputs
            .effects
            .get(index)
            .map_or("Missing", Effect::kind);
        let evidence = |effect: Option<&Effect>| {
            effect.map(|value| {
                let bytes = format!("{value:?}").into_bytes();
                Datum::Bytes {
                    length: bytes.len() as u64,
                    digest: keccak256(bytes),
                }
            })
        };
        return Err(divergence(
            FindingCategory::EffectMismatch,
            1,
            Some(FindingLocation::Operation(index as u32)),
            evidence(effects.get(index)),
            evidence(block.outputs.effects.get(index)),
            format!("effect mismatch: expected {expected_kind}, actual {actual_kind}"),
        ));
    }
    verify_commitment(
        20,
        Datum::Hash(candidate.expected_state.tempo_block_hash),
        Datum::Hash(block.outputs.state.tempo_block_hash),
    )?;
    verify_commitment(
        21,
        Datum::U64(candidate.expected_state.tempo_block_number),
        Datum::U64(block.outputs.state.tempo_block_number),
    )?;
    verify_commitment(
        22,
        Datum::Hash(candidate.expected_state.processed_deposit_hash),
        Datum::Hash(block.outputs.state.processed_deposit_hash),
    )?;
    verify_commitment(
        23,
        Datum::U64(candidate.expected_state.processed_deposit_number),
        Datum::U64(block.outputs.state.processed_deposit_number),
    )?;
    verify_commitment(
        24,
        Datum::Hash(candidate.expected_state.withdrawal_queue_hash),
        Datum::Hash(block.outputs.state.withdrawal_queue_hash),
    )?;
    verify_commitment(
        25,
        Datum::U64(candidate.expected_state.withdrawal_batch_index),
        Datum::U64(block.outputs.state.withdrawal_batch_index),
    )?;
    if block.outputs.supplies != supplies {
        let token = block
            .outputs
            .supplies
            .keys()
            .chain(supplies.keys())
            .find(|token| block.outputs.supplies.get(*token) != supplies.get(*token))
            .copied();
        let Some(token) = token else {
            return Err(divergence(
                FindingCategory::SupplyMismatch,
                30,
                None,
                None,
                None,
                "token supply maps differ without an identifiable key",
            ));
        };
        return Err(divergence(
            FindingCategory::SupplyMismatch,
            30,
            Some(FindingLocation::State(crate::kernel::StateKey::Token(
                token,
            ))),
            supplies.get(&token).copied().map(Datum::U256),
            block.outputs.supplies.get(&token).copied().map(Datum::U256),
            "token supply mismatch",
        ));
    }
    Ok(())
}

/// Compare one derived state commitment with its authenticated observation.
fn verify_commitment(code: u16, expected: Datum, observed: Datum) -> Result<(), Failure> {
    if expected == observed {
        return Ok(());
    }
    Err(divergence(
        FindingCategory::StateMismatch,
        code,
        Some(FindingLocation::Block),
        Some(expected),
        Some(observed),
        "state commitment mismatch",
    ))
}

fn validate_creation_coordinate(
    identity: Identity,
    state: &State,
    block: &AuthenticatedBlock,
) -> Result<(), Failure> {
    use crate::kernel::{ImportedOperation, PortalState};

    // Height zero is retained as the pre-creation-checkpoint sentinel.
    if identity.creation_height == 0 {
        return Ok(());
    }
    let creates = block
        .imported
        .operations
        .iter()
        .filter(|operation| matches!(operation, ImportedOperation::Create { .. }))
        .count();
    let valid = match state.portal() {
        Some(PortalState::AwaitingCreation(_)) if block.tempo.number < identity.creation_height => {
            creates == 0
        }
        Some(PortalState::AwaitingCreation(_))
            if block.tempo.number == identity.creation_height =>
        {
            block.tempo.hash == identity.creation_block && creates == 1
        }
        Some(PortalState::AwaitingCreation(_)) => false,
        _ => creates == 0,
    };
    if !valid {
        return Err(divergence(
            FindingCategory::CreationAnchor,
            1,
            Some(FindingLocation::Block),
            Some(Datum::Hash(identity.creation_block)),
            Some(Datum::Hash(block.tempo.hash)),
            "portal creation anchor mismatch",
        ));
    }
    Ok(())
}
