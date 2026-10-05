//! Deterministic domain and wire types for the instant transfer protocol.
//!
//! The codec is deliberately small and consensus-friendly: integers are big-endian,
//! fields are emitted in declaration order, and every variable-length field has a
//! `u32` byte length. Decoders reject trailing bytes and enforce the protocol limits.

extern crate alloc;

use alloc::{boxed::Box, string::String, vec::Vec};
use alloy_primitives::{Address, B256, U256, b256, keccak256};

/// Maximum encoded user intent size.
pub const MAX_INTENT_BYTES: usize = 2 * 1024;
/// Maximum encoded quorum certificate size.
pub const MAX_CERTIFICATE_BYTES: usize = 4 * 1024;
/// Maximum canonical direct-service envelope size. Each nested value retains its independent
/// bound; this combined bound cannot be used to borrow space from another field.
pub const MAX_SERVICE_ENVELOPE_BYTES: usize =
    2 + 4 + MAX_INTENT_BYTES + 4 + MAX_CERTIFICATE_BYTES + 4 + MAX_CERTIFICATE_BYTES;
/// Maximum framed peer message (tag plus length plus maximum certificate payload).
pub const MAX_PEER_MESSAGE_BYTES: usize = 1 + 4 + MAX_CERTIFICATE_BYTES;
/// Maximum encoded receipt-inclusion plus authenticated header-ancestry evidence.
pub const MAX_RETIREMENT_PROOF_BYTES: usize = 512 * 1024;
/// Maximum receipt-trie nodes accepted in one retirement proof.
pub const MAX_RECEIPT_PROOF_NODES: usize = 1_024;
/// Maximum Zone headers accepted in one authenticated ancestry chunk.
pub const MAX_RETIREMENT_HEADERS: usize = 256;
/// Encoded secp256k1 signature length (`r || s || yParity`).
pub const SIGNATURE_BYTES: usize = 65;

const TRANSFER_ID_TAG: &[u8] = b"tempo.zone.fast-transfer.id.v1";
const INTENT_HASH_TAG: &[u8] = b"tempo.zone.fast-transfer.intent.v1";
const QUOTE_HASH_TAG: &[u8] = b"tempo.zone.fast-transfer.quote.v1";
const OUTCOME_HASH_TAG: &[u8] = b"tempo.zone.fast-transfer.outcome.v1";
const SETTLEMENT_HASH_TAG: &[u8] = b"tempo.zone.fast-transfer.settlement.v1";
const ZONE_DOMAIN_HASH_TAG: &[u8] = b"tempo.zone.fast-transfer.zone-domain.v1";
const CANCELLATION_HASH_TAG: &[u8] = b"tempo.zone.fast-transfer.cancellation.v1";
const TRANSPORT_SESSION_HASH_TAG: &[u8] = b"tempo.zone.fast-transfer.transport-session.v1";
const FAST_BARRIER_DOMAIN: &[u8] = b"TEMPO_ZONE_FAST_BARRIER_T14_V1";
const FAST_BARRIER_RESOLUTION_DOMAIN: &[u8] = b"TEMPO_ZONE_FAST_BARRIER_RESOLUTION_T14_V1";
const FAST_CHECKPOINT_DOMAIN: &[u8] = b"TEMPO_ZONE_FAST_CHECKPOINT_T14_V1";
/// Nonzero commitment used by the finalized registry for an empty unresolved-lock set.
pub const FAST_EMPTY_UNRESOLVED_ROOT: B256 =
    b256!("9926609188c0819360afc29d8a841336912689fd33ce43cd524280a13263456e");

/// A deterministic codec failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CodecError {
    /// Input ended before the declared field did.
    #[error("truncated fast-transfer encoding")]
    Truncated,
    /// A variable-length field exceeded its bound.
    #[error("{field} is {actual} bytes, maximum is {maximum}")]
    TooLarge {
        /// Field name.
        field: &'static str,
        /// Actual byte length.
        actual: usize,
        /// Accepted byte length.
        maximum: usize,
    },
    /// An enum discriminant is unknown.
    #[error("invalid {field} discriminant {value}")]
    InvalidTag {
        /// Field name.
        field: &'static str,
        /// Invalid value.
        value: u8,
    },
    /// Bytes remained after a complete value.
    #[error("trailing bytes in fast-transfer encoding")]
    TrailingBytes,
    /// A semantic wire invariant was violated.
    #[error("invalid fast-transfer value: {0}")]
    InvalidValue(&'static str),
}

/// Canonical encoding used for hashing and transport.
pub trait CanonicalEncode {
    /// Append this value's canonical bytes.
    fn encode_to(&self, out: &mut Vec<u8>);

    /// Return this value's canonical bytes.
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_to(&mut out);
        out
    }
}

/// Canonical decoding used at trust boundaries.
pub trait CanonicalDecode: Sized {
    /// Decode one value from a bounded reader.
    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, CodecError>;
}

/// Decode exactly one bounded value, rejecting trailing data.
pub fn decode_exact<T: CanonicalDecode>(bytes: &[u8], maximum: usize) -> Result<T, CodecError> {
    if bytes.len() > maximum {
        return Err(CodecError::TooLarge {
            field: "message",
            actual: bytes.len(),
            maximum,
        });
    }
    let mut reader = Reader { bytes, offset: 0 };
    let value = T::decode_from(&mut reader)?;
    if reader.offset != bytes.len() {
        return Err(CodecError::TrailingBytes);
    }
    Ok(value)
}

/// Reader for the canonical format.
pub struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], CodecError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(CodecError::Truncated)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(CodecError::Truncated)?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, CodecError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, CodecError> {
        Ok(u16::from_be_bytes(
            self.take(2)?.try_into().expect("exact length"),
        ))
    }

    fn u32(&mut self) -> Result<u32, CodecError> {
        Ok(u32::from_be_bytes(
            self.take(4)?.try_into().expect("exact length"),
        ))
    }

    fn u64(&mut self) -> Result<u64, CodecError> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().expect("exact length"),
        ))
    }

    fn address(&mut self) -> Result<Address, CodecError> {
        Ok(Address::from_slice(self.take(20)?))
    }

    fn b256(&mut self) -> Result<B256, CodecError> {
        Ok(B256::from_slice(self.take(32)?))
    }

    fn u256(&mut self) -> Result<U256, CodecError> {
        Ok(U256::from_be_slice(self.take(32)?))
    }

    fn bounded_bytes(
        &mut self,
        field: &'static str,
        maximum: usize,
    ) -> Result<Vec<u8>, CodecError> {
        let length = self.u32()? as usize;
        if length > maximum {
            return Err(CodecError::TooLarge {
                field,
                actual: length,
                maximum,
            });
        }
        Ok(self.take(length)?.to_vec())
    }
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    let length = u32::try_from(bytes.len()).expect("protocol values fit in u32");
    out.extend_from_slice(&length.to_be_bytes());
    out.extend_from_slice(bytes);
}

fn put_u256(out: &mut Vec<u8>, value: U256) {
    out.extend_from_slice(&value.to_be_bytes::<32>());
}

