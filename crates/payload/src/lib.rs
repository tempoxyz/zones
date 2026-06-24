//! Zone payload types and builder.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod abi {
    pub use tempo_zone_contracts::*;
}

mod attrs;
mod builder;

pub use attrs::{ZonePayloadAttributes, ZonePayloadTypes};
pub use builder::{ZonePayloadBuilder, ZonePayloadFactory, build_advance_tempo_tx};
