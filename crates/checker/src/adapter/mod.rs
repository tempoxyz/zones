//! Converts authenticated observations into independent checker facts and effects.

use crate::kernel::{Effect, ExpectedState, ImportedFacts};
use alloy_consensus::BlockHeader as _;
use alloy_primitives::B256;

use crate::{
    failure::Failure,
    observe::{L1BlockObservation, L2BlockObservation, ZonePostStateOutputs},
    persistence::BlockNumHash,
    runtime::{AuthenticatedBlock, AuthenticatedOutputs},
};

mod deposits;
mod tempo;
mod zone;

use tempo::facts as imported_facts;
use zone::facts as zone_facts;

/// Authenticated L1, L2, and post-state inputs for one Zone block.
pub(crate) struct AuthenticatedObservation {
    pub l2: L2BlockObservation,
    pub l1: Vec<L1BlockObservation>,
    pub state: ZonePostStateOutputs,
    pub portal_creation_block_hash: B256,
    pub zone_id: u32,
}

/// Facts and effects derived from one authenticated Tempo block.
pub(crate) struct ImportedAdaptation {
    pub(crate) facts: ImportedFacts,
    pub(crate) effects: Vec<Effect>,
}

/// Facts and effects derived from one authenticated Zone block.
pub(super) struct ZoneAdaptation {
    pub(super) facts: crate::kernel::ZoneFacts,
    pub(super) effects: Vec<Effect>,
}

/// Stable finding codes emitted while adapting authenticated observations.
#[derive(Debug, Clone, Copy)]
#[repr(u16)]
pub(crate) enum AdapterFindingCode {
    HeaderSequence = 100,
    Grammar = 200,
}

impl AdapterFindingCode {
    /// Build an authenticated-divergence failure for this adapter invariant.
    fn failure(self, message: impl Into<String>) -> Failure {
        Failure::authenticated_divergence(
            message,
            crate::kernel::Finding::coded(
                crate::kernel::FindingCategory::Observation,
                self as u16,
                crate::kernel::FindingLocation::Block,
            ),
        )
    }
}

/// Adapt one authenticated Zone block into kernel inputs and independent outputs.
pub(crate) fn adapt(o: &AuthenticatedObservation) -> Result<AuthenticatedBlock, Failure> {
    let header = o.l2.inputs().advance_tempo().imported_header();
    if o.l1.len() != 1 {
        return Err(AdapterFindingCode::HeaderSequence
            .failure("advanceTempo requires exactly one Tempo observation"));
    }
    let observation = o.l1.first().ok_or_else(|| {
        AdapterFindingCode::HeaderSequence
            .failure("advanceTempo requires exactly one Tempo observation")
    })?;
    let ImportedAdaptation {
        facts: imported_facts,
        effects: mut imported_effects,
    } = adapt_imported(observation, header, o.portal_creation_block_hash, o.zone_id)?;
    let ZoneAdaptation {
        facts: zone_facts,
        effects: mut zone_effects,
    } = zone_facts(o)?;
    imported_effects.append(&mut zone_effects);
    let state = ExpectedState {
        tempo_block_hash: o.state.tempo_block_hash,
        tempo_block_number: o.state.tempo_block_number,
        processed_deposit_hash: o.state.processed_deposit_queue_hash,
        processed_deposit_number: o.state.processed_deposit_number,
        withdrawal_queue_hash: o.state.withdrawal_queue_hash,
        withdrawal_batch_index: o.state.withdrawal_batch_index,
    };
    Ok(AuthenticatedBlock {
        zone: BlockNumHash {
            number: o.l2.block_number(),
            hash: o.l2.block_hash(),
        },
        parent: BlockNumHash {
            number: o.l2.block_number().checked_sub(1).ok_or_else(|| {
                AdapterFindingCode::HeaderSequence.failure("Zone genesis has no parent")
            })?,
            hash: o.l2.parent_hash(),
        },
        tempo: BlockNumHash {
            number: observation.block_number(),
            hash: observation.block_hash(),
        },
        tempo_parent: BlockNumHash {
            number: header.number().checked_sub(1).ok_or_else(|| {
                AdapterFindingCode::HeaderSequence.failure("imported genesis has no parent")
            })?,
            hash: header.header().parent_hash(),
        },
        imported: imported_facts,
        zone_facts,
        outputs: AuthenticatedOutputs {
            effects: imported_effects,
            state,
            supplies: o.state.token_supplies.clone(),
        },
    })
}

/// Adapt one authenticated imported Tempo block for bootstrap or live checking.
pub(crate) fn adapt_imported(
    observation: &L1BlockObservation,
    header: &crate::observe::ImportedTempoHeader,
    portal_creation_block_hash: B256,
    zone_id: u32,
) -> Result<ImportedAdaptation, Failure> {
    if (observation.block_hash(), observation.block_number()) != (header.hash(), header.number()) {
        return Err(
            AdapterFindingCode::Grammar.failure("Tempo observation does not match imported header")
        );
    }
    imported_facts(observation, header, portal_creation_block_hash, zone_id)
}
