//! Sequencer background task orchestration.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::{sync::Arc, time::Duration};

use alloy_primitives::Address;
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_signer_local::PrivateKeySigner;
use alloy_transport::TransportResult;
use tempo_alloy::{TempoNetwork, provider::ext::TempoProviderBuilderExt};
use tokio::sync::Notify;

pub mod abi {
    pub use tempo_zone_contracts::*;
}

mod metrics;
pub mod monitor;
pub mod nonce_keys;
mod rpc;
pub mod settlement;
pub mod withdrawals;

pub use monitor::{ZoneMonitorConfig, spawn_zone_monitor};
pub use settlement::{BatchAnchorConfig, BatchData, BatchSubmitter};
pub use withdrawals::{SharedWithdrawalStore, WithdrawalProcessorConfig, WithdrawalStore};

use crate::rpc::rpc_connection_config;

/// Configuration for all zone sequencer background tasks.
#[derive(Debug, Clone)]
pub struct ZoneSequencerConfig {
    /// ZonePortal contract address on Tempo L1.
    pub portal_address: Address,
    /// Tempo L1 RPC URL.
    pub l1_rpc_url: String,
    /// Interval between WebSocket reconnection attempts for long-lived RPC clients.
    pub retry_connection_interval: Duration,
    /// How often the withdrawal processor polls the L1 queue.
    pub withdrawal_poll_interval: Duration,
    /// ZoneOutbox contract address on Zone L2.
    pub outbox_address: Address,
    /// ZoneInbox contract address on Zone L2.
    pub inbox_address: Address,
    /// TempoState predeploy address on Zone L2.
    pub tempo_state_address: Address,
    /// Zone L2 RPC URL.
    pub zone_rpc_url: String,
    /// How often the zone monitor polls for new L2 blocks.
    pub zone_poll_interval: Duration,
    /// Number of zone blocks between empty withdrawal batch boundaries / L1 submissions.
    pub batch_interval_blocks: u64,
    /// EIP-2935 history and safety-margin limits used by the batch submitter.
    pub batch_anchor_config: BatchAnchorConfig,
}

/// Handles returned by [`spawn_zone_sequencer`] for managing background tasks.
pub struct ZoneSequencerHandle {
    /// Join handle for the withdrawal processor task.
    pub withdrawal_handle: tokio::task::JoinHandle<()>,
    /// Join handle for the zone monitor task (which also handles batch submission).
    pub monitor_handle: tokio::task::JoinHandle<()>,
}

/// Spawn all zone sequencer background tasks.
///
/// This is the top-level POC entrypoint that starts:
/// - **Zone monitor** — polls the Zone L2 for new blocks, extracts withdrawal events into the
///   shared store, builds [`crate::BatchData`], and submits each batch synchronously to the
///   ZonePortal on Tempo L1. Local state only advances on successful submission.
/// - **Withdrawal processor** — polls the ZonePortal withdrawal queue on Tempo L1 and calls
///   `processWithdrawal` for each pending withdrawal.
///
/// Both tasks share a single L1 provider and nonce manager to prevent signing/nonce contention
/// when submitting concurrent L1 transactions.
pub async fn spawn_zone_sequencer(
    config: ZoneSequencerConfig,
    signer: PrivateKeySigner,
) -> ZoneSequencerHandle {
    // Build a single shared L1 provider with the sequencer wallet.
    // Both the batch submitter (inside the zone monitor) and the withdrawal
    // processor use this provider, ensuring nonces are tracked in one place.
    let l1_provider =
        connect_l1_provider(&config.l1_rpc_url, config.retry_connection_interval, signer)
            .await
            .expect("valid L1 RPC URL");

    let withdrawal_store: SharedWithdrawalStore = Default::default();
    let withdrawal_notify = Arc::new(Notify::new());
    let withdrawal_repair_notify = Arc::new(Notify::new());

    let withdrawal_config = WithdrawalProcessorConfig {
        portal_address: config.portal_address,
        l1_rpc_url: config.l1_rpc_url.clone(),
        fallback_poll_interval: config.withdrawal_poll_interval,
    };

    let monitor_config = ZoneMonitorConfig {
        outbox_address: config.outbox_address,
        inbox_address: config.inbox_address,
        tempo_state_address: config.tempo_state_address,
        zone_rpc_url: config.zone_rpc_url,
        retry_connection_interval: config.retry_connection_interval,
        poll_interval: config.zone_poll_interval,
        batch_interval_blocks: config.batch_interval_blocks,
        portal_address: config.portal_address,
        batch_anchor_config: config.batch_anchor_config,
    };

    let withdrawal_handle = withdrawals::spawn_withdrawal_processor(
        withdrawal_config,
        l1_provider.clone(),
        withdrawal_store.clone(),
        withdrawal_notify.clone(),
        withdrawal_repair_notify.clone(),
    );
    let monitor_handle = spawn_zone_monitor(
        monitor_config,
        l1_provider,
        withdrawal_store,
        withdrawal_notify,
        withdrawal_repair_notify,
    );

    ZoneSequencerHandle {
        withdrawal_handle,
        monitor_handle,
    }
}

