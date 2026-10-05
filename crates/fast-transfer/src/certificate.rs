//! Pinned-epoch quorum certificate verification.

use std::collections::HashSet;

use alloy_consensus::crypto::secp256k1::recover_signer;
use alloy_primitives::{Address, B256, Signature, U256, keccak256};
use zone_primitives::fast_transfer::{
    FAST_EMPTY_UNRESOLVED_ROOT, FastBarrierResolution, FastBarrierStatement,
    FastCheckpointStatement, OutcomeCertificate, QuoteCertificate, SettlementStatement,
    SignatureBytes, TransferIntent, TransferOutcome, TransportSessionProof, ZoneDomain,
};

const EIP712_DOMAIN_TYPE: &[u8] =
    b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)";
const OUTCOME_NAME: &[u8] = b"Tempo Zone Fast Transfer Outcome";
const OUTCOME_TYPE: &[u8] =
    b"FastTransferOutcome(bytes32 bodyHash,uint64 authorityEpoch,bytes32 rosterHash)";
const QUOTE_TYPE: &[u8] =
    b"FastTransferQuote(bytes32 quoteHash,uint64 authorityEpoch,bytes32 rosterHash)";
const SETTLEMENT_NAME: &[u8] = b"Tempo Zone Fast Transfer Settlement";
const SETTLEMENT_TYPE: &[u8] =
    b"FastTransferSettlement(bytes32 statementHash,uint64 authorityEpoch,bytes32 rosterHash)";
const VERSION: &[u8] = b"1";
const ROSTER_TAG: &[u8] = b"tempo.zone.fast-transfer.roster.v1";

/// Signature-purpose domain. Outcome and settlement signatures are not interchangeable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SigningDomain {
    /// Committed lock/payment/rejection/disposition outcomes.
    Outcome,
    /// Committed-chain settlement extensions.
    Settlement,
}

impl SigningDomain {
    fn name(self) -> &'static [u8] {
        match self {
            Self::Outcome => OUTCOME_NAME,
            Self::Settlement => SETTLEMENT_NAME,
        }
    }
}

/// The three keys finalized for one exact Zone authority epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EpochRoster {
    /// Exact Zone/epoch/protocol domain.
    pub domain: ZoneDomain,
    /// Registry-order signer addresses.
    pub members: [Address; 3],
}

impl EpochRoster {
    /// Construct and validate an exact pinned roster.
    pub fn new(domain: ZoneDomain, members: [Address; 3]) -> Result<Self, CertificateError> {
        let roster = Self::from_finalized_registry(domain, members)?;
        let actual = Self::calculate_hash(domain, members);
        if domain.roster_hash != actual {
            return Err(CertificateError::RosterHash {
                expected: domain.roster_hash,
                actual,
            });
        }
        Ok(roster)
    }

    /// Construct from a finalized registry record whose roster commitment may additionally bind
    /// peer transport endpoints. The caller must authenticate `domain.roster_hash` and `members`
    /// from the same finalized Portal state.
    pub fn from_finalized_registry(
        domain: ZoneDomain,
        members: [Address; 3],
    ) -> Result<Self, CertificateError> {
        if members.iter().any(|member| member.is_zero()) {
            return Err(CertificateError::InvalidRoster("zero signer"));
        }
        if members[0] == members[1] || members[0] == members[2] || members[1] == members[2] {
            return Err(CertificateError::InvalidRoster("duplicate signer"));
        }
        if domain.roster_hash.is_zero() {
            return Err(CertificateError::InvalidRoster("zero roster commitment"));
        }
        Ok(Self { domain, members })
    }

    /// Hash the exact registry-order roster and its immutable epoch coordinates.
    pub fn calculate_hash(domain: ZoneDomain, members: [Address; 3]) -> B256 {
        let mut encoded = Vec::with_capacity(ROSTER_TAG.len() + 8 + 4 + 8 + 20 + 2 + 60);
        encoded.extend_from_slice(&(ROSTER_TAG.len() as u32).to_be_bytes());
        encoded.extend_from_slice(ROSTER_TAG);
        encoded.extend_from_slice(&domain.l1_chain_id.to_be_bytes());
        encoded.extend_from_slice(&domain.zone_id.to_be_bytes());
        encoded.extend_from_slice(&domain.authority_epoch.to_be_bytes());
        encoded.extend_from_slice(domain.portal.as_slice());
        encoded.extend_from_slice(&domain.protocol_version.to_be_bytes());
        for member in members {
            encoded.extend_from_slice(member.as_slice());
        }
        keccak256(encoded)
    }

