//! Error types for the Privacy Zone ExEx.

use alloy_primitives::{Address, B256};
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};

/// Privacy Zone ExEx error type.
#[derive(Debug, thiserror::Error)]
pub enum PzError {
    /// Serialization error.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Invalid zone ID.
    #[error("invalid zone ID: {0}")]
    InvalidZoneId(u64),

    /// Invalid deposit.
    #[error("invalid deposit: {0}")]
    InvalidDeposit(String),

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

    /// Insufficient balance.
    #[error("insufficient balance: have {have}, need {need}")]
    InsufficientBalance { have: alloy_primitives::U256, need: alloy_primitives::U256 },

    /// Generic error.
    #[error("{0}")]
    Other(String),
}

impl From<eyre::Report> for PzError {
    fn from(err: eyre::Report) -> Self {
        Self::Other(err.to_string())
    }
}

/// Database error wrapper that implements DBErrorMarker for revm compatibility.
#[derive(Debug)]
pub struct PzDbError(pub eyre::Error);

impl Display for PzDbError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl Error for PzDbError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.0.source()
    }
}

impl From<eyre::Error> for PzDbError {
    fn from(value: eyre::Report) -> Self {
        Self(value)
    }
}

impl From<PzError> for PzDbError {
    fn from(value: PzError) -> Self {
        Self(eyre::eyre!(value))
    }
}

impl reth_revm::db::DBErrorMarker for PzDbError {}
