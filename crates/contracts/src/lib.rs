//! ABI bindings for the Tempo Zone protocol contracts.
//!
//! These bindings cover the contracts the sequencer interacts with across both layers:
//!
//! - **ZonePortal** — deployed on Tempo L1. Escrows gas tokens, manages the deposit queue,
//!   accepts batch proofs, and processes withdrawals back to L1 recipients.
//! - **ZoneOutbox** — deployed on the Zone L2. Collects user withdrawal requests, builds
//!   withdrawal hash chains, and exposes `LastBatch` state for proof generation.
//! - **ZoneInbox**, **TempoState**, **TempoStateReader**, **ZoneTxContext** — Zone L2 predeploys.
//! - **ZoneFactory**, **SwapAndDepositRouter** — deployed on Tempo L1.

#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
// auto-generated sol! builders for events/errors/functions with many fields trigger this
#![allow(clippy::too_many_arguments)]

extern crate alloc;

/// Helper macro to allow feature-gating rpc and serde implementations.
macro_rules! sol {
    ($($input:tt)*) => {
        #[cfg(all(feature = "rpc", feature = "serde"))]
        alloy_sol_types::sol! {
            #[sol(rpc)]
            #[derive(serde::Serialize, serde::Deserialize)]
            $($input)*
        }
        #[cfg(all(feature = "rpc", not(feature = "serde")))]
        alloy_sol_types::sol! {
            #[sol(rpc)]
            $($input)*
        }
        #[cfg(all(not(feature = "rpc"), feature = "serde"))]
        alloy_sol_types::sol! {
            #[derive(serde::Serialize, serde::Deserialize)]
            $($input)*
        }
        #[cfg(all(not(feature = "rpc"), not(feature = "serde")))]
        alloy_sol_types::sol! {
            $($input)*
        }
    };
}

pub(crate) use sol;

pub mod precompiles;