    fn contains(&self, signer: Address) -> bool {
        self.members.contains(&signer)
    }
}

/// Certificate validation failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CertificateError {
    /// Roster membership itself is unsafe.
    #[error("invalid epoch roster: {0}")]
    InvalidRoster(&'static str),
    /// Registry roster hash did not match the supplied keys.
    #[error("roster hash mismatch: expected {expected}, calculated {actual}")]
    RosterHash { expected: B256, actual: B256 },
    /// Certificate is not for the pinned Zone epoch.
    #[error("certificate authority domain is not the pinned epoch")]
    WrongEpoch,
    /// Transfer identity/body mismatch.
    #[error("certificate does not match the transfer intent")]
    IntentMismatch,
    /// Outcome fields do not bind the expected token operation.
    #[error("certificate outcome fields do not match the intent")]
    OutcomeMismatch,
    /// Signature bytes were malformed or non-canonical.
    #[error("invalid signature")]
    InvalidSignature,
    /// Signature recovered a key outside the pinned roster.
    #[error("unauthorized signer {0}")]
    UnauthorizedSigner(Address),
    /// Both signatures recovered the same member.
    #[error("quorum signatures are not from distinct members")]
    DuplicateSigner,
    /// Arithmetic overflow while checking escrow amount.
    #[error("principal plus fee overflows")]
    AmountOverflow,
    /// Transport proof role/domain/member did not bind this roster.
    #[error("transport session proof does not bind the pinned roster")]
    TransportIdentityMismatch,
}

/// Verifies two-member certificates against one pinned epoch.
#[derive(Clone, Debug)]
pub struct QuorumVerifier {
    roster: EpochRoster,
}

impl QuorumVerifier {
    /// Pin a verifier to an already validated epoch roster.
    pub const fn new(roster: EpochRoster) -> Self {
        Self { roster }
    }

    /// Return the pinned roster.
    pub const fn roster(&self) -> &EpochRoster {
        &self.roster
    }

    /// Calculate the EIP-712 outcome digest replicas must sign.
    pub fn outcome_digest(&self, certificate: &OutcomeCertificate) -> B256 {
        signing_digest(
            SigningDomain::Outcome,
            keccak256(OUTCOME_TYPE),
            self.roster.domain,
            certificate.body.body_hash(),
        )
    }

    /// Calculate the distinct EIP-712 settlement digest replicas must sign.
    pub fn settlement_digest(&self, statement: &SettlementStatement) -> B256 {
        signing_digest(
            SigningDomain::Settlement,
            keccak256(SETTLEMENT_TYPE),
            self.roster.domain,
            statement.statement_hash(),
        )
    }

    /// Calculate the quote digest under the fast-outcome domain and quote struct type.
    pub fn quote_digest(&self, certificate: &QuoteCertificate) -> B256 {
        signing_digest(
            SigningDomain::Outcome,
            keccak256(QUOTE_TYPE),
            self.roster.domain,
            certificate.quote.quote_hash(),
        )
    }

    /// Verify a bounded standing quote against its exact destination authority epoch.
    pub fn verify_quote(
        &self,
        certificate: &QuoteCertificate,
    ) -> Result<[Address; 2], CertificateError> {
        let quote = &certificate.quote;
        if quote.destination != self.roster.domain {
            return Err(CertificateError::WrongEpoch);
        }
        if quote.source.l1_chain_id != quote.destination.l1_chain_id
            || quote.source.protocol_version != quote.destination.protocol_version
            || quote.source.zone_id == quote.destination.zone_id
            || quote.maximum_principal.is_zero()
        {
            return Err(CertificateError::IntentMismatch);
        }
        self.verify_signatures(self.quote_digest(certificate), &certificate.signatures)
    }

    /// Validate the epoch, intent binding, outcome fields, and distinct quorum.
    pub fn verify_outcome(
        &self,
        certificate: &OutcomeCertificate,
        intent: &TransferIntent,
    ) -> Result<[Address; 2], CertificateError> {
        if certificate.body.zone != self.roster.domain {
            return Err(CertificateError::WrongEpoch);
        }
        if certificate.body.transfer_id != intent.transfer_id()
            || certificate.body.intent_hash != intent.intent_hash()
        {
            return Err(CertificateError::IntentMismatch);
        }
        validate_outcome(&certificate.body.outcome, intent, self.roster.domain)?;
        self.verify_signatures(self.outcome_digest(certificate), &certificate.signatures)
    }

