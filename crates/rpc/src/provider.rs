//! Client provider for the private zone RPC.
//!
//! [`ZoneProvider`] wraps an alloy provider and automatically generates
//! fresh authorization tokens, rebuilding the HTTP client when the
//! current token approaches expiry.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use alloy_primitives::hex;
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
    pub zone_id: u32,
    /// Chain identifier.
    pub chain_id: u64,
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
        self.metrics.token_refresh_attempts_total.increment(1);

        match build_provider_with_token(&self.config) {
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

    let (fields, digest) = build_token_fields(config.zone_id, config.chain_id, now, expires_at);

    let sig = config
        .signer
        .sign_hash_sync(&digest)
        .map_err(|e| eyre::eyre!("failed to sign zone auth token: {e}"))?;

    // Build blob: <65-byte sig><29-byte fields>
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
