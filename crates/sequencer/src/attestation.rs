//! EIP-712 replication ACKs and settlement attestations.

use alloy_contract::CallBuilder;
use alloy_primitives::{Address, B256, Bytes, Signature, b256, keccak256};
use alloy_provider::DynProvider;
use alloy_rpc_types_eth::BlockId;
use alloy_signer::SignerSync as _;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{Eip712Domain, SolStruct as _, SolValue as _, eip712_domain, sol};
use eyre::WrapErr as _;
use serde::{Deserialize, Serialize};
use tempo_alloy::TempoNetwork;

sol! {
    /// T13 settlement statement verified by ZonePortal outside fast enrollment.
    #[derive(Debug, PartialEq, Eq)]
    struct StandardSettlementAttestation {
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

    /// Exact T14 fast-settlement statement verified by ZonePortal.
    #[derive(Debug, PartialEq, Eq)]
    struct FastSettlementAttestation {
        uint32 zoneId;
        uint64 fastEpoch;
        bytes32 rosterHash;
        uint256 previousZoneHeight;
        bytes32 previousBlockHash;
        uint64 previousWithdrawalBatchIndex;
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

    #[derive(Debug, PartialEq, Eq)]
    struct SignedStandardSettlementAttestation {
        StandardSettlementAttestation attestation;
        bytes signature;
    }

    #[derive(Debug, PartialEq, Eq)]
    struct SignedFastSettlementAttestation {
        FastSettlementAttestation attestation;
        bytes signature;
    }
}

/// Settlement-relevant words from the historical `FastEpochConfig` return value.
///
/// The Solidity struct exceeds Alloy's supported tuple arity, so this reader decodes its fixed
/// ABI words directly and rejects truncated responses.
#[allow(non_snake_case)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoricalFastEpochConfig {
    pub threshold: u8,
    pub proofMode: u8,
    pub rosterHash: B256,
    pub expectedVerifierCodeHash: B256,
    pub expectedVerifierConfigHash: B256,
    pub finalSettlementHash: B256,
}

const FAST_EPOCH_CONFIG_WORDS: usize = 27;

fn decode_historical_fast_epoch_config(output: &[u8]) -> eyre::Result<HistoricalFastEpochConfig> {
    eyre::ensure!(
        output.len() >= FAST_EPOCH_CONFIG_WORDS * 32,
        "historical fast epoch config returned {} bytes, expected at least {}",
        output.len(),
        FAST_EPOCH_CONFIG_WORDS * 32
    );
    let word = |index: usize| B256::from_slice(&output[index * 32..(index + 1) * 32]);
    Ok(HistoricalFastEpochConfig {
        threshold: output[63],
        proofMode: output[95],
        rosterHash: word(9),
        expectedVerifierCodeHash: word(11),
        expectedVerifierConfigHash: word(12),
        finalSettlementHash: word(18),
    })
}

/// Read historical fast settlement policy at one hash-canonical imported Tempo block.
pub async fn read_historical_fast_epoch_config(
    provider: &DynProvider<TempoNetwork>,
    portal: Address,
    epoch: u64,
    block: BlockId,
) -> eyre::Result<HistoricalFastEpochConfig> {
    let mut calldata = vec![0u8; 36];
    calldata[..4].copy_from_slice(&keccak256("fastEpochConfig(uint64)")[..4]);
    calldata[28..].copy_from_slice(&epoch.to_be_bytes());
    let output = CallBuilder::new_raw(provider, calldata.into())
        .to(portal)
        .block(block)
        .call()
        .await
        .wrap_err("failed reading historical fast epoch config")?;
    decode_historical_fast_epoch_config(&output)
}

/// Immutable security mode enrolled for a historical fast epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SettlementProofMode {
    /// Threshold operators attest to execution; no cryptographic execution proof is claimed.
    OperatorAttested,
    /// Every settlement requires a non-empty proof accepted by the enrolled verifier.
    ProofRequired,
}

impl SettlementProofMode {
    pub const OPERATOR_ATTESTED: u8 = 1;
    pub const PROOF_REQUIRED: u8 = 2;

    pub fn from_enrollment(value: u8) -> eyre::Result<Self> {
        match value {
            Self::OPERATOR_ATTESTED => Ok(Self::OperatorAttested),
            Self::PROOF_REQUIRED => Ok(Self::ProofRequired),
            _ => eyre::bail!("fast epoch has invalid/unset proof mode {value}"),
        }
    }

