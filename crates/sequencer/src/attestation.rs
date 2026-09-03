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

type SettlementSignatures =
    BTreeMap<u64, BTreeMap<B256, BTreeMap<Address, SignedSettlementAttestation>>>;

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
        attestation.eip712_signing_hash(&self.eip712())
    }
}

impl SettlementAttestation {
    pub fn encode(&self) -> Vec<u8> {
        self.abi_encode()
    }

    pub fn decode(encoded: &[u8]) -> eyre::Result<Self> {
        Self::abi_decode(encoded).wrap_err("invalid settlement proposal encoding")
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
        self.abi_encode()
    }

    pub fn decode(encoded: &[u8]) -> eyre::Result<Self> {
        Self::abi_decode(encoded).wrap_err("invalid settlement signature encoding")
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
    settlements: Arc<RwLock<SettlementSignatures>>,
    settlement_changed: Arc<Notify>,
    submitted_height: watch::Sender<u64>,
}

impl Default for AttestationStore {
    fn default() -> Self {
        let (submitted_height, _) = watch::channel(0);
        Self {
            settlements: Arc::default(),
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
            let mut all = self
                .settlements
                .write()
                .expect("attestation store lock poisoned");

            let signatures = all.entry(height).or_default().entry(digest).or_default();
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
        let all = self
            .settlements
            .read()
            .expect("attestation store lock poisoned");
        let signatures = all
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

    /// The statement `signer` attested to at `(height, digest)`, while it remains stored.
    ///
    /// Lets the leader check an incoming follower signature against the proposal it signed itself,
    /// instead of rebuilding that proposal from the zone chain and L1.
    pub fn stored_attestation(
        &self,
        height: u64,
        digest: B256,
        signer: Address,
    ) -> Option<SettlementAttestation> {
        let all = self
            .settlements
            .read()
            .expect("attestation store lock poisoned");
        all.get(&height)?
            .get(&digest)?
            .get(&signer)
            .map(|signed| signed.attestation.clone())
    }

    /// Most signatures collected for any single statement at `height`.
    pub fn signature_count(&self, height: u64) -> usize {
        self.settlements
            .read()
            .expect("attestation store lock poisoned")
            .get(&height)
            .and_then(|by_digest| by_digest.values().map(BTreeMap::len).max())
            .unwrap_or(0)
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
            let mut all = self
                .settlements
                .write()
                .expect("attestation store lock poisoned");
            let signatures = all
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
        let all = self
            .settlements
            .read()
            .expect("attestation store lock poisoned");
        let (digest, signatures) = all
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
        let mut settlements = self
            .settlements
            .write()
            .expect("attestation store lock poisoned");
        if let Some(by_digest) = settlements.get_mut(&height) {
            by_digest.remove(&digest);
            if by_digest.is_empty() {
                settlements.remove(&height);
            }
        }
    }

    /// Remove all attestations covered by a confirmed batch submission.
    pub fn remove_submitted(&self, height: u64) {
        self.settlements
            .write()
            .expect("attestation store lock poisoned")
            .retain(|settlement_height, _| *settlement_height > height);
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
        const PORTAL_TYPE: &str = "SettlementAttestation(uint32 zoneId,uint64 sequencerSetVersion,uint256 zoneHeight,uint256 withdrawalBatchIndex,address verifier,uint64 tempoBlockNumber,uint64 anchorBlockNumber,bytes32 anchorBlockHash,bytes32 blockTransitionHash,bytes32 depositQueueTransitionHash,bytes32 withdrawalQueueHash,bytes32 verifierConfigHash)";
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
        assert_eq!(store.signature_count(10), 2);
        assert_eq!(store.signature_count(11), 0);

        store.remove_submitted(10);
        assert!(store.settlement_at(10, 1).is_none());
        assert_eq!(store.signature_count(10), 0);
    }
}
