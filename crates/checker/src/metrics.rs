//! Alert-oriented checker metrics.

use reth_metrics::{
    Metrics,
    metrics::{Counter, Gauge, Histogram},
};
use std::time::Duration;

use crate::{
    kernel::FindingCategory,
    persistence::{Coverage, Snapshot},
};

/// Metrics emitted by the checker runtime.
#[derive(Metrics, Clone)]
#[metrics(scope = "tempo_zone_checker")]
pub(crate) struct CheckerMetrics {
    /// Duration of one complete block authentication attempt.
    authentication_duration_seconds: Histogram,
    /// Authentication attempts retried because local or Tempo data was unavailable.
    acquisition_retries_total: Counter,
    /// Zone blocks whose checker transitions committed durably.
    verified_zone_blocks_total: Counter,
    /// Last Zone height whose checker transition committed durably.
    verified_zone_height: Gauge,
    /// Latest canonical Zone height observed from the local node.
    observed_zone_height: Gauge,
    /// Imported Tempo height represented by the verified checker state.
    imported_tempo_height: Gauge,
    /// Oldest Zone height from which local reorg recovery is supported.
    recovery_checkpoint_height: Gauge,
    /// Zone blocks observed locally but not yet verified.
    verification_lag_blocks: Gauge,
    /// Whether an authenticated divergence remains on the canonical branch.
    divergence_active: Gauge,
    /// Whether descendants are durably marked as unchecked.
    coverage_gap: Gauge,
    /// Whether canonical Zone history remains to be verified.
    recovering: Gauge,
    /// Whether a durable terminal condition prevents verification.
    blocked: Gauge,
}

impl CheckerMetrics {
    /// Publish the current lifecycle state before durable state is available.
    pub(crate) fn set_state(&self, state: CheckerState) {
        for candidate in CheckerState::ALL {
            StateMetric::new_with_labels(&[("state", candidate.label().to_owned())])
                .state
                .set(f64::from(candidate == state));
        }
    }

    /// Publish all alert state reconstructed from a durable checker snapshot.
    pub(crate) fn publish_snapshot(&self, snapshot: &Snapshot) {
        let meta = &snapshot.meta;
        self.verified_zone_height
            .set(meta.verified_zone_tip.number as f64);
        self.observed_zone_height
            .set(meta.observed_zone_tip.number as f64);
        self.imported_tempo_height
            .set(meta.imported_tempo_tip.number as f64);
        self.recovery_checkpoint_height
            .set(meta.recovery_checkpoint.height as f64);
        self.verification_lag_blocks.set(
            meta.observed_zone_tip
                .number
                .saturating_sub(meta.verified_zone_tip.number) as f64,
        );
        self.divergence_active
            .set(f64::from(meta.active_finding.is_some()));
        self.coverage_gap
            .set(f64::from(matches!(meta.coverage, Coverage::Gap { .. })));
        self.recovering
            .set(f64::from(matches!(meta.coverage, Coverage::Recovering)));
        self.blocked.set(f64::from(meta.blocked.is_some()));
        self.set_state(CheckerState::from_snapshot(snapshot));
    }

    /// Record a newly persisted authenticated divergence.
    pub(crate) fn record_divergence(&self, category: FindingCategory) {
        DivergenceMetric::new_with_labels(&[(
            "category",
            finding_category_label(category).to_owned(),
        )])
        .divergences_total
        .increment(1);
    }

    /// Record one complete block authentication attempt.
    pub(crate) fn record_authentication(&self, duration: Duration) {
        self.authentication_duration_seconds
            .record(duration.as_secs_f64());
    }

    /// Record an authentication attempt that will be retried.
    pub(crate) fn record_acquisition_retry(&self) {
        self.acquisition_retries_total.increment(1);
    }

    /// Record a Zone block whose checker transition committed durably.
    pub(crate) fn record_verified_block(&self) {
        self.verified_zone_blocks_total.increment(1);
    }
}

/// One mutually exclusive checker lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CheckerState {
    /// Building the initial authenticated checkpoint.
    Bootstrapping,
    /// Inspecting and opening the checker database.
    Opening,
    /// Connecting to Tempo before live verification starts.
    Connecting,
    /// Waiting before another checker startup or acquisition attempt.
    Retrying,
    /// Verifying canonical Zone history behind the local head.
    Recovering,
    /// Verification reaches the current local Zone head.
    Complete,
    /// An authenticated divergence prevents descendant verification.
    Diverged,
    /// A durable terminal condition prevents verification.
    Blocked,
    /// The database cannot be opened safely; notifications are being drained.
    Unavailable,
}

impl CheckerState {
    const ALL: [Self; 9] = [
        Self::Bootstrapping,
        Self::Opening,
        Self::Connecting,
        Self::Retrying,
        Self::Recovering,
        Self::Complete,
        Self::Diverged,
        Self::Blocked,
        Self::Unavailable,
    ];

    const fn from_snapshot(snapshot: &Snapshot) -> Self {
        if snapshot.meta.blocked.is_some() {
            return Self::Blocked;
        }
        match snapshot.meta.coverage {
            Coverage::Complete => Self::Complete,
            Coverage::Recovering => Self::Recovering,
            Coverage::Gap { .. } => Self::Diverged,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Bootstrapping => "bootstrapping",
            Self::Opening => "opening",
            Self::Connecting => "connecting",
            Self::Retrying => "retrying",
            Self::Recovering => "recovering",
            Self::Complete => "complete",
            Self::Diverged => "diverged",
            Self::Blocked => "blocked",
            Self::Unavailable => "unavailable",
        }
    }
}

/// The value of `tempo_zone_checker_state` for one bounded state label.
#[derive(Metrics)]
#[metrics(scope = "tempo_zone_checker")]
struct StateMetric {
    /// Whether the checker is currently in this lifecycle state.
    state: Gauge,
}

/// The value of `tempo_zone_checker_divergences_total` for one finding category.
#[derive(Metrics)]
#[metrics(scope = "tempo_zone_checker")]
struct DivergenceMetric {
    /// Number of durably recorded divergences in this category.
    divergences_total: Counter,
}

const fn finding_category_label(category: FindingCategory) -> &'static str {
    match category {
        FindingCategory::Authentication => "authentication",
        FindingCategory::EffectMismatch => "effect_mismatch",
        FindingCategory::StateMismatch => "state_mismatch",
        FindingCategory::Invariant => "invariant",
        FindingCategory::Unsupported => "unsupported",
        FindingCategory::Observation => "observation",
        FindingCategory::Continuity => "continuity",
        FindingCategory::CreationAnchor => "creation_anchor",
        FindingCategory::SupplyMismatch => "supply_mismatch",
        FindingCategory::CollateralMismatch => "collateral_mismatch",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finding_category_labels_are_stable_snake_case() {
        let labels = [
            (FindingCategory::Authentication, "authentication"),
            (FindingCategory::EffectMismatch, "effect_mismatch"),
            (FindingCategory::StateMismatch, "state_mismatch"),
            (FindingCategory::Invariant, "invariant"),
            (FindingCategory::Unsupported, "unsupported"),
            (FindingCategory::Observation, "observation"),
            (FindingCategory::Continuity, "continuity"),
            (FindingCategory::CreationAnchor, "creation_anchor"),
            (FindingCategory::SupplyMismatch, "supply_mismatch"),
            (FindingCategory::CollateralMismatch, "collateral_mismatch"),
        ];
        for (category, label) in labels {
            assert_eq!(finding_category_label(category), label);
        }
    }
}
