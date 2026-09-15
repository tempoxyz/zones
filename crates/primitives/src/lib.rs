//! Primitive types used by the zone.
//!
//! This crate is `no_std` compatible so it can be used inside SP1 (RISC-V) guest
//! programs and TEE enclaves, as well as in the host-side prover.

#![cfg_attr(not(feature = "std"), no_std)]

pub mod constants;

use alloy_primitives::{Address, B256};

/// Canonical identity of an EVM storage slot read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct StorageReadKey {
    /// Account whose storage is read.
    pub account: Address,
    /// Storage slot key.
    pub slot: B256,
}

impl StorageReadKey {
    /// Creates a storage-read key from its account and raw slot.
    pub const fn new(account: Address, slot: B256) -> Self {
        Self { account, slot }
    }
}
