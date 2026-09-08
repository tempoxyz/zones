//! Process-lifetime P2P routes. Sequencing decisions belong to the local sequencer.

use tokio::sync::mpsc;
use zone_p2p::{BackfillRequest, BackfillResponse, P2pEvent};

/// Propagation and inbound routing for the lifetime of the node.
/// The sequencer supplies finalized blocks; this engine never selects a producer.
pub(crate) struct ZoneP2pEngine {
    pub events: mpsc::Receiver<P2pEvent>,
    pub backfill: mpsc::Receiver<BackfillResponse>,
}

pub(crate) struct P2pEventRoutes {
    pub blocks: mpsc::Sender<P2pEvent>,
    pub transactions: mpsc::Sender<P2pEvent>,
    pub signatures: mpsc::Sender<P2pEvent>,
    pub backfill: mpsc::Sender<BackfillResponse>,
}

impl ZoneP2pEngine {
    pub(crate) async fn run<P: crate::replication::PersistedBlockSource>(
        self,
        provider: P,
        commands: mpsc::Sender<zone_p2p::P2pCommand>,
        finalized: mpsc::Receiver<alloy_eips::NumHash>,
        routes: P2pEventRoutes,
    ) {
        let backfill = routes.backfill.clone();
        tokio::select! {
            () = route_events(self.events, routes) => panic!("P2P event routing stopped"),
            () = route_backfill_responses(self.backfill, backfill) => panic!("P2P backfill routing stopped"),
            result = crate::replication::broadcast_finalized_blocks(provider, commands, finalized) => {
                panic!("finalized block propagation stopped: {result:?}");
            }
        }
    }
}

pub(crate) async fn route_events(mut events: mpsc::Receiver<P2pEvent>, routes: P2pEventRoutes) {
    while let Some(event) = events.recv().await {
        let sink = match &event {
            P2pEvent::TransactionReceived { .. } => &routes.transactions,
            P2pEvent::SettlementSignatureReceived { .. } => &routes.signatures,
            _ => &routes.blocks,
        };
        match sink.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                // Backfill, pool reconciliation and proposal retries recover full queues.
                metrics::counter!("zone_leadership_events_dropped_backpressure_total").increment(1);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => panic!("zone P2P consumer stopped"),
        }
    }
    panic!("P2P event channel closed");
}

pub(crate) async fn route_backfill_requests(
    mut requests: mpsc::Receiver<BackfillRequest>,
    backfill: mpsc::Sender<BackfillRequest>,
) {
    while let Some(request) = requests.recv().await {
        if let Err(error) = backfill.try_send(request) {
            metrics::counter!("zone_leadership_backfill_requests_dropped_total").increment(1);
            tracing::warn!(%error, "Dropped backfill request because the serving queue is unavailable");
        }
    }
}

pub(crate) async fn route_backfill_responses(
    mut responses: mpsc::Receiver<BackfillResponse>,
    backfill: mpsc::Sender<BackfillResponse>,
) {
    while let Some(response) = responses.recv().await {
        match backfill.try_send(response) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                metrics::counter!("zone_leadership_events_dropped_backpressure_total").increment(1);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => panic!("zone backfill consumer stopped"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
    use zone_p2p::PeerTip;

    #[tokio::test]
    async fn accepted_backfill_requests_reach_the_server_before_sequencing_starts() {
        let (network, incoming) = mpsc::channel(1);
        let (server, mut received) = mpsc::channel(1);
        network
            .send(BackfillRequest {
                peer: PrivateKey::from_seed(1).public_key(),
                request_id: 7,
                start: 42,
            })
            .await
            .unwrap();
        drop(network);
        route_backfill_requests(incoming, server).await;
        let request = received.recv().await.unwrap();
        assert_eq!((request.request_id, request.start), (7, 42));
    }

    #[tokio::test]
    async fn a_full_backfill_consumer_does_not_block_network_routing() {
        let (network, incoming) = mpsc::channel(2);
        let (consumer, mut received) = mpsc::channel(1);
        for height in [1, 2] {
            network
                .send(BackfillResponse::Completed {
                    peer: PrivateKey::from_seed(1).public_key(),
                    tip: PeerTip {
                        zone_height: height,
                        zone_hash: B256::ZERO,
                        tempo_block_number: height,
                        tempo_block_hash: B256::ZERO,
                    },
                })
                .await
                .unwrap();
        }
        drop(network);
        route_backfill_responses(incoming, consumer).await;
        assert!(
            matches!(received.recv().await, Some(BackfillResponse::Completed { tip, .. }) if tip.zone_height == 1)
        );
        assert!(received.recv().await.is_none());
    }
}
