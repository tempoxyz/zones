//! Zone payload types and builder.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod abi {
    pub use tempo_zone_contracts::*;
}

mod attrs;
mod builder;
mod prewarming;
mod withdrawal_reveal;

pub use attrs::{
    FAST_SETTLEMENT_BOUNDARY_VERSION, FastSettlementBoundary, FastSettlementTrigger, TempoImport,
    ZonePayloadAttributes, ZonePayloadTypes,
};
pub use builder::{
    DEFAULT_WITHDRAWAL_BATCH_INTERVAL_BLOCKS, FAST_SETTLEMENT_BATCH_MAX_BYTES,
    FAST_SETTLEMENT_BATCH_MAX_DELAY, ZonePayloadBuilder, ZonePayloadFactory,
    build_advance_tempo_headers_tx, build_advance_tempo_tx, should_finalize_fast_settlement_batch,
};
pub use withdrawal_reveal::WithdrawalRevealEncryptor;
