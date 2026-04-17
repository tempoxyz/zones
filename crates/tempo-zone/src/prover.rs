//! Prover abstraction for producing verifier payloads for `submitBatch`.

use alloy_primitives::Bytes;
use eyre::Result;
use futures::future::{BoxFuture, FutureExt};
use url::Url;

use crate::batch::BatchData;

/// Input provided to a prover for a single batch submission.
#[derive(Debug, Clone)]
pub struct ProveBatchRequest {
    /// Locally computed batch summary that will be submitted to the portal.
    pub batch: BatchData,
    /// `recentTempoBlockNumber` argument for `submitBatch`.
    pub recent_tempo_block_number: u64,
    /// RLP-encoded ancestry headers used when anchoring through a recent L1 block.
    pub ancestry_headers: Vec<Bytes>,
}

/// Opaque verifier payload produced by a prover.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProveBatchResponse {
    /// Verifier-specific config bytes passed through to `IVerifier`.
    pub verifier_config: Bytes,
    /// Proof or attestation bytes passed through to `IVerifier`.
    pub proof: Bytes,
}

/// Batch prover interface.
pub trait Prover: Send + Sync {
    /// Produce verifier payloads for a batch.
    fn prove_batch(
        &self,
        request: ProveBatchRequest,
    ) -> BoxFuture<'static, Result<ProveBatchResponse>>;
}

/// Development prover that returns empty verifier payloads.
#[derive(Debug, Default)]
pub struct MockProver;

impl Prover for MockProver {
    fn prove_batch(
        &self,
        _request: ProveBatchRequest,
    ) -> BoxFuture<'static, Result<ProveBatchResponse>> {
        async move { Ok(ProveBatchResponse::default()) }.boxed()
    }
}

/// HTTP client for the AWS Nitro prover service.
#[derive(Debug, Clone)]
pub struct NitroProver {
    client: reqwest::Client,
    prove_batch_url: Url,
}

impl NitroProver {
    /// Create a prover client pointed at the `/prove-batch` endpoint.
    pub fn new(prove_batch_url: Url) -> Self {
        Self {
            client: reqwest::Client::new(),
            prove_batch_url,
        }
    }
}

impl Prover for NitroProver {
    fn prove_batch(
        &self,
        request: ProveBatchRequest,
    ) -> BoxFuture<'static, Result<ProveBatchResponse>> {
        let client = self.client.clone();
        let prove_batch_url = self.prove_batch_url.clone();
        let nitro_request = aws_nitro_prover::ProveBatchRequest {
            prev_block_hash: request.batch.prev_block_hash.into(),
            next_block_hash: request.batch.next_block_hash.into(),
        };
        let expected_verifier_config = nitro_request.verifier_config().to_vec();

        async move {
            let response = client
                .post(prove_batch_url)
                .json(&nitro_request)
                .send()
                .await?
                .error_for_status()?
                .json::<aws_nitro_prover::ProveBatchResponse>()
                .await?;

            eyre::ensure!(
                response.prev_block_hash == nitro_request.prev_block_hash,
                "nitro prover returned mismatched prev_block_hash"
            );
            eyre::ensure!(
                response.next_block_hash == nitro_request.next_block_hash,
                "nitro prover returned mismatched next_block_hash"
            );
            eyre::ensure!(
                response.verifier_config == expected_verifier_config,
                "nitro prover returned unexpected verifier_config"
            );

            Ok(ProveBatchResponse {
                verifier_config: response.verifier_config.into(),
                proof: response.proof.into(),
            })
        }
        .boxed()
    }
}
