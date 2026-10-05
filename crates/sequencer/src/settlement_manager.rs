use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Weak},
    time::Duration,
};

use alloy_consensus::BlockHeader as _;
use alloy_eips::BlockNumberOrTag;
use alloy_primitives::{Address, B256, Sealable as _, U256, keccak256};
use alloy_provider::{DynProvider, Provider as _};
use alloy_rpc_types_eth::BlockId;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolValue as _;
use eyre::OptionExt as _;
use parking_lot::Mutex;
use tempo_alloy::TempoNetwork;
use tempo_chainspec::TempoHardforks as _;
use tempo_zone_contracts::ZonePortal;
use tokio::sync::mpsc;
use tracing::info;
use zone_chainspec::ZoneChainSpec;
use zone_p2p::{P2pCommand, P2pPeerId};
use zone_prover::VerifierMode;

use crate::{
    BatchAnchorConfig, PreparedBatch, SettlementAbi,
    attestation::{
        AttestationDomain, FastSettlementProofPolicy, SettlementAttestation, SettlementCertificate,
        SignedSettlementAttestation, read_historical_fast_epoch_config,
    },
    settlement::BatchSubmitError,
};

const SETTLEMENT_REBROADCAST_INTERVAL: Duration = Duration::from_millis(500);

/// Prepares settlement certificates and routes follower signatures to active preparations.
#[derive(Clone)]
pub struct SettlementManager {
    domain: AttestationDomain,
    pinned_sequencer_set_version: Option<u64>,
    signer: PrivateKeySigner,
    addresses: HashMap<P2pPeerId, Address>,
    l1_provider: DynProvider<TempoNetwork>,
    chain_spec: Arc<ZoneChainSpec>,
    anchor_config: BatchAnchorConfig,
    p2p_tx: mpsc::Sender<P2pCommand>,
    pending: PendingSettlements,
    committed_guard: Option<SettlementCommittedGuard>,
}

/// Runtime hook that proves a proposed batch endpoint is the exact durable Raft committed head.
pub type SettlementCommittedGuard = Arc<dyn Fn(u64, B256) -> eyre::Result<()> + Send + Sync>;

impl std::fmt::Debug for SettlementManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SettlementManager")
            .finish_non_exhaustive()
    }
}

