//! Primitive types used by the zone.
//!
//! This crate is `no_std` compatible so it can be used inside SP1 (RISC-V) guest
//! programs and TEE enclaves, as well as in the host-side prover.

#![cfg_attr(not(feature = "std"), no_std)]

pub mod constants;

/// Return the L1 genesis anchor that makes a fresh zone replay its portal-creation block.
///
/// The creation block must come from the confirmed `createZone` receipt. Sampling the L1 head
/// before submission is unsafe because transaction inclusion can be delayed by one or more blocks.
pub const fn portal_creation_anchor(creation_block_number: u64) -> Option<u64> {
    creation_block_number.checked_sub(1)
}

#[cfg(test)]
mod tests {
    use super::portal_creation_anchor;

    #[test]
    fn anchor_replays_the_creation_block_regardless_of_inclusion_delay() {
        // Derived from the confirmed receipt, so a stale pre-submit head (100) cannot leak in
        // and start the replay before the portal exists.
        assert_eq!(portal_creation_anchor(105), Some(104));
        assert_eq!(portal_creation_anchor(101), Some(100));
    }

    #[test]
    fn genesis_creation_block_has_no_anchor() {
        assert_eq!(portal_creation_anchor(0), None);
    }
}
