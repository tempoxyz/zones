//! Allocation-bounded, canonical protocol calldata decoding.

mod bounds;

use alloy_consensus::BlockHeader as _;
use alloy_primitives::{B256, Bytes};
use alloy_rlp::{Decodable as _, Encodable as _};
use alloy_sol_types::{SolCall as _, SolValue as _};
use reth_primitives_traits::SealedHeader;
use tempo_primitives::TempoHeader;
use tempo_zone_contracts::{IZoneInbox, IZoneOutbox, ZonePortal};

use tempo_zone_contracts::{
    MAX_DEPOSITS_PER_TEMPO_BLOCK, MAX_SEQUENCERS, MAX_TOKEN_METADATA_BYTES,
    MAX_TOKENS_ENABLED_PER_TEMPO_BLOCK,
};
use zone_precompiles::{
    ecies::{AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE, ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE},
    outbox::MAX_CALLBACK_DATA_SIZE,
};

use super::error::{
    AuthenticatedDataEvidence, AuthenticatedTransaction, DataSource, ObservationError,
    PortalCallFamily,
};
use bounds::Bounds;

const WORD: usize = 32;
const SELECTOR_LEN: usize = 4;
// `ZonePortal.Deposit` is one top-level offset, a six-word tuple head,
// a five-word encrypted-payload head, and a three-word ciphertext tail.
const ORDINARY_DEPOSIT_ENCODED_SIZE: usize = 15 * WORD;

/// A malformed ABI surface before it is attached to an authenticated transaction.
#[derive(Debug)]
struct AbiError {
    source: DataSource,
    evidence: AuthenticatedDataEvidence,
    detail: String,
}

impl AbiError {
    fn into_observation(self, transaction: AuthenticatedTransaction) -> ObservationError {
        ObservationError::malformed(self.source, transaction, self.evidence, self.detail)
    }
}

/// Bytes and their protocol source used to construct consistent malformed-data errors.
#[derive(Clone, Copy)]
struct Surface<'a> {
    source: DataSource,
    bytes: &'a [u8],
}

impl<'a> Surface<'a> {
    const fn new(source: DataSource, bytes: &'a [u8]) -> Self {
        Self { source, bytes }
    }

    fn malformed(self, detail: impl core::fmt::Display) -> AbiError {
        AbiError {
            source: self.source,
            evidence: AuthenticatedDataEvidence::from_bytes(self.bytes),
            detail: detail.to_string(),
        }
    }
}

/// Canonical Tempo header selected at an authenticated observation boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportedTempoHeader {
    sealed: SealedHeader<TempoHeader>,
}

impl ImportedTempoHeader {
    pub(super) fn new(header: TempoHeader) -> Self {
        Self {
            sealed: SealedHeader::seal_slow(header),
        }
    }

    pub(crate) fn header(&self) -> &TempoHeader {
        self.sealed.header()
    }

    pub(crate) fn hash(&self) -> B256 {
        self.sealed.hash()
    }

    pub(crate) fn number(&self) -> u64 {
        self.sealed.number()
    }
}

/// A nested queue entry after its opaque `depositData` bytes are decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImportedDeposit {
    kind: ImportedDepositKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ImportedDepositKind {
    WithdrawalBounceBack(IZoneInbox::WithdrawalBounceBackDeposit),
    Ordinary(ZonePortal::Deposit),
}

impl ImportedDeposit {
    pub(crate) fn as_withdrawal_bounce_back(
        &self,
    ) -> Option<&IZoneInbox::WithdrawalBounceBackDeposit> {
        match &self.kind {
            ImportedDepositKind::WithdrawalBounceBack(deposit) => Some(deposit),
            ImportedDepositKind::Ordinary(_) => None,
        }
    }

    pub(crate) fn as_ordinary(&self) -> Option<&ZonePortal::Deposit> {
        match &self.kind {
            ImportedDepositKind::Ordinary(deposit) => Some(deposit),
            ImportedDepositKind::WithdrawalBounceBack(_) => None,
        }
    }
}

/// Authenticated inputs carried by `advanceTempo` calldata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedAdvanceTempo {
    imported_header: ImportedTempoHeader,
    deposits: Vec<ImportedDeposit>,
    enabled_tokens: Vec<IZoneInbox::EnabledToken>,
}

impl DecodedAdvanceTempo {
    pub(crate) fn imported_header(&self) -> &ImportedTempoHeader {
        &self.imported_header
    }

