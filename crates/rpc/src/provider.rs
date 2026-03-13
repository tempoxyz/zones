//! Client provider for the private zone RPC.
//!
//! [`ZoneProvider`] wraps an alloy provider and automatically generates
//! fresh authorization tokens, rebuilding the HTTP client when the
//! current token approaches expiry.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_primitives::{Address, hex};
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use parking_lot::Mutex;
use tempo_alloy::TempoNetwork;

use crate::{
    auth::{X_AUTHORIZATION_TOKEN, build_token_fields},
    metrics::ZoneProviderMetrics,
};

/// How many seconds before expiry to refresh the token.
const REFRESH_BUFFER_SECS: u64 = 30;

/// Configuration for building a [`ZoneProvider`].
#[derive(Clone, Debug)]
pub struct ZoneProviderConfig {
    /// Signer for generating authorization tokens.
    pub signer: PrivateKeySigner,
    /// Zone identifier.
    pub zone_id: u64,
    /// Chain identifier.
    pub chain_id: u64,
    /// ZonePortal contract address on L1.
    pub zone_portal: Address,
    /// How long each generated token is valid. Default: 600s, max: 1800s.
    pub token_ttl: Duration,
    /// The private zone RPC URL.
    pub rpc_url: url::Url,
}

/// An alloy provider that auto-refreshes its zone authorization token.
///
/// Call [`provider()`](Self::provider) to get a `DynProvider` with a valid token.
/// The inner provider is rebuilt transparently when the current token
/// is within [`REFRESH_BUFFER_SECS`] of expiry.
#[derive(Clone)]
pub struct ZoneProvider {
    config: ZoneProviderConfig,
    state: Arc<Mutex<CachedState>>,
    metrics: ZoneProviderMetrics,
}

struct CachedState {
    provider: DynProvider<TempoNetwork>,
    expires_at: u64,
}

impl ZoneProvider {
    /// Create a new `ZoneProvider` with the given config.
    ///
    /// Immediately builds the first provider with a fresh token.
    pub fn new(config: ZoneProviderConfig) -> eyre::Result<Self> {
        let (provider, expires_at) = build_provider_with_token(&config)?;
        Ok(Self {
            config,
            state: Arc::new(Mutex::new(CachedState {
                provider,
                expires_at,
            })),
            metrics: ZoneProviderMetrics::default(),
        })
    }

    /// Get a provider with a valid authorization token.
    ///
    /// Transparently refreshes the token if it's about to expire.
    pub fn provider(&self) -> DynProvider<TempoNetwork> {
        let now = now_secs();
        let mut state = self.state.lock();
        if now + REFRESH_BUFFER_SECS < state.expires_at {
            return state.provider.clone();
        }
        self.refresh_provider_with(&mut state, build_provider_with_token)
    }

    fn refresh_provider_with<F>(
        &self,
        state: &mut CachedState,
        builder: F,
    ) -> DynProvider<TempoNetwork>
    where
        F: FnOnce(&ZoneProviderConfig) -> eyre::Result<(DynProvider<TempoNetwork>, u64)>,
    {
        self.metrics.token_refresh_attempts_total.increment(1);

        match builder(&self.config) {
            Ok((provider, expires_at)) => {
                state.provider = provider;
                state.expires_at = expires_at;
                state.provider.clone()
            }
            Err(e) => {
                self.metrics.token_refresh_failures_total.increment(1);
                tracing::warn!(target: "zone::rpc", err = %e, "failed to refresh zone auth token, reusing stale");
                state.provider.clone()
            }
        }
    }
}

