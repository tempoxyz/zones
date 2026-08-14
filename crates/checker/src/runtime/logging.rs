//! Structured runtime tracing for checker state transitions.

use std::time::Duration;

use crate::{
    kernel::ImportedOperation,
    persistence::{BlockNumHash, Finding},
    runtime::AuthenticatedBlock,
};

pub(super) fn retry(zone: Option<BlockNumHash>, attempt: u32, delay: Duration, error: &str) {
    tracing::warn!(
        target: "zone::checker",
        zone_block = zone.map(|block| block.number),
        zone_hash = ?zone.map(|block| block.hash),
        attempt,
        retry_in_ms = delay.as_millis(),
        error,
        "checker acquisition failed; retrying"
    );
}

pub(super) fn terminal(error: &str) {
    tracing::error!(target: "zone::checker", error, "checker authentication failed permanently");
}

#[derive(Clone, Copy)]
struct BlockCoordinates {
    zone: BlockNumHash,
    tempo: BlockNumHash,
}

impl From<&AuthenticatedBlock> for BlockCoordinates {
    fn from(block: &AuthenticatedBlock) -> Self {
        Self {
            zone: block.zone,
            tempo: block.tempo,
        }
    }
}

pub(super) fn verified(block: &AuthenticatedBlock) {
    let coordinates = BlockCoordinates::from(block);
    tracing::debug!(
        target: "zone::checker",
        zone_block = coordinates.zone.number,
        zone_hash = %coordinates.zone.hash,
        tempo_block = coordinates.tempo.number,
        tempo_hash = %coordinates.tempo.hash,
        imported_operations = block.imported.operations.len(),
        enabled_tokens = block.zone_facts.enabled_tokens.len(),
        deposits = block.zone_facts.deposits.len(),
        deposit_outcomes = block.zone_facts.outcomes.len(),
        zone_operations = block.zone_facts.operations.len(),
        finalized_withdrawals = block
            .zone_facts
            .finalization
            .as_ref()
            .map_or(0, |finalization| finalization.declared_count),
        "verified Zone block"
    );
    for operation in &block.imported.operations {
        log_imported_milestone(coordinates, operation);
    }
}

pub(super) fn finding(finding: &Finding) {
    tracing::error!(
        target: "zone::checker",
        zone_block = finding.zone.number,
        zone_hash = %finding.zone.hash,
        tempo_block = finding.imported_tempo.map(|block| block.number),
        tempo_hash = ?finding.imported_tempo.map(|block| block.hash),
        category = ?finding.details.category,
        code = finding.details.code,
        location = ?finding.details.location,
        summary = finding.summary,
        "checker recorded authenticated divergence"
    );
}

/// Log rare, durable Portal configuration changes at the normal log level.
fn log_imported_milestone(coordinates: BlockCoordinates, operation: &ImportedOperation) {
    match operation {
        ImportedOperation::Create {
            identity,
            initial_token,
        } => {
            tracing::info!(target: "zone::checker", zone_block = coordinates.zone.number, tempo_block = coordinates.tempo.number, portal = %identity.portal, zone_id = identity.zone_id, "verified Portal creation");
            log_token_enablement(coordinates, initial_token);
        }
        ImportedOperation::EnableToken(token) => log_token_enablement(coordinates, token),
        _ => {}
    }
}

fn log_token_enablement(coordinates: BlockCoordinates, token: &crate::kernel::TokenEnable) {
    tracing::info!(target: "zone::checker", zone_block = coordinates.zone.number, tempo_block = coordinates.tempo.number, token = %token.token, name = %token.name, symbol = %token.symbol, currency = %token.currency, "verified token enablement");
}
