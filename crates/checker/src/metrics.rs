//! Alert-oriented checker metrics.

use crate::{CheckerBlockedReason, kernel::FindingCategory, persistence::Snapshot};

/// Publish alert state reconstructed from the durable checker snapshot.
pub(crate) fn set_snapshot(snapshot: &Snapshot) {
    metrics::gauge!("zone_checker_divergence_active")
        .set(f64::from(snapshot.meta.active_finding.is_some()));
    set_blocked(snapshot.meta.blocked);
}

/// Publish whether verification stopped for a non-divergence terminal condition.
pub(crate) fn set_blocked(reason: Option<CheckerBlockedReason>) {
    metrics::gauge!("zone_checker_blocked").set(f64::from(reason.is_some()));
}

/// Record a newly persisted authenticated divergence.
pub(crate) fn record_divergence(category: FindingCategory) {
    metrics::counter!(
        "zone_checker_divergences_total",
        "category" => category.as_label()
    )
    .increment(1);
    metrics::gauge!("zone_checker_divergence_active").set(1.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finding_category_labels_are_stable_snake_case() {
        assert_eq!(FindingCategory::Authentication.as_label(), "authentication");
        assert_eq!(
            FindingCategory::EffectMismatch.as_label(),
            "effect_mismatch"
        );
        assert_eq!(FindingCategory::StateMismatch.as_label(), "state_mismatch");
        assert_eq!(
            FindingCategory::CreationAnchor.as_label(),
            "creation_anchor"
        );
        assert_eq!(
            FindingCategory::SupplyMismatch.as_label(),
            "supply_mismatch"
        );
        assert_eq!(
            FindingCategory::CollateralMismatch.as_label(),
            "collateral_mismatch"
        );
    }
}
