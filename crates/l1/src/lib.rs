//! L1 chain subscription and deposit extraction.
//!
//! Subscribes to L1 block headers and extracts deposit events from the
//! ZonePortal contract for each block. Supports both WebSocket (subscription)
//! and HTTP (polling) transports — the transport is auto-detected from the URL
//! scheme.
//!
//! The module is split into:
//! - [`subscriber`] — the [`L1Subscriber`] background task and its config.
//! - [`deposit`] — deposit value types ([`Deposit`], [`EncryptedDeposit`],
//!   [`L1Deposit`]).
//! - [`event`] — portal event types extracted per L1 block.
//! - [`block`] — per-block deposit grouping and prepared payload types.
//! - [`queue`] — the deposit hash-chain queue consumed by the engine.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

use alloy_consensus::BlockHeader as _;
use alloy_eips::NumHash;
use alloy_network::primitives::HeaderResponse as _;
use alloy_primitives::{Address, B256, Bloom, Bytes, U256, keccak256};
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

    use alloy_rpc_client::ConnectionConfig;

    pub(crate) fn rpc_connection_config(retry_connection_interval: Duration) -> ConnectionConfig {
        ConnectionConfig::new()
            .with_max_retries(u32::MAX)
            .with_retry_interval(retry_connection_interval)
    }
}

use crate::{
    abi::{
        EncryptedDeposit as AbiEncryptedDeposit,
        EncryptedDepositPayload as AbiEncryptedDepositPayload, PORTAL_PENDING_SEQUENCER_SLOT,
        PORTAL_SEQUENCER_SLOT,
        ZonePortal::{
            self, DepositMade, EncryptedDepositMade, SequencerTransferStarted,
            SequencerTransferred, TokenEnabled, WithdrawalBounceBack, ZonePortalEvents,
        },
    },
    state::{cache::L1StateCacheInner, tip403::PolicyEvent},
};

mod block;
mod deposit;
mod event;
mod queue;
mod subscriber;

#[cfg(test)]
mod tests;

pub use block::{L1BlockDeposits, PreparedL1Block};
pub use deposit::{Deposit, EncryptedDeposit, L1Deposit};
pub use event::{EnabledToken, L1PortalEvents, L1SequencerEvent};
pub use ext::{ChainTempoStateExt, TempoStateExt};
pub use queue::DepositQueue;
pub use state::{L1StateCache, PolicyCache, PolicyProvider};
pub use subscriber::{L1Subscriber, L1SubscriberConfig};

pub(crate) use event::EnqueueOutcome;

#[cfg(test)]
pub(crate) use queue::PendingDeposits;
#[cfg(test)]
pub(crate) use subscriber::{
    LocalTempoStateReader, address_to_storage_value, apply_sequencer_events_to_cache,
    verify_receipts,
};