impl SettlementManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        domain: AttestationDomain,
        pinned_sequencer_set_version: Option<u64>,
        signer: PrivateKeySigner,
        addresses: HashMap<P2pPeerId, Address>,
        l1_provider: DynProvider<TempoNetwork>,
        chain_spec: Arc<ZoneChainSpec>,
        anchor_config: BatchAnchorConfig,
        p2p_tx: mpsc::Sender<P2pCommand>,
    ) -> Self {
        Self {
            domain,
            pinned_sequencer_set_version,
            signer,
            addresses,
            l1_provider,
            chain_spec,
            anchor_config,
            p2p_tx,
            pending: PendingSettlements::default(),
            committed_guard: None,
        }
    }

    /// Install the runtime-owned durable committed-prefix guard used by T14 settlement proposals.
    pub fn with_committed_guard(mut self, guard: SettlementCommittedGuard) -> Self {
        self.committed_guard = Some(guard);
        self
    }

    /// Prepare and collect the certificate for one exact batch, anchor, and verifier mode.
    pub async fn prepare(
        &self,
        prepared: &PreparedBatch,
        verifier_mode: VerifierMode,
    ) -> Result<SettlementCertificate, BatchSubmitError> {
        let status = self.settlement_status(prepared).await?;
        if status.portal_zone_height >= U256::from(prepared.batch.zone_height) {
            return Err(BatchSubmitError::PortalAdvanced);
        }
        let config = status.config;
        let mut threshold = status.threshold;

        if let SettlementAuthority::Fast { proof_policy, .. } = config.authority {
            proof_policy.validate_mode(verifier_mode)?;
            let guard = self.committed_guard.as_ref().ok_or_else(|| {
                eyre::eyre!("T14 settlement leader has no runtime committed-prefix guard")
            })?;
            guard(prepared.batch.zone_height, prepared.batch.next_block_hash)?;
        }

        self.validate_anchor(prepared).await?;
        let attestation = settlement_attestation(self.domain, &config, prepared, verifier_mode)?;
        let signed =
            SignedSettlementAttestation::sign(attestation.clone(), self.domain, &self.signer)?;
        if !status.allowed_signers.contains(&self.signer.address()) {
            return Err(eyre::eyre!(
                "local settlement signer is not enrolled at the imported L1 anchor"
            )
            .into());
        }
        let digest = self.domain.settlement_digest(&attestation);
        let mut signatures = BTreeMap::from([(self.signer.address(), signed.signature)]);
        let mut pending = self.pending.subscribe(digest)?;

        self.broadcast(&attestation, digest)?;
        let mut rebroadcast = tokio::time::interval_at(
            tokio::time::Instant::now() + SETTLEMENT_REBROADCAST_INTERVAL,
            SETTLEMENT_REBROADCAST_INTERVAL,
        );
        rebroadcast.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        while signatures.len() < threshold {
            tokio::select! {
                signature = pending.receiver.recv() => {
                    let signature = signature.ok_or_eyre("settlement signature route closed")?;
                    if signature.attestation != attestation {
                        tracing::warn!(target: "zone::p2p", %digest, "Rejected mismatched settlement signature");
                        continue;
                    }
                    match signature.recover_signer(self.domain) {
                        Ok(signer) if status.allowed_signers.contains(&signer) => {
                            signatures.entry(signer).or_insert(signature.signature);
                        }
                        Ok(signer) => tracing::warn!(target: "zone::p2p", %signer, "Rejected unenrolled settlement signer"),
                        Err(error) => tracing::warn!(target: "zone::p2p", %error, "Rejected settlement signature"),
                    }
                }
                _ = rebroadcast.tick() => {
                    self.validate_anchor(prepared).await?;
                    let live = self.settlement_status(prepared).await?;
                    if live.portal_zone_height >= U256::from(prepared.batch.zone_height) {
                        return Err(BatchSubmitError::PortalAdvanced);
                    }
                    if live.config != config {
                        return Err(eyre::eyre!(
                            "portal settlement configuration changed while collecting signatures"
                        )
                        .into());
                    }
                    threshold = live.threshold;
                    self.broadcast(&attestation, digest)?;
                }
            }
        }

        Ok(SettlementCertificate {
            height: prepared.batch.zone_height,
            digest,
            attestation,
            signatures: signatures.into_values().collect(),
        })
    }

    async fn settlement_status(
        &self,
        prepared: &PreparedBatch,
    ) -> Result<SettlementStatus, BatchSubmitError> {
        let portal = ZonePortal::new(self.domain.portal_address, self.l1_provider.clone());
        let imported_number = prepared.batch.tempo_block_number;
        let imported_hash = prepared.batch.tempo_block_hash;
        let finalized = self
            .l1_provider
            .get_header_by_number(BlockNumberOrTag::Finalized)
            .await
            .map_err(|error| eyre::eyre!(error))?
            .ok_or_else(|| eyre::eyre!("finalized L1 header is unavailable"))?;
        if finalized.number() < imported_number {
            return Err(eyre::eyre!(
                "zone batch imported L1 block {imported_number}, but finalized head is {}",
                finalized.number()
            )
            .into());
        }
        let imported = self
            .l1_provider
            .get_header_by_number(imported_number.into())
            .await
            .map_err(|error| eyre::eyre!(error))?
            .ok_or_else(|| eyre::eyre!("imported L1 header {imported_number} is unavailable"))?;
        if imported.hash_slow() != imported_hash {
            return Err(eyre::eyre!(
                "canonical L1 hash for imported Tempo block {imported_number} is {}, but the canonical Zone batch imported {imported_hash}",
                imported.hash_slow()
            )
            .into());
        }
        let block = BlockId::hash_canonical(imported_hash);
        let live_header = self
            .l1_provider
            .get_header_by_number(BlockNumberOrTag::Latest)
            .await
            .map_err(|error| eyre::eyre!(error))?
            .ok_or_else(|| eyre::eyre!("latest L1 header is unavailable"))?;
        let live_block = BlockId::hash_canonical(live_header.hash_slow());
        let settlement_abi =
            SettlementAbi::from_hardfork(self.chain_spec.tempo_hardfork_at(imported.timestamp()));
        let verifier = portal.verifier().block(block).call().await?;
        let portal_zone_height = portal.zoneHeight().block(live_block).call().await?;
        let fast_epoch = portal.fastEpoch().block(block).call().await?;
        let fast_active = portal.fastEpochActive().block(block).call().await?;
        let live_fast_epoch = portal.fastEpoch().block(live_block).call().await?;
        let live_fast_active = portal.fastEpochActive().block(live_block).call().await?;
        if live_fast_epoch != fast_epoch || live_fast_active != fast_active {
            return Err(eyre::eyre!(
                "live fast authority does not match the batch's imported Tempo configuration"
            )
            .into());
        }

        let (authority, threshold, allowed_signers) = if fast_active {
            if fast_epoch == 0 {
                return Err(
                    eyre::eyre!("portal reported active fast authority with epoch zero").into(),
                );
            }
            let config = read_historical_fast_epoch_config(
                &self.l1_provider,
                self.domain.portal_address,
                fast_epoch,
                block,
            )
            .await?;
            let live_config = read_historical_fast_epoch_config(
                &self.l1_provider,
                self.domain.portal_address,
                fast_epoch,
                live_block,
            )
            .await?;
            if !live_config.finalSettlementHash.is_zero() {
                return Err(eyre::eyre!(
                    "fast epoch {fast_epoch} recorded its final settlement; further settlement is fenced"
                )
                .into());
            }
            if !config.finalSettlementHash.is_zero() {
                return Err(eyre::eyre!(
                    "fast epoch {fast_epoch} recorded its final settlement; further settlement is fenced"
                )
                .into());
            }
            if config.threshold != 2 {
                return Err(eyre::eyre!(
                    "fast epoch {fast_epoch} threshold is {}, expected exactly 2",
                    config.threshold
                )
                .into());
            }
            let proof_policy = FastSettlementProofPolicy::from_config(&config)?;
            let verifier_code = self
                .l1_provider
                .get_code_at(verifier)
                .block_id(block)
                .await
                .map_err(|error| eyre::eyre!(error))?;
            let live_verifier_code = self
                .l1_provider
                .get_code_at(verifier)
                .block_id(live_block)
                .await
                .map_err(|error| eyre::eyre!(error))?;
            let verifier_code_hash = keccak256(&verifier_code);
            if verifier_code_hash != proof_policy.expected_verifier_code_hash
                || keccak256(&live_verifier_code) != proof_policy.expected_verifier_code_hash
            {
                return Err(eyre::eyre!(
                    "verifier code hash {verifier_code_hash} does not match fast epoch {fast_epoch} enrollment {}",
                    proof_policy.expected_verifier_code_hash
                )
                .into());
            }
            let count = portal
                .fastEpochMemberCount(fast_epoch)
                .block(block)
                .call()
                .await?;
            if count != U256::from(3) {
                return Err(eyre::eyre!(
                    "fast epoch {fast_epoch} has {count} members, expected exactly 3"
                )
                .into());
            }
            let mut members = Vec::with_capacity(3);
            for index in 0..3 {
                let member = portal
                    .fastEpochMemberAt(fast_epoch, U256::from(index))
                    .block(block)
                    .call()
                    .await?;
                if member.is_zero() || members.contains(&member) {
                    return Err(eyre::eyre!(
                        "fast epoch roster contains a zero or duplicate member"
                    )
                    .into());
                }
                members.push(member);
            }
            let live_verifier = portal.verifier().block(live_block).call().await?;
            if live_verifier != verifier {
                return Err(eyre::eyre!(
                    "live verifier changed from the batch's imported fast enrollment"
                )
                .into());
            }
            let previous_block_hash = portal.blockHash().block(live_block).call().await?;
            let previous_withdrawal_batch_index = portal
                .withdrawalBatchIndex()
                .block(live_block)
                .call()
                .await?;
            (
                SettlementAuthority::Fast {
                    epoch: fast_epoch,
                    roster_hash: config.rosterHash,
                    previous_zone_height: portal_zone_height,
                    previous_block_hash,
                    previous_withdrawal_batch_index,
                    proof_policy,
                },
                2,
                members,
            )
        } else {
            let sequencer_set_version = portal.sequencerSetVersion().block(block).call().await?;
            if let Some(pinned) = self.pinned_sequencer_set_version
                && sequencer_set_version != pinned
            {
                return Err(eyre::eyre!(
                    "portal signer-set version {sequencer_set_version} does not match startup-pinned version {pinned}"
                )
                .into());
            }
            let threshold = usize::from(portal.sequencerThreshold().block(block).call().await?);
            let mut members: Vec<_> = self.addresses.values().copied().collect();
            if !members.contains(&self.signer.address()) {
                members.push(self.signer.address());
            }
            (
                SettlementAuthority::Standard {
                    sequencer_set_version,
                },
                threshold,
                members,
            )
        };
        if threshold == 0 {
            return Err(eyre::eyre!("portal sequencer threshold is zero").into());
        }

        Ok(SettlementStatus {
            config: SettlementConfig {
                abi: settlement_abi,
                authority,
                verifier,
            },
            threshold,
            portal_zone_height,
            allowed_signers,
        })
    }

    /// Authenticate and route one follower response to its pending settlement.
    pub fn add_signature(&self, follower: P2pPeerId, encoded: &[u8]) -> eyre::Result<()> {
        let signed = SignedSettlementAttestation::decode(encoded)?;
        let expected = self
            .addresses
            .get(&follower)
            .ok_or_eyre("unknown follower identity")?;
        let signer = signed.recover_signer(self.domain)?;
        eyre::ensure!(
            signer == *expected,
            "settlement signer does not match authenticated peer"
        );
        let digest = self.domain.settlement_digest(&signed.attestation);
        self.pending.route(digest, signed)
    }

    fn broadcast(&self, attestation: &SettlementAttestation, digest: B256) -> eyre::Result<()> {
        match self
            .p2p_tx
            .try_send(P2pCommand::BroadcastSettlementProposal(
                attestation.encode(),
            )) {
            Ok(()) => {
                info!(target: "zone::p2p", %digest, height = %attestation.zoneHeight, "Broadcast settlement proposal")
            }
            Err(mpsc::error::TrySendError::Full(_)) => {}
            Err(mpsc::error::TrySendError::Closed(_)) => {
                eyre::bail!("P2P command channel closed");
            }
        }
        Ok(())
    }

    async fn validate_anchor(&self, prepared: &PreparedBatch) -> Result<(), BatchSubmitError> {
        let anchor_number = prepared.anchor_block_number();
        let current = self
            .l1_provider
            .get_block_number()
            .await
            .map_err(|error| eyre::eyre!(error))?;
        if anchor_number < prepared.batch.tempo_block_number {
            return Err(BatchSubmitError::PreparedAnchorInvalid(eyre::eyre!(
                "prepared L1 anchor predates the batch Tempo block"
            )));
        }
        if anchor_number > current {
            return Err(BatchSubmitError::PreparedAnchorInvalid(eyre::eyre!(
                "prepared L1 anchor block is ahead of the current L1 tip"
            )));
        }
        if current.saturating_sub(anchor_number) >= self.anchor_config.history_window() {
            return Err(BatchSubmitError::PreparedAnchorInvalid(eyre::eyre!(
                "prepared L1 anchor block fell outside the EIP-2935 history window"
            )));
        }
        let canonical = self
            .l1_provider
            .get_header_by_number(anchor_number.into())
            .await
            .map_err(|error| eyre::eyre!(error))?
            .ok_or_else(|| eyre::eyre!("prepared L1 anchor block {anchor_number} not found"))?;
        if canonical.inner.hash != prepared.anchor.block_hash() {
            return Err(BatchSubmitError::PreparedAnchorInvalid(eyre::eyre!(
                "prepared L1 anchor hash is no longer canonical"
            )));
        }
        Ok(())
    }
}

