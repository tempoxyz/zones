//! EIP-712 replication ACKs, settlement attestations, and leader-side storage.

use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

use alloy_primitives::{Address, B256, Bytes, Signature};
use alloy_signer::SignerSync as _;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{Eip712Domain, SolStruct as _, SolValue as _, eip712_domain, sol};
use eyre::WrapErr as _;
use tokio::sync::{Notify, watch};

use crate::settlement::BatchAnchor;

type SettlementSignatures =
    BTreeMap<u64, BTreeMap<B256, BTreeMap<Address, SignedSettlementAttestation>>>;

#[derive(Debug, Default)]
struct AttestationState {
    settlements: SettlementSignatures,
    prepared_anchors: BTreeMap<u64, BatchAnchor>,
}

sol! {
    /// Exact settlement statement verified by ZonePortal.
    #[derive(Debug, PartialEq, Eq)]
    struct SettlementAttestation {
        uint32 zoneId;
        uint64 sequencerSetVersion;
        uint256 zoneHeight;
        uint256 withdrawalBatchIndex;
        address verifier;
        uint64 tempoBlockNumber;
        uint64 anchorBlockNumber;
        bytes32 anchorBlockHash;
        bytes32 blockTransitionHash;
        bytes32 depositQueueTransitionHash;
        bytes32 tokenEnablementTransitionHash;
        bytes32 withdrawalQueueHash;
        bytes32 verifierConfigHash;
    }

    /// Settlement signature returned to the leader for quorum collection.
    #[derive(Debug, PartialEq, Eq)]
    struct SignedSettlementAttestation {
        SettlementAttestation attestation;
        bytes signature;
    }

}

mod legacy {
    alloy_sol_types::sol! {
        /// Settlement statement used by the pre-T12 portal ABI.
        #[derive(Debug, PartialEq, Eq)]
        struct SettlementAttestation {
            uint32 zoneId;
            uint64 sequencerSetVersion;
            uint256 zoneHeight;
            uint256 withdrawalBatchIndex;
            address verifier;
            uint64 tempoBlockNumber;
            uint64 anchorBlockNumber;
            bytes32 anchorBlockHash;
            bytes32 blockTransitionHash;
            bytes32 depositQueueTransitionHash;
            bytes32 withdrawalQueueHash;
            bytes32 verifierConfigHash;
        }

        /// Signed settlement statement used by the pre-T12 portal ABI.
        #[derive(Debug, PartialEq, Eq)]
        struct SignedSettlementAttestation {
            SettlementAttestation attestation;
            bytes signature;
        }
    }
}

use legacy::{
    SettlementAttestation as LegacySettlementAttestation,
    SignedSettlementAttestation as LegacySignedSettlementAttestation,
};

/// Immutable values that domain-separate one zone's attestations.
#[derive(Debug, Clone, Copy)]
pub struct AttestationDomain {
    pub l1_chain_id: u64,
    pub portal_address: Address,
    pub zone_id: u32,
}

impl AttestationDomain {
    fn eip712(self) -> Eip712Domain {
        eip712_domain! {
            name: "ZonePortal",
            version: "1",
            chain_id: self.l1_chain_id,
            verifying_contract: self.portal_address,
        }
    }

    pub fn settlement_digest(self, attestation: &SettlementAttestation) -> B256 {
        if attestation.is_legacy() {
            attestation.as_legacy().eip712_signing_hash(&self.eip712())
        } else {
            attestation.eip712_signing_hash(&self.eip712())
        }
    }
}

impl SettlementAttestation {
    /// Legacy attestations use a zero sentinel for the field absent from their wire format.
    pub fn is_legacy(&self) -> bool {
        self.tokenEnablementTransitionHash.is_zero()
    }

