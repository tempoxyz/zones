use super::*;
use rayon::prelude::*;
use std::collections::BTreeMap;

/// An L1 block's header paired with the deposits found in that block.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct L1BlockDeposits {
    /// The sealed L1 block header (caches the block hash).
    pub header: SealedHeader<TempoHeader>,
    /// Portal events extracted from this block.
    pub events: L1PortalEvents,
}

impl L1BlockDeposits {
    /// Prepare all deposits for the payload builder.
    ///
    /// Decrypts deposits and ABI-encodes the types the `advanceTempo` call expects.
    /// Mint-recipient policy is enforced by upstream TIP-20 after the L1 state is anchored.
    /// The resulting [`PreparedL1Block`] is ready to be passed via payload attributes to the
    /// builder.
    pub async fn prepare(
        self,
        encryption_keys: &EncryptionKeyRing,
        portal_address: Address,
    ) -> eyre::Result<PreparedL1Block> {
        let start = std::time::Instant::now();
        let Self { header, events } = self;
        let l1_block_number = header.inner.number;
        let total_deposits = events.deposits.len();
        let deposits = events.deposits;

        // Resolve the key material for every distinct index once, in deposit order so a missing
        // index still surfaces as the same error. The per-deposit work then only needs the keys.
        let mut keys: BTreeMap<U256, k256::SecretKey> = BTreeMap::new();
        for deposit in &deposits {
            if let L1Deposit::Deposit(d) = deposit
                && !keys.contains_key(&d.key_index)
            {
                keys.insert(d.key_index, encryption_keys.key(d.key_index)?);
            }
        }

        // Each deposit costs hundreds of microseconds of ECIES work, so a full block would stall
        // the engine task for tens of milliseconds. Blocks without encrypted deposits only need
        // ABI encoding and stay on the calling task.
        let prepared = if keys.is_empty() {
            deposits
                .iter()
                .map(|deposit| prepare_deposit(deposit, &keys, portal_address, l1_block_number))
                .collect::<Vec<_>>()
        } else {
            tokio::task::spawn_blocking(move || {
                deposits
                    .par_iter()
                    .map(|deposit| prepare_deposit(deposit, &keys, portal_address, l1_block_number))
                    .collect::<Vec<_>>()
            })
            .await?
        };

        let mut queued_deposits: Vec<abi::QueuedDeposit> = Vec::with_capacity(prepared.len());
        let mut decryptions: Vec<abi::DecryptionData> = Vec::new();
        for (queued, decryption) in prepared {
            queued_deposits.push(queued);
            decryptions.extend(decryption);
        }

        let enabled_tokens: Vec<_> = events.enabled_tokens.iter().map(|t| t.to_abi()).collect();

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
            header,
            queued_deposits,
            decryptions,
            enabled_tokens,
        })
    }
}

/// An L1 block with deposits fully prepared for the payload builder.
///
/// All ECIES decryption and ABI encoding have been performed.
/// The builder only needs to RLP-encode the header and assemble the `advanceTempo` calldata.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PreparedL1Block {
    /// The sealed L1 block header.
    pub header: SealedHeader<TempoHeader>,
    /// ABI-encoded user deposits and internal withdrawal bounce-backs.
    #[serde(skip)]
    pub queued_deposits: Vec<abi::QueuedDeposit>,
    /// Decryption data for every user deposit submitted for on-chain verification, in order.
    #[serde(skip)]
    pub decryptions: Vec<abi::DecryptionData>,
    /// Tokens newly enabled for bridging in this block.
    #[serde(skip)]
    pub enabled_tokens: Vec<abi::EnabledToken>,
}

/// ABI-encode a single queue entry and, for user deposits, derive the decryption data the on-chain
/// verification expects.
///
/// Depends only on its inputs, so callers are free to run it for several deposits in parallel.
fn prepare_deposit(
    deposit: &L1Deposit,
    keys: &BTreeMap<U256, k256::SecretKey>,
    portal_address: Address,
    l1_block_number: u64,
) -> (abi::QueuedDeposit, Option<abi::DecryptionData>) {
    use crate::precompiles::ecies;

    let queued = deposit.to_abi_queued_deposit();
    let L1Deposit::Deposit(d) = deposit else {
        return (queued, None);
    };
    let decryption_key = keys
        .get(&d.key_index)
        .expect("every deposit's key index is resolved before decryption");

    // Attempt full ECIES decryption.
    let dec = ecies::decrypt_deposit(
        decryption_key,
        &d.ephemeral_pubkey_x,
        d.ephemeral_pubkey_y_parity,
        &d.ciphertext,
        &d.nonce,
        &d.tag,
        portal_address,
        d.key_index,
        d.sender,
    );

    if let Some(dec) = dec {
        debug!(
            target: "zone::engine",
            l1_block = l1_block_number,
            sender = %d.sender,
            recipient = %dec.to,
            token = %d.token,
            amount = %d.amount,
            "Decrypted deposit"
        );

        return (
            queued,
            Some(abi::DecryptionData {
                sharedSecret: dec.proof.shared_secret,
                sharedSecretYParity: dec.proof.shared_secret_y_parity,
                cpProof: abi::ChaumPedersenProof {
                    s: dec.proof.cp_proof_s,
                    c: dec.proof.cp_proof_c,
                },
            }),
        );
    }

    // Full decryption failed — try ECDH proof for on-chain refund.
    let proof = ecies::compute_ecdh_proof(
        decryption_key,
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
        return (
            queued,
            Some(abi::DecryptionData {
                sharedSecret: proof.shared_secret,
                sharedSecretYParity: proof.shared_secret_y_parity,
                cpProof: abi::ChaumPedersenProof {
                    s: proof.cp_proof_s,
                    c: proof.cp_proof_c,
                },
            }),
        );
    }

    warn!(
        target: "zone::payload",
        sender = %d.sender,
        amount = %d.amount,
        "Encrypted deposit has invalid ephemeral pubkey, using zeroed DecryptionData"
    );
    (
        queued,
        Some(abi::DecryptionData {
            sharedSecret: B256::ZERO,
            sharedSecretYParity: 0x02,
            cpProof: abi::ChaumPedersenProof {
                s: B256::ZERO,
                c: B256::ZERO,
            },
        }),
    )
}