/// Portal configuration that must remain fixed while collecting signatures.
#[derive(PartialEq, Eq)]
struct SettlementConfig {
    abi: SettlementAbi,
    authority: SettlementAuthority,
    verifier: Address,
}

#[derive(PartialEq, Eq)]
enum SettlementAuthority {
    Standard {
        sequencer_set_version: u64,
    },
    Fast {
        epoch: u64,
        roster_hash: B256,
        previous_zone_height: U256,
        previous_block_hash: B256,
        previous_withdrawal_batch_index: u64,
        proof_policy: FastSettlementProofPolicy,
    },
}

struct SettlementStatus {
    config: SettlementConfig,
    threshold: usize,
    portal_zone_height: U256,
    allowed_signers: Vec<Address>,
}

fn settlement_attestation(
    domain: AttestationDomain,
    config: &SettlementConfig,
    prepared: &PreparedBatch,
    verifier_mode: VerifierMode,
) -> eyre::Result<SettlementAttestation> {
    let batch = &prepared.batch;
    let (
        sequencer_set_version,
        fast_epoch,
        roster_hash,
        previous_zone_height,
        previous_block_hash,
        previous_withdrawal_batch_index,
    ) = match &config.authority {
        SettlementAuthority::Standard {
            sequencer_set_version,
        } => (
            *sequencer_set_version,
            0,
            B256::ZERO,
            U256::ZERO,
            B256::ZERO,
            0,
        ),
        SettlementAuthority::Fast {
            epoch,
            roster_hash,
            previous_zone_height,
            previous_block_hash,
            previous_withdrawal_batch_index,
            proof_policy,
            ..
        } => {
            proof_policy.validate_mode(verifier_mode)?;
            eyre::ensure!(
                U256::from(batch.zone_height) > *previous_zone_height,
                "fast settlement height does not extend the accepted Portal prefix"
            );
            eyre::ensure!(
                *previous_block_hash == batch.prev_block_hash,
                "fast settlement previous block hash does not match the accepted Portal prefix"
            );
            eyre::ensure!(
                previous_withdrawal_batch_index.checked_add(1)
                    == Some(batch.withdrawal_batch_index),
                "fast settlement withdrawal index does not extend the accepted Portal prefix"
            );
            (
                0,
                *epoch,
                *roster_hash,
                *previous_zone_height,
                *previous_block_hash,
                *previous_withdrawal_batch_index,
            )
        }
    };
    Ok(SettlementAttestation {
        zoneId: domain.zone_id,
        sequencerSetVersion: sequencer_set_version,
        fastEpoch: fast_epoch,
        rosterHash: roster_hash,
        previousZoneHeight: previous_zone_height,
        previousBlockHash: previous_block_hash,
        previousWithdrawalBatchIndex: previous_withdrawal_batch_index,
        zoneHeight: U256::from(batch.zone_height),
        withdrawalBatchIndex: U256::from(batch.withdrawal_batch_index),
        verifier: config.verifier,
        tempoBlockNumber: batch.tempo_block_number,
        anchorBlockNumber: prepared.anchor_block_number(),
        anchorBlockHash: prepared.anchor.block_hash(),
        blockTransitionHash: keccak256((batch.prev_block_hash, batch.next_block_hash).abi_encode()),
        depositQueueTransitionHash: keccak256(
            (
                batch.prev_processed_deposit_hash,
                batch.next_processed_deposit_hash,
                batch.prev_deposit_number,
                batch.next_deposit_number,
            )
                .abi_encode(),
        ),
        tokenEnablementTransitionHash: config.abi.token_transition_hash(
            batch.prev_processed_token_count,
            batch.next_processed_token_count,
        ),
        withdrawalQueueHash: batch.withdrawal_queue_hash,
        verifierConfigHash: verifier_mode.config_hash(),
    })
}

