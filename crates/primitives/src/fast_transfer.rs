//! Deterministic domain and wire types for the instant transfer protocol.
//!
//! The codec is deliberately small and consensus-friendly: integers are big-endian,
//! fields are emitted in declaration order, and every variable-length field has a
//! `u32` byte length. Decoders reject trailing bytes and enforce the protocol limits.

extern crate alloc;

use alloc::{string::String, vec::Vec};
use alloy_primitives::{Address, B256, U256, keccak256};

/// Maximum encoded user intent size.
pub const MAX_INTENT_BYTES: usize = 2 * 1024;
/// Maximum encoded quorum certificate size.
pub const MAX_CERTIFICATE_BYTES: usize = 4 * 1024;
/// Encoded secp256k1 signature length (`r || s || yParity`).
pub const SIGNATURE_BYTES: usize = 65;

const TRANSFER_ID_TAG: &[u8] = b"tempo.zone.fast-transfer.id.v1";
const INTENT_HASH_TAG: &[u8] = b"tempo.zone.fast-transfer.intent.v1";
const QUOTE_HASH_TAG: &[u8] = b"tempo.zone.fast-transfer.quote.v1";
const OUTCOME_HASH_TAG: &[u8] = b"tempo.zone.fast-transfer.outcome.v1";
const SETTLEMENT_HASH_TAG: &[u8] = b"tempo.zone.fast-transfer.settlement.v1";

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
}