/// Build a fresh provider with a newly-signed auth token.
fn build_provider_with_token(
    config: &ZoneProviderConfig,
) -> eyre::Result<(DynProvider<TempoNetwork>, u64)> {
    let now = now_secs();
    let expires_at = now + config.token_ttl.as_secs();

    let (fields, digest) = build_token_fields(
        config.zone_id,
        config.chain_id,
        config.zone_portal,
        now,
        expires_at,
    );

    let sig = config
        .signer
        .sign_hash_sync(&digest)
        .map_err(|e| eyre::eyre!("failed to sign zone auth token: {e}"))?;

    // Build blob: <65-byte sig><53-byte fields>
    let mut blob = Vec::with_capacity(65 + fields.len());
    blob.extend_from_slice(&sig.r().to_be_bytes::<32>());
    blob.extend_from_slice(&sig.s().to_be_bytes::<32>());
    blob.push(sig.v() as u8);
    blob.extend_from_slice(&fields);

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        X_AUTHORIZATION_TOKEN,
        reqwest::header::HeaderValue::from_str(&hex::encode(&blob))?,
    );

    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()?;

    let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
        .connect_reqwest(client, config.rpc_url.clone())
        .erased();

    Ok((provider, expires_at))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics_util::{
        CompositeKey, MetricKind,
        debugging::{DebugValue, DebuggingRecorder, Snapshotter},
    };
    use reth_metrics::metrics::{SharedString, Unit};
    use std::sync::{Mutex as StdMutex, OnceLock};

    type SnapshotEntry = (
        CompositeKey,
        (Option<Unit>, Option<SharedString>, DebugValue),
    );
    // `CompositeKey` trips clippy's `mutable_key_type`, so these tests keep
    // snapshot data as a flat list and do linear lookups.
    type SnapshotEntries = Vec<SnapshotEntry>;

    fn snapshotter() -> &'static Snapshotter {
        static SNAPSHOTTER: OnceLock<Snapshotter> = OnceLock::new();

        SNAPSHOTTER.get_or_init(|| {
            let recorder = DebuggingRecorder::new();
            let snapshotter = recorder.snapshotter();
            let _ = recorder.install();
            snapshotter
        })
    }

    fn metric_lock() -> &'static StdMutex<()> {
        static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| StdMutex::new(()))
    }

    fn with_metrics_snapshot<T>(action: impl FnOnce() -> T) -> (T, SnapshotEntries) {
        let _guard = metric_lock().lock().unwrap();
        let _ = snapshotter().snapshot();
        let result = action();
        let snapshot = snapshotter()
            .snapshot()
            .into_hashmap()
            .into_iter()
            .collect();
        (result, snapshot)
    }

    fn counter(snapshot: &SnapshotEntries, name: &str) -> u64 {
        snapshot
            .iter()
            .find(|(key, _)| key.kind() == MetricKind::Counter && key.key().name() == name)
            .map(|(_, (_, _, value))| match value {
                DebugValue::Counter(value) => *value,
                other => panic!("expected counter for {name}, got {other:?}"),
            })
            .unwrap_or_else(|| panic!("metric {name} not found"))
    }

    fn test_config() -> ZoneProviderConfig {
        ZoneProviderConfig {
            signer: PrivateKeySigner::random(),
            zone_id: 1,
            chain_id: 42,
            zone_portal: Address::ZERO,
            token_ttl: Duration::from_secs(600),
            rpc_url: "http://127.0.0.1:8545".parse().unwrap(),
        }
    }

    #[test]
    fn refresh_success_increments_attempt_metric() {
        let (_, snapshot) = with_metrics_snapshot(|| {
            let provider = ZoneProvider::new(test_config()).unwrap();
            let mut state = provider.state.lock();
            state.expires_at = 0;
            let _ = provider.refresh_provider_with(&mut state, build_provider_with_token);
        });

        assert_eq!(
            counter(
                &snapshot,
                "zone_private_rpc.provider.token_refresh_attempts_total",
            ),
            1
        );
        assert_eq!(
            counter(
                &snapshot,
                "zone_private_rpc.provider.token_refresh_failures_total",
            ),
            0
        );
    }

    #[test]
    fn refresh_failure_reuses_stale_provider() {
        let ((before, after, expires_at), snapshot) = with_metrics_snapshot(|| {
            let provider = ZoneProvider::new(test_config()).unwrap();
            let mut state = provider.state.lock();
            state.expires_at = 0;
            let before = state.provider.clone();
            let after = provider
                .refresh_provider_with(&mut state, |_| Err(eyre::eyre!("forced refresh failure")));
            let expires_at = state.expires_at;
            (before, after, expires_at)
        });

        assert!(std::ptr::eq(before.root(), after.root()));
        assert_eq!(expires_at, 0);
        assert_eq!(
            counter(
                &snapshot,
                "zone_private_rpc.provider.token_refresh_attempts_total",
            ),
            1
        );
        assert_eq!(
            counter(
                &snapshot,
                "zone_private_rpc.provider.token_refresh_failures_total",
            ),
            1
        );
    }
}