#[derive(Clone, Default)]
struct PendingSettlements {
    senders: Arc<Mutex<HashMap<B256, Weak<mpsc::UnboundedSender<SignedSettlementAttestation>>>>>,
}

impl PendingSettlements {
    fn subscribe(&self, digest: B256) -> eyre::Result<PendingSettlement> {
        let mut senders = self.senders.lock();
        senders.retain(|_, sender| sender.strong_count() > 0);
        eyre::ensure!(
            !senders.contains_key(&digest),
            "settlement digest is already pending"
        );
        let (sender, receiver) = mpsc::unbounded_channel();
        let sender = Arc::new(sender);
        senders.insert(digest, Arc::downgrade(&sender));
        Ok(PendingSettlement {
            _sender: sender,
            receiver,
        })
    }

    fn route(&self, digest: B256, signed: SignedSettlementAttestation) -> eyre::Result<()> {
        let mut senders = self.senders.lock();
        let Some(sender) = senders.get(&digest).and_then(|sender| sender.upgrade()) else {
            senders.remove(&digest);
            eyre::bail!("settlement response has no pending request");
        };
        if sender.send(signed).is_err() {
            senders.remove(&digest);
            eyre::bail!("pending settlement receiver closed");
        }
        Ok(())
    }
}

struct PendingSettlement {
    _sender: Arc<mpsc::UnboundedSender<SignedSettlementAttestation>>,
    receiver: mpsc::UnboundedReceiver<SignedSettlementAttestation>,
}

