//! Stable evidence carried by checker findings.

use alloy_primitives::{Address, B256, U256};
use serde::{Deserialize, Serialize};

use crate::kernel::state::StateKey;

/// A typed expected or observed value recorded in a finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Datum {
    U64(u64),
    U128(u128),
    U256(U256),
    Address(Address),
    Hash(B256),
    Bool(bool),
    Bytes {
        length: u64,
        digest: B256,
    },
    /// A stable protocol discriminator, not display text.
    Code(u16),
}

/// Stable family of checker finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FindingCategory {
    Authentication,
    EffectMismatch,
    StateMismatch,
    Invariant,
    Unsupported,
    Observation,
    Continuity,
    CreationAnchor,
    SupplyMismatch,
    CollateralMismatch,
}

/// Optional protocol location of a finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum FindingLocation {
    Operation(u32),
    State(StateKey),
    Block,
    ImportedOperation(u32),
}

/// Stable, structured evidence for one checker divergence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Finding {
    pub(crate) category: FindingCategory,
    pub(crate) code: u16,
    pub(crate) location: Option<FindingLocation>,
    pub(crate) expected: Option<Datum>,
    pub(crate) actual: Option<Datum>,
}

impl Finding {
    /// Construct a finding from typed expected and observed evidence.
    pub(crate) fn new(
        category: FindingCategory,
        code: u16,
        location: Option<FindingLocation>,
        expected: Option<Datum>,
        actual: Option<Datum>,
    ) -> Self {
        Self {
            category,
            code,
            location,
            expected,
            actual,
        }
    }

    /// Construct a finding whose code is also its observed protocol value.
    pub(crate) fn coded(category: FindingCategory, code: u16, location: FindingLocation) -> Self {
        Self::new(
            category,
            code,
            Some(location),
            None,
            Some(Datum::Code(code)),
        )
    }
}

impl Datum {
    /// Canonical, version-independent bytes used for finding evidence identity.
    pub(crate) fn canonical_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(41);
        match self {
            Self::U64(v) => {
                out.push(0);
                out.extend(v.to_be_bytes());
            }
            Self::U128(v) => {
                out.push(1);
                out.extend(v.to_be_bytes());
            }
            Self::U256(v) => {
                out.push(2);
                out.extend(v.to_be_bytes::<32>());
            }
            Self::Address(v) => {
                out.push(3);
                out.extend(v.as_slice());
            }
            Self::Hash(v) => {
                out.push(4);
                out.extend(v.as_slice());
            }
            Self::Bool(v) => {
                out.push(5);
                out.push(u8::from(*v));
            }
            Self::Bytes { length, digest } => {
                out.push(6);
                out.extend(length.to_be_bytes());
                out.extend(digest.as_slice());
            }
            Self::Code(v) => {
                out.push(7);
                out.extend(v.to_be_bytes());
            }
        }
        out
    }
}