fn tagged_hash(tag: &[u8], value: &impl CanonicalEncode) -> B256 {
    let encoded = value.canonical_bytes();
    let mut preimage = Vec::with_capacity(4 + tag.len() + encoded.len());
    put_bytes(&mut preimage, tag);
    put_bytes(&mut preimage, &encoded);
    keccak256(preimage)
}

/// Immutable authority domain for one Zone epoch.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ZoneDomain {
    /// Tempo L1 chain ID.
    pub l1_chain_id: u64,
    /// Factory Zone ID.
    pub zone_id: u32,
    /// Zone execution chain ID.
    pub chain_id: u64,
    /// Zone Portal on L1.
    pub portal: Address,
    /// Finalized authority epoch.
    pub authority_epoch: u64,
    /// Hash of the three-member roster.
    pub roster_hash: B256,
    /// Fast protocol version.
    pub protocol_version: u16,
}

impl CanonicalEncode for ZoneDomain {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.l1_chain_id.to_be_bytes());
        out.extend_from_slice(&self.zone_id.to_be_bytes());
        out.extend_from_slice(&self.chain_id.to_be_bytes());
        out.extend_from_slice(self.portal.as_slice());
        out.extend_from_slice(&self.authority_epoch.to_be_bytes());
        out.extend_from_slice(self.roster_hash.as_slice());
        out.extend_from_slice(&self.protocol_version.to_be_bytes());
    }
}

impl CanonicalDecode for ZoneDomain {
    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            l1_chain_id: reader.u64()?,
            zone_id: reader.u32()?,
            chain_id: reader.u64()?,
            portal: reader.address()?,
            authority_epoch: reader.u64()?,
            roster_hash: reader.b256()?,
            protocol_version: reader.u16()?,
        })
    }
}

impl ZoneDomain {
    /// Stable storage/routing key for the complete authority domain.
    pub fn domain_hash(&self) -> B256 {
        tagged_hash(ZONE_DOMAIN_HASH_TAG, self)
    }
}

/// Cross-Zone identity and mappings for one TIP-20 asset.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AssetId {
    /// L1 TIP-20 address.
    pub l1_token: Address,
    /// Source Zone token mapping.
    pub source_token: Address,
    /// Destination Zone token mapping.
    pub destination_token: Address,
    /// Shared token decimals.
    pub decimals: u8,
}

impl CanonicalEncode for AssetId {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self.l1_token.as_slice());
        out.extend_from_slice(self.source_token.as_slice());
        out.extend_from_slice(self.destination_token.as_slice());
        out.push(self.decimals);
    }
}

impl CanonicalDecode for AssetId {
    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            l1_token: reader.address()?,
            source_token: reader.address()?,
            destination_token: reader.address()?,
            decimals: reader.u8()?,
        })
    }
}

/// Complete user-authorized transfer intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferIntent {
    /// Source authority domain.
    pub source: ZoneDomain,
    /// Destination authority domain.
    pub destination: ZoneDomain,
    /// Same underlying asset and both mappings.
    pub asset: AssetId,
    /// User account on the source.
    pub sender: Address,
    /// Recipient on the destination.
    pub recipient: Address,
    /// Refund account; version one requires this to equal `sender`.
    pub refund_account: Address,
    /// Operator-owned pool debited on the destination.
    pub destination_pool: Address,
    /// Operator-owned reimbursement account on the source.
    pub reimbursement_account: Address,
    /// Destination principal.
    pub principal: U256,
    /// Exact source-side route fee.
    pub fee: U256,
    /// Standing quote identifier.
    pub quote_id: B256,
    /// Last destination committed height at which payment may occur.
    pub destination_expiry_height: u64,
    /// Dedicated source business nonce.
    pub transfer_nonce: u64,
}

impl TransferIntent {
    /// Validate invariants that do not require external state.
    pub fn validate(&self) -> Result<(), CodecError> {
        if self.source.l1_chain_id != self.destination.l1_chain_id {
            return Err(CodecError::InvalidValue(
                "source and destination L1 chains differ",
            ));
        }
        if self.source.zone_id == self.destination.zone_id {
            return Err(CodecError::InvalidValue(
                "source and destination Zones are equal",
            ));
        }
        if self.source.protocol_version != self.destination.protocol_version {
            return Err(CodecError::InvalidValue("protocol versions differ"));
        }
        if self.refund_account != self.sender {
            return Err(CodecError::InvalidValue("refund account must equal sender"));
        }
        if self.principal.is_zero() {
            return Err(CodecError::InvalidValue("principal is zero"));
        }
        if self.canonical_bytes().len() > MAX_INTENT_BYTES {
            return Err(CodecError::TooLarge {
                field: "intent",
                actual: self.canonical_bytes().len(),
                maximum: MAX_INTENT_BYTES,
            });
        }
        Ok(())
    }

    /// Stable transfer identifier. It intentionally excludes mutable submission details.
    pub fn transfer_id(&self) -> B256 {
        struct Id<'a>(&'a TransferIntent);
        impl CanonicalEncode for Id<'_> {
            fn encode_to(&self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.0.source.l1_chain_id.to_be_bytes());
                out.extend_from_slice(self.0.source.portal.as_slice());
                out.extend_from_slice(&self.0.source.protocol_version.to_be_bytes());
                out.extend_from_slice(&self.0.source.zone_id.to_be_bytes());
                out.extend_from_slice(self.0.sender.as_slice());
                out.extend_from_slice(&self.0.transfer_nonce.to_be_bytes());
            }
        }
        tagged_hash(TRANSFER_ID_TAG, &Id(self))
    }

    /// Hash binding every intent field.
    pub fn intent_hash(&self) -> B256 {
        tagged_hash(INTENT_HASH_TAG, self)
    }

    /// Decode and validate an intent.
    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        let intent: Self = decode_exact(bytes, MAX_INTENT_BYTES)?;
        intent.validate()?;
        Ok(intent)
    }
}

impl CanonicalEncode for TransferIntent {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.source.encode_to(out);
        self.destination.encode_to(out);
        self.asset.encode_to(out);
        out.extend_from_slice(self.sender.as_slice());
        out.extend_from_slice(self.recipient.as_slice());
        out.extend_from_slice(self.refund_account.as_slice());
        out.extend_from_slice(self.destination_pool.as_slice());
        out.extend_from_slice(self.reimbursement_account.as_slice());
        put_u256(out, self.principal);
        put_u256(out, self.fee);
        out.extend_from_slice(self.quote_id.as_slice());
        out.extend_from_slice(&self.destination_expiry_height.to_be_bytes());
        out.extend_from_slice(&self.transfer_nonce.to_be_bytes());
    }
}

impl CanonicalDecode for TransferIntent {
    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            source: ZoneDomain::decode_from(reader)?,
            destination: ZoneDomain::decode_from(reader)?,
            asset: AssetId::decode_from(reader)?,
            sender: reader.address()?,
            recipient: reader.address()?,
            refund_account: reader.address()?,
            destination_pool: reader.address()?,
            reimbursement_account: reader.address()?,
            principal: reader.u256()?,
            fee: reader.u256()?,
            quote_id: reader.b256()?,
            destination_expiry_height: reader.u64()?,
            transfer_nonce: reader.u64()?,
        })
    }
}