#[cfg(test)]
mod tests {
    use alloy_signer_local::PrivateKeySigner;

    use super::*;

    fn domain() -> AttestationDomain {
        AttestationDomain {
            l1_chain_id: 1337,
            portal_address: Address::repeat_byte(0x11),
            zone_id: 7,
        }
    }

    fn signed(height: u64) -> SignedSettlementAttestation {
        SignedSettlementAttestation::sign(
            SettlementAttestation {
                zoneId: 7,
                sequencerSetVersion: 1,
                fastEpoch: 0,
                rosterHash: B256::ZERO,
                previousZoneHeight: U256::ZERO,
                previousBlockHash: B256::ZERO,
                previousWithdrawalBatchIndex: 0,
                zoneHeight: U256::from(height),
                withdrawalBatchIndex: U256::from(height),
                verifier: Address::repeat_byte(0x22),
                tempoBlockNumber: height,
                anchorBlockNumber: height,
                anchorBlockHash: B256::repeat_byte(height as u8),
                blockTransitionHash: B256::repeat_byte(0x33),
                depositQueueTransitionHash: B256::repeat_byte(0x44),
                tokenEnablementTransitionHash: B256::repeat_byte(0x55),
                withdrawalQueueHash: B256::repeat_byte(0x66),
                verifierConfigHash: B256::repeat_byte(0x77),
            },
            domain(),
            &PrivateKeySigner::random(),
        )
        .unwrap()
    }

