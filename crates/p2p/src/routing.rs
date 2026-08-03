use std::collections::BTreeSet;

use commonware_cryptography::ed25519::PublicKey;

use crate::{LeadershipSchedule, ZoneManifest, manifest::AuthoritySnapshot};

/// Authenticated Commonware identity used to address one manifest peer.
pub type P2pPeerId = PublicKey;

/// Authenticated peers and the static settlement quorum derived from a validated manifest.
#[derive(Debug, Clone)]
pub(crate) struct RoutingMembership {
    local: PublicKey,
    peers: BTreeSet<PublicKey>,
    quorum: BTreeSet<PublicKey>,
}

impl RoutingMembership {
    pub(crate) fn from_manifest(manifest: &ZoneManifest, local: PublicKey) -> Self {
        let peers = manifest
            .nodes()
            .iter()
            .map(|node| node.ed25519_public_key().clone())
            .collect();
        let quorum = manifest
            .quorum_nodes()
            .map(|(node, _)| node.ed25519_public_key().clone())
            .collect();
        Self {
            local,
            peers,
            quorum,
        }
    }

    pub(crate) fn local(&self) -> &PublicKey {
        &self.local
    }

    pub(crate) fn contains(&self, peer: &PublicKey) -> bool {
        self.peers.contains(peer)
    }

    pub(crate) fn is_quorum_member(&self, peer: &PublicKey) -> bool {
        self.quorum.contains(peer)
    }

    pub(crate) fn other_peers(&self) -> Vec<PublicKey> {
        self.peers
            .iter()
            .filter(|peer| *peer != &self.local)
            .cloned()
            .collect()
    }

    pub(crate) fn other_quorum_peers(&self) -> Vec<PublicKey> {
        self.quorum
            .iter()
            .filter(|peer| *peer != &self.local)
            .cloned()
            .collect()
    }
}

/// Pure identity, membership, and retained-authority decisions for one routing operation.
pub(crate) struct RoutingPolicy<'a> {
    membership: &'a RoutingMembership,
    authority: AuthoritySnapshot,
}

impl<'a> RoutingPolicy<'a> {
    pub(crate) fn new(membership: &'a RoutingMembership, leadership: &LeadershipSchedule) -> Self {
        Self {
            membership,
            authority: leadership.authority_snapshot(),
        }
    }

    pub(crate) fn is_leadership_initialized(&self) -> bool {
        self.authority.next_anchor_record.is_some()
    }

    pub(crate) fn am_i_retained_leader(&self) -> bool {
        self.authority
            .retained_leaders
            .contains(self.membership.local())
    }

    pub(crate) fn am_i_quorum_member(&self) -> bool {
        self.membership.is_quorum_member(self.membership.local())
    }

    pub(crate) fn other_peers(&self) -> Vec<PublicKey> {
        self.membership.other_peers()
    }

    pub(crate) fn other_quorum_peers(&self) -> Vec<PublicKey> {
        self.membership.other_quorum_peers()
    }

    pub(crate) fn may_accept_block(&self, peer: &PublicKey) -> bool {
        self.is_remote_retained_leader(peer)
    }

    pub(crate) fn may_accept_settlement_proposal(&self, peer: &PublicKey) -> bool {
        self.am_i_quorum_member() && self.is_remote_retained_leader(peer)
    }

    pub(crate) fn may_send_settlement_signature(&self, leader: &PublicKey) -> bool {
        self.am_i_quorum_member()
            && leader != self.membership.local()
            && self.is_retained_leader(leader)
            && self.membership.contains(leader)
    }

    pub(crate) fn may_accept_settlement_signature(&self, peer: &PublicKey) -> bool {
        self.is_remote_quorum_peer(peer) && self.am_i_retained_leader()
    }

    pub(crate) fn preferred_backfill_leader(&self) -> Option<PublicKey> {
        let leader = self.authority.next_anchor_record.as_ref()?.leader.clone();
        (self.membership.is_quorum_member(&leader) && leader != *self.membership.local())
            .then_some(leader)
    }

    pub(crate) fn may_forward_transaction(&self) -> bool {
        self.authority
            .next_anchor_record
            .as_ref()
            .is_some_and(|record| record.leader != *self.membership.local())
    }

    pub(crate) fn may_accept_transaction(&self, peer: &PublicKey) -> bool {
        self.is_remote_peer(peer) && self.am_i_quorum_member()
    }

    fn is_retained_leader(&self, peer: &PublicKey) -> bool {
        self.authority.retained_leaders.contains(peer)
    }

    fn is_remote_retained_leader(&self, peer: &PublicKey) -> bool {
        peer != self.membership.local()
            && self.membership.contains(peer)
            && self.is_retained_leader(peer)
    }

    pub(crate) fn is_remote_peer(&self, peer: &PublicKey) -> bool {
        peer != self.membership.local() && self.membership.contains(peer)
    }