    /// Validate a settlement extension under the settlement-only domain.
    pub fn verify_settlement(
        &self,
        statement: &SettlementStatement,
        signatures: &[SignatureBytes; 2],
    ) -> Result<[Address; 2], CertificateError> {
        if statement.zone != self.roster.domain {
            return Err(CertificateError::WrongEpoch);
        }
        self.verify_signatures(self.settlement_digest(statement), signatures)
    }

    /// Verify a source barrier under the exact Tempo factory purpose and historical source roster.
    pub fn verify_source_barrier(
        &self,
        statement: &FastBarrierStatement,
        signatures: &[SignatureBytes; 2],
    ) -> Result<[Address; 2], CertificateError> {
        if statement.source_portal != self.roster.domain.portal
            || statement.source_epoch != self.roster.domain.authority_epoch
            || statement.destination_portal == statement.source_portal
            || statement.destination_epoch == 0
            || statement.closure_hash.is_zero()
        {
            return Err(CertificateError::WrongEpoch);
        }
        self.verify_signatures(
            statement.registry_digest(self.roster.domain.l1_chain_id),
            signatures,
        )
    }

    /// Verify the terminal/disposition roots for one exact authenticated source barrier.
    pub fn verify_barrier_resolution(
        &self,
        barrier: &FastBarrierStatement,
        resolution: &FastBarrierResolution,
        signatures: &[SignatureBytes; 2],
    ) -> Result<[Address; 2], CertificateError> {
        let barrier_hash = barrier.registry_digest(self.roster.domain.l1_chain_id);
        if barrier.source_portal != self.roster.domain.portal
            || barrier.source_epoch != self.roster.domain.authority_epoch
            || resolution.barrier_hash != barrier_hash
            || resolution.terminal_root.is_zero()
            || resolution.disposition_root.is_zero()
            || resolution.resolved_count != barrier.unresolved_count
            || resolution.remaining_unresolved_root != FAST_EMPTY_UNRESOLVED_ROOT
            || resolution.remaining_unresolved_count != 0
        {
            return Err(CertificateError::OutcomeMismatch);
        }
        self.verify_signatures(
            resolution.registry_digest(
                self.roster.domain.l1_chain_id,
                barrier.destination_portal,
                barrier.destination_epoch,
                barrier.source_portal,
            ),
            signatures,
        )
    }

    /// Verify the exact old accepted prefix/checkpoint acknowledgment by two distinct members of
    /// the proposed next roster. No current-roster signature can substitute for this handoff.
    pub fn verify_next_roster_checkpoint(
        l1_chain_id: u64,
        statement: &FastCheckpointStatement,
        next_members: [Address; 3],
        signatures: &[SignatureBytes; 2],
    ) -> Result<[Address; 2], CertificateError> {
        let roster = EpochRoster::from_finalized_registry(
            ZoneDomain {
                l1_chain_id,
                zone_id: 0,
                chain_id: 0,
                portal: statement.portal,
                authority_epoch: statement.next_epoch,
                roster_hash: statement.next_roster_hash,
                protocol_version: 0,
            },
            next_members,
        )?;
        Self::new(roster).verify_signatures(statement.registry_digest(l1_chain_id), signatures)
    }

    /// Verify one side of a replay-resistant transport handshake with the same member identity
    /// authorized to certify outcomes. Transport transcript hashes remain distinct from EIP-712
    /// outcome and settlement digests.
    pub fn verify_transport_session(
        &self,
        proof: &TransportSessionProof,
    ) -> Result<Address, CertificateError> {
        let (domain, claimed) = if proof.responder_role {
            (proof.responder, proof.responder_member)
        } else {
            (proof.initiator, proof.initiator_member)
        };
        if domain != self.roster.domain || !self.roster.contains(claimed) {
            return Err(CertificateError::TransportIdentityMismatch);
        }
        let signature = Signature::try_from(proof.signature.0.as_slice())
            .map_err(|_| CertificateError::InvalidSignature)?;
        let recovered = recover_signer(&signature, proof.session_hash())
            .map_err(|_| CertificateError::InvalidSignature)?;
        if recovered != claimed {
            return Err(CertificateError::TransportIdentityMismatch);
        }
        Ok(recovered)
    }

