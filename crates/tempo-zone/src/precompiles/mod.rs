//! Cryptographic precompiles for encrypted deposit processing.
//!
//! Two precompiles that enable the [`ZoneInbox`](crate::abi::ZoneInbox) contract
//! to verify and decrypt encrypted deposits during `advanceTempo`:
//!
//! - **Chaum-Pedersen Verify** (`0x1C00...0100`) — verifies a DLOG equality proof
//!   for ECDH shared secret derivation.
//!
//! - **AES-256-GCM Decrypt** (`0x1C00...0101`) — decrypts ECIES ciphertext and
//!   verifies the GCM authentication tag.
//!
//! Both use NCC-audited RustCrypto implementations:
//! - [`k256`] v0.13.4 for secp256k1 elliptic curve operations
//! - [`aes-gcm`] v0.10.3 for AES-256-GCM authenticated decryption
//!
//! The [`ecies`] submodule provides the sequencer-side ECIES decryption logic
//! that produces [`DecryptionData`](crate::abi::DecryptionData) verified on-chain.

pub mod aes_gcm;
pub mod chaum_pedersen;
pub mod ecies;

pub use aes_gcm::{AES_GCM_DECRYPT_ADDRESS, AesGcmDecrypt};
pub use chaum_pedersen::{CHAUM_PEDERSEN_VERIFY_ADDRESS, ChaumPedersenVerify};

#[cfg(test)]
mod test_utils;
