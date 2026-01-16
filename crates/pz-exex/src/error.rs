//! Error types for the Privacy Zone ExEx.

use alloy_primitives::{Address, B256};

/// Privacy Zone ExEx error type.
#[derive(Debug, thiserror::Error)]
pub enum PzError {
    /// Database error.
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    /// Serialization error.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Invalid zone ID.
    #[error("invalid zone ID: {0}")]
    InvalidZoneId(u64),

    /// Invalid deposit.
    #[error("invalid deposit: {0}")]
    InvalidDeposit(String),

    /// Invalid exit intent.
    #[error("invalid exit intent: {0}")]
    InvalidExitIntent(String),

    /// Block not found.
    #[error("block not found: {0}")]
    BlockNotFound(u64),

    /// Account not found.
    #[error("account not found: {0}")]
    AccountNotFound(Address),

    /// Invalid state transition.
    #[error("invalid state transition: prev={prev}, expected={expected}")]
    InvalidStateTransition { prev: B256, expected: B256 },

    /// Deposit hash mismatch.
    #[error("deposit hash mismatch: expected={expected}, got={got}")]
    DepositHashMismatch { expected: B256, got: B256 },

    /// Generic error.
    #[error("{0}")]
    Other(String),
}

impl From<eyre::Report> for PzError {
    fn from(err: eyre::Report) -> Self {
        Self::Other(err.to_string())
    }
}