    pub(crate) fn deposits(&self) -> &[ImportedDeposit] {
        &self.deposits
    }

    pub(crate) fn enabled_tokens(&self) -> &[IZoneInbox::EnabledToken] {
        &self.enabled_tokens
    }
}

/// Authenticated inputs carried by the optional final system transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedFinalization {
    count: usize,
    block_number: u64,
    encrypted_senders: Vec<Bytes>,
}

impl DecodedFinalization {
    pub(crate) fn count(&self) -> usize {
        self.count
    }

    pub(crate) fn block_number(&self) -> u64 {
        self.block_number
    }

    pub(crate) fn encrypted_senders(&self) -> &[Bytes] {
        &self.encrypted_senders
    }
}

/// Decoded top-level Portal calldata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodedPortalCall {
    kind: DecodedPortalCallKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DecodedPortalCallKind {
    SubmitBatch(Box<ZonePortal::submitBatchCall>),
    ProcessWithdrawals(ZonePortal::processWithdrawalsCall),
    SetBouncebackGas(ZonePortal::setBouncebackGasCall),
    EnableToken(ZonePortal::enableTokenCall),
    KnownIgnoredStateChange,
    Deposit,
    ClaimRefund,
}

impl DecodedPortalCall {
    pub(crate) fn as_submit_batch(&self) -> Option<&ZonePortal::submitBatchCall> {
        match &self.kind {
            DecodedPortalCallKind::SubmitBatch(call) => Some(call),
            DecodedPortalCallKind::ProcessWithdrawals(_) => None,
            DecodedPortalCallKind::SetBouncebackGas(_) => None,
            DecodedPortalCallKind::EnableToken(_) => None,
            DecodedPortalCallKind::KnownIgnoredStateChange
            | DecodedPortalCallKind::Deposit
            | DecodedPortalCallKind::ClaimRefund => None,
        }
    }

    pub(crate) fn as_process_withdrawals(&self) -> Option<&ZonePortal::processWithdrawalsCall> {
        match &self.kind {
            DecodedPortalCallKind::ProcessWithdrawals(call) => Some(call),
            DecodedPortalCallKind::SubmitBatch(_) => None,
            DecodedPortalCallKind::SetBouncebackGas(_) => None,
            DecodedPortalCallKind::EnableToken(_) => None,
            DecodedPortalCallKind::KnownIgnoredStateChange
            | DecodedPortalCallKind::Deposit
            | DecodedPortalCallKind::ClaimRefund => None,
        }
    }

    pub(crate) fn as_set_bounceback_gas(&self) -> Option<&ZonePortal::setBouncebackGasCall> {
        match &self.kind {
            DecodedPortalCallKind::SetBouncebackGas(call) => Some(call),
            DecodedPortalCallKind::SubmitBatch(_)
            | DecodedPortalCallKind::ProcessWithdrawals(_)
            | DecodedPortalCallKind::EnableToken(_)
            | DecodedPortalCallKind::KnownIgnoredStateChange
            | DecodedPortalCallKind::Deposit
            | DecodedPortalCallKind::ClaimRefund => None,
        }
    }

    pub(crate) fn as_enable_token(&self) -> Option<&ZonePortal::enableTokenCall> {
        match &self.kind {
            DecodedPortalCallKind::EnableToken(call) => Some(call),
            DecodedPortalCallKind::SubmitBatch(_)
            | DecodedPortalCallKind::ProcessWithdrawals(_)
            | DecodedPortalCallKind::SetBouncebackGas(_)
            | DecodedPortalCallKind::KnownIgnoredStateChange
            | DecodedPortalCallKind::Deposit
            | DecodedPortalCallKind::ClaimRefund => None,
        }
    }

    pub(crate) const fn is_known_ignored_state_change(&self) -> bool {
        matches!(self.kind, DecodedPortalCallKind::KnownIgnoredStateChange)
    }

    pub(crate) const fn is_deposit(&self) -> bool {
        matches!(self.kind, DecodedPortalCallKind::Deposit)
    }

    pub(crate) const fn is_claim_refund(&self) -> bool {
        matches!(self.kind, DecodedPortalCallKind::ClaimRefund)
    }

    pub(crate) fn is_nonempty_process_withdrawals(&self) -> bool {
        self.as_process_withdrawals()
            .is_some_and(|call| !call.withdrawals.is_empty())
    }

