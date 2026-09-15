//! Pure TIP-1012 codecs and root authorization. Stateful execution belongs to ZoneInbox.
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::vec::Vec;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_sol_types::{SolStruct, SolType, SolValue, eip712_domain, sol_data};
type Plaintext = (sol_data::Uint<8>, ForcedExitAuthorization, sol_data::Bytes);
use tempo_primitives::transaction::PrimitiveSignature;
use tempo_zone_contracts::{DepositType, Withdrawal};
pub use tempo_zone_contracts::{
    ForcedExit, ForcedExitAuthorization, ForcedExitMetadata, ForcedExitOutcome, ForcedExitReason,
    ForcedExitStatus,
};

pub const VERSION: u8 = 1;
pub const FORCED_EXIT_COMPENSATION: u128 = 100_000;
pub const MIN_CIPHERTEXT_SIZE: usize = 384;
pub const MAX_CIPHERTEXT_SIZE: usize = 2368;
/// TIP-0001's maximum primitive encoding, including the type byte.
pub const MAX_SIGNATURE_SIZE: usize = 2049;

/// Admission checks length only; payload and authorization validity are determined on Zone.
pub const fn valid_ciphertext_length(length: usize) -> bool {
    length >= MIN_CIPHERTEXT_SIZE && length <= MAX_CIPHERTEXT_SIZE && length.is_multiple_of(32)
}

/// Encode the top-level ABI tuple, without an additional dynamic tuple wrapper.
pub fn encode_payload(auth: &ForcedExitAuthorization, signature: &[u8]) -> Vec<u8> {
    Plaintext::abi_encode_params(&(VERSION, auth.clone(), Bytes::copy_from_slice(signature)))
}

/// Decode only the payload category. Signature errors must remain InvalidAuthorization.
pub fn decode_payload(
    plaintext: &[u8],
) -> Result<(ForcedExitAuthorization, Bytes), ForcedExitReason> {
    if !valid_ciphertext_length(plaintext.len()) {
        return Err(ForcedExitReason::InvalidPayload);
    }
    let (version, auth, signature) =
        Plaintext::abi_decode_params(plaintext).map_err(|_| ForcedExitReason::InvalidPayload)?;
    if version != VERSION || encode_payload(&auth, &signature) != plaintext {
        return Err(ForcedExitReason::InvalidPayload);
    }
    Ok((auth, signature))
}

pub fn authorization_digest(
    auth: &ForcedExitAuthorization,
    l1_chain_id: U256,
    portal: Address,
) -> B256 {
    let mut domain = eip712_domain! {
        name: "TempoZoneForcedExit",
        version: "1",
        verifying_contract: portal,
    };
    domain.chain_id = Some(l1_chain_id);
    auth.eip712_signing_hash(&domain)
}

/// Check signed fields and root identity. Replay and balance/policy checks are separate stages.
/// `requested_at_time` must come from the authenticated queued entry, never the execution clock.
pub fn authorize(
    auth: &ForcedExitAuthorization,
    signature: &[u8],
    l1_chain_id: U256,
    portal: Address,
    zone_chain_id: U256,
    public_token: Address,
    requested_at_time: u64,
) -> Result<(), ForcedExitReason> {
    let invalid = ForcedExitReason::InvalidAuthorization;
    if auth.account.is_zero()
        || auth.recipient.is_zero()
        || auth.zoneChainId != zone_chain_id
        || auth.token != public_token
        || requested_at_time >= auth.admitBefore
        || signature.len() > MAX_SIGNATURE_SIZE
    {
        return Err(invalid);
    }
    // Decode primitives directly, never TempoSignature (which also accepts keychain envelopes).
    let primitive = PrimitiveSignature::from_bytes(signature).map_err(|_| invalid)?;
    // TIP-0001's P256 pre_hash is a boolean. The upstream decoder treats any nonzero byte as
    // true; the forced-exit envelope rejects noncanonical booleans instead of normalizing them.
    if matches!(primitive, PrimitiveSignature::P256(_)) && signature.last().is_some_and(|b| *b > 1)
    {
        return Err(invalid);
    }
    let signer = primitive
        .recover_signer(&authorization_digest(auth, l1_chain_id, portal))
        .map_err(|_| invalid)?;
    if signer != auth.account {
        return Err(invalid);
    }
    Ok(())
}

pub fn request_queue_hash(entry: &ForcedExit, previous: B256) -> B256 {
    keccak256((DepositType::ForcedExit, entry.clone(), previous).abi_encode_params())
}

pub fn sender_tag(portal: Address, request_id: u64) -> B256 {
    keccak256(("TempoZoneForcedExit", portal, request_id).abi_encode_params())
}

/// Hash the individual withdrawal, including its dynamic tuple offset and empty fields.
pub fn withdrawal_hash(withdrawal: &Withdrawal) -> B256 {
    keccak256(withdrawal.abi_encode())
}

pub fn outcomes_hash(outcomes: &[ForcedExitOutcome]) -> B256 {
    keccak256(outcomes.abi_encode())
}

/// Public shape validation; matching requests and actual execution is the settlement layer's job.
pub fn valid_outcome(outcome: &ForcedExitOutcome) -> bool {
    if outcome.requestId == 0 || outcome.depositNumber == 0 {
        return false;
    }
    match outcome.status {
        ForcedExitStatus::Exited => {
            outcome.reason == 0
                && outcome.amount > 0
                && !outcome.recipient.is_zero()
                && !outcome.withdrawalHash.is_zero()
        }
        ForcedExitStatus::Empty | ForcedExitStatus::Rejected => {
            let reason_valid = if outcome.status == ForcedExitStatus::Empty {
                outcome.reason == 0
            } else {
                (1..=5).contains(&outcome.reason)
            };
            reason_valid
                && outcome.amount == 0
                && outcome.recipient.is_zero()
                && outcome.withdrawalHash.is_zero()
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests;
