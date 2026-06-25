use super::*;

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
        policy_provider: &crate::state::PolicyProvider,
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
                        bouncebackRecipient: d.bounceback_recipient,
                        memo: d.memo,
                    };
                    queued_deposits.push(abi::QueuedDeposit {
                        depositType: abi::DepositType::Regular,
                        depositData: Bytes::from(deposit.abi_encode()),
                        rejected: false,
                    });
                }
                L1Deposit::Encrypted(d) => {
                    let mut queued = abi::QueuedDeposit {
                        depositType: abi::DepositType::Encrypted,
                        depositData: Bytes::from(
                            abi::EncryptedDeposit {
                                token: d.token,
                                sender: d.sender,
                                amount: d.amount,
                                bouncebackRecipient: d.bounceback_recipient,
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
                        rejected: false,
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
                                crate::state::AuthRole::MintRecipient,
                            )
                            .await?;

                        if authorized {
                            debug!(
                                target: "zone::engine",
                                recipient = %dec.to,
                                token = %d.token,
                                "Policy authorized encrypted deposit recipient"
                            );
                        } else {
                            warn!(
                                target: "zone::engine",
                                sender = %d.sender,
                                recipient = %dec.to,
                                token = %d.token,
                                amount = %d.amount,
                                "Encrypted deposit recipient unauthorized; queuing deposit bounce-back"
                            );
                            queued.rejected = true;
                            queued_deposits.push(queued);
                            continue;
                        }

                        let decryption = abi::DecryptionData {
                            sharedSecret: dec.proof.shared_secret,
                            sharedSecretYParity: dec.proof.shared_secret_y_parity,
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PreparedL1Block {
    /// The sealed L1 block header.
    pub header: SealedHeader<TempoHeader>,
    /// ABI-encoded queued deposits (regular + encrypted).
    #[serde(skip)]
    pub queued_deposits: Vec<abi::QueuedDeposit>,
    /// Decryption data for non-rejected encrypted deposits, in order.
    #[serde(skip)]
    pub decryptions: Vec<abi::DecryptionData>,
    /// Tokens newly enabled for bridging in this block.
    #[serde(skip)]
    pub enabled_tokens: Vec<abi::EnabledToken>,
}