/// Destination standing quote body.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StandingQuote {
    /// Source route domain.
    pub source: ZoneDomain,
    /// Destination route domain and signing authority.
    pub destination: ZoneDomain,
    /// Quoted asset mappings.
    pub asset: AssetId,
    /// Stable quote identifier.
    pub quote_id: B256,
    /// Exact route fee.
    pub fee: U256,
    /// Largest permitted principal.
    pub maximum_principal: U256,
    /// Destination expiry height.
    pub expiry_height: u64,
    /// Pool that will fund successful payments.
    pub destination_pool: Address,
    /// Source account receiving principal plus fee.
    pub reimbursement_account: Address,
}

impl StandingQuote {
    /// Hash binding every quote field.
    pub fn quote_hash(&self) -> B256 {
        tagged_hash(QUOTE_HASH_TAG, self)
    }

    /// Decode one complete bounded standing quote.
    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        decode_exact(bytes, MAX_CERTIFICATE_BYTES)
    }
}

impl CanonicalEncode for StandingQuote {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.source.encode_to(out);
        self.destination.encode_to(out);
        self.asset.encode_to(out);
        out.extend_from_slice(self.quote_id.as_slice());
        put_u256(out, self.fee);
        put_u256(out, self.maximum_principal);
        out.extend_from_slice(&self.expiry_height.to_be_bytes());
        out.extend_from_slice(self.destination_pool.as_slice());
        out.extend_from_slice(self.reimbursement_account.as_slice());
    }
}

impl CanonicalDecode for StandingQuote {
    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            source: ZoneDomain::decode_from(reader)?,
            destination: ZoneDomain::decode_from(reader)?,
            asset: AssetId::decode_from(reader)?,
            quote_id: reader.b256()?,
            fee: reader.u256()?,
            maximum_principal: reader.u256()?,
            expiry_height: reader.u64()?,
            destination_pool: reader.address()?,
            reimbursement_account: reader.address()?,
        })
    }
}

/// Quorum-certified standing quote. A quote advertises capacity but does not reserve it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuoteCertificate {
    /// Complete route/asset/fee quote.
    pub quote: StandingQuote,
    /// Exactly two signatures from distinct destination epoch members.
    pub signatures: [SignatureBytes; 2],
}

impl QuoteCertificate {
    /// Decode a bounded quote certificate.
    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        decode_exact(bytes, MAX_CERTIFICATE_BYTES)
    }
}

impl CanonicalEncode for QuoteCertificate {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.quote.encode_to(out);
        self.signatures[0].encode_to(out);
        self.signatures[1].encode_to(out);
    }
}

impl CanonicalDecode for QuoteCertificate {
    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            quote: StandingQuote::decode_from(reader)?,
            signatures: [
                SignatureBytes::decode_from(reader)?,
                SignatureBytes::decode_from(reader)?,
            ],
        })
    }
}

/// Permanent destination refusal reason.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RejectionReason {
    /// An authorized cancellation won the destination order.
    Cancelled = 0,
    /// Quote expiry height was reached.
    QuoteExpired = 1,
    /// Route admission is suspended.
    RouteDisabled = 2,
    /// Token or account policy denied payment.
    PolicyDenied = 3,
    /// Pool spendable liquidity was insufficient.
    InsufficientLiquidity = 4,
    /// Source exposure cap was exhausted.
    ExposureLimit = 5,
}

impl TryFrom<u8> for RejectionReason {
    type Error = CodecError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Cancelled),
            1 => Ok(Self::QuoteExpired),
            2 => Ok(Self::RouteDisabled),
            3 => Ok(Self::PolicyDenied),
            4 => Ok(Self::InsufficientLiquidity),
            5 => Ok(Self::ExposureLimit),
            value => Err(CodecError::InvalidTag {
                field: "rejection reason",
                value,
            }),
        }
    }
}

/// Full token-relevant outcome certified by a Zone quorum.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TransferOutcome {
    /// Source escrow committed for principal plus fee.
    Locked { escrow: Address, amount: U256 },
    /// Destination pool paid the ordinary recipient balance.
    Paid {
        pool: Address,
        recipient: Address,
        principal: U256,
    },
    /// Destination committed a permanent no-payment tombstone.
    Rejected { reason: RejectionReason },
    /// Source escrow was transferred to the reimbursement account.
    Released { beneficiary: Address, amount: U256 },
    /// Source escrow was returned to the sender.
    Refunded { beneficiary: Address, amount: U256 },
}

impl CanonicalEncode for TransferOutcome {
    fn encode_to(&self, out: &mut Vec<u8>) {
        match self {
            Self::Locked { escrow, amount } => {
                out.push(0);
                out.extend_from_slice(escrow.as_slice());
                put_u256(out, *amount);
            }
            Self::Paid {
                pool,
                recipient,
                principal,
            } => {
                out.push(1);
                out.extend_from_slice(pool.as_slice());
                out.extend_from_slice(recipient.as_slice());
                put_u256(out, *principal);
            }
            Self::Rejected { reason } => {
                out.push(2);
                out.push(*reason as u8);
            }
            Self::Released {
                beneficiary,
                amount,
            } => {
                out.push(3);
                out.extend_from_slice(beneficiary.as_slice());
                put_u256(out, *amount);
            }
            Self::Refunded {
                beneficiary,
                amount,
            } => {
                out.push(4);
                out.extend_from_slice(beneficiary.as_slice());
                put_u256(out, *amount);
            }
        }
    }
}

impl CanonicalDecode for TransferOutcome {
    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        match reader.u8()? {
            0 => Ok(Self::Locked {
                escrow: reader.address()?,
                amount: reader.u256()?,
            }),
            1 => Ok(Self::Paid {
                pool: reader.address()?,
                recipient: reader.address()?,
                principal: reader.u256()?,
            }),
            2 => Ok(Self::Rejected {
                reason: reader.u8()?.try_into()?,
            }),
            3 => Ok(Self::Released {
                beneficiary: reader.address()?,
                amount: reader.u256()?,
            }),
            4 => Ok(Self::Refunded {
                beneficiary: reader.address()?,
                amount: reader.u256()?,
            }),
            value => Err(CodecError::InvalidTag {
                field: "outcome",
                value,
            }),
        }
    }
}

/// Committed outcome body signed by replicas.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CertificateBody {
    /// Stable transfer ID.
    pub transfer_id: B256,
    /// Hash of the complete original intent.
    pub intent_hash: B256,
    /// Zone whose committed execution produced this outcome.
    pub zone: ZoneDomain,
    /// Original Raft term of the committed entry.
    pub log_term: u64,
    /// Original Raft index of the committed entry.
    pub log_index: u64,
    /// Committed Zone block height.
    pub block_height: u64,
    /// Committed Zone block hash.
    pub block_hash: B256,
    /// Post-execution state root.
    pub state_root: B256,
    /// Transaction hash that produced the outcome.
    pub transaction_hash: B256,
    /// Complete outcome fields.
    pub outcome: TransferOutcome,
}

