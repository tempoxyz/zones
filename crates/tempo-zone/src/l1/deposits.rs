//! Deposit types and preparation logic.

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_sol_types::SolValue;
use reth_primitives_traits::SealedHeader;
use tempo_primitives::TempoHeader;
use tracing::{debug, info, warn};

use crate::abi::{
    self, EncryptedDeposit as AbiEncryptedDeposit,
    EncryptedDepositPayload as AbiEncryptedDepositPayload,
    ZonePortal::{BounceBack, DepositMade, EncryptedDepositMade},
};
use crate::l1_state::tip403::PolicyEvent;

use super::L1PortalEvents;

/// A deposit extracted from L1.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Deposit {
    /// TIP-20 token being deposited.
    pub token: Address,
    /// Sender on L1.
    pub sender: Address,
    /// Recipient on the zone.
    pub to: Address,
    /// Net amount deposited (fee already deducted on L1).
    pub amount: u128,
    /// Fee paid on L1.
    pub fee: u128,
    /// User-provided memo.
    pub memo: B256,
}

impl Deposit {
    /// Create a new deposit from an event.
    pub fn from_event(event: DepositMade) -> Self {
        Self {
            token: event.token,
            sender: event.sender,
            to: event.to,
            amount: event.netAmount,
            fee: event.fee,
            memo: event.memo,
        }
    }

    /// Create a bounce-back deposit from an event.
    pub fn from_bounce_back(event: BounceBack, portal_address: Address) -> Self {
        Self {
            token: event.token,
            sender: portal_address,
            to: event.fallbackRecipient,
            amount: event.amount,
            fee: 0,
            memo: B256::ZERO,
        }
    }
}

/// An encrypted deposit extracted from L1.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EncryptedDeposit {
    /// TIP-20 token being deposited.
    pub token: Address,
    /// Sender on L1.
    pub sender: Address,
    /// Net amount deposited (fee already deducted on L1).
    pub amount: u128,
    /// Fee paid on L1.
    pub fee: u128,
    /// Index of the encryption key used.
    pub key_index: U256,
    /// Ephemeral public key X coordinate.
    pub ephemeral_pubkey_x: B256,
    /// Ephemeral public key Y parity (0x02 or 0x03).
    pub ephemeral_pubkey_y_parity: u8,
    /// AES-256-GCM ciphertext.
    pub ciphertext: Vec<u8>,
    /// GCM nonce (12 bytes).
    pub nonce: [u8; 12],
    /// GCM authentication tag (16 bytes).
    pub tag: [u8; 16],
}

impl EncryptedDeposit {
    /// Create a new encrypted deposit from an event.
    pub fn from_event(event: EncryptedDepositMade) -> Self {
        Self {
            token: event.token,
            sender: event.sender,
            amount: event.netAmount,
            fee: event.fee,
            key_index: event.keyIndex,
            ephemeral_pubkey_x: event.ephemeralPubkeyX,
            ephemeral_pubkey_y_parity: event.ephemeralPubkeyYParity,
            ciphertext: event.ciphertext.to_vec(),
            nonce: event.nonce.0,
            tag: event.tag.0,
        }
    }
}

/// A deposit from L1 — either regular (plaintext) or encrypted.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum L1Deposit {
    /// A regular deposit with plaintext recipient and memo.
    Regular(Deposit),
    /// An encrypted deposit where recipient and memo are encrypted.
    Encrypted(EncryptedDeposit),
}

impl L1Deposit {
    /// Compute the next hash chain value: `keccak256(abi.encode(deposit, prevHash))`.
    pub fn hash_chain(&self, prev_hash: B256) -> B256 {
        match self {
            Self::Regular(d) => keccak256(
                (
                    abi::DepositType::Regular,
                    abi::Deposit {
                        token: d.token,
                        sender: d.sender,
                        to: d.to,
                        amount: d.amount,
                        memo: d.memo,
                    },
                    prev_hash,
                )
                    .abi_encode(),
            ),
            Self::Encrypted(d) => keccak256(
                (
                    abi::DepositType::Encrypted,
                    AbiEncryptedDeposit {
                        token: d.token,
                        sender: d.sender,
                        amount: d.amount,
                        keyIndex: d.key_index,
                        encrypted: AbiEncryptedDepositPayload {
                            ephemeralPubkeyX: d.ephemeral_pubkey_x,
                            ephemeralPubkeyYParity: d.ephemeral_pubkey_y_parity,
                            ciphertext: d.ciphertext.clone().into(),
                            nonce: d.nonce.into(),
                            tag: d.tag.into(),
                        },
                    },
                    prev_hash,
                )
                    .abi_encode(),
            ),
        }
    }
}

