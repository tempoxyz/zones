//! Native TIP-1091 `ZoneFactory` precompile ABI.

use alloy_primitives::{FixedBytes, fixed_bytes};

pub use tempo_contracts::precompiles::zone_factory::{
    IZoneFactory as ZoneFactory, ZONE_FACTORY_ADDRESS, ZONE_MESSENGER_ADDRESS,
    ZONE_PORTAL_IMPL_ADDRESS, ZONE_VERIFIER_ADDRESS, ZoneInfo,
};

pub const ZONE_PORTAL_PREFIX: FixedBytes<12> = fixed_bytes!("5AD000000000000000000000");