    pub(crate) const fn family(&self) -> PortalCallFamily {
        match &self.kind {
            DecodedPortalCallKind::SubmitBatch(_) => PortalCallFamily::SubmitBatch,
            DecodedPortalCallKind::ProcessWithdrawals(_) => PortalCallFamily::ProcessWithdrawals,
            DecodedPortalCallKind::SetBouncebackGas(_) => PortalCallFamily::StateUpdate,
            DecodedPortalCallKind::EnableToken(_) => PortalCallFamily::StateUpdate,
            DecodedPortalCallKind::KnownIgnoredStateChange
            | DecodedPortalCallKind::Deposit
            | DecodedPortalCallKind::ClaimRefund => PortalCallFamily::StateUpdate,
        }
    }
}

/// A checked view over an ABI payload, excluding its four-byte selector.
/// Every helper checks integer conversion and range arithmetic before a
/// generated decoder can allocate from an attacker-controlled length word.
/// Validate nested ordinary-deposit offsets before decoding its generated ABI type.
fn preflight_ordinary_deposit(data: &[u8]) -> Result<(), AbiError> {
    let surface = Surface::new(DataSource::OrdinaryDepositData, data);
    if data.len() != ORDINARY_DEPOSIT_ENCODED_SIZE {
        return Err(surface.malformed(format!(
            "encoded deposit length {}, expected {ORDINARY_DEPOSIT_ENCODED_SIZE}",
            data.len()
        )));
    }
    let bounds = Bounds::from_data(surface, data);
    bounds.ensure_head(1)?;
    let deposit = bounds.relative(0, 0, 1)?;
    let encrypted = bounds.relative(deposit, 5, 6)?;
    let ciphertext = bounds.bytes_field(
        encrypted,
        2,
        5,
        ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE,
        "ciphertext",
    )?;
    if ciphertext.len() != ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE {
        return Err(surface.malformed(format!(
            "ciphertext length {}, expected {ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE}",
            ciphertext.len()
        )));
    }
    Ok(())
}

/// Bound every dynamic `advanceTempo` field before its generated decoder can allocate.
fn preflight_advance_tempo(calldata: &[u8]) -> Result<(), AbiError> {
    let surface = Surface::new(DataSource::AdvanceTempoCalldata, calldata);
    let bounds = Bounds::from_call(
        DataSource::AdvanceTempoCalldata,
        calldata,
        &IZoneInbox::advanceTempoCall::SELECTOR,
    )?;
    bounds.ensure_head(4)?;
    bounds.bytes_field(0, 0, 4, bounds.data.len(), "header")?;

    let (deposit_head, deposit_count) =
        bounds.dynamic_array(0, 1, 4, MAX_DEPOSITS_PER_TEMPO_BLOCK, "deposits")?;
    let mut ordinary_count = 0usize;
    for index in 0..deposit_count {
        let deposit = bounds.dynamic_element(deposit_head, deposit_count, index)?;
        let kind = bounds.usize_word(deposit)?;
        let data = bounds.bytes_field(deposit, 1, 2, bounds.data.len(), "depositData")?;
        match kind {
            0 => {
                if data.len() != 3 * WORD {
                    return Err(Surface::new(DataSource::WithdrawalBounceBackData, data)
                        .malformed(format!(
                            "withdrawal bounce-back depositData length {}, expected {}",
                            data.len(),
                            3 * WORD
                        )));
                }
            }
            1 => {
                ordinary_count += 1;
                preflight_ordinary_deposit(data)?;
            }
            other => {
                return Err(surface.malformed(format!("unsupported deposit discriminator {other}")));
            }
        }
    }

    let decryption_count =
        bounds.static_array(0, 2, 4, 4, MAX_DEPOSITS_PER_TEMPO_BLOCK, "decryptions")?;
    if decryption_count != ordinary_count {
        return Err(surface.malformed(format!(
                "decryption count {decryption_count} does not match ordinary deposit count {ordinary_count}"
            )));
    }

    let (token_head, token_count) =
        bounds.dynamic_array(0, 3, 4, MAX_TOKENS_ENABLED_PER_TEMPO_BLOCK, "enabledTokens")?;
    for index in 0..token_count {
        let token = bounds.dynamic_element(token_head, token_count, index)?;
        bounds.bytes_field(token, 1, 4, MAX_TOKEN_METADATA_BYTES, "token name")?;
        bounds.bytes_field(token, 2, 4, MAX_TOKEN_METADATA_BYTES, "token symbol")?;
        bounds.bytes_field(token, 3, 4, MAX_TOKEN_METADATA_BYTES, "token currency")?;
    }
    Ok(())
}

