#![doc = include_str!("../README.md")]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod identity;
mod manifest;
mod network;
mod runtime;

pub use manifest::{
    Leadership, LeadershipState, ManifestAddress, ManifestError, ManifestNode, Role, ZoneManifest,
};
pub use network::P2pNetworkId;
pub use runtime::{
    P2pCommand, P2pConfig, P2pEvent, P2pHandle, P2pHandleParts, P2pPeerId, spawn_p2p,
};
