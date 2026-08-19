//! Checker metrics and verified protocol activity logs.

use reth_metrics::{
    Metrics,
    metrics::{Counter, Gauge},
};

use crate::{
    l1::{L1BlockEvidence, L1PortalEvent},
    l2::{L2BlockEvidence, L2BridgeEvent},
    persistence::{BlockRef, Snapshot, Status},
};

#[derive(Metrics, Clone)]
#[metrics(scope = "tempo_zone_checker")]
pub(crate) struct CheckerMetrics {
    /// Highest durably verified Zone block.
    pub(crate) verified_zone_height: Gauge,
    /// Tempo block imported by the verified Zone tip.
    pub(crate) imported_tempo_height: Gauge,
    /// Highest Zone block delivered to the checker.
    pub(crate) observed_zone_height: Gauge,
    /// Delivered blocks not yet verified.
    pub(crate) verification_lag_blocks: Gauge,
    /// One when a deterministic finding has stopped verification.
    pub(crate) divergence_active: Gauge,
    /// Number of transient acquisition retries.
    pub(crate) acquisition_retries_total: Counter,
    /// Number of verified Zone blocks.
    pub(crate) verified_zone_blocks_total: Counter,
    /// Number of deep reorg rebuilds.
    pub(crate) recovery_rebuilds_total: Counter,
}

impl CheckerMetrics {
    /// Publish the latest durable checker state.
    pub(crate) fn update(&self, snapshot: &Snapshot) {
        let verified = snapshot.metadata.verified_zone.number;
        let observed = snapshot.metadata.observed_zone.number;
        self.verified_zone_height.set(verified as f64);
        self.imported_tempo_height
            .set(snapshot.metadata.imported_tempo.number as f64);
        self.observed_zone_height.set(observed as f64);
        self.verification_lag_blocks
            .set(observed.saturating_sub(verified) as f64);
        self.divergence_active.set(
            if matches!(snapshot.metadata.status, Status::Diverged { .. }) {
                1.0
            } else {
                0.0
            },
        );
    }
}

/// Log authenticated protocol activity after a Zone block is verified.
pub(crate) fn log_verified_activity(tempo: &L1BlockEvidence, l2: &L2BlockEvidence, zone: BlockRef) {
    let tempo_block = tempo.block().number;
    for event in tempo.portal_events() {
        log_tempo_event(event, zone, tempo_block);
    }
    for event in l2.bridge_events() {
        log_zone_event(event, zone);
    }
}

fn log_tempo_event(event: &L1PortalEvent, zone: BlockRef, tempo_block: u64) {
    match event {
        L1PortalEvent::DepositMade {
            token,
            net_amount,
            deposit_number,
        } => tracing::info!(
            target: "zone::checker",
            zone_block = zone.number,
            tempo_block,
            %token,
            amount = net_amount,
            deposit_number,
            "accounted authenticated Portal deposit"
        ),
        L1PortalEvent::TokenEnabled { token } => tracing::info!(
            target: "zone::checker",
            zone_block = zone.number,
            tempo_block,
            %token,
            "added Portal token to accounting coverage"
        ),
        L1PortalEvent::WithdrawalProcessed {
            to,
            token,
            amount,
            callback_success,
        } => tracing::info!(
            target: "zone::checker",
            zone_block = zone.number,
            tempo_block,
            %token,
            recipient = %to,
            amount,
            callback_success,
            "accounted authenticated Portal withdrawal"
        ),
        L1PortalEvent::WithdrawalBounceBack { token, amount } => tracing::info!(
            target: "zone::checker",
            zone_block = zone.number,
            tempo_block,
            %token,
            amount,
            "observed authenticated Portal withdrawal bounce-back"
        ),
        L1PortalEvent::DepositBounceBack { token, amount, .. } => tracing::info!(
            target: "zone::checker",
            zone_block = zone.number,
            tempo_block,
            %token,
            amount,
            "accounted authenticated Portal deposit bounce-back"
        ),
        L1PortalEvent::DepositBounceBackPending { token, amount, .. } => tracing::info!(
            target: "zone::checker",
            zone_block = zone.number,
            tempo_block,
            %token,
            amount,
            "accounted authenticated pending Portal deposit bounce-back"
        ),
        L1PortalEvent::RefundClaimed {
            recipient,
            token,
            amount,
        } => tracing::info!(
            target: "zone::checker",
            zone_block = zone.number,
            tempo_block,
            %token,
            %recipient,
            amount,
            "accounted authenticated Portal refund"
        ),
        L1PortalEvent::BatchSubmitted => {}
    }
}

fn log_zone_event(event: &L2BridgeEvent, zone: BlockRef) {
    match event {
        L2BridgeEvent::DepositOutcome {
            recipient,
            token,
            amount,
            processed: true,
            ..
        } => tracing::info!(
            target: "zone::checker",
            zone_block = zone.number,
            %token,
            recipient = ?recipient,
            amount,
            "verified Zone deposit mint"
        ),
        L2BridgeEvent::DepositOutcome {
            token,
            amount,
            processed: false,
            ..
        } => tracing::info!(
            target: "zone::checker",
            zone_block = zone.number,
            %token,
            amount,
            "verified Zone deposit failure"
        ),
        L2BridgeEvent::WithdrawalRequested {
            withdrawal_index,
            sender,
            token,
            principal,
            fee,
            is_deposit_bounce_back: false,
            ..
        } => tracing::info!(
            target: "zone::checker",
            zone_block = zone.number,
            %token,
            %sender,
            principal,
            fee,
            withdrawal_index,
            "verified Zone withdrawal debit and burn"
        ),
        L2BridgeEvent::WithdrawalRequested {
            withdrawal_index,
            token,
            principal,
            is_deposit_bounce_back: true,
            ..
        } => tracing::info!(
            target: "zone::checker",
            zone_block = zone.number,
            %token,
            amount = principal,
            withdrawal_index,
            "accounted authenticated Zone deposit bounce-back request"
        ),
        L2BridgeEvent::WithdrawalBounceBack {
            recipient,
            token,
            amount,
            processed: true,
        } => tracing::info!(
            target: "zone::checker",
            zone_block = zone.number,
            %token,
            %recipient,
            amount,
            "verified Zone withdrawal bounce-back mint"
        ),
        L2BridgeEvent::WithdrawalBounceBack {
            recipient,
            token,
            amount,
            processed: false,
        } => tracing::info!(
            target: "zone::checker",
            zone_block = zone.number,
            %token,
            %recipient,
            amount,
            "verified pending Zone withdrawal bounce-back"
        ),
        L2BridgeEvent::RefundClaimed {
            recipient,
            token,
            amount,
        } => tracing::info!(
            target: "zone::checker",
            zone_block = zone.number,
            %token,
            %recipient,
            amount,
            "verified Zone refund mint"
        ),
        L2BridgeEvent::TempoAdvanced(_)
        | L2BridgeEvent::Transfer { .. }
        | L2BridgeEvent::TokenBurn { .. }
        | L2BridgeEvent::BatchFinalized { .. } => {}
    }
}
