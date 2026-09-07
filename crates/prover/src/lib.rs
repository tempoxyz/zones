//! Versioned request protocol for the Tempo Zone prover service.

mod chainspec;
mod connection;
mod protocol;

pub use chainspec::{TrustedChainSpecError, TrustedChainSpecs};
pub use connection::{ProverConnection, ProverConnectionError, request_error_response};
pub use protocol::{ErrorCode, PROTOCOL_VERSION, VerifyRequest, VerifyResponse};

/// Allows large batch witnesses containing trie proofs and bytecode pools while bounding the
/// allocation an untrusted frame length can request.
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 512 * 1024 * 1024;