/// Strictly decode canonical `advanceTempo` calldata from its authenticated transaction.
pub(crate) fn decode_advance_tempo(
    calldata: &[u8],
    transaction: AuthenticatedTransaction,
) -> Result<DecodedAdvanceTempo, ObservationError> {
    parse_advance_tempo(calldata).map_err(|error| error.into_observation(transaction))
}

/// Parse `advanceTempo` calldata and reject oversized or non-canonical encodings.
fn parse_advance_tempo(calldata: &[u8]) -> Result<DecodedAdvanceTempo, AbiError> {
    let advance_surface = Surface::new(DataSource::AdvanceTempoCalldata, calldata);
    preflight_advance_tempo(calldata)?;
    let decoded = IZoneInbox::advanceTempoCall::abi_decode_validate(calldata)
        .map_err(|error| advance_surface.malformed(error))?;
    if decoded.abi_encode() != calldata {
        return Err(advance_surface.malformed("encoding is non-canonical or has trailing bytes"));
    }

    let header_surface = Surface::new(DataSource::AdvanceHeaderRlp, &decoded.header);
    let mut remaining = decoded.header.as_ref();
    let header =
        TempoHeader::decode(&mut remaining).map_err(|error| header_surface.malformed(error))?;
    if !remaining.is_empty() {
        return Err(header_surface.malformed(format!("{} trailing bytes", remaining.len())));
    }
    let mut canonical = Vec::with_capacity(header.length());
    header.encode(&mut canonical);
    if canonical != decoded.header {
        return Err(header_surface.malformed("non-canonical encoding"));
    }
    let imported_header = ImportedTempoHeader::new(header);

    let mut deposits = Vec::with_capacity(decoded.deposits.len());
    for queued in decoded.deposits {
        let data = queued.depositData;
        let deposit = match queued.depositType as u8 {
            0 => {
                let surface = Surface::new(DataSource::WithdrawalBounceBackData, data.as_ref());
                let decoded = IZoneInbox::WithdrawalBounceBackDeposit::abi_decode_validate(&data)
                    .map_err(|error| surface.malformed(error))?;
                if decoded.abi_encode() != data {
                    return Err(
                        surface.malformed("encoding is non-canonical or has trailing bytes")
                    );
                }
                ImportedDeposit {
                    kind: ImportedDepositKind::WithdrawalBounceBack(decoded),
                }
            }
            1 => {
                let surface = Surface::new(DataSource::OrdinaryDepositData, data.as_ref());
                let decoded = ZonePortal::Deposit::abi_decode_validate(&data)
                    .map_err(|error| surface.malformed(error))?;
                if decoded.abi_encode() != data {
                    return Err(
                        surface.malformed("encoding is non-canonical or has trailing bytes")
                    );
                }
                ImportedDeposit {
                    kind: ImportedDepositKind::Ordinary(decoded),
                }
            }
            _ => {
                return Err(advance_surface.malformed("unsupported deposit discriminator"));
            }
        };
        deposits.push(deposit);
    }

    Ok(DecodedAdvanceTempo {
        imported_header,
        deposits,
        enabled_tokens: decoded.enabledTokens,
    })
}

/// Bound finalization sender data before its generated decoder can allocate.
fn preflight_finalization(calldata: &[u8]) -> Result<(), AbiError> {
    let surface = Surface::new(DataSource::FinalizationCalldata, calldata);
    let bounds = Bounds::from_call(
        DataSource::FinalizationCalldata,
        calldata,
        &IZoneOutbox::finalizeWithdrawalBatchCall::SELECTOR,
    )?;
    bounds.ensure_head(3)?;
    let count = bounds.usize_word(0)?;
    let maximum = bounds.data.len() / WORD;
    let (sender_head, sender_count) = bounds.dynamic_array(0, 2, 3, maximum, "encryptedSenders")?;
    if count != sender_count {
        return Err(surface.malformed(format!(
            "count {count} does not match encryptedSenders length {sender_count}"
        )));
    }
    for index in 0..sender_count {
        let sender = bounds.dynamic_element(sender_head, sender_count, index)?;
        let bytes = bounds.direct_bytes(
            sender,
            AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE,
            "encrypted sender",
        )?;
        if !matches!(bytes.len(), 0 | AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE) {
            return Err(surface.malformed(format!(
                    "encrypted sender {index} has length {}, expected 0 or {AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE}",
                    bytes.len()
                )));
        }
    }
    Ok(())
}