/// Build the shared L1 provider used by all sequencer-side L1 transaction tasks.
async fn connect_l1_provider(
    l1_rpc_url: &str,
    retry_connection_interval: Duration,
    signer: PrivateKeySigner,
) -> TransportResult<DynProvider<TempoNetwork>> {
    let wallet = alloy_network::EthereumWallet::from(signer);
    let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
        .with_nonce_key_filler()
        .wallet(wallet)
        .connect_with_config(l1_rpc_url, rpc_connection_config(retry_connection_interval))
        .await?
        .erased();

    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_provider::Provider;
    use futures::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::{
        net::{TcpListener, TcpStream},
        time::{Duration, timeout},
    };
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    async fn serve_block_number(
        stream: TcpStream,
        result: &'static str,
        close_after_response: bool,
    ) {
        let mut ws = accept_async(stream).await.unwrap();
        while let Some(message) = ws.next().await {
            let message = message.unwrap();
            let Message::Text(text) = message else {
                continue;
            };
            let request: Value = serde_json::from_str(&text).unwrap();
            if request["method"] != "eth_blockNumber" {
                continue;
            }

            let response = json!({
                "jsonrpc": "2.0",
                "id": request["id"].clone(),
                "result": result,
            });
            ws.send(Message::Text(response.to_string().into()))
                .await
                .unwrap();

            if close_after_response {
                let _ = ws.close(None).await;
                break;
            }
        }
    }

    #[tokio::test]
    async fn l1_provider_reconnects_after_wss_backend_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("ws://{addr}");
        let connections = Arc::new(AtomicUsize::new(0));
        let server_connections = connections.clone();

        let server = tokio::spawn(async move {
            let (first_stream, _) = listener.accept().await.unwrap();
            server_connections.fetch_add(1, Ordering::SeqCst);

            // Drop the listener while closing the first connection so the
            // provider's immediate reconnect attempt fails. With the configured
            // 10ms retry interval below, it should recover quickly once the
            // listener comes back; Alloy's default 3s interval would miss the
            // test timeout.
            drop(listener);
            serve_block_number(first_stream, "0x1", true).await;

            tokio::time::sleep(Duration::from_millis(100)).await;

            let listener = TcpListener::bind(addr).await.unwrap();
            let (second_stream, _) = listener.accept().await.unwrap();
            server_connections.fetch_add(1, Ordering::SeqCst);
            serve_block_number(second_stream, "0x2", false).await;
        });

        let provider =
            connect_l1_provider(&url, Duration::from_millis(10), PrivateKeySigner::random())
                .await
                .unwrap();

        assert_eq!(provider.get_block_number().await.unwrap(), 1);

        let second_block = timeout(Duration::from_secs(2), provider.get_block_number())
            .await
            .expect("provider should reconnect after first WSS backend closes")
            .unwrap();
        assert_eq!(second_block, 2);
        assert!(
            connections.load(Ordering::SeqCst) >= 2,
            "provider should have opened a replacement WSS connection"
        );

        server.abort();
    }
}