    pub const fn verifier_mode(self) -> zone_prover::VerifierMode {
        match self {
            Self::OperatorAttested => zone_prover::VerifierMode::NoProof,
            Self::ProofRequired => zone_prover::VerifierMode::NitroV1,
        }
    }

    pub const fn status_label(self) -> &'static str {
        match self {
            Self::OperatorAttested => "operator-attested",
            Self::ProofRequired => "proof-required",
        }
    }
}

/// Historical verifier policy pinned to one fast epoch and imported Tempo anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FastSettlementProofPolicy {
    pub mode: SettlementProofMode,
    pub expected_verifier_code_hash: B256,
    pub expected_verifier_config_hash: B256,
}

impl FastSettlementProofPolicy {
    pub fn from_config(config: &HistoricalFastEpochConfig) -> eyre::Result<Self> {
        let policy = Self {
            mode: SettlementProofMode::from_enrollment(config.proofMode)?,
            expected_verifier_code_hash: config.expectedVerifierCodeHash,
            expected_verifier_config_hash: config.expectedVerifierConfigHash,
        };
        eyre::ensure!(
            !policy.expected_verifier_code_hash.is_zero(),
            "fast epoch has zero expected verifier code hash"
        );
        eyre::ensure!(
            !policy.expected_verifier_config_hash.is_zero(),
            "fast epoch has zero expected verifier config hash"
        );
        if policy.mode == SettlementProofMode::ProofRequired {
            let prototype = keccak256(&tempo_contracts::zones::T13_ZONE_VERIFIER_RUNTIME);
            let development_prototype =
                b256!("c6bc17dc6724fb475ce3c59ec94e01bd733c2996b41bc82281b4d638b928cc33");
            eyre::ensure!(
                policy.expected_verifier_code_hash != prototype
                    && policy.expected_verifier_code_hash != development_prototype,
                "proof-required fast epoch enrolled an always-true prototype verifier"
            );
        }
        Ok(policy)
    }

    pub fn validate_mode(self, actual: zone_prover::VerifierMode) -> eyre::Result<()> {
        let expected = self.mode.verifier_mode();
        eyre::ensure!(
            actual == expected,
            "{} enrollment requires verifier mode {expected:?}, got {actual:?}",
            self.mode.status_label()
        );
        eyre::ensure!(
            actual.config_hash() == self.expected_verifier_config_hash,
            "local verifier config hash {} does not match enrolled hash {}",
            actual.config_hash(),
            self.expected_verifier_config_hash
        );
        Ok(())
    }
}

mod legacy {
    alloy_sol_types::sol! {
        /// Settlement statement used by the pre-T13 portal ABI.
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

        /// Signed settlement statement used by the pre-T13 portal ABI.
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

/// One settlement statement carried over P2P.
///
/// Fast-only fields are zero for legacy/T13 statements. `fastEpoch != 0` selects the exact
/// `FastSettlementAttestation` ABI and digest; no fast statement can be decoded as a legacy one.
#[allow(non_snake_case)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementAttestation {
    pub zoneId: u32,
    pub sequencerSetVersion: u64,
    pub fastEpoch: u64,
    pub rosterHash: B256,
    pub previousZoneHeight: alloy_primitives::U256,
    pub previousBlockHash: B256,
    pub previousWithdrawalBatchIndex: u64,
    pub zoneHeight: alloy_primitives::U256,
    pub withdrawalBatchIndex: alloy_primitives::U256,
    pub verifier: Address,
    pub tempoBlockNumber: u64,
    pub anchorBlockNumber: u64,
    pub anchorBlockHash: B256,
    pub blockTransitionHash: B256,
    pub depositQueueTransitionHash: B256,
    pub tokenEnablementTransitionHash: B256,
    pub withdrawalQueueHash: B256,
    pub verifierConfigHash: B256,
}

/// Settlement signature returned to the leader for quorum collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedSettlementAttestation {
    pub attestation: SettlementAttestation,
    pub signature: Bytes,
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
        if attestation.is_fast() {
            attestation.as_fast().eip712_signing_hash(&self.eip712())
        } else if attestation.is_legacy() {
            attestation.as_legacy().eip712_signing_hash(&self.eip712())
        } else {
            attestation
                .as_standard()
                .eip712_signing_hash(&self.eip712())
        }
    }
}