    fn as_legacy(&self) -> LegacySettlementAttestation {
        LegacySettlementAttestation {
            zoneId: self.zoneId,
            sequencerSetVersion: self.sequencerSetVersion,
            zoneHeight: self.zoneHeight,
            withdrawalBatchIndex: self.withdrawalBatchIndex,
            verifier: self.verifier,
            tempoBlockNumber: self.tempoBlockNumber,
            anchorBlockNumber: self.anchorBlockNumber,
            anchorBlockHash: self.anchorBlockHash,
            blockTransitionHash: self.blockTransitionHash,
            depositQueueTransitionHash: self.depositQueueTransitionHash,
            withdrawalQueueHash: self.withdrawalQueueHash,
            verifierConfigHash: self.verifierConfigHash,
        }
    }

    fn from_legacy(attestation: LegacySettlementAttestation) -> Self {
        Self {
            zoneId: attestation.zoneId,
            sequencerSetVersion: attestation.sequencerSetVersion,
            zoneHeight: attestation.zoneHeight,
            withdrawalBatchIndex: attestation.withdrawalBatchIndex,
            verifier: attestation.verifier,
            tempoBlockNumber: attestation.tempoBlockNumber,
            anchorBlockNumber: attestation.anchorBlockNumber,
            anchorBlockHash: attestation.anchorBlockHash,
            blockTransitionHash: attestation.blockTransitionHash,
            depositQueueTransitionHash: attestation.depositQueueTransitionHash,
            tokenEnablementTransitionHash: B256::ZERO,
            withdrawalQueueHash: attestation.withdrawalQueueHash,
            verifierConfigHash: attestation.verifierConfigHash,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        if self.is_legacy() {
            self.as_legacy().abi_encode()
        } else {
            self.abi_encode()
        }
    }

    pub fn decode(encoded: &[u8]) -> eyre::Result<Self> {
        Self::abi_decode(encoded)
            .or_else(|_| LegacySettlementAttestation::abi_decode(encoded).map(Self::from_legacy))
            .wrap_err("invalid settlement proposal encoding")
    }
}

impl SignedSettlementAttestation {
    pub fn sign(
        attestation: SettlementAttestation,
        domain: AttestationDomain,
        signer: &PrivateKeySigner,
    ) -> eyre::Result<Self> {
        let signature = signer.sign_hash_sync(&domain.settlement_digest(&attestation))?;
        Ok(Self {
            attestation,
            signature: Bytes::copy_from_slice(&signature.as_bytes()),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        if self.attestation.is_legacy() {
            LegacySignedSettlementAttestation {
                attestation: self.attestation.as_legacy(),
                signature: self.signature.clone(),
            }
            .abi_encode()
        } else {
            self.abi_encode()
        }
    }

    pub fn decode(encoded: &[u8]) -> eyre::Result<Self> {
        Self::abi_decode(encoded)
            .or_else(|_| {
                LegacySignedSettlementAttestation::abi_decode(encoded).map(|signed| Self {
                    attestation: SettlementAttestation::from_legacy(signed.attestation),
                    signature: signed.signature,
                })
            })
            .wrap_err("invalid settlement signature encoding")
    }

    pub fn recover_signer(&self, domain: AttestationDomain) -> eyre::Result<Address> {
        let signature = Signature::try_from(self.signature.as_ref())
            .wrap_err("invalid settlement signature")?;
        alloy_consensus::crypto::secp256k1::recover_signer(
            &signature,
            domain.settlement_digest(&self.attestation),
        )
        .wrap_err("failed recovering settlement signer")
    }
}

/// A settlement statement and its distinct signer signatures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementCertificate {
    pub height: u64,
    pub digest: B256,
    pub attestation: SettlementAttestation,
    pub signatures: Vec<Bytes>,
}

/// Settlement certificates shared by P2P and batch submission.
#[derive(Debug, Clone)]
pub struct AttestationStore {
    state: Arc<RwLock<AttestationState>>,
    settlement_changed: Arc<Notify>,
    submitted_height: watch::Sender<u64>,
}

impl Default for AttestationStore {
    fn default() -> Self {
        let (submitted_height, _) = watch::channel(0);
        Self {
            state: Arc::default(),
            settlement_changed: Arc::default(),
            submitted_height,
        }
    }
}

impl AttestationStore {
    /// Insert one settlement signature per recovered signer and statement digest.
    pub fn insert_settlement(
        &self,
        domain: AttestationDomain,
        signer: Address,
        signed: SignedSettlementAttestation,
    ) -> (bool, usize) {
        let height = signed
            .attestation
            .zoneHeight
            .try_into()
            .expect("validated settlement zone height must fit in u64");
        let digest = domain.settlement_digest(&signed.attestation);

        let (inserted, signature_count) = {
            let mut state = self.state.write().expect("attestation store lock poisoned");

            let signatures = state
                .settlements
                .entry(height)
                .or_default()
                .entry(digest)
                .or_default();
            let inserted = signatures.insert(signer, signed).is_none();
            (inserted, signatures.len())
        };

        // There is one in-order batch submission waiter; notify_one retains a permit if insertion
        // races between its store check and awaiting the notification.
        self.settlement_changed.notify_one();

        (inserted, signature_count)
    }

