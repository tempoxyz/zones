//! Versioned request protocol for the Tempo Zone prover service.

mod chainspec;
mod connection;
mod protocol;

pub use chainspec::{TrustedChainSpecError, TrustedChainSpecs};
pub use connection::{ProverConnection, ProverConnectionError, request_error_response};
pub use protocol::{
    ErrorCode, NITRO_VERIFIER_CONFIG_V1, NitroBatchAttestation, PROTOCOL_VERSION, ProofBundle,
    VerifyRequest, VerifyResponse, nitro_batch_attestation_hash,
};

/// Default maximum encoded size of one logical prover request.
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 512 * 1024 * 1024;

/// Fixed maximum payload size of one physical protocol frame.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
