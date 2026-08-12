//! L1 chain subscription and deposit extraction.
//!
//! Uses L1 block notifications to follow the finalized chain and extracts
//! deposit events from the ZonePortal contract for each finalized block.
//! WebSocket connections use `newHeads`; HTTP connections use block-filter
//! polling.
//!
//! The module is split into:
//! - [`subscriber`] — the [`L1Subscriber`] background task and its config.
//! - [`deposit`] — deposit value types ([`WithdrawalBounceBackDeposit`], [`Deposit`],
//!   [`L1Deposit`]).
//! - [`event`] — portal event types extracted per L1 block.
//! - [`block`] — per-block deposit grouping and prepared payload types.
//! - [`queue`] — the finalized L1 block queue consumed by the engine.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

use alloy_consensus::BlockHeader as _;
use alloy_eips::{BlockNumberOrTag, NumHash};
use alloy_network::primitives::HeaderResponse as _;
use alloy_primitives::{Address, B256, Bloom, U256, keccak256};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::{BlockId, Log};
use alloy_sol_types::{SolEvent, SolEventInterface, SolValue};
use alloy_transport::Authorization;
use futures::{Stream, StreamExt, TryStreamExt as _};
use parking_lot::Mutex;
use reth_primitives_traits::SealedHeader;
use reth_storage_api::StateProviderFactory;
use std::{pin::Pin, sync::Arc};
use tempo_alloy::TempoNetwork;
use tempo_primitives::TempoHeader;
use tracing::{debug, error, info, instrument, warn};

pub mod abi {
    pub use tempo_zone_contracts::*;
}

pub mod ext;
mod metrics;
pub mod state;

pub(crate) mod precompiles {
    pub(crate) use zone_precompiles::*;
}

pub(crate) mod rpc {
    use std::time::Duration;

    use alloy_rpc_client::{ConnectionConfig, WebSocketConfig};

    const MAX_WS_FRAME_AND_MESSAGE_SIZE: usize = 128 * 1024 * 1024;

    pub(crate) fn rpc_connection_config(retry_connection_interval: Duration) -> ConnectionConfig {
        ConnectionConfig::new()
            .with_max_retries(u32::MAX)
            .with_retry_interval(retry_connection_interval)
            .with_ws_config(
                WebSocketConfig::default()
                    // Large blocks can exceed tungstenite's default 16 MiB frame limit.
                    .max_frame_size(Some(MAX_WS_FRAME_AND_MESSAGE_SIZE))
                    .max_message_size(Some(MAX_WS_FRAME_AND_MESSAGE_SIZE)),
            )
    }
}

use crate::abi::{
    Deposit as AbiDeposit, DepositPayload as AbiDepositPayload,
    ZonePortal::{
        DepositMade, LeaderUpdated, SequencerEncryptionKeyUpdated, TokenEnabled,
        WithdrawalBounceBack, ZonePortalEvents,
    },
};

mod block;
mod deposit;
mod encryption_keys;
mod event;
mod queue;
mod subscriber;

#[cfg(test)]
mod tests;

pub use block::{L1BlockDeposits, PreparedL1Block};
pub use deposit::{Deposit, L1Deposit, WithdrawalBounceBackDeposit};
pub use encryption_keys::{
    BoundPublicKeyFingerprint, EncryptionKeyPublicStatus, EncryptionKeyRing, PublicKeyFingerprint,
};
pub use event::{EnabledToken, EncryptionKeyRotation, L1PortalEvents, LeaderTransition};
pub use ext::{ChainTempoStateExt, TempoStateExt};
pub use queue::DepositQueue;
pub use state::L1StateCache;
pub use subscriber::{
    L1BlockTracker, L1Subscriber, L1SubscriberConfig, LeadershipSink, MAX_L1_LOOKAHEAD_BLOCKS,
};

#[cfg(test)]
pub(crate) use queue::PendingDeposits;
#[cfg(test)]
pub(crate) use subscriber::{LocalTempoCheckpointReader, verify_receipts};
