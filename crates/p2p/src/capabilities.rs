//! Optional peer features. Silence always means legacy block encoding.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use crate::P2pPeerId;

pub(crate) const ANNOUNCEMENT_INTERVAL: Duration = Duration::from_secs(5);
pub(crate) const ANNOUNCEMENT_TTL: Duration = Duration::from_secs(15);

/// Shared by live replication and backfill; never persisted across local restarts.
///
/// Commonware does not expose connection generations to applications. Expiring announcements
/// bounds stale support after a remote rollback, but is not an instantaneous disconnect fence.
#[derive(Clone, Default)]
pub(crate) struct PeerCapabilities(Arc<Mutex<HashMap<P2pPeerId, Instant>>>);

impl PeerCapabilities {
    pub(crate) fn announce_witnesses(&self, peer: P2pPeerId, now: Instant) {
        let mut peers = self.0.lock().expect("peer capabilities lock poisoned");
        peers.retain(|_, last_seen| now.duration_since(*last_seen) < ANNOUNCEMENT_TTL);
        peers.insert(peer, now);
    }

    pub(crate) fn supports_witnesses(&self, peer: &P2pPeerId, now: Instant) -> bool {
        self.0
            .lock()
            .expect("peer capabilities lock poisoned")
            .get(peer)
            .is_some_and(|last_seen| now.duration_since(*last_seen) < ANNOUNCEMENT_TTL)
    }
}

#[cfg(test)]
mod tests {
    use commonware_cryptography::{Signer as _, ed25519::PrivateKey};

    use super::*;

    #[test]
    fn support_requires_an_announcement_and_expires_without_refresh() {
        let peer = PrivateKey::from_seed(1).public_key();
        let other = PrivateKey::from_seed(2).public_key();
        let capabilities = PeerCapabilities::default();
        let shared = capabilities.clone();
        let now = Instant::now();
        assert!(!capabilities.supports_witnesses(&peer, now));
        capabilities.announce_witnesses(peer.clone(), now);
        assert!(shared.supports_witnesses(&peer, now));
        assert!(!capabilities.supports_witnesses(&other, now));

        let refreshed = now + ANNOUNCEMENT_INTERVAL;
        capabilities.announce_witnesses(peer.clone(), refreshed);
        assert!(capabilities.supports_witnesses(&peer, now + ANNOUNCEMENT_TTL));
        assert!(!capabilities.supports_witnesses(&peer, refreshed + ANNOUNCEMENT_TTL));
        assert!(!PeerCapabilities::default().supports_witnesses(&peer, refreshed));
    }
}
