//! Pure per-block protocol models and their shared value types.

use alloy_primitives::Address;

mod token_enabled;

pub(crate) use token_enabled::TokenEnablementModel;

/// Normalized token specification independent of protocol ABI types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TokenSpec {
    pub(crate) token: Address,
    pub(crate) name: String,
    pub(crate) symbol: String,
    pub(crate) currency: String,
}

/// Result of evaluating the token-enablement model for one block.
#[derive(Debug)]
pub(crate) enum TokenModelResult {
    /// All checks passed.
    Pass { token_count: usize },
    /// One or more typed violations were found.
    Violations(Vec<TokenModelViolation>),
}

/// One typed token-enablement model violation.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum TokenModelViolation {
    /// One L1 block enabled the same token more than once.
    DuplicateL1Token {
        token: Address,
        first_index: usize,
        duplicate_index: usize,
    },
    /// L2 `advanceTempo` calldata does not match expected from L1 events.
    L2CalldataMismatch {
        reason: CalldataMismatch,
        expected: Vec<TokenSpec>,
        observed: Vec<TokenSpec>,
    },
    /// L2 `ZoneInbox.TokenEnabled` events do not match expected from L1 events.
    L2EventMismatch {
        expected: Vec<TokenSpec>,
        observed: Vec<TokenSpec>,
    },
    /// L2 token state does not match expected.
    L2StateMismatch {
        token: Address,
        mismatch: StateMismatch,
    },
}

/// Kind of token state mismatch shared by L1 and L2 observations.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum StateMismatch {
    /// No state observation exists for an expected token.
    MissingObservation,
    /// A state observation exists for a token not expected by any event.
    UnexpectedObservation,
    /// L2: token account does not exist.
    NotEnabled,
    /// L2 token account exists but its precompile marker is not initialized.
    NotInitialized,
    /// Token metadata field does not match.
    MetadataMismatch {
        field: &'static str,
        expected: String,
        observed: String,
    },
    /// L2 only: Zone Inbox does not hold `ISSUER_ROLE`.
    MissingInboxRole,
    /// L2 only: Zone Outbox does not hold `ISSUER_ROLE`.
    MissingOutboxRole,
}

/// Reason the L2 `advanceTempo` calldata does not match expected.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) enum CalldataMismatch {
    /// No `advanceTempo` call was found.
    Missing,
    /// `advanceTempo` was called but failed (reverted).
    Failed,
    /// More than one successful `advanceTempo` call was found.
    Multiple,
    /// The successful call is not the opening Tempo system transaction.
    InvalidProvenance,
    /// The `enabledTokens` sequence does not match expected.
    TokenSequence,
}