impl CertificateBody {
    /// Domain-neutral body hash used inside the outcome signing digest.
    pub fn body_hash(&self) -> B256 {
        tagged_hash(OUTCOME_HASH_TAG, self)
    }
}

impl CanonicalEncode for CertificateBody {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self.transfer_id.as_slice());
        out.extend_from_slice(self.intent_hash.as_slice());
        self.zone.encode_to(out);
        out.extend_from_slice(&self.log_term.to_be_bytes());
        out.extend_from_slice(&self.log_index.to_be_bytes());
        out.extend_from_slice(&self.block_height.to_be_bytes());
        out.extend_from_slice(self.block_hash.as_slice());
        out.extend_from_slice(self.state_root.as_slice());
        out.extend_from_slice(self.transaction_hash.as_slice());
        self.outcome.encode_to(out);
    }
}

impl CanonicalDecode for CertificateBody {
    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            transfer_id: reader.b256()?,
            intent_hash: reader.b256()?,
            zone: ZoneDomain::decode_from(reader)?,
            log_term: reader.u64()?,
            log_index: reader.u64()?,
            block_height: reader.u64()?,
            block_hash: reader.b256()?,
            state_root: reader.b256()?,
            transaction_hash: reader.b256()?,
            outcome: TransferOutcome::decode_from(reader)?,
        })
    }
}

/// Exactly one recoverable EIP-712 signature.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignatureBytes(pub [u8; SIGNATURE_BYTES]);

impl CanonicalEncode for SignatureBytes {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.0);
    }
}

impl CanonicalDecode for SignatureBytes {
    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self(
            reader
                .take(SIGNATURE_BYTES)?
                .try_into()
                .expect("exact length"),
        ))
    }
}

/// Two-signature committed outcome certificate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutcomeCertificate {
    /// Deterministic committed body.
    pub body: CertificateBody,
    /// Exactly two signatures; validation also requires distinct authorized signers.
    pub signatures: [SignatureBytes; 2],
}

impl OutcomeCertificate {
    /// Decode a bounded certificate.
    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        decode_exact(bytes, MAX_CERTIFICATE_BYTES)
    }
}

impl CanonicalEncode for OutcomeCertificate {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.body.encode_to(out);
        self.signatures[0].encode_to(out);
        self.signatures[1].encode_to(out);
    }
}

impl CanonicalDecode for OutcomeCertificate {
    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            body: CertificateBody::decode_from(reader)?,
            signatures: [
                SignatureBytes::decode_from(reader)?,
                SignatureBytes::decode_from(reader)?,
            ],
        })
    }
}

/// Sender-authorized cancellation serialized against destination payment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationRequest {
    /// Stable transfer identifier.
    pub transfer_id: B256,
    /// Complete immutable intent hash.
    pub intent_hash: B256,
    /// Source sender whose escrow is locked.
    pub sender: Address,
    /// Source authority domain pinned by the lock.
    pub source: ZoneDomain,
    /// Recoverable signature by `sender` over [`Self::request_hash`].
    pub signature: SignatureBytes,
}

impl CancellationRequest {
    /// Hash signed by the sender. Certificate and transport domains cannot replay as cancellation.
    pub fn request_hash(&self) -> B256 {
        struct Unsigned<'a>(&'a CancellationRequest);
        impl CanonicalEncode for Unsigned<'_> {
            fn encode_to(&self, out: &mut Vec<u8>) {
                out.extend_from_slice(self.0.transfer_id.as_slice());
                out.extend_from_slice(self.0.intent_hash.as_slice());
                out.extend_from_slice(self.0.sender.as_slice());
                self.0.source.encode_to(out);
            }
        }
        tagged_hash(CANCELLATION_HASH_TAG, &Unsigned(self))
    }

    /// Decode one bounded cancellation request.
    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        decode_exact(bytes, MAX_CERTIFICATE_BYTES)
    }
}

impl CanonicalEncode for CancellationRequest {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self.transfer_id.as_slice());
        out.extend_from_slice(self.intent_hash.as_slice());
        out.extend_from_slice(self.sender.as_slice());
        self.source.encode_to(out);
        self.signature.encode_to(out);
    }
}

impl CanonicalDecode for CancellationRequest {
    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            transfer_id: reader.b256()?,
            intent_hash: reader.b256()?,
            sender: reader.address()?,
            source: ZoneDomain::decode_from(reader)?,
            signature: SignatureBytes::decode_from(reader)?,
        })
    }
}

/// Canonical direct-operator service payload.
///
/// The variant tags and independent length prefixes are part of the wire protocol. Decoding the
/// envelope also verifies that every attached certificate/cancellation binds the exact intent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServiceEnvelope {
    /// Destination quote publication.
    Quote(QuoteCertificate),
    /// Source lock, with an optional sender cancellation already serialized against payment.
    Locked {
        intent: TransferIntent,
        certificate: OutcomeCertificate,
        cancellation: Option<Box<CancellationRequest>>,
    },
    /// Destination `Paid` or permanent `Rejected` result.
    Terminal {
        intent: TransferIntent,
        certificate: OutcomeCertificate,
    },
    /// Source `Released` or `Refunded` result.
    Disposition {
        intent: TransferIntent,
        certificate: OutcomeCertificate,
    },
}

impl ServiceEnvelope {
    /// Decode one complete bounded envelope and reject trailing bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        decode_exact(bytes, MAX_SERVICE_ENVELOPE_BYTES)
    }

    fn validate_binding(&self) -> Result<(), CodecError> {
        let (intent, certificate) = match self {
            Self::Quote(_) => return Ok(()),
            Self::Locked {
                intent,
                certificate,
                cancellation,
            } => {
                if let Some(cancellation) = cancellation
                    && (cancellation.transfer_id != intent.transfer_id()
                        || cancellation.intent_hash != intent.intent_hash()
                        || cancellation.sender != intent.sender
                        || cancellation.source != intent.source)
                {
                    return Err(CodecError::InvalidValue(
                        "cancellation does not bind service intent",
                    ));
                }
                (intent, certificate)
            }
            Self::Terminal {
                intent,
                certificate,
            }
            | Self::Disposition {
                intent,
                certificate,
            } => (intent, certificate),
        };
        if certificate.body.transfer_id != intent.transfer_id()
            || certificate.body.intent_hash != intent.intent_hash()
        {
            return Err(CodecError::InvalidValue(
                "certificate does not bind service intent",
            ));
        }
        Ok(())
    }
}

impl CanonicalEncode for ServiceEnvelope {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.push(1); // service-envelope version
        match self {
            Self::Quote(quote) => {
                out.push(0);
                put_bytes(out, &quote.canonical_bytes());
            }
            Self::Locked {
                intent,
                certificate,
                cancellation,
            } => {
                out.push(1);
                put_bytes(out, &intent.canonical_bytes());
                put_bytes(out, &certificate.canonical_bytes());
                put_bytes(
                    out,
                    &cancellation
                        .as_ref()
                        .map(|request| request.canonical_bytes())
                        .unwrap_or_default(),
                );
            }
            Self::Terminal {
                intent,
                certificate,
            } => {
                out.push(2);
                put_bytes(out, &intent.canonical_bytes());
                put_bytes(out, &certificate.canonical_bytes());
            }
            Self::Disposition {
                intent,
                certificate,
            } => {
                out.push(3);
                put_bytes(out, &intent.canonical_bytes());
                put_bytes(out, &certificate.canonical_bytes());
            }
        }
    }
}

