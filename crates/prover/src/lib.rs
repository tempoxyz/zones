//! Versioned request protocol for the Tempo Zone prover service.

mod chainspec;
mod connection;
mod protocol;

pub use chainspec::{TrustedChainSpecError, TrustedChainSpecs};
pub use connection::{
    ProverConnection, ProverConnectionError, decode_exact, request_error_response,
};
pub use protocol::{
    ErrorCode, NITRO_VERIFIER_CONFIG_V1, NitroBatchAttestation, PROTOCOL_VERSION, ProofBundle,
    VerifyRequest, VerifyResponse, nitro_batch_attestation_hash,
};

/// Allows batch witnesses up to 2 GiB to bound untrusted frame allocations.
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024 * 1024;