    /// Check that a follower signature belongs to an active leader proposal and is new.
    pub fn precheck_follower_settlement(
        &self,
        height: u64,
        digest: B256,
        leader: Address,
        follower: Address,
    ) -> eyre::Result<()> {
        let state = self.state.read().expect("attestation store lock poisoned");
        let signatures = state
            .settlements
            .get(&height)
            .and_then(|by_digest| by_digest.get(&digest))
            .filter(|signatures| signatures.contains_key(&leader))
            .ok_or_else(|| eyre::eyre!("settlement response has no active leader proposal"))?;
        eyre::ensure!(
            !signatures.contains_key(&follower),
            "settlement response signer is already stored"
        );
        Ok(())
    }

    /// Insert a new follower signature only while its leader proposal remains active.
    pub fn insert_follower_settlement(
        &self,
        domain: AttestationDomain,
        leader: Address,
        follower: Address,
        signed: SignedSettlementAttestation,
    ) -> eyre::Result<usize> {
        let height = signed
            .attestation
            .zoneHeight
            .try_into()
            .expect("validated settlement zone height must fit in u64");
        let digest = domain.settlement_digest(&signed.attestation);

        let signature_count = {
            let mut state = self.state.write().expect("attestation store lock poisoned");
            let signatures = state
                .settlements
                .get_mut(&height)
                .and_then(|by_digest| by_digest.get_mut(&digest))
                .filter(|signatures| signatures.contains_key(&leader))
                .ok_or_else(|| eyre::eyre!("settlement response has no active leader proposal"))?;
            eyre::ensure!(
                !signatures.contains_key(&follower),
                "settlement response signer is already stored"
            );
            signatures.insert(follower, signed);
            signatures.len()
        };

        self.settlement_changed.notify_one();
        Ok(signature_count)
    }

    /// Wait until any statement at `height` has at least `quorum` distinct signatures, or return
    /// `None` when the leader generation is cancelled.
    pub async fn wait_for_settlement(
        &self,
        height: u64,
        quorum: usize,
        shutdown: &tokio_util::sync::CancellationToken,
    ) -> Option<SettlementCertificate> {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => None,
            certificate = async {
                loop {
                    let notified = self.settlement_changed.notified();
                    if let Some(certificate) = self.settlement_at(height, quorum) {
                        break certificate;
                    }
                    notified.await;
                }
            } => Some(certificate),
        }
    }

    /// Get the settlement certificate at the zone block height
    fn settlement_at(&self, height: u64, quorum: usize) -> Option<SettlementCertificate> {
        let state = self.state.read().expect("attestation store lock poisoned");
        let (digest, signatures) = state
            .settlements
            .get(&height)?
            .iter()
            .find(|(_, signatures)| signatures.len() >= quorum)?;
        let attestation = signatures.values().next()?.attestation.clone();

        Some(SettlementCertificate {
            height,
            digest: *digest,
            attestation,
            // Signer-address ordering makes transaction calldata deterministic.
            signatures: signatures
                .values()
                .map(|signed| signed.signature.clone())
                .collect(),
        })
    }