    fn verify_signatures(
        &self,
        digest: B256,
        signatures: &[SignatureBytes; 2],
    ) -> Result<[Address; 2], CertificateError> {
        let mut recovered = [Address::ZERO; 2];
        let mut distinct = HashSet::with_capacity(2);
        for (index, signature) in signatures.iter().enumerate() {
            let signature = Signature::try_from(signature.0.as_slice())
                .map_err(|_| CertificateError::InvalidSignature)?;
            let signer = recover_signer(&signature, digest)
                .map_err(|_| CertificateError::InvalidSignature)?;
            if !self.roster.contains(signer) {
                return Err(CertificateError::UnauthorizedSigner(signer));
            }
            if !distinct.insert(signer) {
                return Err(CertificateError::DuplicateSigner);
            }
            recovered[index] = signer;
        }
        Ok(recovered)
    }
}

fn validate_outcome(
    outcome: &TransferOutcome,
    intent: &TransferIntent,
    signed_zone: ZoneDomain,
) -> Result<(), CertificateError> {
    let escrow_amount = intent
        .principal
        .checked_add(intent.fee)
        .ok_or(CertificateError::AmountOverflow)?;
    let valid = match outcome {
        TransferOutcome::Locked { amount, .. } => {
            signed_zone == intent.source && *amount == escrow_amount
        }
        TransferOutcome::Paid {
            pool,
            recipient,
            principal,
        } => {
            signed_zone == intent.destination
                && *pool == intent.destination_pool
                && *recipient == intent.recipient
                && *principal == intent.principal
        }
        TransferOutcome::Rejected { .. } => signed_zone == intent.destination,
        TransferOutcome::Released {
            beneficiary,
            amount,
        } => {
            signed_zone == intent.source
                && *beneficiary == intent.reimbursement_account
                && *amount == escrow_amount
        }
        TransferOutcome::Refunded {
            beneficiary,
            amount,
        } => {
            signed_zone == intent.source
                && *beneficiary == intent.refund_account
                && *amount == escrow_amount
        }
    };
    if valid {
        Ok(())
    } else {
        Err(CertificateError::OutcomeMismatch)
    }
}

fn signing_digest(
    purpose: SigningDomain,
    type_hash: B256,
    zone: ZoneDomain,
    value_hash: B256,
) -> B256 {
    let domain_separator = eip712_domain_separator(purpose, zone);
    let mut epoch_word = [0u8; 32];
    epoch_word[24..].copy_from_slice(&zone.authority_epoch.to_be_bytes());
    let mut structure = Vec::with_capacity(128);
    structure.extend_from_slice(type_hash.as_slice());
    structure.extend_from_slice(value_hash.as_slice());
    structure.extend_from_slice(&epoch_word);
    structure.extend_from_slice(zone.roster_hash.as_slice());
    let struct_hash = keccak256(structure);
    let mut preimage = Vec::with_capacity(66);
    preimage.extend_from_slice(&[0x19, 0x01]);
    preimage.extend_from_slice(domain_separator.as_slice());
    preimage.extend_from_slice(struct_hash.as_slice());
    keccak256(preimage)
}

fn eip712_domain_separator(purpose: SigningDomain, zone: ZoneDomain) -> B256 {
    let mut chain_word = [0u8; 32];
    U256::from(zone.l1_chain_id)
        .to_be_bytes::<32>()
        .clone_into(&mut chain_word);
    let mut address_word = [0u8; 32];
    address_word[12..].copy_from_slice(zone.portal.as_slice());
    let mut encoded = Vec::with_capacity(160);
    encoded.extend_from_slice(keccak256(EIP712_DOMAIN_TYPE).as_slice());
    encoded.extend_from_slice(keccak256(purpose.name()).as_slice());
    encoded.extend_from_slice(keccak256(VERSION).as_slice());
    encoded.extend_from_slice(&chain_word);
    encoded.extend_from_slice(&address_word);
    keccak256(encoded)
}

#[cfg(test)]
mod tests {
    use alloy_signer::SignerSync as _;
    use alloy_signer_local::PrivateKeySigner;
    use zone_primitives::fast_transfer::{
        AssetId, CertificateBody, RejectionReason, SignatureBytes,
    };

    use super::*;