/// Strictly decode canonical finalization calldata from its authenticated transaction.
pub(crate) fn decode_finalization(
    calldata: &[u8],
    transaction: AuthenticatedTransaction,
) -> Result<DecodedFinalization, ObservationError> {
    parse_finalization(calldata).map_err(|error| error.into_observation(transaction))
}

/// Parse finalization calldata and reject oversized or non-canonical encodings.
fn parse_finalization(calldata: &[u8]) -> Result<DecodedFinalization, AbiError> {
    let surface = Surface::new(DataSource::FinalizationCalldata, calldata);
    preflight_finalization(calldata)?;
    let call = IZoneOutbox::finalizeWithdrawalBatchCall::abi_decode_validate(calldata)
        .map_err(|error| surface.malformed(error))?;
    if call.abi_encode() != calldata {
        return Err(surface.malformed("encoding is non-canonical or has trailing bytes"));
    }
    let count =
        usize::try_from(call.count).map_err(|_| surface.malformed("count overflows usize"))?;
    Ok(DecodedFinalization {
        count,
        block_number: call.blockNumber,
        encrypted_senders: call.encryptedSenders,
    })
}

/// Bound every withdrawal field before decoding `processWithdrawals` calldata.
fn preflight_process_withdrawals(calldata: &[u8]) -> Result<(), AbiError> {
    let surface = Surface::new(DataSource::ProcessWithdrawalsCalldata, calldata);
    let bounds = Bounds::from_call(
        DataSource::ProcessWithdrawalsCalldata,
        calldata,
        &ZonePortal::processWithdrawalsCall::SELECTOR,
    )?;
    bounds.ensure_head(2)?;
    let maximum = bounds.data.len() / WORD;
    let (withdrawal_head, withdrawal_count) =
        bounds.dynamic_array(0, 0, 2, maximum, "withdrawals")?;
    for index in 0..withdrawal_count {
        let withdrawal = bounds.dynamic_element(withdrawal_head, withdrawal_count, index)?;
        bounds.bytes_field(withdrawal, 7, 9, MAX_CALLBACK_DATA_SIZE, "callbackData")?;
        let encrypted = bounds.bytes_field(
            withdrawal,
            8,
            9,
            AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE,
            "encryptedSender",
        )?;
        if !matches!(encrypted.len(), 0 | AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE) {
            return Err(surface.malformed(format!(
                    "encryptedSender {index} has length {}, expected 0 or {AUTHENTICATED_WITHDRAWAL_ENCRYPTED_SIZE}",
                    encrypted.len()
                )));
        }
    }
    Ok(())
}

/// Bound all byte strings and signatures before decoding `submitBatch` calldata.
fn preflight_submit_batch(calldata: &[u8]) -> Result<(), AbiError> {
    let bounds = Bounds::from_call(
        DataSource::SubmitBatchCalldata,
        calldata,
        &ZonePortal::submitBatchCall::SELECTOR,
    )?;
    bounds.ensure_head(13)?;
    let maximum = bounds.data.len();
    bounds.bytes_field(0, 9, 13, maximum, "verifierConfig")?;
    bounds.bytes_field(0, 10, 13, maximum, "proof")?;
    let (signature_head, signature_count) =
        bounds.dynamic_array(0, 12, 13, MAX_SEQUENCERS, "signatures")?;
    for index in 0..signature_count {
        let signature = bounds.dynamic_element(signature_head, signature_count, index)?;
        bounds.direct_bytes(signature, maximum, "signature")?;
    }
    Ok(())
}

/// Strictly decode the Portal calldata whose family was implied by receipt outcomes.
pub(crate) fn decode_portal_call(
    calldata: &[u8],
    transaction: AuthenticatedTransaction,
) -> Result<DecodedPortalCall, ObservationError> {
    parse_portal_call(calldata).map_err(|error| error.into_observation(transaction))
}