    /// Remove one unusable certificate without discarding other anchor candidates.
    pub fn remove_settlement(&self, height: u64, digest: B256) {
        let mut state = self.state.write().expect("attestation store lock poisoned");
        if let Some(by_digest) = state.settlements.get_mut(&height) {
            by_digest.remove(&digest);
            if by_digest.is_empty() {
                state.settlements.remove(&height);
            }
        }
    }

    /// Publish the monitor-owned anchor for a height, invalidating signatures for any previous
    /// anchor at that height.
    pub fn replace_prepared_anchor(&self, height: u64, anchor: BatchAnchor) {
        let mut state = self.state.write().expect("attestation store lock poisoned");
        if state.prepared_anchors.get(&height) != Some(&anchor) {
            state.settlements.remove(&height);
            state.prepared_anchors.insert(height, anchor);
            self.settlement_changed.notify_one();
        }
    }

    /// Return the monitor-owned anchor for a Zone height.
    pub fn prepared_anchor(&self, height: u64) -> Option<BatchAnchor> {
        self.state
            .read()
            .expect("attestation store lock poisoned")
            .prepared_anchors
            .get(&height)
            .cloned()
    }

    /// Remove all attestations covered by a confirmed batch submission.
    pub fn remove_submitted(&self, height: u64) {
        {
            let mut state = self.state.write().expect("attestation store lock poisoned");
            state
                .settlements
                .retain(|settlement_height, _| *settlement_height > height);
            state
                .prepared_anchors
                .retain(|anchor_height, _| *anchor_height > height);
        }
        self.submitted_height.send_if_modified(|submitted| {
            if height > *submitted {
                *submitted = height;
                true
            } else {
                false
            }
        });
    }

    /// Subscribe to the latest zone height confirmed by a batch submission or portal resync.
    pub fn subscribe_submitted_height(&self) -> watch::Receiver<u64> {
        self.submitted_height.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, B256, U256, keccak256, uint};
    use alloy_signer_local::PrivateKeySigner;

    use super::*;

    fn domain() -> AttestationDomain {
        AttestationDomain {
            l1_chain_id: 1337,
            portal_address: Address::repeat_byte(0x11),
            zone_id: 7,
        }
    }

    #[test]
    fn settlement_type_and_signature_match_zone_portal() {
        const PORTAL_TYPE: &str = "SettlementAttestation(uint32 zoneId,uint64 sequencerSetVersion,uint256 zoneHeight,uint256 withdrawalBatchIndex,address verifier,uint64 tempoBlockNumber,uint64 anchorBlockNumber,bytes32 anchorBlockHash,bytes32 blockTransitionHash,bytes32 depositQueueTransitionHash,bytes32 tokenEnablementTransitionHash,bytes32 withdrawalQueueHash,bytes32 verifierConfigHash)";
        assert_eq!(SettlementAttestation::eip712_encode_type(), PORTAL_TYPE);

        let attestation = SettlementAttestation {
            zoneId: 7,
            sequencerSetVersion: 3,
            zoneHeight: U256::from(120),
            withdrawalBatchIndex: U256::from(1),
            verifier: Address::repeat_byte(2),
            tempoBlockNumber: 100,
            anchorBlockNumber: 100,
            anchorBlockHash: B256::repeat_byte(3),
            blockTransitionHash: B256::repeat_byte(4),
            depositQueueTransitionHash: B256::repeat_byte(5),
            tokenEnablementTransitionHash: B256::repeat_byte(8),
            withdrawalQueueHash: B256::repeat_byte(6),
            verifierConfigHash: B256::repeat_byte(7),
        };
        let struct_hash = keccak256(
            (
                keccak256(PORTAL_TYPE),
                attestation.zoneId,
                attestation.sequencerSetVersion,
                attestation.zoneHeight,
                attestation.withdrawalBatchIndex,
                attestation.verifier,
                attestation.tempoBlockNumber,
                attestation.anchorBlockNumber,
                attestation.anchorBlockHash,
                attestation.blockTransitionHash,
                attestation.depositQueueTransitionHash,
                attestation.tokenEnablementTransitionHash,
                attestation.withdrawalQueueHash,
                attestation.verifierConfigHash,
            )
                .abi_encode(),
        );
        let domain = domain();
        let domain_separator = keccak256(
            (
                keccak256(
                    "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
                ),
                keccak256("ZonePortal"),
                keccak256("1"),
                U256::from(domain.l1_chain_id),
                domain.portal_address,
            )
                .abi_encode(),
        );
        let mut encoded_digest = Vec::with_capacity(66);
        encoded_digest.extend_from_slice(&[0x19, 0x01]);
        encoded_digest.extend_from_slice(domain_separator.as_slice());
        encoded_digest.extend_from_slice(struct_hash.as_slice());
        assert_eq!(
            domain.settlement_digest(&attestation),
            keccak256(encoded_digest)
        );

        let signer = PrivateKeySigner::random();
        let signed = SignedSettlementAttestation::sign(attestation, domain, &signer).unwrap();
        let decoded = SignedSettlementAttestation::decode(&signed.encode()).unwrap();
        assert_eq!(decoded, signed);
        assert_eq!(decoded.recover_signer(domain).unwrap(), signer.address());

        let store = AttestationStore::default();
        assert_eq!(
            store.insert_settlement(domain, signer.address(), signed),
            (true, 1)
        );
    }

