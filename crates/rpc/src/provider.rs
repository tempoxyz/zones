//! Client provider for the redacted zone RPC.
//!
//! [`ZoneProvider`] wraps an alloy provider and automatically generates
//! fresh authorization tokens, rebuilding the HTTP client when the
//! current token approaches expiry.

use std::{sync::Arc, time::Duration};

use alloy_primitives::hex;
use alloy_provider::{DynProvider, Provider, ProviderBuilder};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use parking_lot::Mutex;
use tempo_alloy::TempoNetwork;

use crate::{
    auth::{X_AUTHORIZATION_TOKEN, build_token_fields, now_unix_seconds},
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
    /// How long each generated token is valid. Default: 600s.
    /// Must not exceed the server's configured maximum validity window
    /// (default protocol limit: 2592000s / 30 days).
    pub token_ttl: Duration,
    /// The redacted zone RPC URL.
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
        let now = now_unix_seconds();
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
    let now = now_unix_seconds();
    let expires_at = now + config.token_ttl.as_secs();

    let auth_header = build_auth_header(config, now, expires_at)?;

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(X_AUTHORIZATION_TOKEN, auth_header);

    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()?;

    let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
        .connect_reqwest(client, config.rpc_url.clone())
        .erased();

    Ok((provider, expires_at))
}

fn build_auth_header(
    config: &ZoneProviderConfig,
    now: u64,
    expires_at: u64,
) -> eyre::Result<reqwest::header::HeaderValue> {
    let (fields, digest) = build_token_fields(config.zone_id, config.chain_id, now, expires_at);

    let sig = config
        .signer
        .sign_hash_sync(&digest)
        .map_err(|e| eyre::eyre!("failed to sign zone auth token: {e}"))?;

    // Build blob: <65-byte sig><29-byte fields>
    let mut blob = Vec::with_capacity(65 + fields.len());
    // Authorization tokens use raw 0/1 parity, unlike settlement signatures.
    blob.extend_from_slice(&sig.as_rsy());
    blob.extend_from_slice(&fields);

    let mut auth_header = reqwest::header::HeaderValue::from_str(&hex::encode(&blob))?;
    auth_header.set_sensitive(true);

    Ok(auth_header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::parse_auth_header;
    use alloy_primitives::{B256, Signature};

    #[test]
    fn authorization_header_preserves_raw_parity_and_fields() {
        let config = ZoneProviderConfig {
            signer: PrivateKeySigner::from_bytes(&B256::repeat_byte(1)).unwrap(),
            zone_id: 42,
            chain_id: 1337,
            token_ttl: Duration::from_secs(600),
            rpc_url: "http://localhost:8545".parse().unwrap(),
        };
        // Fixed timestamps cover both recovery parities.
        for (now, parity) in [(1_700_000_000, false), (1_700_000_005, true)] {
            let expires_at = now + 600;
            let (fields, digest) =
                build_token_fields(config.zone_id, config.chain_id, now, expires_at);
            let signature = config.signer.sign_hash_sync(&digest).unwrap();
            assert_eq!(signature.v(), parity);
            let mut expected = Vec::new();
            expected.extend_from_slice(&signature.r().to_be_bytes::<32>());
            expected.extend_from_slice(&signature.s().to_be_bytes::<32>());
            expected.push(u8::from(signature.v()));
            expected.extend_from_slice(&fields);

            let header = build_auth_header(&config, now, expires_at).unwrap();
            assert!(header.is_sensitive());
            assert_eq!(header.to_str().unwrap(), hex::encode(expected));
            let parsed = parse_auth_header(header.to_str().unwrap()).unwrap();
            assert_eq!(parsed.digest, digest);
            assert_eq!(parsed.issued_at, now);
            assert_eq!(parsed.expires_at, expires_at);
            let recovered = Signature::try_from(parsed.signature.as_slice()).unwrap();
            assert_eq!(
                recovered
                    .recover_address_from_prehash(&parsed.digest)
                    .unwrap(),
                config.signer.address()
            );
        }
    }
}
