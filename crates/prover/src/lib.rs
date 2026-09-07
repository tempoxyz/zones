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

/// Allows large batch witnesses containing trie proofs and bytecode pools while bounding the
/// allocation an untrusted frame length can request.
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 512 * 1024 * 1024;