    #[test]
    fn legacy_settlement_uses_pre_t12_wire_format_and_digest() {
        const LEGACY_PORTAL_TYPE: &str = "SettlementAttestation(uint32 zoneId,uint64 sequencerSetVersion,uint256 zoneHeight,uint256 withdrawalBatchIndex,address verifier,uint64 tempoBlockNumber,uint64 anchorBlockNumber,bytes32 anchorBlockHash,bytes32 blockTransitionHash,bytes32 depositQueueTransitionHash,bytes32 withdrawalQueueHash,bytes32 verifierConfigHash)";
        assert_eq!(
            LegacySettlementAttestation::eip712_encode_type(),
            LEGACY_PORTAL_TYPE
        );

        let attestation = SettlementAttestation {
            zoneId: 7,
            sequencerSetVersion: 3,
            zoneHeight: U256::from(120),
            withdrawalBatchIndex: U256::from(1),
            verifier: Address::repeat_byte(2),
            tempoBlockNumber: 100,
            anchorBlockNumber: 100,
            anchorBlockHash: B256::repeat_byte(3),
            blockTransitionHash: B256::repeat_byte(4),
            depositQueueTransitionHash: B256::repeat_byte(5),
            tokenEnablementTransitionHash: B256::ZERO,
            withdrawalQueueHash: B256::repeat_byte(6),
            verifierConfigHash: B256::repeat_byte(7),
        };
        let legacy = attestation.as_legacy();
        assert_eq!(attestation.encode(), legacy.abi_encode());
        assert_eq!(
            SettlementAttestation::decode(&legacy.abi_encode()).unwrap(),
            attestation
        );
        assert_eq!(
            domain().settlement_digest(&attestation),
            legacy.eip712_signing_hash(&domain().eip712())
        );

        let signer = PrivateKeySigner::random();
        let signed = SignedSettlementAttestation::sign(attestation, domain(), &signer).unwrap();
        let legacy_signed = LegacySignedSettlementAttestation {
            attestation: legacy,
            signature: signed.signature.clone(),
        };
        assert_eq!(signed.encode(), legacy_signed.abi_encode());
        assert_eq!(
            SignedSettlementAttestation::decode(&legacy_signed.abi_encode()).unwrap(),
            signed
        );
        assert_eq!(signed.recover_signer(domain()).unwrap(), signer.address());
    }