    fn fixture() -> (TransferIntent, QuorumVerifier, [PrivateKeySigner; 3]) {
        let signers = [
            PrivateKeySigner::random(),
            PrivateKeySigner::random(),
            PrivateKeySigner::random(),
        ];
        let members = signers.each_ref().map(|signer| signer.address());
        let mut destination = ZoneDomain {
            l1_chain_id: 1337,
            zone_id: 2,
            chain_id: 2002,
            portal: Address::repeat_byte(2),
            authority_epoch: 7,
            roster_hash: B256::ZERO,
            protocol_version: 1,
        };
        destination.roster_hash = EpochRoster::calculate_hash(destination, members);
        let source = ZoneDomain {
            zone_id: 1,
            chain_id: 2001,
            portal: Address::repeat_byte(1),
            roster_hash: B256::repeat_byte(1),
            ..destination
        };
        let intent = TransferIntent {
            source,
            destination,
            asset: AssetId {
                l1_token: Address::repeat_byte(3),
                source_token: Address::repeat_byte(4),
                destination_token: Address::repeat_byte(5),
                decimals: 6,
            },
            sender: Address::repeat_byte(6),
            recipient: Address::repeat_byte(7),
            refund_account: Address::repeat_byte(6),
            destination_pool: Address::repeat_byte(8),
            reimbursement_account: Address::repeat_byte(9),
            principal: U256::from(10),
            fee: U256::from(1),
            quote_id: B256::repeat_byte(10),
            destination_expiry_height: 100,
            transfer_nonce: 1,
        };
        let roster = EpochRoster::new(destination, members).unwrap();
        (intent, QuorumVerifier::new(roster), signers)
    }

    fn certificate(intent: &TransferIntent) -> OutcomeCertificate {
        OutcomeCertificate {
            body: CertificateBody {
                transfer_id: intent.transfer_id(),
                intent_hash: intent.intent_hash(),
                zone: intent.destination,
                log_term: 2,
                log_index: 4,
                block_height: 9,
                block_hash: B256::repeat_byte(11),
                state_root: B256::repeat_byte(12),
                transaction_hash: B256::repeat_byte(13),
                outcome: TransferOutcome::Paid {
                    pool: intent.destination_pool,
                    recipient: intent.recipient,
                    principal: intent.principal,
                },
            },
            signatures: [SignatureBytes([0; 65]), SignatureBytes([0; 65])],
        }
    }

    fn sign(
        certificate: &mut OutcomeCertificate,
        verifier: &QuorumVerifier,
        signers: [&PrivateKeySigner; 2],
    ) {
        let digest = verifier.outcome_digest(certificate);
        certificate.signatures = signers
            .map(|signer| SignatureBytes(signer.sign_hash_sync(&digest).unwrap().as_bytes()));
    }

    #[test]
    fn accepts_two_distinct_pinned_members() {
        let (intent, verifier, signers) = fixture();
        let mut certificate = certificate(&intent);
        sign(&mut certificate, &verifier, [&signers[0], &signers[1]]);
        assert_eq!(
            verifier.verify_outcome(&certificate, &intent).unwrap(),
            [signers[0].address(), signers[1].address()]
        );
    }

    #[test]
    fn rejects_duplicate_domain_and_field_mutation() {
        let (intent, verifier, signers) = fixture();
        let mut certificate = certificate(&intent);
        sign(&mut certificate, &verifier, [&signers[0], &signers[0]]);
        assert_eq!(
            verifier.verify_outcome(&certificate, &intent),
            Err(CertificateError::DuplicateSigner)
        );

        certificate.body.outcome = TransferOutcome::Rejected {
            reason: RejectionReason::PolicyDenied,
        };
        assert!(matches!(
            verifier.verify_outcome(&certificate, &intent),
            Err(CertificateError::InvalidSignature) | Err(CertificateError::UnauthorizedSigner(_))
        ));
    }

    #[test]
    fn settlement_signatures_cannot_authorize_outcomes() {
        let (intent, verifier, signers) = fixture();
        let mut certificate = certificate(&intent);
        let statement = SettlementStatement {
            zone: intent.destination,
            settled_height: 9,
            settled_block_hash: B256::repeat_byte(11),
            transition_hash: B256::repeat_byte(12),
            anchor_hash: B256::repeat_byte(13),
        };
        let digest = verifier.settlement_digest(&statement);
        certificate.signatures = [&signers[0], &signers[1]]
            .map(|signer| SignatureBytes(signer.sign_hash_sync(&digest).unwrap().as_bytes()));
        assert!(verifier.verify_outcome(&certificate, &intent).is_err());
    }
}
