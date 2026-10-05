#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod backfill;
mod capabilities;
mod identity;
mod inter_zone;
mod manifest;
mod network;
mod protocol;
mod routing;
mod runtime;

pub use backfill::{BackfillCommand, BackfillPorts, BackfillRequest, BackfillResponse};
pub use inter_zone::{
    AuthenticatedInterZoneRequest, InterZoneAcknowledgment, InterZoneAuthoritySet,
    InterZonePeerAuthority, InterZoneRoutingConfig, InterZoneRoutingPeer, InterZoneServicePorts,
    InterZoneServiceRequest, MAX_INTER_ZONE_MESSAGE_SIZE, NEXT_ROSTER_CHECKPOINT_STREAM_PREFIX,
    NextRosterHandoffAuthoritySet, NextRosterHandoffRoutingConfig,
};
pub use manifest::{
    ForcedRecoveryConfig, ForcedRecoveryState, LeadershipSchedule, LeadershipState,
    ManifestAddress, ManifestError, ManifestNode, Role, ZoneManifest,
};
pub use network::{MAX_TRANSACTION_MESSAGE_SIZE, P2pNetworkId};
pub use protocol::{EncodedBlock, PeerTip};
pub use routing::P2pPeerId;
pub use runtime::{
    MAX_RAFT_MESSAGE_SIZE, NextRosterHandoffHandle, P2pCommand, P2pConfig, P2pEvent, P2pHandle,
    P2pHandleParts, RaftPorts, RaftRequestFrame, RaftResponseFrame,
    spawn_next_roster_handoff_endpoint, spawn_p2p,
};
