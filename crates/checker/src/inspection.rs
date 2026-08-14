//! Read-only inspection of durable checker progress and alert state.

use std::path::Path;

use alloy_eips::BlockNumHash;

use crate::{
    CheckerBlockedReason,
    persistence::{Coverage, Persistence},
};

/// Durable checker watermarks and alert state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckerSnapshot {
    /// Oldest Zone coordinate from which local reorg recovery is supported.
    pub recovery_zone_tip: BlockNumHash,
    /// Last Zone block whose checker transition committed durably.
    pub verified_zone_tip: BlockNumHash,
    /// Imported Tempo tip represented by the verified checker state.
    pub imported_tempo_tip: BlockNumHash,
    /// Latest canonical Zone head observed from the local node.
    pub observed_zone_tip: BlockNumHash,
    /// Whether observed Zone history remains to be verified.
    pub recovering: bool,
    /// Whether an authenticated divergence remains on the canonical branch.
    pub active_finding: bool,
    /// Number of divergences that were later removed from the canonical branch.
    pub cleared_findings: u64,
    /// Key of the most recently reorg-cleared finding retained in the database.
    pub last_cleared_finding: Option<CheckerFindingKey>,
    /// Whether descendants are durably marked as unchecked.
    pub has_coverage_gap: bool,
    /// Durable reason verification cannot resume automatically.
    pub blocked_reason: Option<CheckerBlockedReason>,
}

/// Stable operator-readable key for retained finding evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckerFindingKey {
    pub zone: BlockNumHash,
    pub operation: u32,
    pub code: u16,
}

/// Inspect a stopped checker database or a consistent copy.
pub fn inspect_database(path: impl AsRef<Path>) -> eyre::Result<CheckerSnapshot> {
    let snapshot = Persistence::inspect_snapshot(path)?;
    Ok(CheckerSnapshot {
        recovery_zone_tip: BlockNumHash::new(
            snapshot.meta.recovery_checkpoint.height,
            snapshot.meta.recovery_checkpoint.hash,
        ),
        verified_zone_tip: snapshot.meta.verified_zone_tip.into(),
        imported_tempo_tip: snapshot.meta.imported_tempo_tip.into(),
        observed_zone_tip: snapshot.meta.observed_zone_tip.into(),
        recovering: matches!(snapshot.meta.coverage, Coverage::Recovering),
        active_finding: snapshot.meta.active_finding.is_some(),
        cleared_findings: snapshot.meta.cleared_findings,
        last_cleared_finding: snapshot
            .meta
            .last_cleared_finding
            .map(|key| CheckerFindingKey {
                zone: key.zone.into(),
                operation: key.operation,
                code: key.code,
            }),
        has_coverage_gap: matches!(snapshot.meta.coverage, Coverage::Gap { .. }),
        blocked_reason: snapshot.meta.blocked,
    })
}