impl CanonicalDecode for ServiceEnvelope {
    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        if reader.u8()? != 1 {
            return Err(CodecError::InvalidValue(
                "unsupported service envelope version",
            ));
        }
        let value = match reader.u8()? {
            0 => Self::Quote(decode_exact(
                &reader.bounded_bytes("quote", MAX_CERTIFICATE_BYTES)?,
                MAX_CERTIFICATE_BYTES,
            )?),
            1 => {
                let intent = decode_exact(
                    &reader.bounded_bytes("intent", MAX_INTENT_BYTES)?,
                    MAX_INTENT_BYTES,
                )?;
                let certificate = decode_exact(
                    &reader.bounded_bytes("certificate", MAX_CERTIFICATE_BYTES)?,
                    MAX_CERTIFICATE_BYTES,
                )?;
                let cancellation = reader.bounded_bytes("cancellation", MAX_CERTIFICATE_BYTES)?;
                Self::Locked {
                    intent,
                    certificate,
                    cancellation: if cancellation.is_empty() {
                        None
                    } else {
                        Some(Box::new(decode_exact(
                            &cancellation,
                            MAX_CERTIFICATE_BYTES,
                        )?))
                    },
                }
            }
            2 => Self::Terminal {
                intent: decode_exact(
                    &reader.bounded_bytes("intent", MAX_INTENT_BYTES)?,
                    MAX_INTENT_BYTES,
                )?,
                certificate: decode_exact(
                    &reader.bounded_bytes("certificate", MAX_CERTIFICATE_BYTES)?,
                    MAX_CERTIFICATE_BYTES,
                )?,
            },
            3 => Self::Disposition {
                intent: decode_exact(
                    &reader.bounded_bytes("intent", MAX_INTENT_BYTES)?,
                    MAX_INTENT_BYTES,
                )?,
                certificate: decode_exact(
                    &reader.bounded_bytes("certificate", MAX_CERTIFICATE_BYTES)?,
                    MAX_CERTIFICATE_BYTES,
                )?,
            },
            value => {
                return Err(CodecError::InvalidTag {
                    field: "service envelope",
                    value,
                });
            }
        };
        value.validate_binding()?;
        Ok(value)
    }
}

/// Historical source-quorum statement proving the complete lock set at a destination closure.
/// Field order exactly matches `IZoneFactory.FastBarrierStatement`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FastBarrierStatement {
    pub destination_portal: Address,
    pub destination_epoch: u64,
    pub closure_hash: B256,
    pub source_portal: Address,
    pub source_epoch: u64,
    pub imported_anchor_number: u64,
    pub imported_anchor_hash: B256,
    pub log_term: u64,
    pub log_index: u64,
    pub block_height: U256,
    pub block_hash: B256,
    pub state_root: B256,
    pub lock_log_watermark: u64,
    pub complete_lock_root: B256,
    pub unresolved_root: B256,
    pub unresolved_count: u64,
}

impl FastBarrierStatement {
    /// Exact purpose-separated Solidity ABI digest verified by the Tempo factory.
    pub fn registry_digest(&self, l1_chain_id: u64) -> B256 {
        let mut encoded = Vec::with_capacity(18 * 32);
        put_abi_b256(&mut encoded, keccak256(FAST_BARRIER_DOMAIN));
        put_abi_u256(&mut encoded, U256::from(l1_chain_id));
        put_abi_address(&mut encoded, self.destination_portal);
        put_abi_u64(&mut encoded, self.destination_epoch);
        put_abi_b256(&mut encoded, self.closure_hash);
        put_abi_address(&mut encoded, self.source_portal);
        put_abi_u64(&mut encoded, self.source_epoch);
        put_abi_u64(&mut encoded, self.imported_anchor_number);
        put_abi_b256(&mut encoded, self.imported_anchor_hash);
        put_abi_u64(&mut encoded, self.log_term);
        put_abi_u64(&mut encoded, self.log_index);
        put_abi_u256(&mut encoded, self.block_height);
        put_abi_b256(&mut encoded, self.block_hash);
        put_abi_b256(&mut encoded, self.state_root);
        put_abi_u64(&mut encoded, self.lock_log_watermark);
        put_abi_b256(&mut encoded, self.complete_lock_root);
        put_abi_b256(&mut encoded, self.unresolved_root);
        put_abi_u64(&mut encoded, self.unresolved_count);
        keccak256(encoded)
    }
}

/// Source-quorum proof that the exact authenticated barrier set reached terminal disposition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FastBarrierResolution {
    pub barrier_hash: B256,
    pub terminal_root: B256,
    pub disposition_root: B256,
    pub resolved_count: u64,
    pub remaining_unresolved_root: B256,
    pub remaining_unresolved_count: u64,
}

impl FastBarrierResolution {
    /// Exact purpose-separated Solidity ABI digest verified by the Tempo factory.
    pub fn registry_digest(
        &self,
        l1_chain_id: u64,
        destination_portal: Address,
        destination_epoch: u64,
        source_portal: Address,
    ) -> B256 {
        let mut encoded = Vec::with_capacity(11 * 32);
        put_abi_b256(&mut encoded, keccak256(FAST_BARRIER_RESOLUTION_DOMAIN));
        put_abi_u256(&mut encoded, U256::from(l1_chain_id));
        put_abi_address(&mut encoded, destination_portal);
        put_abi_u64(&mut encoded, destination_epoch);
        put_abi_address(&mut encoded, source_portal);
        put_abi_b256(&mut encoded, self.barrier_hash);
        put_abi_b256(&mut encoded, self.terminal_root);
        put_abi_b256(&mut encoded, self.disposition_root);
        put_abi_u64(&mut encoded, self.resolved_count);
        put_abi_b256(&mut encoded, self.remaining_unresolved_root);
        put_abi_u64(&mut encoded, self.remaining_unresolved_count);
        keccak256(encoded)
    }
}

/// Next-roster acknowledgment of the exact retired accepted prefix and installed checkpoint.
/// Field order exactly matches `IZoneFactory.FastCheckpointStatement`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FastCheckpointStatement {
    pub portal: Address,
    pub old_epoch: u64,
    pub next_epoch: u64,
    pub next_roster_hash: B256,
    pub final_zone_height: U256,
    pub final_block_hash: B256,
    pub final_withdrawal_batch_index: u64,
    pub final_settlement_hash: B256,
    pub checkpoint_log_term: u64,
    pub checkpoint_log_index: u64,
    pub checkpoint_height: U256,
    pub checkpoint_block_hash: B256,
    pub checkpoint_state_root: B256,
}