pub use precompiles::*;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloy_primitives::{B256, Bytes, U256, address, keccak256};
    use alloy_sol_types::{SolCall, SolValue};

    #[test]
    fn test_deposit_abi_encode_vs_params() {
        let d = Deposit {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 1000u128,
            bouncebackRecipient: address!("0x0000000000000000000000000000000000000001"),
            memo: B256::ZERO,
        };

        let encoded = d.abi_encode();
        let encoded_params = d.abi_encode_params();

        println!("abi_encode length: {}", encoded.len());
        println!("abi_encode_params length: {}", encoded_params.len());
        println!("abi_encode hex:\n{}", const_hex::encode(&encoded));
        println!(
            "abi_encode_params hex:\n{}",
            const_hex::encode(&encoded_params)
        );
        println!("Are they equal: {}", encoded == encoded_params);
    }

    #[test]
    fn test_queued_deposit_encoding() {
        let deposit = Deposit {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 1000u128,
            bouncebackRecipient: address!("0x0000000000000000000000000000000000000001"),
            memo: B256::ZERO,
        };

        let deposit_data = Bytes::from(deposit.abi_encode());

        let qd = QueuedDeposit {
            depositType: DepositType::Regular,
            depositData: deposit_data,
            rejected: false,
        };

        println!(
            "DepositType::Regular abi_encode: {}",
            const_hex::encode(DepositType::Regular.abi_encode())
        );
        println!(
            "deposit.abi_encode() length: {}",
            deposit.abi_encode().len()
        );
        println!(
            "deposit.abi_encode(): {}",
            const_hex::encode(deposit.abi_encode())
        );
        println!(
            "QueuedDeposit.abi_encode() length: {}",
            qd.abi_encode().len()
        );
        println!(
            "QueuedDeposit.abi_encode(): {}",
            const_hex::encode(qd.abi_encode())
        );

        // Now test the full advanceTempo call encoding
        let header_bytes = Bytes::from(vec![0xc0]); // minimal RLP empty list
        let calldata = ZoneInbox::advanceTempoCall {
            header: header_bytes,
            deposits: vec![qd],
            decryptions: vec![],
            enabledTokens: vec![],
        }
        .abi_encode();

        println!("\nadvanceTempo calldata length: {}", calldata.len());
        println!(
            "advanceTempo selector: 0x{}",
            const_hex::encode(&calldata[..4])
        );
        println!(
            "advanceTempo full calldata:\n{}",
            const_hex::encode(&calldata)
        );
    }

    #[test]
    fn test_deposit_hash_chain_matches_solidity() {
        let deposit = Deposit {
            token: address!("0x0000000000000000000000000000000000001000"),
            sender: address!("0x0000000000000000000000000000000000000001"),
            to: address!("0x0000000000000000000000000000000000000002"),
            amount: 1000u128,
            bouncebackRecipient: address!("0x0000000000000000000000000000000000000001"),
            memo: B256::ZERO,
        };
        let prev_hash = B256::ZERO;

        let solidity_encoding = (DepositType::Regular, deposit.clone(), prev_hash).abi_encode();
        let solidity_hash = keccak256(&solidity_encoding);

        let rust_encoding = (DepositType::Regular, deposit, prev_hash).abi_encode();
        let rust_hash = keccak256(&rust_encoding);

        assert_eq!(solidity_encoding, rust_encoding, "ABI encodings must match");
        assert_eq!(solidity_hash, rust_hash, "Deposit hash chains must match");
    }

    #[test]
    fn test_decryption_data_encoding_uses_trimmed_layout() {
        let shared_secret = B256::from([0x11; 32]);
        let shared_secret_y_parity = 0x02;
        let proof_s = B256::from([0x33; 32]);
        let proof_c = B256::from([0x44; 32]);

        let decryption = DecryptionData {
            sharedSecret: shared_secret,
            sharedSecretYParity: shared_secret_y_parity,
            cpProof: ChaumPedersenProof {
                s: proof_s,
                c: proof_c,
            },
        };

        let encoded = decryption.abi_encode();
        let mut expected_y_parity_word = [0u8; 32];
        expected_y_parity_word[31] = shared_secret_y_parity;

        assert_eq!(
            encoded.len(),
            4 * 32,
            "DecryptionData must encode as four ABI words"
        );
        assert_eq!(
            &encoded[0..32],
            shared_secret.as_slice(),
            "word 0 is sharedSecret"
        );
        assert_eq!(
            &encoded[32..64],
            expected_y_parity_word,
            "word 1 is sharedSecretYParity"
        );
        assert_eq!(&encoded[64..96], proof_s.as_slice(), "word 2 is cpProof.s");
        assert_eq!(&encoded[96..128], proof_c.as_slice(), "word 3 is cpProof.c");
    }

    #[test]
    fn test_router_plaintext_callback_encoding_matches_tuple() {
        let callback = SwapAndDepositRouterPlaintextCallback {
            token_out: address!("0x0000000000000000000000000000000000001001"),
            target_portal: address!("0x0000000000000000000000000000000000002001"),
            recipient: address!("0x0000000000000000000000000000000000003001"),
            memo: B256::from([0x11; 32]),
            min_amount_out: 1234,
        };

        let tuple_encoding = (
            false,
            callback.token_out,
            callback.target_portal,
            callback.recipient,
            callback.memo,
            callback.min_amount_out,
        )
            .abi_encode_params();

        assert_eq!(callback.abi_encode(), tuple_encoding);
    }

    #[test]
    fn test_sender_tag_matches_plaintext_hash() {
        let sender = address!("0x0000000000000000000000000000000000000001");
        let tx_hash = B256::repeat_byte(0x22);
        let plaintext = Withdrawal::authenticated_sender_plaintext(sender, tx_hash);

        assert_eq!(&plaintext[..20], sender.as_slice());
        assert_eq!(&plaintext[20..], tx_hash.as_slice());
        assert_eq!(
            Withdrawal::sender_tag(sender, tx_hash),
            keccak256(plaintext)
        );
    }

    #[test]
    fn test_router_encrypted_callback_encoding_matches_tuple() {
        let encrypted = EncryptedDepositPayload {
            ephemeralPubkeyX: B256::from([0x22; 32]),
            ephemeralPubkeyYParity: 0x02,
            ciphertext: Bytes::from(vec![0xaa, 0xbb, 0xcc, 0xdd]),
            nonce: [0x33; 12].into(),
            tag: [0x44; 16].into(),
        };
        let callback = SwapAndDepositRouterEncryptedCallback {
            token_out: address!("0x0000000000000000000000000000000000001002"),
            target_portal: address!("0x0000000000000000000000000000000000002002"),
            key_index: U256::from(7),
            encrypted: encrypted.clone(),
            min_amount_out: 5678,
        };

        let tuple_encoding = (
            true,
            callback.token_out,
            callback.target_portal,
            callback.key_index,
            encrypted,
            callback.min_amount_out,
        )
            .abi_encode_params();

        assert_eq!(callback.abi_encode(), tuple_encoding);
    }
}