    #[test]
    fn routes_concurrent_settlements_by_digest() {
        let pending = PendingSettlements::default();
        let first_digest = B256::repeat_byte(1);
        let second_digest = B256::repeat_byte(2);
        let mut first = pending.subscribe(first_digest).unwrap();
        let mut second = pending.subscribe(second_digest).unwrap();
        let first_signature = signed(120);
        let second_signature = signed(240);

        pending
            .route(second_digest, second_signature.clone())
            .unwrap();
        pending
            .route(first_digest, first_signature.clone())
            .unwrap();

        assert_eq!(first.receiver.try_recv().unwrap(), first_signature);
        assert_eq!(second.receiver.try_recv().unwrap(), second_signature);
    }

    #[test]
    fn dropping_request_releases_digest() {
        let pending = PendingSettlements::default();
        let digest = B256::repeat_byte(1);
        let request = pending.subscribe(digest).unwrap();
        assert!(pending.subscribe(digest).is_err());

        drop(request);

        assert!(pending.subscribe(digest).is_ok());
    }

    #[test]
    fn explicit_verifier_modes_require_distinct_signatures() {
        let prepared = PreparedBatch {
            batch: crate::BatchData {
                zone_height: 120,
                tempo_block_number: 100,
                tempo_block_hash: B256::repeat_byte(3),
                prev_block_hash: B256::repeat_byte(1),
                next_block_hash: B256::repeat_byte(2),
                prev_processed_deposit_hash: B256::ZERO,
                next_processed_deposit_hash: B256::ZERO,
                prev_deposit_number: 0,
                next_deposit_number: 0,
                prev_processed_token_count: 0,
                next_processed_token_count: 0,
                withdrawal_queue_hash: B256::ZERO,
                withdrawal_batch_index: 1,
            },
            anchor: crate::BatchAnchor::Direct {
                block_hash: B256::repeat_byte(3),
            },
        };
        let config = SettlementConfig {
            abi: SettlementAbi::T13,
            authority: SettlementAuthority::Standard {
                sequencer_set_version: 1,
            },
            verifier: Address::repeat_byte(4),
        };
        let signer = PrivateKeySigner::random();
        let nitro =
            settlement_attestation(domain(), &config, &prepared, VerifierMode::NitroV1).unwrap();
        let operator_attested =
            settlement_attestation(domain(), &config, &prepared, VerifierMode::NoProof).unwrap();
        assert_eq!(
            operator_attested.verifierConfigHash,
            VerifierMode::NoProof.config_hash()
        );
        let nitro_digest = domain().settlement_digest(&nitro);
        let operator_digest = domain().settlement_digest(&operator_attested);
        assert_ne!(nitro_digest, operator_digest);

        let pending = PendingSettlements::default();
        let nitro_request = pending.subscribe(nitro_digest).unwrap();
        drop(nitro_request);
        let mut operator_request = pending.subscribe(operator_digest).unwrap();
        let stale = SignedSettlementAttestation::sign(nitro, domain(), &signer).unwrap();
        assert!(pending.route(nitro_digest, stale).is_err());
        assert!(operator_request.receiver.try_recv().is_err());
        let fresh =
            SignedSettlementAttestation::sign(operator_attested, domain(), &signer).unwrap();
        pending.route(operator_digest, fresh.clone()).unwrap();
        assert_eq!(operator_request.receiver.try_recv().unwrap(), fresh);
    }
}