impl FastCheckpointStatement {
    /// Exact purpose-separated Solidity ABI digest signed by two distinct next-roster members.
    pub fn registry_digest(&self, l1_chain_id: u64) -> B256 {
        let mut encoded = Vec::with_capacity(15 * 32);
        put_abi_b256(&mut encoded, keccak256(FAST_CHECKPOINT_DOMAIN));
        put_abi_u256(&mut encoded, U256::from(l1_chain_id));
        put_abi_address(&mut encoded, self.portal);
        put_abi_u64(&mut encoded, self.old_epoch);
        put_abi_u64(&mut encoded, self.next_epoch);
        put_abi_b256(&mut encoded, self.next_roster_hash);
        put_abi_u256(&mut encoded, self.final_zone_height);
        put_abi_b256(&mut encoded, self.final_block_hash);
        put_abi_u64(&mut encoded, self.final_withdrawal_batch_index);
        put_abi_b256(&mut encoded, self.final_settlement_hash);
        put_abi_u64(&mut encoded, self.checkpoint_log_term);
        put_abi_u64(&mut encoded, self.checkpoint_log_index);
        put_abi_u256(&mut encoded, self.checkpoint_height);
        put_abi_b256(&mut encoded, self.checkpoint_block_hash);
        put_abi_b256(&mut encoded, self.checkpoint_state_root);
        keccak256(encoded)
    }
}

fn put_abi_b256(out: &mut Vec<u8>, value: B256) {
    out.extend_from_slice(value.as_slice());
}

fn put_abi_u256(out: &mut Vec<u8>, value: U256) {
    out.extend_from_slice(&value.to_be_bytes::<32>());
}

fn put_abi_u64(out: &mut Vec<u8>, value: u64) {
    put_abi_u256(out, U256::from(value));
}

fn put_abi_address(out: &mut Vec<u8>, value: Address) {
    out.extend_from_slice(&[0; 12]);
    out.extend_from_slice(value.as_slice());
}

/// Finalized proof that a source release was included in an accepted source prefix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExposureRetirementEvidence {
    /// Transfer whose destination exposure is retired.
    pub transfer_id: B256,
    /// Exact immutable intent hash.
    pub intent_hash: B256,
    /// Source Portal accepted block hash, or a transitive ancestry checkpoint previously
    /// authenticated to that accepted hash at the destination's finalized L1 anchor.
    pub accepted_source_block_hash: B256,
    /// Hash of the successful release receipt proven below.
    pub release_receipt_hash: B256,
    /// Destination token whose source exposure is decremented.
    pub destination_token: Address,
    /// Source reimbursement beneficiary bound by the paid record.
    pub beneficiary: Address,
    /// Destination principal only; the separately accounted route fee is excluded.
    pub principal: U256,
    /// Receipt inclusion proof against the releasing Zone block header.
    pub receipt_proof: Vec<u8>,
    /// At most 256 headers linking that block to an authenticated descendant/checkpoint.
    pub header_chain: Vec<u8>,
}

/// Bounded ordered-receipt-trie inclusion proof.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceiptInclusionProof {
    /// Zero-based transaction/receipt index in the releasing block.
    pub transaction_index: u64,
    /// Canonical EIP-2718 receipt bytes including the receipt bloom.
    pub receipt: Vec<u8>,
    /// Root-to-leaf Merkle-Patricia proof nodes.
    pub nodes: Vec<Vec<u8>>,
}

impl ReceiptInclusionProof {
    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        decode_exact(bytes, MAX_RETIREMENT_PROOF_BYTES)
    }
}

impl CanonicalEncode for ReceiptInclusionProof {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.transaction_index.to_be_bytes());
        put_bytes(out, &self.receipt);
        let count = u16::try_from(self.nodes.len()).expect("receipt proof node count fits u16");
        out.extend_from_slice(&count.to_be_bytes());
        for node in &self.nodes {
            put_bytes(out, node);
        }
    }
}

impl CanonicalDecode for ReceiptInclusionProof {
    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        let transaction_index = reader.u64()?;
        let receipt = reader.bounded_bytes("receipt", MAX_RETIREMENT_PROOF_BYTES)?;
        let count = usize::from(reader.u16()?);
        if count > MAX_RECEIPT_PROOF_NODES {
            return Err(CodecError::TooLarge {
                field: "receipt proof nodes",
                actual: count,
                maximum: MAX_RECEIPT_PROOF_NODES,
            });
        }
        let mut nodes = Vec::with_capacity(count);
        for _ in 0..count {
            nodes.push(reader.bounded_bytes("receipt proof node", MAX_RETIREMENT_PROOF_BYTES)?);
        }
        Ok(Self {
            transaction_index,
            receipt,
            nodes,
        })
    }
}

/// Consecutive RLP Zone headers ordered from the releasing block to an accepted descendant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeaderAncestryProof {
    pub headers: Vec<Vec<u8>>,
}

impl HeaderAncestryProof {
    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        decode_exact(bytes, MAX_RETIREMENT_PROOF_BYTES)
    }
}

impl CanonicalEncode for HeaderAncestryProof {
    fn encode_to(&self, out: &mut Vec<u8>) {
        let count = u16::try_from(self.headers.len()).expect("header count fits u16");
        out.extend_from_slice(&count.to_be_bytes());
        for header in &self.headers {
            put_bytes(out, header);
        }
    }
}

impl CanonicalDecode for HeaderAncestryProof {
    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        let count = usize::from(reader.u16()?);
        if count == 0 || count > MAX_RETIREMENT_HEADERS {
            return Err(CodecError::TooLarge {
                field: "retirement headers",
                actual: count,
                maximum: MAX_RETIREMENT_HEADERS,
            });
        }
        let mut headers = Vec::with_capacity(count);
        for _ in 0..count {
            headers.push(reader.bounded_bytes("retirement header", MAX_RETIREMENT_PROOF_BYTES)?);
        }
        Ok(Self { headers })
    }
}

/// Replay-resistant proof binding an authenticated transport session to a certificate roster key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransportSessionProof {
    /// Fresh request identity, retained through the challenge and acknowledgment.
    pub request_id: B256,
    /// Actual authenticated Commonware initiating peer key, not a socket address.
    pub initiator_ed25519: B256,
    /// Actual authenticated Commonware responding peer key.
    pub responder_ed25519: B256,
    /// Initiating Zone epoch.
    pub initiator: ZoneDomain,
    /// Initiating replica certificate key.
    pub initiator_member: Address,
    /// Responding Zone epoch.
    pub responder: ZoneDomain,
    /// Responding replica certificate key.
    pub responder_member: Address,
    /// Fresh 32-byte nonce generated by the initiator.
    pub initiator_nonce: B256,
    /// Fresh 32-byte nonce generated by the responder.
    pub responder_nonce: B256,
    /// Durable delivery stream bound to this fresh authenticated exchange.
    pub stream: u64,
    /// Exact frame sequence; a proof cannot authenticate a different frame position.
    pub sequence: u64,
    /// `false` for the initiator proof, `true` for the responder proof.
    pub responder_role: bool,
    /// Signature by the role's member over the complete transcript hash.
    pub signature: SignatureBytes,
}