/// An L1 block's header paired with the deposits found in that block.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct L1BlockDeposits {
    /// The sealed L1 block header (caches the block hash).
    pub header: SealedHeader<TempoHeader>,
    /// Portal events extracted from this block.
    pub events: L1PortalEvents,
    /// TIP-403 policy events extracted from this block's receipts.
    pub policy_events: Vec<PolicyEvent>,
    /// Deposit queue hash chain value before this block's deposits.
    pub queue_hash_before: B256,
    /// Deposit queue hash chain value after this block's deposits.
    pub queue_hash_after: B256,
}

impl L1BlockDeposits {
    /// Prepare all deposits for the payload builder.
    ///
    /// Decrypts encrypted deposits, checks TIP-403 policy authorization,
    /// and ABI-encodes everything into the types the `advanceTempo` call expects.
    /// The resulting [`PreparedL1Block`] is ready to be passed through payload
    /// attributes to the builder.
    pub async fn prepare(
        self,
        sequencer_key: &k256::SecretKey,
        portal_address: Address,
        policy_provider: &crate::l1_state::PolicyProvider,
    ) -> eyre::Result<PreparedL1Block> {
        use crate::precompiles::ecies;

        let start = std::time::Instant::now();
        let l1_block_number = self.header.inner.number;
        let total_deposits = self.events.deposits.len();
        let mut queued_deposits: Vec<abi::QueuedDeposit> = Vec::new();
        let mut decryptions: Vec<abi::DecryptionData> = Vec::new();

        for deposit in &self.events.deposits {
            match deposit {
                L1Deposit::Regular(d) => {
                    let deposit = abi::Deposit {
                        token: d.token,
                        sender: d.sender,
                        to: d.to,
                        amount: d.amount,
                        memo: d.memo,
                    };
                    queued_deposits.push(abi::QueuedDeposit {
                        depositType: abi::DepositType::Regular,
                        depositData: Bytes::from(deposit.abi_encode()),
                    });
                }
                L1Deposit::Encrypted(d) => {
                    let queued = abi::QueuedDeposit {
                        depositType: abi::DepositType::Encrypted,
                        depositData: Bytes::from(
                            abi::EncryptedDeposit {
                                token: d.token,
                                sender: d.sender,
                                amount: d.amount,
                                keyIndex: d.key_index,
                                encrypted: abi::EncryptedDepositPayload {
                                    ephemeralPubkeyX: d.ephemeral_pubkey_x,
                                    ephemeralPubkeyYParity: d.ephemeral_pubkey_y_parity,
                                    ciphertext: d.ciphertext.clone().into(),
                                    nonce: d.nonce.into(),
                                    tag: d.tag.into(),
                                },
                            }
                            .abi_encode(),
                        ),
                    };

                    // Attempt full ECIES decryption.
                    let dec = ecies::decrypt_deposit(
                        sequencer_key,
                        &d.ephemeral_pubkey_x,
                        d.ephemeral_pubkey_y_parity,
                        &d.ciphertext,
                        &d.nonce,
                        &d.tag,
                        portal_address,
                        d.key_index,
                    );

                    if let Some(dec) = dec {
                        debug!(
                            target: "zone::engine",
                            l1_block = l1_block_number,
                            sender = %d.sender,
                            recipient = %dec.to,
                            token = %d.token,
                            amount = %d.amount,
                            "Decrypted encrypted deposit, checking policy"
                        );

                        // Check TIP-403 policy via the provider (cache-first, RPC fallback).
                        // Errors are propagated so the engine retries rather than allowing
                        // unauthorized deposits through.
                        let authorized = policy_provider
                            .is_authorized_async(
                                d.token,
                                dec.to,
                                l1_block_number,
                                crate::l1_state::AuthRole::MintRecipient,
                            )
                            .await?;

                        let recipient = if authorized {
                            debug!(
                                target: "zone::engine",
                                recipient = %dec.to,
                                token = %d.token,
                                "Policy authorized encrypted deposit recipient"
                            );
                            dec.to
                        } else {
                            warn!(
                                target: "zone::engine",
                                sender = %d.sender,
                                recipient = %dec.to,
                                token = %d.token,
                                amount = %d.amount,
                                "Encrypted deposit recipient unauthorized, redirecting to sender"
                            );
                            d.sender
                        };

                        let decryption = abi::DecryptionData {
                            sharedSecret: dec.proof.shared_secret,
                            sharedSecretYParity: dec.proof.shared_secret_y_parity,
                            to: recipient,
                            memo: dec.memo,
                            cpProof: abi::ChaumPedersenProof {
                                s: dec.proof.cp_proof_s,
                                c: dec.proof.cp_proof_c,
                            },
                        };
                        queued_deposits.push(queued);
                        decryptions.push(decryption);
                        continue;
                    }

                    // Full decryption failed — try ECDH proof for on-chain refund.
                    let proof = ecies::compute_ecdh_proof(
                        sequencer_key,
                        &d.ephemeral_pubkey_x,
                        d.ephemeral_pubkey_y_parity,
                    );

                    if let Some(proof) = proof {
                        warn!(
                            target: "zone::payload",
                            sender = %d.sender,
                            amount = %d.amount,
                            "Encrypted deposit decryption failed, providing valid proof for on-chain refund"
                        );
                        let decryption = abi::DecryptionData {
                            sharedSecret: proof.shared_secret,
                            sharedSecretYParity: proof.shared_secret_y_parity,
                            to: d.sender,
                            memo: B256::ZERO,
                            cpProof: abi::ChaumPedersenProof {
                                s: proof.cp_proof_s,
                                c: proof.cp_proof_c,
                            },
                        };
                        queued_deposits.push(queued);
                        decryptions.push(decryption);
                        continue;
                    }

                    warn!(
                        target: "zone::payload",
                        sender = %d.sender,
                        amount = %d.amount,
                        "Encrypted deposit has invalid ephemeral pubkey, using zeroed DecryptionData"
                    );
                    let decryption = abi::DecryptionData {
                        sharedSecret: B256::ZERO,
                        sharedSecretYParity: 0x02,
                        to: d.sender,
                        memo: B256::ZERO,
                        cpProof: abi::ChaumPedersenProof {
                            s: B256::ZERO,
                            c: B256::ZERO,
                        },
                    };
                    queued_deposits.push(queued);
                    decryptions.push(decryption);
                }
            }
        }

        let enabled_tokens: Vec<_> = self
            .events
            .enabled_tokens
            .iter()
            .map(|t| t.to_abi())
            .collect();

        let elapsed = start.elapsed();
        info!(
            target: "zone::engine",
            l1_block = l1_block_number,
            total_deposits,
            encrypted = decryptions.len(),
            enabled_tokens = enabled_tokens.len(),
            ?elapsed,
            "Prepared L1 block deposits"
        );

        Ok(PreparedL1Block {
            header: self.header,
            queued_deposits,
            decryptions,
            enabled_tokens,
        })
    }
}

/// An L1 block with deposits fully prepared for the payload builder.
///
/// All ECIES decryption, TIP-403 policy checks, and ABI encoding have been
/// performed. The builder only needs to RLP-encode the header and assemble
/// the `advanceTempo` calldata.
///
/// Implements `Serialize`/`Deserialize` to satisfy the `PayloadAttributes`
/// trait bound, but the deposit fields are `#[serde(skip)]` because the sol!
/// types don't derive serde. This is fine — payload attributes only flow
/// through in-process channels and are never serialised to the wire.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PreparedL1Block {
    /// The sealed L1 block header.
    pub header: SealedHeader<TempoHeader>,
    /// ABI-encoded queued deposits (regular + encrypted).
    #[serde(skip)]
    pub queued_deposits: Vec<abi::QueuedDeposit>,
    /// Decryption data for encrypted deposits (one per encrypted deposit, in order).
    #[serde(skip)]
    pub decryptions: Vec<abi::DecryptionData>,
    /// Tokens newly enabled for bridging in this block.
    #[serde(skip)]
    pub enabled_tokens: Vec<abi::EnabledToken>,
}