    #[test]
    fn rejects_high_s_settlement_signature() {
        const SECP256K1_ORDER: U256 =
            uint!(0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFEBAAEDCE6AF48A03BBFD25E8CD0364141_U256);

        let signer = PrivateKeySigner::random();
        let domain = domain();
        let attestation = SettlementAttestation {
            zoneId: 7,
            sequencerSetVersion: 3,
            zoneHeight: U256::from(10),
            withdrawalBatchIndex: U256::from(1),
            verifier: Address::repeat_byte(2),
            tempoBlockNumber: 100,
            anchorBlockNumber: 100,
            anchorBlockHash: B256::repeat_byte(3),
            blockTransitionHash: B256::repeat_byte(4),
            depositQueueTransitionHash: B256::repeat_byte(5),
            tokenEnablementTransitionHash: B256::repeat_byte(8),
            withdrawalQueueHash: B256::repeat_byte(6),
            verifierConfigHash: B256::repeat_byte(7),
        };
        let mut signed = SignedSettlementAttestation::sign(attestation, domain, &signer).unwrap();
        let signature = Signature::try_from(signed.signature.as_ref()).unwrap();
        let high_s_signature = Signature::new(
            signature.r(),
            SECP256K1_ORDER - signature.s(),
            !signature.v(),
        );
        signed.signature = Bytes::copy_from_slice(&high_s_signature.as_bytes());

        assert!(signed.recover_signer(domain).is_err());
    }

    #[tokio::test]
    async fn waits_for_quorum_and_removes_confirmed_attestations() {
        let store = AttestationStore::default();
        let signer_a = PrivateKeySigner::random();
        let signer_b = PrivateKeySigner::random();
        let attestation = SettlementAttestation {
            zoneId: 7,
            sequencerSetVersion: 3,
            zoneHeight: U256::from(10),
            withdrawalBatchIndex: U256::from(1),
            verifier: Address::repeat_byte(2),
            tempoBlockNumber: 100,
            anchorBlockNumber: 100,
            anchorBlockHash: B256::repeat_byte(3),
            blockTransitionHash: B256::repeat_byte(4),
            depositQueueTransitionHash: B256::repeat_byte(5),
            tokenEnablementTransitionHash: B256::repeat_byte(8),
            withdrawalQueueHash: B256::repeat_byte(6),
            verifierConfigHash: B256::repeat_byte(7),
        };
        store.insert_settlement(
            domain(),
            signer_a.address(),
            SignedSettlementAttestation::sign(attestation.clone(), domain(), &signer_a).unwrap(),
        );

        let waiting = {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .wait_for_settlement(10, 2, &tokio_util::sync::CancellationToken::new())
                    .await
            })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());

        store.insert_settlement(
            domain(),
            signer_b.address(),
            SignedSettlementAttestation::sign(attestation, domain(), &signer_b).unwrap(),
        );
        let certificate = waiting.await.unwrap().unwrap();
        assert_eq!(certificate.signatures.len(), 2);

        store.remove_submitted(10);
        assert!(store.settlement_at(10, 1).is_none());
    }

    #[test]
    fn replacing_prepared_anchor_discards_the_old_certificate() {
        let store = AttestationStore::default();
        let signer = PrivateKeySigner::random();
        let attestation = SettlementAttestation {
            zoneId: 7,
            sequencerSetVersion: 3,
            zoneHeight: U256::from(10),
            withdrawalBatchIndex: U256::from(1),
            verifier: Address::repeat_byte(2),
            tempoBlockNumber: 100,
            anchorBlockNumber: 100,
            anchorBlockHash: B256::repeat_byte(3),
            blockTransitionHash: B256::repeat_byte(4),
            depositQueueTransitionHash: B256::repeat_byte(5),
            tokenEnablementTransitionHash: B256::ZERO,
            withdrawalQueueHash: B256::repeat_byte(6),
            verifierConfigHash: B256::repeat_byte(7),
        };
        let first = crate::BatchAnchor::Direct {
            block_hash: B256::repeat_byte(3),
        };
        store.replace_prepared_anchor(10, first);
        store.insert_settlement(
            domain(),
            signer.address(),
            SignedSettlementAttestation::sign(attestation, domain(), &signer).unwrap(),
        );
        assert!(store.settlement_at(10, 1).is_some());

        let replacement = crate::BatchAnchor::Ancestry {
            block_number: 108,
            block_hash: B256::repeat_byte(10),
            ancestry_headers: vec![Bytes::from_static(&[1])],
        };
        store.replace_prepared_anchor(10, replacement.clone());

        assert!(store.settlement_at(10, 1).is_none());
        assert_eq!(store.prepared_anchor(10), Some(replacement));
    }
}