    pub(crate) fn is_remote_quorum_peer(&self, peer: &PublicKey) -> bool {
        self.is_remote_peer(peer) && self.membership.is_quorum_member(peer)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use commonware_cryptography::{Signer as _, ed25519::PrivateKey};

    use super::{RoutingMembership, RoutingPolicy};
    use crate::{LeadershipSchedule, LeadershipState, ZoneManifest};

    fn key(seed: u64) -> crate::P2pPeerId {
        PrivateKey::from_seed(seed).public_key()
    }

    fn manifest() -> ZoneManifest {
        let mut input = format!(
            "zone_id = 7\nleader_ed25519_public_key = \"0x{}\"\n",
            hex(key(1))
        );
        for (seed, rpc_only) in [(1, false), (2, false), (3, false), (4, true)] {
            input.push_str(&format!(
                "\n[[nodes]]\nname = \"node-{seed}\"\ned25519_public_key = \"0x{}\"\naddress = \"127.0.0.1:{}\"\nrpc_only = {rpc_only}\n",
                hex(key(seed)),
                9000 + seed,
            ));
            if !rpc_only {
                input.push_str(&format!("secp256k1_address = \"0x{seed:040x}\"\n"));
            }
        }
        ZoneManifest::parse(&input).unwrap()
    }

    fn hex(key: crate::P2pPeerId) -> String {
        const_hex::encode(key.as_ref())
    }

    #[test]
    fn membership_rejects_unknown_peers_as_quorum() {
        let membership = RoutingMembership::from_manifest(&manifest(), key(1));
        assert!(membership.contains(&key(1)));
        assert!(membership.is_quorum_member(&key(1)));
        assert!(!membership.is_quorum_member(&key(99)));
        assert!(!membership.contains(&key(99)));
        assert!(!membership.is_quorum_member(&key(4)));
    }

    #[test]
    fn routing_policy_covers_roles_and_transaction_fanout() {
        let manifest = manifest();
        let schedule = LeadershipSchedule::for_membership([key(4)].into_iter().collect());
        schedule
            .publish(LeadershipState::new(1, key(1), 0))
            .unwrap();
        schedule
            .publish(LeadershipState::new(2, key(2), 100))
            .unwrap();

        let leader_membership = RoutingMembership::from_manifest(&manifest, key(1));
        let leader = RoutingPolicy::new(&leader_membership, &schedule);
        assert!(leader.am_i_retained_leader());
        assert!(leader.may_accept_block(&key(2)));
        assert!(!leader.may_accept_block(&key(99)));
        assert_eq!(
            leader.other_peers().into_iter().collect::<BTreeSet<_>>(),
            [key(2), key(3), key(4)].into_iter().collect()
        );
        assert_eq!(
            leader
                .other_quorum_peers()
                .into_iter()
                .collect::<BTreeSet<_>>(),
            [key(2), key(3)].into_iter().collect()
        );
        assert!(leader.may_forward_transaction());
        assert!(leader.may_accept_settlement_signature(&key(2)));

        let follower_membership = RoutingMembership::from_manifest(&manifest, key(3));
        let follower = RoutingPolicy::new(&follower_membership, &schedule);
        assert!(!follower.am_i_retained_leader());
        assert!(follower.may_forward_transaction());
        assert_eq!(
            follower
                .other_quorum_peers()
                .into_iter()
                .collect::<BTreeSet<_>>(),
            [key(1), key(2)].into_iter().collect()
        );
        assert_eq!(follower.preferred_backfill_leader(), Some(key(2)));
        assert_eq!(
            follower
                .other_quorum_peers()
                .into_iter()
                .collect::<BTreeSet<_>>(),
            [key(1), key(2)].into_iter().collect()
        );
        assert!(follower.may_accept_settlement_proposal(&key(1)));

        let rpc_membership = RoutingMembership::from_manifest(&manifest, key(4));
        let rpc = RoutingPolicy::new(&rpc_membership, &schedule);
        assert!(rpc.may_forward_transaction());
        assert_eq!(
            rpc.other_quorum_peers()
                .into_iter()
                .collect::<BTreeSet<_>>(),
            [key(1), key(2), key(3)].into_iter().collect()
        );
        assert!(!rpc.may_accept_settlement_proposal(&key(1)));
        assert!(!rpc.may_accept_transaction(&key(3)));
    }

    #[test]
    fn uninitialized_authority_does_not_authorize_dynamic_traffic() {
        let manifest = manifest();
        let membership = RoutingMembership::from_manifest(&manifest, key(2));
        let schedule = manifest.leadership_schedule();
        let policy = RoutingPolicy::new(&membership, &schedule);
        assert!(!policy.is_leadership_initialized());
        assert!(!policy.am_i_retained_leader());
        assert!(!policy.may_forward_transaction());
        assert_eq!(policy.preferred_backfill_leader(), None);
    }
}
