//! Hand-written helpers for the shared ABI types.

use crate::bindings::{Withdrawal, ZoneOutbox};
use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_sol_types::SolValue;
use zone_primitives::constants::EMPTY_SENTINEL;

impl Withdrawal {
    /// Build the authenticated-withdrawal sender plaintext `[sender(20) | tx_hash(32)]`.
    pub fn authenticated_sender_plaintext(sender: Address, tx_hash: B256) -> [u8; 52] {
        let mut plaintext = [0u8; 52];
        plaintext[..20].copy_from_slice(sender.as_slice());
        plaintext[20..].copy_from_slice(tx_hash.as_slice());
        plaintext
    }

    /// Compute the authenticated sender tag `keccak256(sender || tx_hash)`.
    pub fn sender_tag(sender: Address, tx_hash: B256) -> B256 {
        keccak256(Self::authenticated_sender_plaintext(sender, tx_hash))
    }

    /// Reconstruct the public L1-facing withdrawal from a zone-side withdrawal request event.
    pub fn from_requested_event(
        event: &ZoneOutbox::WithdrawalRequested,
        tx_hash: B256,
        encrypted_sender: Bytes,
    ) -> Self {
        Self {
            token: event.token,
            senderTag: Self::sender_tag(event.sender, tx_hash),
            to: event.to,
            amount: event.amount,
            fee: event.fee,
            memo: event.memo,
            gasLimit: event.gasLimit,
            fallbackRecipient: event.fallbackRecipient,
            callbackData: event.data.clone(),
            encryptedSender: encrypted_sender,
        }
    }

    /// Compute the withdrawal queue hash for a slice of withdrawals.
    ///
    /// The hash chain has the oldest withdrawal at the outermost layer for efficient FIFO removal:
    ///
    /// ```text
    /// hash = keccak256(encode(w[0], keccak256(encode(w[1], keccak256(encode(w[2], EMPTY_SENTINEL))))))
    /// ```
    ///
    /// Building proceeds from the newest (innermost) to the oldest (outermost).
    /// Returns `B256::ZERO` if `withdrawals` is empty.
    pub fn queue_hash(withdrawals: &[Self]) -> B256 {
        if withdrawals.is_empty() {
            return B256::ZERO;
        }

        let mut hash = EMPTY_SENTINEL;
        for w in withdrawals.iter().rev() {
            hash = keccak256((w.clone(), hash).abi_encode_params());
        }
        hash
    }
}