impl TransportSessionProof {
    /// Domain-separated handshake transcript digest.
    pub fn session_hash(&self) -> B256 {
        struct Unsigned<'a>(&'a TransportSessionProof);
        impl CanonicalEncode for Unsigned<'_> {
            fn encode_to(&self, out: &mut Vec<u8>) {
                out.extend_from_slice(self.0.request_id.as_slice());
                out.extend_from_slice(self.0.initiator_ed25519.as_slice());
                out.extend_from_slice(self.0.responder_ed25519.as_slice());
                self.0.initiator.encode_to(out);
                out.extend_from_slice(self.0.initiator_member.as_slice());
                self.0.responder.encode_to(out);
                out.extend_from_slice(self.0.responder_member.as_slice());
                out.extend_from_slice(self.0.initiator_nonce.as_slice());
                out.extend_from_slice(self.0.responder_nonce.as_slice());
                out.extend_from_slice(&self.0.stream.to_be_bytes());
                out.extend_from_slice(&self.0.sequence.to_be_bytes());
                out.push(u8::from(self.0.responder_role));
            }
        }
        tagged_hash(TRANSPORT_SESSION_HASH_TAG, &Unsigned(self))
    }

    /// Decode one bounded session proof.
    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        decode_exact(bytes, MAX_CERTIFICATE_BYTES)
    }
}

impl CanonicalEncode for TransportSessionProof {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self.request_id.as_slice());
        out.extend_from_slice(self.initiator_ed25519.as_slice());
        out.extend_from_slice(self.responder_ed25519.as_slice());
        self.initiator.encode_to(out);
        out.extend_from_slice(self.initiator_member.as_slice());
        self.responder.encode_to(out);
        out.extend_from_slice(self.responder_member.as_slice());
        out.extend_from_slice(self.initiator_nonce.as_slice());
        out.extend_from_slice(self.responder_nonce.as_slice());
        out.extend_from_slice(&self.stream.to_be_bytes());
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.push(u8::from(self.responder_role));
        self.signature.encode_to(out);
    }
}

impl CanonicalDecode for TransportSessionProof {
    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            request_id: reader.b256()?,
            initiator_ed25519: reader.b256()?,
            responder_ed25519: reader.b256()?,
            initiator: ZoneDomain::decode_from(reader)?,
            initiator_member: reader.address()?,
            responder: ZoneDomain::decode_from(reader)?,
            responder_member: reader.address()?,
            initiator_nonce: reader.b256()?,
            responder_nonce: reader.b256()?,
            stream: reader.u64()?,
            sequence: reader.u64()?,
            responder_role: match reader.u8()? {
                0 => false,
                1 => true,
                value => {
                    return Err(CodecError::InvalidTag {
                        field: "transport role",
                        value,
                    });
                }
            },
            signature: SignatureBytes::decode_from(reader)?,
        })
    }
}

impl ExposureRetirementEvidence {
    /// Decode one bounded retirement proof, rejecting trailing data and oversized proof chunks.
    pub fn decode(bytes: &[u8]) -> Result<Self, CodecError> {
        decode_exact(bytes, MAX_RETIREMENT_PROOF_BYTES)
    }
}

impl CanonicalEncode for ExposureRetirementEvidence {
    fn encode_to(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(self.transfer_id.as_slice());
        out.extend_from_slice(self.intent_hash.as_slice());
        out.extend_from_slice(self.accepted_source_block_hash.as_slice());
        out.extend_from_slice(self.release_receipt_hash.as_slice());
        out.extend_from_slice(self.destination_token.as_slice());
        out.extend_from_slice(self.beneficiary.as_slice());
        put_u256(out, self.principal);
        put_bytes(out, &self.receipt_proof);
        put_bytes(out, &self.header_chain);
    }
}

impl CanonicalDecode for ExposureRetirementEvidence {
    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            transfer_id: reader.b256()?,
            intent_hash: reader.b256()?,
            accepted_source_block_hash: reader.b256()?,
            release_receipt_hash: reader.b256()?,
            destination_token: reader.address()?,
            beneficiary: reader.address()?,
            principal: reader.u256()?,
            receipt_proof: reader.bounded_bytes("receipt proof", MAX_RETIREMENT_PROOF_BYTES)?,
            header_chain: reader.bounded_bytes("header chain", MAX_RETIREMENT_PROOF_BYTES)?,
        })
    }
}

/// Settlement statement signed under a domain distinct from transfer outcomes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettlementStatement {
    /// Zone authority domain.
    pub zone: ZoneDomain,
    /// Settled committed chain height.
    pub settled_height: u64,
    /// Hash at the settled height.
    pub settled_block_hash: B256,
    /// Batch or transition commitment submitted to L1.
    pub transition_hash: B256,
    /// Finalized L1 anchor used by the statement.
    pub anchor_hash: B256,
}

impl SettlementStatement {
    /// Domain-neutral statement hash.
    pub fn statement_hash(&self) -> B256 {
        tagged_hash(SETTLEMENT_HASH_TAG, self)
    }
}

impl CanonicalEncode for SettlementStatement {
    fn encode_to(&self, out: &mut Vec<u8>) {
        self.zone.encode_to(out);
        out.extend_from_slice(&self.settled_height.to_be_bytes());
        out.extend_from_slice(self.settled_block_hash.as_slice());
        out.extend_from_slice(self.transition_hash.as_slice());
        out.extend_from_slice(self.anchor_hash.as_slice());
    }
}

impl CanonicalDecode for SettlementStatement {
    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        Ok(Self {
            zone: ZoneDomain::decode_from(reader)?,
            settled_height: reader.u64()?,
            settled_block_hash: reader.b256()?,
            transition_hash: reader.b256()?,
            anchor_hash: reader.b256()?,
        })
    }
}

/// Transport frame payload. Authentication is deliberately outside this enum.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PeerMessage {
    /// Canonical quote bytes.
    Quote(Vec<u8>),
    /// Canonical outcome certificate bytes.
    Certificate(Vec<u8>),
    /// Monotonic durable delivery acknowledgment.
    Acknowledgment { stream: u64, sequence: u64 },
}

impl CanonicalEncode for PeerMessage {
    fn encode_to(&self, out: &mut Vec<u8>) {
        match self {
            Self::Quote(bytes) => {
                out.push(0);
                put_bytes(out, bytes);
            }
            Self::Certificate(bytes) => {
                out.push(1);
                put_bytes(out, bytes);
            }
            Self::Acknowledgment { stream, sequence } => {
                out.push(2);
                out.extend_from_slice(&stream.to_be_bytes());
                out.extend_from_slice(&sequence.to_be_bytes());
            }
        }
    }
}

impl CanonicalDecode for PeerMessage {
    fn decode_from(reader: &mut Reader<'_>) -> Result<Self, CodecError> {
        match reader.u8()? {
            0 => Ok(Self::Quote(
                reader.bounded_bytes("quote", MAX_CERTIFICATE_BYTES)?,
            )),
            1 => Ok(Self::Certificate(
                reader.bounded_bytes("certificate", MAX_CERTIFICATE_BYTES)?,
            )),
            2 => Ok(Self::Acknowledgment {
                stream: reader.u64()?,
                sequence: reader.u64()?,
            }),
            value => Err(CodecError::InvalidTag {
                field: "peer message",
                value,
            }),
        }
    }
}