/// Parse a supported Portal call using the same trailing-byte tolerance as Solidity.
fn parse_portal_call(calldata: &[u8]) -> Result<DecodedPortalCall, AbiError> {
    if calldata.starts_with(&ZonePortal::submitBatchCall::SELECTOR) {
        return decode_submit_batch(calldata);
    }
    if calldata.starts_with(&ZonePortal::processWithdrawalsCall::SELECTOR) {
        return decode_process_withdrawals(calldata);
    }
    if calldata.starts_with(&ZonePortal::setBouncebackGasCall::SELECTOR) {
        let surface = Surface::new(DataSource::PortalTransactionCalldata, calldata);
        let call = ZonePortal::setBouncebackGasCall::abi_decode_validate(calldata)
            .map_err(|error| surface.malformed(error))?;
        return Ok(DecodedPortalCall {
            kind: DecodedPortalCallKind::SetBouncebackGas(call),
        });
    }
    if calldata.starts_with(&ZonePortal::enableTokenCall::SELECTOR) {
        let surface = Surface::new(DataSource::PortalTransactionCalldata, calldata);
        let call = ZonePortal::enableTokenCall::abi_decode_validate(calldata)
            .map_err(|error| surface.malformed(error))?;
        return Ok(DecodedPortalCall {
            kind: DecodedPortalCallKind::EnableToken(call),
        });
    }
    let kind = if calldata.starts_with(&ZonePortal::depositCall::SELECTOR)
        || calldata.starts_with(&ZonePortal::depositEncryptedCall::SELECTOR)
    {
        Some(DecodedPortalCallKind::Deposit)
    } else if calldata.starts_with(&ZonePortal::claimRefundCall::SELECTOR) {
        Some(DecodedPortalCallKind::ClaimRefund)
    } else if is_known_ignored_state_change(calldata) {
        Some(DecodedPortalCallKind::KnownIgnoredStateChange)
    } else {
        None
    };
    if let Some(kind) = kind {
        return Ok(DecodedPortalCall { kind });
    }
    Err(
        Surface::new(DataSource::PortalTransactionCalldata, calldata)
            .malformed("selector does not match its authenticated protocol events"),
    )
}

pub(crate) fn is_direct_portal_state_change(calldata: &[u8]) -> bool {
    calldata.starts_with(&ZonePortal::setBouncebackGasCall::SELECTOR)
        || calldata.starts_with(&ZonePortal::enableTokenCall::SELECTOR)
        || calldata.starts_with(&ZonePortal::depositCall::SELECTOR)
        || calldata.starts_with(&ZonePortal::depositEncryptedCall::SELECTOR)
        || calldata.starts_with(&ZonePortal::claimRefundCall::SELECTOR)
        || is_known_ignored_state_change(calldata)
}

fn is_known_ignored_state_change(calldata: &[u8]) -> bool {
    calldata.starts_with(&ZonePortal::pauseCall::SELECTOR)
        || calldata.starts_with(&ZonePortal::resumeCall::SELECTOR)
        || calldata.starts_with(&ZonePortal::abdicateCall::SELECTOR)
        || calldata.starts_with(&ZonePortal::pauseDepositsCall::SELECTOR)
        || calldata.starts_with(&ZonePortal::resumeDepositsCall::SELECTOR)
        || calldata.starts_with(&ZonePortal::setZoneGasRateCall::SELECTOR)
        || calldata.starts_with(&ZonePortal::setMaxTempoGasRateCall::SELECTOR)
        || calldata.starts_with(&ZonePortal::transferAdminCall::SELECTOR)
        || calldata.starts_with(&ZonePortal::acceptAdminCall::SELECTOR)
        || calldata.starts_with(&ZonePortal::setRpcUrlCall::SELECTOR)
        || calldata.starts_with(&ZonePortal::setSequencerEncryptionKeyCall::SELECTOR)
}

/// Preflight and decode chain-valid `submitBatch` calldata.
fn decode_submit_batch(calldata: &[u8]) -> Result<DecodedPortalCall, AbiError> {
    let surface = Surface::new(DataSource::SubmitBatchCalldata, calldata);
    preflight_submit_batch(calldata)?;
    let call = ZonePortal::submitBatchCall::abi_decode_validate(calldata)
        .map_err(|error| surface.malformed(error))?;
    Ok(DecodedPortalCall {
        kind: DecodedPortalCallKind::SubmitBatch(Box::new(call)),
    })
}

/// Preflight and decode chain-valid `processWithdrawals` calldata.
fn decode_process_withdrawals(calldata: &[u8]) -> Result<DecodedPortalCall, AbiError> {
    let surface = Surface::new(DataSource::ProcessWithdrawalsCalldata, calldata);
    preflight_process_withdrawals(calldata)?;
    let call = ZonePortal::processWithdrawalsCall::abi_decode_validate(calldata)
        .map_err(|error| surface.malformed(error))?;
    Ok(DecodedPortalCall {
        kind: DecodedPortalCallKind::ProcessWithdrawals(call),
    })
}

#[cfg(test)]
mod tests;