impl SettlementAttestation {
    pub fn is_fast(&self) -> bool {
        self.fastEpoch != 0
    }

    /// Legacy attestations use a zero sentinel for the field absent from their wire format.
    pub fn is_legacy(&self) -> bool {
        !self.is_fast() && self.tokenEnablementTransitionHash.is_zero()
    }

    fn as_standard(&self) -> StandardSettlementAttestation {
        StandardSettlementAttestation {
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
            tokenEnablementTransitionHash: self.tokenEnablementTransitionHash,
            withdrawalQueueHash: self.withdrawalQueueHash,
            verifierConfigHash: self.verifierConfigHash,
        }
    }

    fn as_fast(&self) -> FastSettlementAttestation {
        FastSettlementAttestation {
            zoneId: self.zoneId,
            fastEpoch: self.fastEpoch,
            rosterHash: self.rosterHash,
            previousZoneHeight: self.previousZoneHeight,
            previousBlockHash: self.previousBlockHash,
            previousWithdrawalBatchIndex: self.previousWithdrawalBatchIndex,
            zoneHeight: self.zoneHeight,
            withdrawalBatchIndex: self.withdrawalBatchIndex,
            verifier: self.verifier,
            tempoBlockNumber: self.tempoBlockNumber,
            anchorBlockNumber: self.anchorBlockNumber,
            anchorBlockHash: self.anchorBlockHash,
            blockTransitionHash: self.blockTransitionHash,
            depositQueueTransitionHash: self.depositQueueTransitionHash,
            tokenEnablementTransitionHash: self.tokenEnablementTransitionHash,
            withdrawalQueueHash: self.withdrawalQueueHash,
            verifierConfigHash: self.verifierConfigHash,
        }
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
            fastEpoch: 0,
            rosterHash: B256::ZERO,
            previousZoneHeight: alloy_primitives::U256::ZERO,
            previousBlockHash: B256::ZERO,
            previousWithdrawalBatchIndex: 0,
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

    fn from_standard(attestation: StandardSettlementAttestation) -> Self {
        Self {
            zoneId: attestation.zoneId,
            sequencerSetVersion: attestation.sequencerSetVersion,
            fastEpoch: 0,
            rosterHash: B256::ZERO,
            previousZoneHeight: alloy_primitives::U256::ZERO,
            previousBlockHash: B256::ZERO,
            previousWithdrawalBatchIndex: 0,
            zoneHeight: attestation.zoneHeight,
            withdrawalBatchIndex: attestation.withdrawalBatchIndex,
            verifier: attestation.verifier,
            tempoBlockNumber: attestation.tempoBlockNumber,
            anchorBlockNumber: attestation.anchorBlockNumber,
            anchorBlockHash: attestation.anchorBlockHash,
            blockTransitionHash: attestation.blockTransitionHash,
            depositQueueTransitionHash: attestation.depositQueueTransitionHash,
            tokenEnablementTransitionHash: attestation.tokenEnablementTransitionHash,
            withdrawalQueueHash: attestation.withdrawalQueueHash,
            verifierConfigHash: attestation.verifierConfigHash,
        }
    }

    fn from_fast(attestation: FastSettlementAttestation) -> Self {
        Self {
            zoneId: attestation.zoneId,
            sequencerSetVersion: 0,
            fastEpoch: attestation.fastEpoch,
            rosterHash: attestation.rosterHash,
            previousZoneHeight: attestation.previousZoneHeight,
            previousBlockHash: attestation.previousBlockHash,
            previousWithdrawalBatchIndex: attestation.previousWithdrawalBatchIndex,
            zoneHeight: attestation.zoneHeight,
            withdrawalBatchIndex: attestation.withdrawalBatchIndex,
            verifier: attestation.verifier,
            tempoBlockNumber: attestation.tempoBlockNumber,
            anchorBlockNumber: attestation.anchorBlockNumber,
            anchorBlockHash: attestation.anchorBlockHash,
            blockTransitionHash: attestation.blockTransitionHash,
            depositQueueTransitionHash: attestation.depositQueueTransitionHash,
            tokenEnablementTransitionHash: attestation.tokenEnablementTransitionHash,
            withdrawalQueueHash: attestation.withdrawalQueueHash,
            verifierConfigHash: attestation.verifierConfigHash,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        if self.is_fast() {
            self.as_fast().abi_encode()
        } else if self.is_legacy() {
            self.as_legacy().abi_encode()
        } else {
            self.as_standard().abi_encode()
        }
    }

    pub fn decode(encoded: &[u8]) -> eyre::Result<Self> {
        FastSettlementAttestation::abi_decode(encoded)
            .map(Self::from_fast)
            .or_else(|_| {
                StandardSettlementAttestation::abi_decode(encoded).map(Self::from_standard)
            })
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
            signature: signature.as_bytes().into(),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        if self.attestation.is_fast() {
            SignedFastSettlementAttestation {
                attestation: self.attestation.as_fast(),
                signature: self.signature.clone(),
            }
            .abi_encode()
        } else if self.attestation.is_legacy() {
            LegacySignedSettlementAttestation {
                attestation: self.attestation.as_legacy(),
                signature: self.signature.clone(),
            }
            .abi_encode()
        } else {
            SignedStandardSettlementAttestation {
                attestation: self.attestation.as_standard(),
                signature: self.signature.clone(),
            }
            .abi_encode()
        }
    }

    pub fn decode(encoded: &[u8]) -> eyre::Result<Self> {
        SignedFastSettlementAttestation::abi_decode(encoded)
            .map(|signed| Self {
                attestation: SettlementAttestation::from_fast(signed.attestation),
                signature: signed.signature,
            })
            .or_else(|_| {
                SignedStandardSettlementAttestation::abi_decode(encoded).map(|signed| Self {
                    attestation: SettlementAttestation::from_standard(signed.attestation),
                    signature: signed.signature,
                })
            })
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettlementCertificate {
    pub height: u64,
    pub digest: B256,
    pub attestation: SettlementAttestation,
    pub signatures: Vec<Bytes>,
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
        assert_eq!(
            StandardSettlementAttestation::eip712_encode_type(),
            PORTAL_TYPE
        );

        let attestation = SettlementAttestation {
            zoneId: 7,
            sequencerSetVersion: 3,
            fastEpoch: 0,
            rosterHash: B256::ZERO,
            previousZoneHeight: U256::ZERO,
            previousBlockHash: B256::ZERO,
            previousWithdrawalBatchIndex: 0,
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
    }

    #[test]
    fn legacy_settlement_uses_pre_t13_wire_format_and_digest() {
        const LEGACY_PORTAL_TYPE: &str = "SettlementAttestation(uint32 zoneId,uint64 sequencerSetVersion,uint256 zoneHeight,uint256 withdrawalBatchIndex,address verifier,uint64 tempoBlockNumber,uint64 anchorBlockNumber,bytes32 anchorBlockHash,bytes32 blockTransitionHash,bytes32 depositQueueTransitionHash,bytes32 withdrawalQueueHash,bytes32 verifierConfigHash)";
        assert_eq!(
            LegacySettlementAttestation::eip712_encode_type(),
            LEGACY_PORTAL_TYPE
        );

        let attestation = SettlementAttestation {
            zoneId: 7,
            sequencerSetVersion: 3,
            fastEpoch: 0,
            rosterHash: B256::ZERO,
            previousZoneHeight: U256::ZERO,
            previousBlockHash: B256::ZERO,
            previousWithdrawalBatchIndex: 0,
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
    fn fast_settlement_literal_portal_vector_and_mutations() {
        const PORTAL_TYPE: &str = "FastSettlementAttestation(uint32 zoneId,uint64 fastEpoch,bytes32 rosterHash,uint256 previousZoneHeight,bytes32 previousBlockHash,uint64 previousWithdrawalBatchIndex,uint256 zoneHeight,uint256 withdrawalBatchIndex,address verifier,uint64 tempoBlockNumber,uint64 anchorBlockNumber,bytes32 anchorBlockHash,bytes32 blockTransitionHash,bytes32 depositQueueTransitionHash,bytes32 tokenEnablementTransitionHash,bytes32 withdrawalQueueHash,bytes32 verifierConfigHash)";
        assert_eq!(FastSettlementAttestation::eip712_encode_type(), PORTAL_TYPE);

        let domain = domain();
        let attestation = SettlementAttestation {
            zoneId: 7,
            sequencerSetVersion: 0,
            fastEpoch: 14,
            rosterHash: B256::repeat_byte(0x12),
            previousZoneHeight: U256::from(119),
            previousBlockHash: B256::repeat_byte(0x13),
            previousWithdrawalBatchIndex: 8,
            zoneHeight: U256::from(120),
            withdrawalBatchIndex: U256::from(9),
            verifier: Address::repeat_byte(0x22),
            tempoBlockNumber: 100,
            anchorBlockNumber: 101,
            anchorBlockHash: B256::repeat_byte(0x23),
            blockTransitionHash: B256::repeat_byte(0x24),
            depositQueueTransitionHash: B256::repeat_byte(0x25),
            tokenEnablementTransitionHash: B256::repeat_byte(0x26),
            withdrawalQueueHash: B256::repeat_byte(0x27),
            verifierConfigHash: B256::repeat_byte(0x28),
        };
        let digest = domain.settlement_digest(&attestation);
        assert_eq!(
            format!("{digest:#x}"),
            "0xd9de63cdb9af962d3aadcd945b350632c9bbbb98cbe2a56c6bc92d6d26e67103"
        );

        let signer = PrivateKeySigner::from_bytes(&B256::with_last_byte(0x31)).unwrap();
        let signed =
            SignedSettlementAttestation::sign(attestation.clone(), domain, &signer).unwrap();
        assert_eq!(
            format!("{:#x}", signed.signature),
            "0xc4852270d3a258ceca27906d87aa09c11c475a30b00712b442503fbef237484b3fb5317d0d9f087822ed511569d05541c9db4f9a0a0d416039b3b2947daf21961c"
        );
        assert_eq!(
            SignedSettlementAttestation::decode(&signed.encode()).unwrap(),
            signed
        );

        let mut old_domain = attestation.clone();
        old_domain.sequencerSetVersion = 3;
        old_domain.fastEpoch = 0;
        old_domain.rosterHash = B256::ZERO;
        old_domain.previousZoneHeight = U256::ZERO;
        old_domain.previousBlockHash = B256::ZERO;
        old_domain.previousWithdrawalBatchIndex = 0;
        let replayed = SignedSettlementAttestation {
            attestation: old_domain,
            signature: signed.signature.clone(),
        };
        assert_ne!(replayed.recover_signer(domain).unwrap(), signer.address());

        let mut mutations = Vec::new();
        let mut changed = attestation.clone();
        changed.fastEpoch += 1;
        mutations.push(changed);
        let mut changed = attestation.clone();
        changed.rosterHash = B256::repeat_byte(0x32);
        mutations.push(changed);
        let mut changed = attestation.clone();
        changed.previousZoneHeight += U256::from(1);
        mutations.push(changed);
        let mut changed = attestation.clone();
        changed.previousBlockHash = B256::repeat_byte(0x33);
        mutations.push(changed);
        let mut changed = attestation.clone();
        changed.previousWithdrawalBatchIndex += 1;
        mutations.push(changed);
        for changed in mutations {
            assert_ne!(domain.settlement_digest(&changed), digest);
            assert_ne!(
                SignedSettlementAttestation::sign(changed, domain, &signer)
                    .unwrap()
                    .signature,
                signed.signature
            );
        }

        for changed in [
            SettlementAttestation {
                zoneId: 8,
                ..attestation.clone()
            },
            SettlementAttestation {
                zoneHeight: attestation.zoneHeight + U256::from(1),
                ..attestation.clone()
            },
            SettlementAttestation {
                withdrawalBatchIndex: attestation.withdrawalBatchIndex + U256::from(1),
                ..attestation.clone()
            },
            SettlementAttestation {
                verifier: Address::repeat_byte(0x39),
                ..attestation.clone()
            },
            SettlementAttestation {
                tempoBlockNumber: attestation.tempoBlockNumber + 1,
                ..attestation.clone()
            },
            SettlementAttestation {
                anchorBlockNumber: attestation.anchorBlockNumber + 1,
                ..attestation.clone()
            },
            SettlementAttestation {
                anchorBlockHash: B256::repeat_byte(0x3a),
                ..attestation.clone()
            },
            SettlementAttestation {
                blockTransitionHash: B256::repeat_byte(0x34),
                ..attestation.clone()
            },
            SettlementAttestation {
                depositQueueTransitionHash: B256::repeat_byte(0x35),
                ..attestation.clone()
            },
            SettlementAttestation {
                tokenEnablementTransitionHash: B256::repeat_byte(0x36),
                ..attestation.clone()
            },
            SettlementAttestation {
                withdrawalQueueHash: B256::repeat_byte(0x37),
                ..attestation.clone()
            },
            SettlementAttestation {
                verifierConfigHash: B256::repeat_byte(0x38),
                ..attestation
            },
        ] {
            assert_ne!(domain.settlement_digest(&changed), digest);
        }
    }

    #[test]
    fn historical_fast_epoch_decoder_pins_policy_and_final_fence_words() {
        let mut encoded = vec![0_u8; FAST_EPOCH_CONFIG_WORDS * 32];
        encoded[63] = 2;
        encoded[95] = SettlementProofMode::PROOF_REQUIRED;
        encoded[9 * 32..10 * 32].copy_from_slice(B256::repeat_byte(0x19).as_slice());
        encoded[11 * 32..12 * 32].copy_from_slice(B256::repeat_byte(0x11).as_slice());
        encoded[12 * 32..13 * 32].copy_from_slice(B256::repeat_byte(0x12).as_slice());
        encoded[18 * 32..19 * 32].copy_from_slice(B256::repeat_byte(0x18).as_slice());

        let decoded = decode_historical_fast_epoch_config(&encoded).unwrap();
        assert_eq!(decoded.threshold, 2);
        assert_eq!(decoded.proofMode, SettlementProofMode::PROOF_REQUIRED);
        assert_eq!(decoded.rosterHash, B256::repeat_byte(0x19));
        assert_eq!(decoded.expectedVerifierCodeHash, B256::repeat_byte(0x11));
        assert_eq!(decoded.expectedVerifierConfigHash, B256::repeat_byte(0x12));
        assert_eq!(decoded.finalSettlementHash, B256::repeat_byte(0x18));
        assert!(decode_historical_fast_epoch_config(&encoded[..encoded.len() - 1]).is_err());
    }

    #[test]
    fn historical_proof_mode_is_explicit_and_hash_pinned() {
        let mut config = HistoricalFastEpochConfig {
            threshold: 2,
            proofMode: SettlementProofMode::OPERATOR_ATTESTED,
            rosterHash: B256::repeat_byte(1),
            expectedVerifierCodeHash: B256::repeat_byte(2),
            expectedVerifierConfigHash: zone_prover::VerifierMode::NoProof.config_hash(),
            finalSettlementHash: B256::ZERO,
        };
        let policy = FastSettlementProofPolicy::from_config(&config).unwrap();
        assert_eq!(policy.mode, SettlementProofMode::OperatorAttested);
        policy
            .validate_mode(zone_prover::VerifierMode::NoProof)
            .unwrap();
        assert!(
            policy
                .validate_mode(zone_prover::VerifierMode::NitroV1)
                .is_err()
        );

        config.proofMode = SettlementProofMode::PROOF_REQUIRED;
        config.expectedVerifierConfigHash = zone_prover::VerifierMode::NitroV1.config_hash();
        let policy = FastSettlementProofPolicy::from_config(&config).unwrap();
        assert_eq!(policy.mode, SettlementProofMode::ProofRequired);
        policy
            .validate_mode(zone_prover::VerifierMode::NitroV1)
            .unwrap();
        assert!(
            policy
                .validate_mode(zone_prover::VerifierMode::NoProof)
                .is_err()
        );
        config.expectedVerifierCodeHash =
            keccak256(&tempo_contracts::zones::T13_ZONE_VERIFIER_RUNTIME);
        assert!(FastSettlementProofPolicy::from_config(&config).is_err());
        config.expectedVerifierCodeHash =
            b256!("c6bc17dc6724fb475ce3c59ec94e01bd733c2996b41bc82281b4d638b928cc33");
        assert!(FastSettlementProofPolicy::from_config(&config).is_err());
        config.expectedVerifierCodeHash = B256::repeat_byte(2);

        config.proofMode = 0;
        assert!(FastSettlementProofPolicy::from_config(&config).is_err());
        config.proofMode = SettlementProofMode::PROOF_REQUIRED;
        config.expectedVerifierConfigHash = B256::repeat_byte(0xff);
        assert!(
            FastSettlementProofPolicy::from_config(&config)
                .unwrap()
                .validate_mode(zone_prover::VerifierMode::NitroV1)
                .is_err()
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
            fastEpoch: 0,
            rosterHash: B256::ZERO,
            previousZoneHeight: U256::ZERO,
            previousBlockHash: B256::ZERO,
            previousWithdrawalBatchIndex: 0,
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
        signed.signature = high_s_signature.as_bytes().into();

        assert!(signed.recover_signer(domain).is_err());
    }
}
