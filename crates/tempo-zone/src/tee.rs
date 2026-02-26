//! TEE (Trusted Execution Environment) batch signing for zone proofs.
//!
//! Computes a `batchDigest` matching the on-chain NitroVerifier.sol and signs it
//! with the enclave's secp256k1 key. In production, this key lives inside an
//! AWS Nitro Enclave; in development, it's a local key passed via CLI.

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_sol_types::SolValue;
use eyre::Result;

use crate::batch::BatchData;

/// Configuration for the TEE signer.
#[derive(Debug, Clone)]
pub struct TeeConfig {
    /// The enclave signing key (secp256k1 secret key).
    /// In production, this is generated inside the Nitro Enclave.
    /// In development/testing, passed via --tee.signing-key.
    pub signing_key: k256::ecdsa::SigningKey,
    /// The keccak256(PCR0 || PCR1 || PCR2) measurement hash.
    /// Included in verifierConfig for on-chain verification.
    pub measurement_hash: B256,
    /// Address of the enclave signer (derived from signing_key).
    pub enclave_address: Address,
    /// Expiration timestamp for the enclave key registration.
    pub expires_at: u64,
}

impl TeeConfig {
    /// Create a new TeeConfig from a signing key and measurement hash.
    pub fn new(signing_key: k256::ecdsa::SigningKey, measurement_hash: B256) -> Self {
        let verifying_key = signing_key.verifying_key();
        let public_key_bytes = verifying_key.to_encoded_point(false);
        let hash = keccak256(&public_key_bytes.as_bytes()[1..]);
        let enclave_address = Address::from_slice(&hash[12..]);

        Self {
            signing_key,
            measurement_hash,
            enclave_address,
            expires_at: 0,
        }
    }
}

/// Signs a batch with the enclave key, producing proof bytes for on-chain verification.
pub struct TeeSigner {
    config: TeeConfig,
    /// Chain ID for domain separation.
    chain_id: u64,
    /// Verifier contract address for domain separation.
    verifier_address: Address,
}

impl TeeSigner {
    pub fn new(config: TeeConfig, chain_id: u64, verifier_address: Address) -> Self {
        Self { config, chain_id, verifier_address }
    }

    /// Compute the batch digest matching NitroVerifier.sol's `computeBatchDigest`.
    ///
    /// This MUST produce the exact same hash as the Solidity function:
    /// ```solidity
    /// keccak256(abi.encode(
    ///     "NitroVerifier.BatchDigest",
    ///     block.chainid,
    ///     address(this),        // verifier
    ///     portal,
    ///     tempoBlockNumber,
    ///     anchorBlockNumber,
    ///     anchorBlockHash,
    ///     expectedWithdrawalBatchIndex,
    ///     sequencer,
    ///     blockTransition.prevBlockHash,
    ///     blockTransition.nextBlockHash,
    ///     depositQueueTransition.prevProcessedHash,
    ///     depositQueueTransition.nextProcessedHash,
    ///     withdrawalQueueHash
    /// ))
    /// ```
    ///
    /// Note: The string `"NitroVerifier.BatchDigest"` is ABI-encoded as a dynamic
    /// `string` type (with offset, length, and padded data), matching Solidity's
    /// `abi.encode` behavior for string literals.
    pub fn compute_batch_digest(
        &self,
        batch: &BatchData,
        portal_address: Address,
        sequencer: Address,
        expected_withdrawal_batch_index: u64,
        anchor_block_number: u64,
        anchor_block_hash: B256,
    ) -> B256 {
        let encoded = (
            String::from("NitroVerifier.BatchDigest"),
            U256::from(self.chain_id),
            self.verifier_address,
            portal_address,
            batch.tempo_block_number,
            anchor_block_number,
            anchor_block_hash,
            expected_withdrawal_batch_index,
            sequencer,
            batch.prev_block_hash,
            batch.next_block_hash,
            batch.prev_processed_deposit_hash,
            batch.next_processed_deposit_hash,
            batch.withdrawal_queue_hash,
        )
            .abi_encode();

        keccak256(&encoded)
    }

    /// Sign the batch digest with the enclave key.
    ///
    /// Returns `(verifier_config, proof)` as ABI-encoded [`Bytes`]:
    /// - `verifier_config`: `abi.encode(bytes32 measurementHash)`
    /// - `proof`: `abi.encode(bytes enclaveSig)` — matches NitroVerifier.sol's
    ///   `abi.decode(proof, (bytes))` in `verify()`
    pub fn sign_batch(
        &self,
        batch: &BatchData,
        portal_address: Address,
        sequencer: Address,
        expected_withdrawal_batch_index: u64,
        anchor_block_number: u64,
        anchor_block_hash: B256,
    ) -> Result<(Bytes, Bytes)> {
        let digest = self.compute_batch_digest(
            batch,
            portal_address,
            sequencer,
            expected_withdrawal_batch_index,
            anchor_block_number,
            anchor_block_hash,
        );

        use k256::ecdsa::{RecoveryId, signature::hazmat::PrehashSigner};
        let (signature, recovery_id): (k256::ecdsa::Signature, RecoveryId) = self
            .config
            .signing_key
            .sign_prehash(digest.as_slice())
            .map_err(|e| eyre::eyre!("failed to sign batch digest: {e}"))?;

        // Encode as 65-byte Ethereum signature (r || s || v)
        let mut sig_bytes = [0u8; 65];
        sig_bytes[..32].copy_from_slice(&signature.r().to_bytes());
        sig_bytes[32..64].copy_from_slice(&signature.s().to_bytes());
        sig_bytes[64] = recovery_id.to_byte() + 27; // Ethereum v = recovery_id + 27

        let enclave_sig = Bytes::from(sig_bytes.to_vec());

        // ABI-encode proof as (bytes enclaveSig) — single dynamic bytes,
        // matching NitroVerifier.verify() which does abi.decode(proof, (bytes))
        let proof = (enclave_sig,).abi_encode();

        // ABI-encode verifierConfig: (bytes32 measurementHash)
        let verifier_config = self.config.measurement_hash.abi_encode();

        Ok((Bytes::from(verifier_config), Bytes::from(proof)))
    }
}
