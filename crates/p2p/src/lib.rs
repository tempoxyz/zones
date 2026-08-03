#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod identity;
mod manifest;
mod network;
mod protocol;
mod routing;
mod runtime;

pub use manifest::{
    ForcedRecoveryState, LeadershipSchedule, LeadershipState, ManifestAddress, ManifestError,
    ManifestNode, Role, ZoneManifest,
};
pub use network::P2pNetworkId;
pub use protocol::PeerTip;
pub use routing::P2pPeerId;
pub use runtime::{P2pCommand, P2pConfig, P2pEvent, P2pHandle, P2pHandleParts, spawn_p2p};