/// Owned diagnostic string with no wire significance.
pub type Diagnostic = String;

#[cfg(test)]
mod tests {
    use super::*;

    fn domain(zone: u32, byte: u8) -> ZoneDomain {
        ZoneDomain {
            l1_chain_id: 1337,
            zone_id: zone,
            chain_id: 10_000 + zone as u64,
            portal: Address::repeat_byte(byte),
            authority_epoch: 3,
            roster_hash: B256::repeat_byte(byte),
            protocol_version: 1,
        }
    }

    fn intent() -> TransferIntent {
        TransferIntent {
            source: domain(1, 1),
            destination: domain(2, 2),
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
            principal: U256::from(10_000_000),
            fee: U256::from(100),
            quote_id: B256::repeat_byte(10),
            destination_expiry_height: 99,
            transfer_nonce: 42,
        }
    }

    #[test]
    fn intent_round_trip_and_hashes_are_field_binding() {
        let intent = intent();
        let bytes = intent.canonical_bytes();
        assert_eq!(TransferIntent::decode(&bytes).unwrap(), intent);

        let mut changed = intent.clone();
        changed.recipient = Address::repeat_byte(0xff);
        assert_eq!(changed.transfer_id(), intent.transfer_id());
        assert_ne!(changed.intent_hash(), intent.intent_hash());

        changed = intent.clone();
        changed.transfer_nonce += 1;
        assert_ne!(changed.transfer_id(), intent.transfer_id());
    }

    #[test]
    fn decoder_rejects_trailing_and_oversized_data() {
        let mut bytes = intent().canonical_bytes();
        bytes.push(0);
        assert_eq!(
            TransferIntent::decode(&bytes),
            Err(CodecError::TrailingBytes)
        );
        assert!(matches!(
            decode_exact::<TransferIntent>(&vec![0; MAX_INTENT_BYTES + 1], MAX_INTENT_BYTES),
            Err(CodecError::TooLarge { .. })
        ));
    }

    #[test]
    fn certificate_round_trip_is_exact() {
        let intent = intent();
        let certificate = OutcomeCertificate {
            body: CertificateBody {
                transfer_id: intent.transfer_id(),
                intent_hash: intent.intent_hash(),
                zone: intent.destination,
                log_term: 4,
                log_index: 8,
                block_height: 12,
                block_hash: B256::repeat_byte(11),
                state_root: B256::repeat_byte(12),
                transaction_hash: B256::repeat_byte(13),
                outcome: TransferOutcome::Paid {
                    pool: intent.destination_pool,
                    recipient: intent.recipient,
                    principal: intent.principal,
                },
            },
            signatures: [SignatureBytes([1; 65]), SignatureBytes([2; 65])],
        };
        let encoded = certificate.canonical_bytes();
        assert_eq!(OutcomeCertificate::decode(&encoded).unwrap(), certificate);
        assert!(encoded.len() <= MAX_CERTIFICATE_BYTES);
    }

    #[test]
    fn service_envelope_round_trip_rejects_trailing_and_cross_intent_certificate() {
        let intent = intent();
        let certificate = OutcomeCertificate {
            body: CertificateBody {
                transfer_id: intent.transfer_id(),
                intent_hash: intent.intent_hash(),
                zone: intent.destination,
                log_term: 1,
                log_index: 2,
                block_height: 3,
                block_hash: B256::repeat_byte(4),
                state_root: B256::repeat_byte(5),
                transaction_hash: B256::repeat_byte(6),
                outcome: TransferOutcome::Rejected {
                    reason: RejectionReason::Cancelled,
                },
            },
            signatures: [SignatureBytes([7; 65]), SignatureBytes([8; 65])],
        };
        let envelope = ServiceEnvelope::Terminal {
            intent: intent.clone(),
            certificate: certificate.clone(),
        };
        let encoded = envelope.canonical_bytes();
        assert!(encoded.len() <= MAX_SERVICE_ENVELOPE_BYTES);
        assert_eq!(ServiceEnvelope::decode(&encoded).unwrap(), envelope);

        let mut trailing = encoded;
        trailing.push(0);
        assert_eq!(
            ServiceEnvelope::decode(&trailing),
            Err(CodecError::TrailingBytes)
        );

        let mut other = intent;
        other.transfer_nonce += 1;
        let mismatched = ServiceEnvelope::Terminal {
            intent: other,
            certificate,
        }
        .canonical_bytes();
        assert!(matches!(
            ServiceEnvelope::decode(&mismatched),
            Err(CodecError::InvalidValue(_))
        ));
    }

    #[test]
    fn barrier_and_checkpoint_digests_are_purpose_separated() {
        let barrier = FastBarrierStatement {
            destination_portal: Address::repeat_byte(1),
            destination_epoch: 2,
            closure_hash: B256::repeat_byte(3),
            source_portal: Address::repeat_byte(4),
            source_epoch: 5,
            imported_anchor_number: 6,
            imported_anchor_hash: B256::repeat_byte(7),
            log_term: 8,
            log_index: 9,
            block_height: U256::from(10),
            block_hash: B256::repeat_byte(11),
            state_root: B256::repeat_byte(12),
            lock_log_watermark: 13,
            complete_lock_root: B256::repeat_byte(14),
            unresolved_root: FAST_EMPTY_UNRESOLVED_ROOT,
            unresolved_count: 0,
        };
        let resolution = FastBarrierResolution {
            barrier_hash: barrier.registry_digest(1),
            terminal_root: B256::repeat_byte(15),
            disposition_root: B256::repeat_byte(16),
            resolved_count: 0,
            remaining_unresolved_root: FAST_EMPTY_UNRESOLVED_ROOT,
            remaining_unresolved_count: 0,
        };
        assert_ne!(
            barrier.registry_digest(1),
            resolution.registry_digest(
                1,
                barrier.destination_portal,
                barrier.destination_epoch,
                barrier.source_portal,
            )
        );
        let mut changed = barrier.clone();
        changed.log_index += 1;
        assert_ne!(barrier.registry_digest(1), changed.registry_digest(1));
    }

    #[test]
    fn retirement_subproofs_round_trip_and_enforce_counts() {
        let receipt = ReceiptInclusionProof {
            transaction_index: 7,
            receipt: vec![1, 2, 3],
            nodes: vec![vec![4, 5], vec![6]],
        };
        assert_eq!(
            ReceiptInclusionProof::decode(&receipt.canonical_bytes()).unwrap(),
            receipt
        );

        let ancestry = HeaderAncestryProof {
            headers: vec![vec![0xf8, 1], vec![0xf8, 2]],
        };
        assert_eq!(
            HeaderAncestryProof::decode(&ancestry.canonical_bytes()).unwrap(),
            ancestry
        );
        assert!(matches!(
            HeaderAncestryProof::decode(&[0, 0]),
            Err(CodecError::TooLarge {
                field: "retirement headers",
                actual: 0,
                ..
            })
        ));
    }
}
