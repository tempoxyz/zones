//! Shared websocket subscription types for the private zone RPC.

use std::{future::Future, pin::Pin};

use futures::Stream;
use serde_json::value::RawValue;
use tokio::task::JoinHandle;

use crate::types::JsonRpcError;

/// A boxed stream of serialized websocket subscription items.
pub type WsSubscriptionStream =
    Pin<Box<dyn Stream<Item = Result<Box<RawValue>, JsonRpcError>> + Send + 'static>>;

/// A boxed future that resolves to a websocket subscription.
pub type BoxWsSubscriptionFut<'a> =
    Pin<Box<dyn Future<Output = Result<WsSubscription, JsonRpcError>> + Send + 'a>>;

/// A transport-agnostic websocket subscription returned by the RPC API.
pub struct WsSubscription {
    /// Stream of serialized `eth_subscription` payloads.
    pub(crate) stream: WsSubscriptionStream,
    cleanup_task: Option<JoinHandle<()>>,
}

impl WsSubscription {
    /// Create a subscription backed by a direct event stream.
    pub fn new(stream: WsSubscriptionStream) -> Self {
        Self {
            stream,
            cleanup_task: None,
        }
    }

    /// Create a subscription with a cleanup task that should be aborted when
    /// the subscription is dropped.
    pub fn with_cleanup(stream: WsSubscriptionStream, cleanup_task: JoinHandle<()>) -> Self {
        Self {
            stream,
            cleanup_task: Some(cleanup_task),
        }
    }
}

impl Drop for WsSubscription {
    fn drop(&mut self) {
        if let Some(task) = self.cleanup_task.take() {
            task.abort();
        }
    }
}
